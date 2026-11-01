//! What a libSQL server can do, for `check`: whether its SQLite has FTS5 (the keyword index) and
//! vectors (`F32_BLOB`, `vector32`, `vector_top_k`). Nothing is created or written: the probe only
//! evaluates constant expressions and reads the module list.
//!
//! A server too old for vectors fails the probe statement (`no such function: vector32`), which is
//! `libsql-no-vector`. The same words, and the `vector index(...)` errors of a database whose
//! index is gone, are what [`problem_of`] recognises in the message of any statement that fails
//! later, so a search says what is missing instead of "statement failed".

use super::endpoint;
use super::hrana::{Client, Kind};
use super::wire::{Outcome, Response, execute};
use crate::store::StoreError;

/// A capability the server lacks, named by its error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Problem {
    NoFts5,
    NoVector,
}

/// The fragments of the messages a server without vectors (or with a lost vector index) gives.
const NO_VECTOR: [&str; 5] = [
    "no such function: vector",
    "no such function: libsql_vector",
    "no such table: vector_top_k",
    "vector index(",
    "F32_BLOB",
];

/// What a store error says when the server has no vector support (`libsql-no-vector`).
pub const NO_VECTOR_MESSAGE: &str = "this libSQL server has no vector support (F32_BLOB, vector32, \
     vector_top_k), use a libSQL server build that has it";

/// The capability `message` (a server's error text) says is missing.
pub(super) fn problem_of(message: &str) -> Option<Problem> {
    if message.contains("no such module: fts5") {
        return Some(Problem::NoFts5);
    }

    NO_VECTOR
        .iter()
        .any(|fragment| message.contains(fragment))
        .then_some(Problem::NoVector)
}

/// What the server can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// FTS5 is built in: the keyword index can exist.
    pub keyword: bool,
    /// Vector columns, `vector32` and `vector_top_k` work.
    pub vector: bool,
}

/// Why the server could not be asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trouble {
    /// HTTP 401 or 403: the token is missing or wrong.
    Auth,
    /// No usable answer: the host, the network, TLS or a server that is not libSQL.
    Unreachable(String),
    /// The URL or the token cannot be used at all.
    Invalid(String),
}

/// Asks the server at `url` what it can do. No database is created or changed.
pub fn probe(url: &str, token: Option<&str>) -> Result<Capabilities, Trouble> {
    let invalid = |error: StoreError| Trouble::Invalid(error.to_string());
    let client = Client::new(endpoint::parse(url).map_err(invalid)?, token).map_err(invalid)?;

    let requests = vec![
        execute("SELECT vector_extract(vector32('[1,2]'))"),
        execute("SELECT 1 FROM pragma_module_list WHERE name = 'fts5'"),
    ];

    let outcomes = client.single().exchange(requests).map_err(|failed| match failed.kind {
        Kind::Auth => Trouble::Auth,
        Kind::Unreachable { .. } | Kind::Other => Trouble::Unreachable(match failed.error {
            StoreError::Backend(message) => message,
            other => other.to_string(),
        }),
    })?;

    let mut outcomes = outcomes.into_iter();
    let vector = matches!(outcomes.next(), Some(Outcome::Ok { .. }));
    let keyword = matches!(
        outcomes.next(),
        Some(Outcome::Ok { response: Response::Execute { result } }) if result.has_rows()
    );

    Ok(Capabilities { keyword, vector })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    #[test]
    fn the_messages_of_a_server_without_vectors_are_recognised() {
        // The error recorded from a database without its vector index (experiment X5), and the
        // ones an older build gives for each part of the vector feature.
        for message in [
            "SQLite error: vector index(search): failed to parse vector index parameters",
            "SQLite error: no such function: vector32",
            "SQLite error: no such function: vector_extract",
            "SQLite error: no such function: libsql_vector_idx",
            "SQLite error: no such table: vector_top_k",
            "SQLite error: unknown column type F32_BLOB(384)",
        ] {
            assert_eq!(problem_of(message), Some(Problem::NoVector), "{message}");
        }
    }

    #[test]
    fn a_missing_fts5_module_is_recognised_and_other_errors_are_not() {
        assert_eq!(problem_of("SQLite error: no such module: fts5"), Some(Problem::NoFts5));

        for message in [
            "SQLite error: database is locked",
            "SQLite error: no such table: knowledge_chunks",
            "SQLite error: CHECK constraint failed: (embedding IS NULL) = (model_id IS NULL)",
        ] {
            assert_eq!(problem_of(message), None, "{message}");
        }
    }

    /// A server that answers every request with `status` and nothing else.
    fn server_answering(status: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let url = format!("http://127.0.0.1:{}", listener.local_addr().expect("address").port());

        thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0; 4096];
                let _read = stream.read(&mut buffer);
                let reply = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                stream.write_all(reply.as_bytes()).ok();
            }
        });

        url
    }

    #[test]
    fn a_refused_token_is_an_auth_problem() {
        for status in ["401 Unauthorized", "403 Forbidden"] {
            assert_eq!(
                probe(&server_answering(status), Some("secret-token")),
                Err(Trouble::Auth)
            );
        }
    }

    #[test]
    fn a_server_that_is_not_there_or_not_libsql_is_unreachable_without_the_url() {
        let closed = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            format!("http://127.0.0.1:{}", listener.local_addr().expect("address").port())
        };

        for url in [closed, server_answering("502 Bad Gateway")] {
            let Err(Trouble::Unreachable(message)) = probe(&url, Some("secret-token")) else {
                panic!("expected unreachable for {url}");
            };

            assert!(
                !message.contains("127.0.0.1") && !message.contains("secret-token"),
                "{message}"
            );
        }
    }

    #[test]
    fn an_unusable_url_or_token_is_invalid() {
        assert!(matches!(probe("ftp://db", None), Err(Trouble::Invalid(_))));
        assert!(matches!(probe("http://db.example.com", None), Err(Trouble::Invalid(_))));
        assert!(matches!(
            probe("http://127.0.0.1:9", Some("line\nbreak")),
            Err(Trouble::Invalid(_))
        ));
    }
}
