//! The client's behaviour that needs no real server: a fake one answers or hangs up.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use crate::backend::{Access, OpenError};
use crate::config::Secrets;
use crate::store::libsql::hrana::Client;
use crate::store::libsql::sql::{Sql, args};
use crate::store::libsql::wire::execute;
use crate::store::libsql::{CONNECT_TIMEOUT, LibsqlStore, endpoint, open_boxed};
use crate::store::{Columns, KnowledgeStore, StoreError};

/// A server that hangs up on the first `hangups` connections without a word, then answers every
/// request with `body`. `abrupt` drops the socket unread (a reset), else it closes it cleanly
/// after reading the request (what a server ending an idle connection does).
fn flaky_server(hangups: usize, body: &'static str, abrupt: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let url = format!("http://127.0.0.1:{}", listener.local_addr().expect("address").port());

    thread::spawn(move || {
        for (index, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { continue };

            if index < hangups && abrupt {
                drop(stream);
                continue;
            }

            // The whole request first: closing or answering a socket with unread bytes resets it.
            let _request = read_body(&mut stream);

            if index < hangups {
                close_cleanly(stream);
                continue;
            }

            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).ok();
            close_cleanly(stream);
        }
    });

    url
}

const ONE_ROW: &str = r#"{"baton":null,"results":[{"type":"ok","response":{"type":"execute","result":{"cols":[],"rows":[[{"type":"integer","value":"7"}]],"affected_row_count":0,"last_insert_rowid":null}}},{"type":"ok","response":{"type":"close"}}]}"#;

fn client(url: &str) -> Client {
    Client::new(endpoint::parse(url).expect("url"), Some("secret-token")).expect("client")
}

/// A server that accepts every connection and never answers.
fn silent_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let url = format!("http://127.0.0.1:{}", listener.local_addr().expect("address").port());

    thread::spawn(move || {
        let mut held = Vec::new();

        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });

    url
}

#[test]
fn a_request_the_server_takes_too_long_to_answer_is_not_an_unreachable_server() {
    let url = silent_server();
    let client = Client::with_timeouts(
        endpoint::parse(&url).expect("url"),
        None,
        CONNECT_TIMEOUT,
        std::time::Duration::from_millis(300),
    )
    .expect("client");

    let error = client.single().query("SELECT 7", args![]).expect_err("no answer");

    assert!(matches!(error, StoreError::Backend(_)), "{error:?}");
}

#[test]
fn a_read_is_repeated_once_when_the_connection_broke_before_any_reply() {
    // a reset on Linux, and on macOS either a reset or a clean close, so both are asked for
    for abrupt in [true, false] {
        let url = flaky_server(1, ONE_ROW, abrupt);
        let rows = client(&url)
            .single()
            .query("SELECT 7", args![])
            .unwrap_or_else(|error| panic!("the second try answers (abrupt {abrupt}): {error:?}"));

        assert_eq!(rows[0].get::<i64>(0).expect("a number"), 7);
    }
}

#[test]
fn a_read_is_not_repeated_forever() {
    let url = flaky_server(2, ONE_ROW, false);
    let error = client(&url)
        .single()
        .query("SELECT 7", args![])
        .expect_err("both tries hang up");

    assert!(matches!(error, StoreError::Unreachable(_)), "{error:?}");
    assert!(
        !error.to_string().contains("127.0.0.1") && !error.to_string().contains("secret-token"),
        "{error}"
    );
}

#[test]
fn a_transaction_is_never_repeated_when_the_connection_broke() {
    let url = flaky_server(1, ONE_ROW, false);
    let client = client(&url);
    let error = client
        .stream()
        .send(vec![execute("BEGIN IMMEDIATE")])
        .err()
        .expect("the request of a transaction is sent once");

    assert!(matches!(error, StoreError::Unreachable(_)), "{error:?}");
}

#[test]
fn a_blob_in_a_reply_is_decoded() {
    const BLOB: &str = r#"{"baton":null,"results":[{"type":"ok","response":{"type":"execute","result":{"cols":[],"rows":[[{"type":"blob","base64":"AACAPw"}]],"affected_row_count":0,"last_insert_rowid":null}}},{"type":"ok","response":{"type":"close"}}]}"#;
    let url = flaky_server(0, BLOB, false);
    let rows = client(&url).single().query("SELECT e", args![]).expect("rows");

    let bytes: Vec<u8> = rows[0].get(0).expect("a blob");
    assert_eq!(
        f32::from_le_bytes(bytes.try_into().expect("four bytes")),
        1.0,
        "unpadded base64 reads too"
    );
}

#[test]
fn opening_never_shows_the_url_or_the_token() {
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        format!("http://127.0.0.1:{}", listener.local_addr().expect("address").port())
    };
    let secrets = Secrets::of(&closed, Some("secret-token"));

    for access in [Access::Read, Access::Existing, Access::Create] {
        let error = open_boxed(&closed, Some("secret-token"), access, CONNECT_TIMEOUT, &secrets)
            .err()
            .expect("refused");
        let message = error.to_string();

        assert!(matches!(error, OpenError::Unreachable(_)), "{error:?}");
        assert!(message.starts_with("cannot reach the libSQL server"), "{message}");
        assert!(
            !message.contains("secret-token") && !message.contains("127.0.0.1"),
            "{message}"
        );
    }
}

