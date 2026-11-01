//! The offline fallback against a real PostgreSQL server that goes away and comes back, as real
//! processes. The test skips unless both variables are set:
//!
//! - `HARNESS_KNOWLEDGE_TEST_OFFLINE_POSTGRES_URL`: the URL of a PostgreSQL with pgvector that the
//!   test may fill (a throwaway one).
//! - `HARNESS_KNOWLEDGE_TEST_OFFLINE_CONTAINER`: the name of the docker container that runs it, which
//!   the test stops, pauses and starts again.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BINARY: &str = env!("CARGO_BIN_EXE_harness-knowledge");
const URL_VAR: &str = "HARNESS_KNOWLEDGE_TEST_OFFLINE_POSTGRES_URL";
const CONTAINER_VAR: &str = "HARNESS_KNOWLEDGE_TEST_OFFLINE_CONTAINER";

/// The most a command may take while the remote is down, the first time (the connect timeout of
/// the probe, doubled for a server that never answers, and some room).
const FIRST_FALLBACK: Duration = Duration::from_secs(8);

/// What one run printed.
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    took: Duration,
}

impl Ran {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|_| panic!("JSON: {:?}", self.stdout))
    }

    fn last_line(&self) -> &str {
        self.stderr.lines().last().unwrap_or_default()
    }
}

struct Computer<'a> {
    root: &'a Path,
    url: &'a str,
}

impl Computer<'_> {
    fn run(&self, args: &[&str], stdin: &str) -> Ran {
        let started = Instant::now();
        let mut child = Command::new(BINARY)
            .args(args)
            .env_clear()
            .env("HOME", self.root)
            .env("HARNESS_KNOWLEDGE_DIR", self.root.join("knowledge"))
            .env("HARNESS_KNOWLEDGE_DATABASE_URL", self.url)
            .env("HARNESS_KNOWLEDGE_REPOSITORY_DOCUMENTS", "remote+local")
            .env("HARNESS_KNOWLEDGE_GLOBAL_DOCUMENTS", "remote+local")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("starts");

        let _ = child.stdin.take().expect("stdin").write_all(stdin.as_bytes());
        let output = child.wait_with_output().expect("finishes");

        Ran {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            took: started.elapsed(),
        }
    }

    fn cooldown_file(&self) -> std::path::PathBuf {
        self.root.join("knowledge").join("remote-unreachable.json")
    }

    /// Polls `check` until the remote answers (a check clears the cooldown when it does).
    fn wait_until_reachable(&self) {
        for _ in 0..60 {
            if self.run(&["check", "--json"], "").code == Some(0) {
                return;
            }

            std::thread::sleep(Duration::from_secs(1));
        }

        panic!("the remote database did not come up");
    }

    fn import(&self, file: &Path, place: &str) -> Ran {
        self.run(
            &["docs", "import", "--ndjson"],
            &json!({ "paths": [file], "place": place }).to_string(),
        )
    }
}

fn docker(action: &str, container: &str) {
    let status = Command::new("docker")
        .args([action, container])
        .stdout(Stdio::null())
        .status();

    assert!(
        status.is_ok_and(|status| status.success()),
        "docker {action} {container}"
    );
}

fn titles(listed: &Value) -> Vec<String> {
    let mut titles: Vec<String> = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["title"].as_str().expect("title").to_owned())
        .collect();
    titles.sort();
    titles
}

