//! What the embedder and its clients say to each other: one JSON line per request and one per
//! reply, one request per connection, every request carrying `"v":1`.
//!
//! A reply is `{"ok":true,...}` or `{"ok":false,"error":"..."}`:
//!
//! - `ping` answers `{ok, ready, model}`: `ready` is true once the model is downloaded and has
//!   loaded once (it is dropped when idle and loaded again by the next text);
//! - `warm` answers `{ok}` at once and then loads the model if it was unloaded;
//! - `embed_query {text}` answers `{ok, model, vector}`, the vector of `query: ` and the text;
//! - `embed_passages {texts}` (at most [`PASSAGES_MAX`]) answers `{ok, model, vectors}`, the vectors
//!   of `passage: ` and each text, in order;
//! - `status` answers `{ok, ready, loaded, model, dim, workers, queued_queries, queued_passages}`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The protocol version in every request.
pub const VERSION: u32 = 1;

/// The longest request line, in bytes.
pub const MAX_REQUEST: usize = 2 * 1024 * 1024;

/// The most texts one `embed_passages` request carries.
pub const PASSAGES_MAX: usize = 64;

/// One request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    /// Loads the model in the background so a query a moment later does not wait for it.
    Warm,
    EmbedQuery {
        text: String,
    },
    EmbedPassages {
        texts: Vec<String>,
    },
    Status,
}

#[derive(Serialize)]
struct Framed<'a> {
    v: u32,
    #[serde(flatten)]
    request: &'a Request,
}

impl Request {
    /// The request line, without its newline.
    pub fn to_line(&self) -> String {
        serde_json::to_string(&Framed {
            v: VERSION,
            request: self,
        })
        .unwrap_or_default()
    }

    /// Reads a request line; the error is the reply text.
    pub fn parse(line: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(line).map_err(|_| "the request is not JSON".to_owned())?;

        if value.get("v").and_then(Value::as_u64) != Some(u64::from(VERSION)) {
            return Err(format!("unsupported protocol version (this embedder speaks {VERSION})"));
        }

        serde_json::from_value(value).map_err(|_| "unknown or malformed request".to_owned())
    }
}

/// The reply line for a failure.
pub fn error_reply(message: &str) -> Value {
    json!({ "ok": false, "error": message })
}

/// What `ping` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pong {
    /// The model that answers, once it is ready.
    pub model: Option<String>,
    /// Whether texts are answered.
    pub ready: bool,
}

/// What `status` answers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Status {
    pub ready: bool,
    /// Whether the model is in memory now (it is dropped when idle).
    pub loaded: bool,
    pub model: Option<String>,
    pub dim: usize,
    pub workers: usize,
    pub queued_queries: usize,
    pub queued_passages: usize,
}

/// Any reply, read by a client.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub loaded: bool,
    #[serde(default)]
    pub dim: usize,
    #[serde(default)]
    pub workers: usize,
    #[serde(default)]
    pub queued_queries: usize,
    #[serde(default)]
    pub queued_passages: usize,
    #[serde(default)]
    pub vector: Vec<f32>,
    #[serde(default)]
    pub vectors: Vec<Vec<f32>>,
}

impl Reply {
    pub fn status(&self) -> Status {
        Status {
            ready: self.ready,
            loaded: self.loaded,
            model: self.model.clone(),
            dim: self.dim,
            workers: self.workers,
            queued_queries: self.queued_queries,
            queued_passages: self.queued_passages,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_with_its_version() {
        let request = Request::EmbedPassages {
            texts: vec!["one".to_owned(), "zażółć gęślą jaźń".to_owned()],
        };

        let line = request.to_line();
        assert!(
            line.contains("\"v\":1") && line.contains("\"op\":\"embed_passages\""),
            "{line}"
        );
        assert_eq!(Request::parse(&line), Ok(request));
    }

    #[test]
    fn another_version_or_a_bad_line_is_refused() {
        assert!(Request::parse("{\"v\":2,\"op\":\"ping\"}").is_err());
        assert!(Request::parse("{\"op\":\"ping\"}").is_err());
        assert!(Request::parse("{\"v\":1,\"op\":\"shell\"}").is_err());
        assert!(Request::parse("nope").is_err());
        assert_eq!(Request::parse("{\"v\":1,\"op\":\"ping\"}"), Ok(Request::Ping));
        assert_eq!(Request::parse("{\"v\":1,\"op\":\"warm\"}"), Ok(Request::Warm));
        assert_eq!(Request::parse("{\"v\":1,\"op\":\"status\"}"), Ok(Request::Status));
        assert_eq!(
            Request::parse("{\"v\":1,\"op\":\"embed_query\",\"text\":\"hi\"}"),
            Ok(Request::EmbedQuery { text: "hi".to_owned() })
        );
    }
}