#[test]
fn a_url_that_would_send_the_token_in_clear_or_carry_a_secret_is_refused() {
    for url in [
        "http://db.example.com",
        "ftp://db.example.com",
        "libsql://user:hunter2@db.example.com",
        "https://db.example.com?authToken=abc123",
    ] {
        let error = LibsqlStore::connect(url, None).expect_err("refused");

        assert!(matches!(error, StoreError::Invalid(_)), "{url}: {error:?}");
        assert!(
            !error.to_string().contains("hunter2") && !error.to_string().contains("abc123"),
            "{error}"
        );
    }
}

#[test]
fn the_floats_of_a_vector_survive_their_text() {
    let vector = crate::store::testing::fake_vector(3);
    let text = crate::store::libsql::documents::vector_text(&vector);
    let back: Vec<f32> = serde_json::from_str(&text).expect("a JSON array");

    assert_eq!(back, vector);
}

/// A server whose every statement succeeds with no rows, except the ones that mention
/// `refuse`: those fail with `message`.
fn refusing_server(refuse: &'static str, message: &'static str) -> String {
    refusing_server_with(refuse, message, "SQLITE_ERROR")
}

/// [`refusing_server`] that names `code` for the error.
fn refusing_server_with(refuse: &'static str, message: &'static str, code: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let url = format!("http://127.0.0.1:{}", listener.local_addr().expect("address").port());

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = read_body(&mut stream);
            let requests = serde_json::from_str::<serde_json::Value>(&body).expect("a JSON body")["requests"].clone();

            let results: Vec<serde_json::Value> = requests
                .as_array()
                .expect("requests")
                .iter()
                .map(|request| {
                    let sql = request["sql"]
                        .as_str()
                        .or(request["stmt"]["sql"].as_str())
                        .unwrap_or("");

                    if sql.contains(refuse) {
                        serde_json::json!({ "type": "error", "error": { "message": message, "code": code } })
                    } else if request["type"] == "close" {
                        serde_json::json!({ "type": "ok", "response": { "type": "close" } })
                    } else {
                        serde_json::json!({ "type": "ok", "response": { "type": "execute", "result": {
                            "cols": [], "rows": [], "affected_row_count": 0, "last_insert_rowid": null } } })
                    }
                })
                .collect();

            // A transaction's stream stays open: it gets a baton, a one-shot stream is closed.
            let closing = requests
                .as_array()
                .is_some_and(|all| all.iter().any(|one| one["type"] == "close"));
            let baton = if closing {
                serde_json::Value::Null
            } else {
                "baton".into()
            };
            let reply = serde_json::json!({ "baton": baton, "results": results }).to_string();
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            stream.write_all(head.as_bytes()).ok();
            stream.write_all(reply.as_bytes()).ok();
        }
    });

    url
}

/// Ends the conversation with a FIN, never a reset: no more writes, then whatever the client still
/// sends is read until it closes its side (bounded, so a client that keeps the socket open cannot
/// hold the fake server).
fn close_cleanly(mut stream: std::net::TcpStream) {
    stream.shutdown(std::net::Shutdown::Write).ok();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok();

    let mut buffer = [0; 1024];
    while matches!(stream.read(&mut buffer), Ok(read) if read > 0) {}
}

/// The body of the HTTP request on `stream`.
fn read_body(stream: &mut std::net::TcpStream) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    let mut buffer = [0; 8192];

    loop {
        let read = stream.read(&mut buffer).expect("request bytes");
        bytes.extend_from_slice(&buffer[..read]);

        let text = String::from_utf8_lossy(&bytes).into_owned();

        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|n| n.trim().parse::<usize>())
                })
                .and_then(Result::ok)
                .unwrap_or(0);

            if body.len() >= length || read == 0 {
                return body.to_owned();
            }
        }
    }
}

#[test]
fn opening_a_server_without_vectors_is_a_missing_capability() {
    let url = refusing_server("libsql_vector_idx", "SQLite error: no such function: libsql_vector_idx");
    let secrets = Secrets::of(&url, None);

    let error = open_boxed(&url, None, Access::Create, CONNECT_TIMEOUT, &secrets)
        .err()
        .expect("the migration fails");

    assert_eq!(
        error,
        OpenError::Missing {
            code: "libsql-no-vector",
            message: crate::store::libsql::NO_VECTOR_MESSAGE.to_owned()
        }
    );
}

#[test]
fn opening_a_server_without_fts5_is_a_missing_capability() {
    let url = refusing_server("fts5", "SQLite error: no such module: fts5");
    let secrets = Secrets::of(&url, None);

    let error = open_boxed(&url, None, Access::Create, CONNECT_TIMEOUT, &secrets)
        .err()
        .expect("the migration fails");

    assert_eq!(error, OpenError::no_fts5());
}

#[test]
fn a_counter_refused_for_a_lock_is_busy_and_any_other_refusal_is_a_failure() {
    for (code, busy) in [
        ("SQLITE_BUSY", true),
        ("SQLITE_BUSY_SNAPSHOT", true),
        ("SQLITE_LOCKED", true),
        ("SQLITE_ERROR", false),
    ] {
        let url = refusing_server_with("UPDATE", "database is locked", code);
        let mut store = LibsqlStore {
            client: client(&url),
            newer: false,
            writable: false,
            columns: Columns::ALL,
        };

        for counted in [store.record_shown(&[1]), store.record_useful(&[1], 5)] {
            match counted {
                Err(StoreError::Busy) => assert!(busy, "{code} is a failure"),
                Err(StoreError::Backend(_)) => assert!(!busy, "{code} is busy"),
                other => panic!("{code}: {other:?}"),
            }
        }
    }
}
