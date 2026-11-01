//! The MCP server: newline-delimited JSON-RPC 2.0 over stdio, read synchronously one bounded line at
//! a time.
//!
//! - A line longer than [`LINE_MAX`] is answered with an error and skipped up to its newline.
//! - Unparsable JSON gets `-32700`, a batch (array) or a malformed request `-32600`, an unknown
//!   method `-32601`, and serving goes on after each.
//! - Notifications are never answered, answers from the client are ignored.
//! - Tool validation and store failures are tool results with `isError: true`.
//! - Nothing a client sends is written to stderr.
//! - The tools read the documentation ([`calls`]) and `knowledge_add` writes only to the review
//!   queue ([`add`]), never to the database. Every document text in a reply is wrapped as untrusted
//!   ([`crate::search::render`]).
//! - The store opens on the first call that needs it and is opened again after a database failure,
//!   so a team database that was down, or a local one that did not exist yet, is picked up later.

mod add;
mod calls;
mod folder;
mod get;
mod list;
pub mod protocol;
mod reply;
mod scope;
pub mod tools;

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

use crate::backend::{self, Access};
use crate::config::{Backend, Selection};
use crate::held::HeldStore;
use crate::project::ProjectResolver;
use crate::search::{AutoEmbedder, QueryEmbedder};
use crate::store::{KnowledgeStore, StoreError};
use crate::support::Env;
use protocol::{INVALID_PARAMS, INVALID_REQUEST, Line, METHOD_NOT_FOUND, PARSE_ERROR};

pub use crate::held::CONTENT_MAX;

/// The longest line a client may send, in bytes.
pub const LINE_MAX: usize = 4 * 1024 * 1024;

/// Most characters of a search query.
pub const QUERY_MAX: usize = 1000;

/// Results of `knowledge_search` when `limit` is not given, and the most it may ask for.
pub const SEARCH_LIMIT_DEFAULT: usize = 5;
pub const SEARCH_LIMIT_MAX: usize = 10;

/// Most paths of one search.
pub const PATHS_MAX: usize = 20;

/// Chunks of context `knowledge_get` may add on each side.
pub const NEIGHBORS_MAX: usize = 2;

/// The size of a `knowledge_get` reply when `max_bytes` is not given, and the most it may ask for.
pub const GET_BYTES_DEFAULT: usize = 16 * 1024;
pub const GET_BYTES_MAX: usize = 64 * 1024;

/// The line a search starts with when the query could not be embedded: the first time in a session.
pub const VECTORS_UNAVAILABLE: &str = "(vector search unavailable: keyword results only)";

/// The state one server process keeps: where the database is, the lazily opened store, the
/// embedder for query vectors, the review queue and the client's name.
pub struct Server {
    env: Env,
    backend: Backend,
    store: Option<Box<dyn KnowledgeStore>>,
    projects: ProjectResolver,
    embedder: Box<dyn QueryEmbedder + Send>,
    held: Option<HeldStore>,
    client: Option<String>,
    /// `HARNESS_KNOWLEDGE_SESSION`, recorded in every proposal.
    session: Option<String>,
    /// Whether the line saying vector search was unavailable was shown already.
    vector_notice: bool,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// A server over the database `backend` names, with `embedder` for the vectors of queries.
    pub fn new(env: Env, backend: Backend, projects: ProjectResolver, embedder: Box<dyn QueryEmbedder + Send>) -> Self {
        let held = HeldStore::open(&env);
        let session = env
            .var("HARNESS_KNOWLEDGE_SESSION")
            .map(str::trim)
            .filter(|session| !session.is_empty())
            .map(|session| session.chars().take(200).collect());

        Self {
            env,
            backend,
            store: None,
            projects,
            embedder,
            held,
            client: None,
            session,
            vector_notice: false,
        }
    }

    /// The server of this computer's settings: the embedder process first, then the model in this
    /// process when it is installed. `cwd` is the folder the server runs in.
    pub fn open(env: &Env, selection: &Selection, cwd: Option<std::path::PathBuf>) -> Self {
        let projects = ProjectResolver::new(env.clone(), cwd);

        let embedder: Box<dyn QueryEmbedder + Send> = match crate::embed::data_folder(env) {
            Some(root) => Box::new(AutoEmbedder::new(&root)),
            None => Box::new(NoEmbedder),
        };

        Self::new(env.clone(), selection.backend.clone(), projects, embedder)
    }

    /// The store, opened now when it is not yet: `Ok(None)` while there is no database (a read
    /// never creates it).
    fn store(&mut self) -> Result<Option<&mut (dyn KnowledgeStore + 'static)>, String> {
        open_store(&mut self.store, &self.env, &self.backend)
    }

    /// The message of a store failure, safe to show. A database failure drops the connection, so the
    /// next call opens it again.
    fn failed(&mut self, error: &StoreError) -> String {
        if matches!(error, StoreError::Backend(_)) {
            self.store = None;
        }

        self.backend.secrets().scrub(&error.to_string())
    }

