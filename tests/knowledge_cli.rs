//! The search, recall, review queue and MCP commands as real processes: their exit codes and what
//! they print, from an agent's proposal to a search that finds the approved document.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const BINARY: &str = env!("CARGO_BIN_EXE_harness-knowledge");

/// The command with its own home and knowledge folder, so nothing of the machine's is read.
fn command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(BINARY);

    command
        .args(args)
        .env_clear()
        .env("HOME", root)
        .env("HARNESS_KNOWLEDGE_DIR", root.join("knowledge"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());

    command
}

/// Runs a command with `stdin`: its exit code and standard output.
fn run(mut command: Command, stdin: &[u8]) -> (Option<i32>, String) {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("starts");

    // The command may end before it reads everything.
    let _ = child.stdin.take().expect("stdin").write_all(stdin);

    let output = child.wait_with_output().expect("finishes");

    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

fn json(root: &Path, args: &[&str]) -> (Option<i32>, Value) {
    let (code, out) = run(command(root, args), b"");

    (
        code,
        serde_json::from_str(out.trim()).unwrap_or_else(|_| panic!("JSON from {args:?}: {out:?}")),
    )
}

#[test]
fn features_list_what_this_build_can_do() {
    let root = tempfile::tempdir().expect("temp dir");
    let (code, out) = run(command(root.path(), &["features"]), b"");
    let features: Vec<&str> = out.lines().collect();

    assert_eq!(code, Some(0));

    for feature in [
        "docs",
        "embedder",
        "search",
        "recall",
        "held",
        "mcp",
        "offline-fallback",
        "docs-useful",
        "docs-similar",
        "docs-authors",
        "docs-drafts",
        "docs-approval",
        "docs-sort-paths",
    ] {
        assert!(features.contains(&feature), "{feature} in {features:?}");
    }
}

/// The arguments, the standard input and an environment variable of one run.
type Attempt<'a> = (Vec<&'a str>, &'a [u8], Option<(&'a str, &'a str)>);

#[test]
fn recall_always_exits_zero_and_prints_nothing_when_there_is_nothing_to_recall() {
    let root = tempfile::tempdir().expect("temp dir");
    let prompt = b"How do I install the gateway and restart it afterwards?";

    let attempts: Vec<Attempt> = vec![
        // No database.
        (vec!["recall", "--hook"], prompt, None),
        (vec!["recall", "--hook", "--project", "/no/such/folder"], prompt, None),
        // Wrong arguments, and nothing to read.
        (vec!["recall"], prompt, None),
        (vec!["recall", "--bogus"], prompt, None),
        (vec!["recall", "--hook", "extra"], prompt, None),
        (vec!["recall", "--hook"], b"", None),
        (vec!["recall", "--hook"], &[0xff, 0xfe, 0x00, 0x80], None),
        // Settings that are wrong, a team database that cannot be reached.
        (
            vec!["recall", "--hook"],
            prompt,
            Some(("HARNESS_KNOWLEDGE_DATABASE_URL", "mysql://nobody@localhost/x")),
        ),
        (
            vec!["recall", "--hook"],
            prompt,
            Some(("HARNESS_KNOWLEDGE_DATABASE_URL", "postgres://u:p@127.0.0.1:1/db")),
        ),
        (
            vec!["recall", "--hook"],
            prompt,
            Some(("HARNESS_HOOK_PROJECT", "/no/such/folder")),
        ),
    ];

    for (args, stdin, env) in attempts {
        let mut cmd = command(root.path(), &args);

        if let Some((name, value)) = env {
            cmd.env(name, value);
        }

        let started = std::time::Instant::now();
        let (code, out) = run(cmd, stdin);

        assert_eq!(code, Some(0), "{args:?} {env:?}");
        assert_eq!(out, "", "{args:?} {env:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{args:?}: {:?}",
            started.elapsed()
        );
    }

    assert!(
        !root.path().join("knowledge").exists(),
        "recall never creates the database"
    );
}

#[test]
fn search_and_held_read_an_empty_machine_without_creating_anything() {
    let root = tempfile::tempdir().expect("temp dir");

    assert_eq!(
        json(root.path(), &["search", "--json", "--query", "gateway"]),
        (Some(0), json!([]))
    );
    assert_eq!(
        json(root.path(), &["held", "list", "--json"]),
        (Some(0), json!({ "held": [] }))
    );
    assert!(!root.path().join("knowledge").exists());

    let (code, _) = run(command(root.path(), &["search", "--json"]), b"");
    assert_eq!(code, Some(2), "a usage error");
}

/// An MCP session over the standard streams of a real server.
struct Session {
    child: std::process::Child,
    reader: BufReader<std::process::ChildStdout>,
    next: i64,
}

impl Session {
    fn start(root: &Path, args: &[&str]) -> Self {
        let mut child = command(root, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("starts");
        let reader = BufReader::new(child.stdout.take().expect("stdout"));

        Self { child, reader, next: 1 }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;

        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{line}").expect("write");
        stdin.flush().expect("flush");

        let mut answer = String::new();
        self.reader.read_line(&mut answer).expect("read");
        let reply: Value = serde_json::from_str(&answer).unwrap_or_else(|_| panic!("a JSON reply: {answer:?}"));

        assert_eq!(reply["id"], id);
        reply
    }

    fn call(&mut self, name: &str, arguments: Value) -> String {
        let reply = self.request("tools/call", json!({ "name": name, "arguments": arguments }));

        reply["result"]["content"][0]["text"].as_str().expect("text").to_owned()
    }

    /// Closes the input, the way a harness ends its server: the exit code.
    fn end(mut self) -> Option<i32> {
        drop(self.child.stdin.take());
        self.child.wait().expect("exits").code()
    }
}

#[test]
fn a_proposal_goes_from_an_agent_to_review_to_a_search_that_finds_it() {
    let root = tempfile::tempdir().expect("temp dir");
    let project = root.path().join("project");
    std::fs::create_dir(&project).expect("project");
    let project = dunce::canonicalize(&project).expect("canonical");

    // `harness-knowledge` without an argument is the MCP server.
    let mut session = Session::start(root.path(), &[]);
    let initialized = session.request(
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "clientInfo": { "name": "e2e" } }),
    );
    assert_eq!(initialized["result"]["serverInfo"]["name"], "harness-knowledge");

    let listed = session.request("tools/list", json!({}));
    assert_eq!(listed["result"]["tools"].as_array().map(Vec::len), Some(5));

    assert_eq!(
        session.call(
            "knowledge_search",
            json!({ "query": "zebrafish", "project_path": project })
        ),
        "no documents yet"
    );

    let reply = session.call(
        "knowledge_add",
        json!({
            "title": "Zebrafish runbook",
            "content": "# Zebrafish runbook\n\nRestart the zebrafish gateway with the zebrafish command.",
            "project_path": project,
        }),
    );
    let id = reply
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("held:"))
        .expect("held:<id>")
        .to_owned();

    assert!(
        !root.path().join("knowledge").join("knowledge.sqlite3").exists(),
        "the proposal wrote no database"
    );

    // The same server finds nothing yet, and the review queue lists the proposal.
    let before = session.call(
        "knowledge_search",
        json!({ "query": "zebrafish", "project_path": project }),
    );
    assert_eq!(before, "no documents yet");

    let (code, listed) = json(root.path(), &["held", "list", "--json"]);
    assert_eq!(
        (code, listed["held"][0]["heldId"].as_str()),
        (Some(0), Some(id.as_str()))
    );

    let (code, approved) = json(root.path(), &["held", "approve", "--json", &id]);
    assert_eq!(code, Some(0), "{approved}");
    assert_eq!(approved["approved"]["status"], "added");

    // The database appeared after the server started, and the server picks it up.
    let after = session.call(
        "knowledge_search",
        json!({ "query": "zebrafish gateway", "project_path": project }),
    );
    assert!(after.contains("title=\"Zebrafish runbook\""), "{after}");
    assert!(after.contains("Restart the zebrafish gateway"), "{after}");

    let (code, found) = json(
        root.path(),
        &[
            "search",
            "--json",
            "--query",
            "zebrafish gateway",
            "--project",
            &project.to_string_lossy(),
        ],
    );
    assert_eq!(code, Some(0));
    assert_eq!(found[0]["title"], "Zebrafish runbook");

    let (code, listed) = json(root.path(), &["held", "list", "--json"]);
    assert_eq!((code, listed), (Some(0), json!({ "held": [] })));

    assert_eq!(session.end(), Some(0), "closing the input ends the server");

    // And a recall hook, with no embedder, stays quiet for a prompt the keywords alone cannot vouch for.
    let (code, out) = run(
        command(
            root.path(),
            &["recall", "--hook", "--project", &project.to_string_lossy()],
        ),
        b"How do I restart the zebrafish gateway?",
    );
    assert_eq!(code, Some(0));
    assert!(
        out.is_empty() || out.starts_with("<harness-knowledge-recall>\n"),
        "{out}"
    );
}

#[test]
fn a_rejected_proposal_never_reaches_the_database() {
    let root = tempfile::tempdir().expect("temp dir");
    let project = dunce::canonicalize(root.path()).expect("canonical").join("project");
    std::fs::create_dir(&project).expect("project");

    let mut session = Session::start(root.path(), &["mcp"]);
    session.request("initialize", json!({ "clientInfo": { "name": "e2e" } }));
    let reply = session.call(
        "knowledge_add",
        json!({ "title": "Unwanted", "content": "Some text nobody wants.", "project_path": project }),
    );
    let id = reply
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("held:"))
        .expect("held:<id>")
        .to_owned();
    assert_eq!(session.end(), Some(0));

    let (code, rejected) = json(root.path(), &["held", "reject", "--json", &id]);

    assert_eq!((code, rejected), (Some(0), json!({ "rejected": id })));
    assert!(!root.path().join("knowledge").join("knowledge.sqlite3").exists());

    let (code, again) = json(root.path(), &["held", "reject", "--json", &id]);
    assert_eq!((code, again["error"].as_str()), (Some(1), Some("notFound")));
}
