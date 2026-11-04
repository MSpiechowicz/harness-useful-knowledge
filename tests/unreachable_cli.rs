//! A command that fails because the remote database cannot be reached says so with the error code
//! `unreachable` on stdout (Harness Desktop matches on it), besides the line on stderr. The remote
//! is a port nobody listens on, so the connection is refused at once.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const BINARY: &str = env!("CARGO_BIN_EXE_harness-knowledge");

/// What one run printed.
struct Ran {
    code: Option<i32>,
    stdout: Value,
    stderr: String,
}

fn run(root: &Path, layout: &str, args: &[&str]) -> Ran {
    let output = Command::new(BINARY)
        .args(args)
        .env_clear()
        .env("HOME", root)
        .env("HARNESS_KNOWLEDGE_DIR", root.join("knowledge"))
        .env("HARNESS_KNOWLEDGE_DATABASE_URL", "postgres://nobody@127.0.0.1:1/none")
        .env("HARNESS_KNOWLEDGE_REPOSITORY_DOCUMENTS", layout)
        .env("HARNESS_KNOWLEDGE_GLOBAL_DOCUMENTS", layout)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .output()
        .expect("runs");
    let text = String::from_utf8_lossy(&output.stdout);

    Ran {
        code: output.status.code(),
        stdout: serde_json::from_str(text.trim()).unwrap_or_else(|_| panic!("JSON: {text:?}")),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn assert_unreachable(ran: &Ran, what: &str) {
    assert_eq!(ran.code, Some(1), "{what}: {}", ran.stdout);
    assert_eq!(ran.stdout["error"], json!("unreachable"), "{what}: {}", ran.stdout);
    assert!(
        ran.stdout["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "{what}"
    );
    assert!(
        ran.stderr.contains("harness-knowledge: "),
        "{what}: the stderr line stays: {}",
        ran.stderr
    );
}

#[test]
fn a_command_that_needs_the_unreachable_remote_fails_with_the_unreachable_code() {
    let root = tempfile::tempdir().expect("temp dir");

    for args in [
        &["docs", "get", "--json", "5"][..],
        &["docs", "matches", "--json", "5", "--query", "gateway"],
        &["docs", "list", "--json", "--place", "remote"],
        &["docs", "rename", "--json", "5", "--title", "New"],
        &["docs", "delete", "--json", "5"],
    ] {
        let ran = run(root.path(), "remote+local", args);

        assert_unreachable(&ran, &args.join(" "));
        assert!(
            ran.stdout["message"]
                .as_str()
                .is_some_and(|message| message.starts_with("remote knowledge is unreachable")),
            "{args:?}: {}",
            ran.stdout
        );
    }

    // The review commands never answer from this computer instead.
    for args in [
        &["docs", "ask", "--json", "5", "--expect", "0"][..],
        &["docs", "approve", "--json", "5", "--expect", "0"],
        &["docs", "go-live", "--json", "5", "--expect", "0"],
        &[
            "docs",
            "request-changes",
            "--json",
            "5",
            "--expect",
            "0",
            "--comment",
            "No",
        ],
        &[
            "docs",
            "review",
            "--json",
            "5",
            "--expect",
            "0",
            "--verdict",
            "comment",
            "--body",
            "Hi",
        ],
        &["docs", "history", "--json", "5"],
        &["docs", "comments", "--json", "5"],
        &["docs", "comment", "reply", "--json", "5", "1", "--body", "Hi"],
        &["docs", "compare", "--json", "5"],
        &["docs", "download", "--json", "5", "--to", "/tmp/never-written.md"],
        &["approval", "get", "--json"],
        &["approval", "set", "--json", "on"],
        &["upgrade", "--json"],
    ] {
        assert_unreachable(&run(root.path(), "remote+local", args), &args.join(" "));
    }

    // With the remote alone nothing can answer instead, and it is the same condition.
    assert_unreachable(&run(root.path(), "remote", &["docs", "list", "--json"]), "remote only");
}