#[test]
fn team_the_fallback_follows_a_remote_that_stops_pauses_and_returns() {
    let (Some(url), Some(container)) = (
        std::env::var(URL_VAR).ok().filter(|value| !value.trim().is_empty()),
        std::env::var(CONTAINER_VAR)
            .ok()
            .filter(|value| !value.trim().is_empty()),
    ) else {
        eprintln!("skipped: set {URL_VAR} and {CONTAINER_VAR} to run the offline fallback test");
        return;
    };

    let root = tempfile::tempdir().expect("temp dir");
    let computer = Computer {
        root: root.path(),
        url: &url,
    };

    let shared = root.path().join("shared-gateway.md");
    let mine = root.path().join("my-gateway.md");
    std::fs::write(&shared, "# Shared gateway\n\nThe gateway routes requests by key.").expect("write");
    std::fs::write(&mine, "# My gateway\n\nMy notes about the gateway routing.").expect("write");

    computer.wait_until_reachable();

    let remote_import = computer.import(&shared, "remote");
    assert_eq!(remote_import.code, Some(0), "{}", remote_import.stderr);
    let local_import = computer.import(&mine, "local");
    assert_eq!(local_import.code, Some(0), "{}", local_import.stderr);

    // Online: both places answer, nothing says offline.
    let online = computer.run(&["docs", "list", "--json"], "");
    assert_eq!(
        titles(&online.json()),
        ["My gateway", "Shared gateway"],
        "{}",
        online.stdout
    );
    assert!(online.json().get("offline").is_none());
    let remote_id = online.json()["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["title"] == "Shared gateway")
        .expect("remote document")["id"]
        .clone();
    let remote_id = remote_id.as_str().map_or_else(|| remote_id.to_string(), str::to_owned);

    // The remote stops: the first command waits at most the probe, then this computer answers.
    docker("stop", &container);

    let first = computer.run(&["docs", "list", "--json"], "");
    eprintln!("first fallback after stop: {:?}", first.took);
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert_eq!(titles(&first.json()), ["My gateway"]);
    assert_eq!(first.json()["offline"], json!(true));
    assert!(first.took < FIRST_FALLBACK, "{:?}", first.took);
    assert!(
        computer.cooldown_file().is_file(),
        "the failure is recorded for the other processes"
    );

    // The cooldown: no wait, the same answer.
    let second = computer.run(&["docs", "list", "--json"], "");
    eprintln!("within the cooldown: {:?}", second.took);
    assert_eq!(second.json()["offline"], json!(true));
    assert!(second.took < Duration::from_secs(2), "{:?}", second.took);

    let searched = computer.run(&["search", "--json", "--query", "gateway", "--scope", "all"], "");
    let found = searched.json();
    assert_eq!(searched.code, Some(0));
    assert!(
        found.as_array().is_some_and(|items| !items.is_empty()
            && items
                .iter()
                .all(|item| item["docId"].as_str().is_some_and(|id| id.starts_with("local:")))),
        "{found}"
    );
    assert!(
        searched
            .stderr
            .contains("Remote knowledge is unreachable. Showing documents on this computer only."),
        "{}",
        searched.stderr
    );

    let stats = computer.run(&["stats", "--json"], "").json();
    assert_eq!(stats["offline"], json!(true));
    assert_eq!(stats["places"]["remote"], json!({ "offline": true }));
    assert_eq!(stats["places"]["local"]["documents"], 1);

    let get = computer.run(&["docs", "get", "--json", &remote_id], "");
    assert_eq!(get.code, Some(1));
    assert_eq!(
        get.last_line(),
        "harness-knowledge: remote knowledge is unreachable right now"
    );

    let matches = computer.run(&["docs", "matches", "--json", &remote_id, "--query", "gateway"], "");
    assert_eq!(matches.code, Some(1));
    assert_eq!(
        matches.last_line(),
        "harness-knowledge: remote knowledge is unreachable right now"
    );

    let refused = computer.import(&shared, "remote");
    assert_eq!(refused.code, Some(1), "{}", refused.stdout);
    assert!(
        refused
            .last_line()
            .contains("remote knowledge is unreachable right now"),
        "{}",
        refused.stderr
    );

    let check = computer.run(&["check", "--json"], "");
    assert_eq!(check.code, Some(3));
    assert_eq!(check.json()["problem"], "unreachable");

    // The remote returns: a check that reaches it clears the cooldown for everyone.
    docker("start", &container);
    computer.wait_until_reachable();
    assert!(!computer.cooldown_file().exists());

    let back = computer.run(&["docs", "list", "--json"], "");
    assert_eq!(titles(&back.json()), ["My gateway", "Shared gateway"]);
    assert!(back.json().get("offline").is_none());

    // A server that accepts the connection and never speaks (the container is paused) is cut off too.
    docker("pause", &container);
    let silent = computer.run(&["docs", "list", "--json"], "");
    eprintln!("first fallback with a paused server: {:?}", silent.took);
    assert_eq!(silent.json()["offline"], json!(true), "{}", silent.stderr);
    assert_eq!(titles(&silent.json()), ["My gateway"]);
    assert!(silent.took < FIRST_FALLBACK, "{:?}", silent.took);

    docker("unpause", &container);
    computer.wait_until_reachable();
    let again = computer.run(&["docs", "list", "--json"], "");
    assert_eq!(titles(&again.json()), ["My gateway", "Shared gateway"]);
}