    /// Answers one parsed message; `None` for notifications and client answers.
    pub fn handle(&mut self, message: Value) -> Option<Value> {
        let null = Value::Null;

        let Some(object) = message.as_object() else {
            return Some(protocol::error(&null, INVALID_REQUEST, "Invalid Request"));
        };

        let id = object.get("id");
        let method = object.get("method").and_then(Value::as_str);
        let valid_id = id.is_none_or(|id| id.is_string() || id.is_number() || id.is_null());
        let reply_id = id.filter(|_| valid_id).cloned().unwrap_or(Value::Null);

        let Some(method) = method else {
            let is_answer = id.is_some() && (object.contains_key("result") || object.contains_key("error"));

            return (!is_answer).then(|| protocol::error(&reply_id, INVALID_REQUEST, "Invalid Request"));
        };

        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !valid_id {
            return Some(protocol::error(&reply_id, INVALID_REQUEST, "Invalid Request"));
        }

        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));

        // A notification (no id) is never answered, whatever its method.
        let id = id?;

        let answer = match method {
            "initialize" => {
                self.client = params
                    .pointer("/clientInfo/name")
                    .and_then(Value::as_str)
                    .map(|name| crate::store::clean::line(name).chars().take(64).collect::<String>())
                    .filter(|name| !name.is_empty());

                self.embedder.warm();

                protocol::result(id, protocol::initialize_result(&params))
            }
            "ping" => protocol::result(id, json!({})),
            "tools/list" => protocol::result(id, tools::definitions()),
            "tools/call" => self.tools_call(id, &params),
            _ => protocol::error(id, METHOD_NOT_FOUND, "Method not found"),
        };

        Some(answer)
    }

    fn tools_call(&mut self, id: &Value, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return protocol::error(id, INVALID_PARAMS, "tools/call needs a tool name");
        };

        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

        match calls::call(self, name, arguments) {
            Some(reply) => protocol::result(id, tool_result(&reply)),
            None => protocol::error(id, INVALID_PARAMS, &format!("unknown tool: {}", short(name))),
        }
    }
}

/// The store in `slot`, opened now when it is not yet: `Ok(None)` while there is no database (a read
/// never creates it). Takes the fields apart so a search can hold the store and the embedder at once.
fn open_store<'a>(
    slot: &'a mut Option<Box<dyn KnowledgeStore>>,
    env: &Env,
    backend: &Backend,
) -> Result<Option<&'a mut (dyn KnowledgeStore + 'static)>, String> {
    if slot.is_none() {
        *slot =
            backend::open(env, backend, Access::Read).map_err(|error| backend.secrets().scrub(&error.to_string()))?;
    }

    Ok(slot.as_deref_mut())
}

/// An embedder for a computer whose data folder is unknown: it never has a vector.
struct NoEmbedder;

impl QueryEmbedder for NoEmbedder {
    fn embed_query(&mut self, _text: &str) -> Result<Vec<f32>, String> {
        Err("the data folder is unknown".to_owned())
    }
}

/// The `tools/call` result for `reply`.
fn tool_result(reply: &calls::Reply) -> Value {
    json!({
        "content": [{ "type": "text", "text": reply.text }],
        "isError": reply.is_error,
    })
}

/// Serves messages from `reader` until its end; fails only when reading or writing fails.
pub fn serve<R: BufRead, W: Write>(mut reader: R, mut writer: W, server: &mut Server) -> io::Result<()> {
    let mut buffer = Vec::new();

    loop {
        let reply = match protocol::read_line(&mut reader, &mut buffer, LINE_MAX)? {
            Line::End => return Ok(()),
            Line::TooLong => Some(protocol::error(
                &Value::Null,
                INVALID_REQUEST,
                &format!("message exceeds {LINE_MAX} bytes"),
            )),
            Line::Text(line) if line.iter().all(u8::is_ascii_whitespace) => None,
            Line::Text(line) => match serde_json::from_slice::<Value>(&line) {
                Ok(message) => server.handle(message),
                Err(_) => Some(protocol::error(&Value::Null, PARSE_ERROR, "Parse error")),
            },
        };

        if let Some(reply) = reply {
            protocol::write_message(&mut writer, &reply)?;
        }
    }
}

/// `harness-knowledge mcp`: serves MCP over stdio until the client closes its end.
pub fn run() -> std::process::ExitCode {
    use std::process::ExitCode;

    let Some((env, selection)) = crate::cli::settings() else {
        return ExitCode::FAILURE;
    };

    let mut server = Server::open(&env, &selection, std::env::current_dir().ok());
    let stdin = std::io::BufReader::new(std::io::stdin());
    let mut stdout = std::io::stdout().lock();
    let served = serve(stdin, &mut stdout, &mut server);
    stdout.flush().ok();

    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("harness-knowledge: stdio failed ({})", error.kind());
            ExitCode::FAILURE
        }
    }
}

/// At most 64 characters of agent text, for error messages.
fn short(text: &str) -> String {
    text.chars().take(64).collect()
}

#[cfg(test)]
mod tests;
