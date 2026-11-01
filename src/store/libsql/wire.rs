//! The wire format of the pipeline API: the requests built and the replies read (see [`super::hrana`]
//! for the client that sends them).

use serde::Deserialize;
use serde_json::json;

use super::probe::{self, NO_VECTOR_MESSAGE, Problem};
use super::sql::{Value, base64_decode, base64_encode};
use crate::store::StoreError;

/// An `execute` request for `sql` without arguments.
pub(super) fn execute(sql: &str) -> serde_json::Value {
    execute_request(sql, &[], false)
}

pub(super) fn execute_request(sql: &str, args: &[Value], want_rows: bool) -> serde_json::Value {
    json!({
        "type": "execute",
        "stmt": { "sql": sql, "args": args.iter().map(value_json).collect::<Vec<_>>(), "want_rows": want_rows },
    })
}

#[derive(Deserialize)]
pub(super) struct Reply {
    pub(super) baton: Option<String>,
    pub(super) base_url: Option<String>,
    pub(super) results: Vec<Outcome>,
}

/// The outcome of one stream request.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Outcome {
    Ok { response: Response },
    Error { error: Failure },
}

impl Outcome {
    /// The response, or the server's error as a [`StoreError`].
    pub(super) fn into_result(self) -> Result<Response, StoreError> {
        match self {
            Self::Ok { response } => Ok(response),
            Self::Error { error } => Err(error.into_store_error()),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Response {
    Execute {
        result: StatementResult,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub(super) struct StatementResult {
    #[serde(default)]
    pub(super) rows: Vec<Vec<Cell>>,
    #[serde(default)]
    pub(super) affected_row_count: u64,
    pub(super) last_insert_rowid: Option<String>,
}

impl StatementResult {
    pub(super) fn has_rows(&self) -> bool {
        !self.rows.is_empty()
    }
}

/// An error the server reports for one request; its message may quote the statement, so only the
/// code and the capability it names are kept.
#[derive(Deserialize)]
pub(super) struct Failure {
    code: Option<String>,
    message: Option<String>,
}

impl Failure {
    /// True when another writer held the lock.
    pub(super) fn is_busy(&self) -> bool {
        self.code().is_some_and(|code| code.contains("BUSY"))
    }

    /// The server's error code when it looks like one (`[A-Z_]{1,40}`); anything else may be
    /// free text, which never reaches a message.
    pub(super) fn code(&self) -> Option<&str> {
        self.code.as_deref().filter(|code| {
            (1..=40).contains(&code.len()) && code.bytes().all(|byte| byte.is_ascii_uppercase() || byte == b'_')
        })
    }

    /// The missing capability the server's message names, if it does.
    pub(super) fn problem(&self) -> Option<Problem> {
        self.message.as_deref().and_then(probe::problem_of)
    }

    pub(super) fn into_store_error(self) -> StoreError {
        match self.problem() {
            Some(Problem::NoFts5) => StoreError::NoFts5,
            Some(Problem::NoVector) => backend(NO_VECTOR_MESSAGE.to_owned()),
            None => backend(self.code().unwrap_or("statement failed").to_owned()),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Cell {
    Null,
    Integer { value: String },
    Float { value: f64 },
    Text { value: String },
    Blob { base64: String },
}

impl Cell {
    pub(super) fn into_value(self) -> Result<Value, StoreError> {
        match self {
            Self::Null => Ok(Value::Null),
            Self::Integer { value } => value.parse().map(Value::Integer).map_err(|_| unexpected()),
            Self::Float { value } => Ok(Value::Real(value)),
            Self::Text { value } => Ok(Value::Text(value)),
            Self::Blob { base64 } => base64_decode(&base64).map(Value::Blob).ok_or_else(unexpected),
        }
    }
}

fn value_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => json!({ "type": "null" }),
        Value::Integer(number) => json!({ "type": "integer", "value": number.to_string() }),
        Value::Real(number) => json!({ "type": "float", "value": number }),
        Value::Text(text) => json!({ "type": "text", "value": text }),
        Value::Blob(bytes) => json!({ "type": "blob", "base64": base64_encode(bytes) }),
    }
}

pub(super) fn backend(message: String) -> StoreError {
    StoreError::Backend(message)
}

pub(super) fn unexpected() -> StoreError {
    backend("unexpected reply from the libSQL server".to_owned())
}
