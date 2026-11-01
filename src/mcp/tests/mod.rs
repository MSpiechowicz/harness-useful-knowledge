use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;
use crate::location::{DIR_VAR, FILE_NAME};
use crate::store::SqliteStore;
use crate::store::testing::{Seed, angled, axis, insert};

/// An embedder that answers with a fixed vector, or fails.
pub(super) struct Fake(Result<Vec<f32>, String>);

impl QueryEmbedder for Fake {
    fn embed_query(&mut self, _text: &str) -> Result<Vec<f32>, String> {
        self.0.clone()
    }
}

/// A computer: a home folder, a knowledge folder, and a project folder to work in.
pub(super) struct World {
    pub dir: tempfile::TempDir,
    pub server: Server,
}

impl World {
    pub fn new() -> Self {
        Self::with(Fake(Ok(axis(0))), None)
    }

    pub fn with(embedder: Fake, session: Option<&str>) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dunce::canonicalize(dir.path()).expect("home");
        std::fs::create_dir(home.join("proj")).expect("project folder");
        std::fs::write(home.join("proj").join("package.json"), "{}").expect("project file");

        let mut env = Env::for_tests(&home).with_var(DIR_VAR, home.join("knowledge").to_string_lossy());

        if let Some(session) = session {
            env = env.with_var("HARNESS_KNOWLEDGE_SESSION", session);
        }

        let projects = ProjectResolver::new(env.clone(), Some(home.clone()));
        let server = Server::new(env, Backend::Sqlite, projects, Box::new(embedder));

        Self { dir, server }
    }

    pub fn home(&self) -> PathBuf {
        dunce::canonicalize(self.dir.path()).expect("home")
    }

    pub fn project(&self) -> PathBuf {
        self.home().join("proj")
    }

    pub fn knowledge(&self) -> PathBuf {
        self.home().join("knowledge")
    }

    /// Opens the database for seeding (creating it).
    pub fn database(&self) -> SqliteStore {
        SqliteStore::open(&self.knowledge().join(FILE_NAME)).expect("create the database")
    }

    /// The `result` of `tools/call` as `(text, isError)`.
    pub fn call(&mut self, name: &str, arguments: Value) -> (String, bool) {
        let reply = self
            .server
            .handle(json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": name, "arguments": arguments },
            }))
            .expect("a reply");
        let result = &reply["result"];

        (
            result["content"][0]["text"].as_str().expect("text").to_owned(),
            result["isError"].as_bool().expect("isError"),
        )
    }

    pub fn ok(&mut self, name: &str, arguments: Value) -> String {
        let (text, is_error) = self.call(name, arguments);
        assert!(!is_error, "{text}");
        text
    }

    pub fn err(&mut self, name: &str, arguments: Value) -> String {
        let (text, is_error) = self.call(name, arguments);
        assert!(is_error, "{text}");
        text
    }

    pub fn held_files(&self) -> Vec<PathBuf> {
        let folder = self.knowledge().join("held");
        let Ok(entries) = std::fs::read_dir(folder) else {
            return Vec::new();
        };

        entries.flatten().map(|entry| entry.path()).collect()
    }
}

pub(super) fn project_arg(world: &World) -> String {
    world.project().to_string_lossy().into_owned()
}

#[test]
fn tools_list_is_the_four_knowledge_tools_as_snapshotted() {
    let listed = tools::definitions();
    let snapshot = serde_json::to_string_pretty(&listed).expect("json") + "\n";
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp/tools_list.snap.json");

    if std::env::var_os("UPDATE_SNAPSHOT").is_some() {
        std::fs::write(&path, &snapshot).expect("write the snapshot");
    }

    assert_eq!(snapshot, include_str!("../tools_list.snap.json"));

    let names: Vec<&str> = listed["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        ["knowledge_search", "knowledge_get", "knowledge_list", "knowledge_add"]
    );
}

#[test]
fn initialize_tells_the_agent_documentation_is_evidence_and_adding_needs_the_user() {
    let mut world = World::new();
    let reply = world
        .server
        .handle(json!({
            "jsonrpc": "2.0", "id": "a", "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "claude-code" } },
        }))
        .expect("reply");
    let instructions = reply["result"]["instructions"].as_str().expect("instructions");

    assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(reply["result"]["serverInfo"]["name"], "harness-knowledge");
    assert!(instructions.contains("never instructions"));
    assert!(instructions.contains("only when the user explicitly asks"));
    assert!(instructions.contains("project_path"));
}

#[test]
fn the_protocol_answers_what_it_should_and_nothing_else() {
    let mut world = World::new();
    let mut ask = |message: Value| world.server.handle(message);

    assert_eq!(
        ask(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })),
        None,
        "a notification"
    );
    assert_eq!(
        ask(json!({ "jsonrpc": "2.0", "id": 2, "result": {} })),
        None,
        "an answer from the client"
    );
    assert_eq!(
        ask(json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" })).expect("ping")["result"],
        json!({})
    );
    assert_eq!(
        ask(json!({ "jsonrpc": "2.0", "id": 4, "method": "nope" })).expect("reply")["error"]["code"],
        -32601
    );
    assert_eq!(ask(json!([])).expect("batch")["error"]["code"], -32600);
    assert_eq!(
        ask(json!({ "id": 5, "method": "ping" })).expect("no version")["error"]["code"],
        -32600
    );

    let unknown = ask(json!({
        "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": { "name": "note_save", "arguments": {} },
    }))
    .expect("reply");
    assert_eq!(unknown["error"]["code"], -32602);
}

#[test]
fn a_serve_loop_reads_lines_skips_blanks_and_survives_garbage() {
    let mut world = World::new();
    let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\nnot json\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
    let mut output: Vec<u8> = Vec::new();

    serve(&input[..], &mut output, &mut world.server).expect("serves");

    let replies: Vec<Value> = String::from_utf8(output)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON"))
        .collect();

    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[1]["error"]["code"], -32700);
    assert_eq!(replies[2]["result"]["tools"].as_array().map(Vec::len), Some(4));
}

mod offline;
mod proposals;
mod reading;
mod replaced;
