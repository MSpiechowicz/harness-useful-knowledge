//! `harness-knowledge docs import --ndjson` as a real process: the PDF worker child, the event
//! stream and a cancel by signal, and the exit code of a team database that lacks a capability.

mod support;

use std::io::Write;
#[cfg(unix)]
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

#[cfg(target_os = "linux")]
use support::pdf::flate_bomb;
use support::pdf::pdf;

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

/// Runs a command with `stdin` and returns its stdout lines as JSON, and its exit code.
fn run(root: &Path, args: &[&str], stdin: &str) -> (Vec<Value>, Option<i32>) {
    run_on(root, None, args, stdin)
}

/// [`run`] against the team database at `url`, when there is one.
fn run_on(root: &Path, url: Option<&str>, args: &[&str], stdin: &str) -> (Vec<Value>, Option<i32>) {
    let mut command = command(root, args);

    if let Some(url) = url {
        command.env("HARNESS_KNOWLEDGE_DATABASE_URL", url);
    }

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("starts");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("manifest");

    let output = child.wait_with_output().expect("finishes");
    let lines = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();

    (lines, output.status.code())
}

fn file<'a>(events: &'a [Value], name: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["type"] == "file" && event["path"].as_str().is_some_and(|path| path.ends_with(name)))
        .unwrap_or_else(|| panic!("no event for {name}: {events:?}"))
}

#[test]
fn pdfs_are_read_by_the_worker_process_and_a_bad_one_fails_only_itself() {
    let root = tempfile::tempdir().expect("temp dir");
    let docs = root.path().join("docs");
    std::fs::create_dir_all(&docs).expect("folder");

    let text = format!("Gateway handbook\n{}", "requests are routed by key ".repeat(40));
    std::fs::write(docs.join("a-good.pdf"), pdf(&[&text, &text])).expect("pdf");
    std::fs::write(docs.join("b-scanned.pdf"), pdf(&["", "", ""])).expect("scanned");
    std::fs::write(docs.join("c-malformed.pdf"), b"%PDF-1.4\nnothing that is a PDF").expect("malformed");
    std::fs::write(docs.join("d-notes.md"), "# Notes\n\nSome notes about the gateway.").expect("notes");

    let manifest = json!({ "folders": [docs] }).to_string();
    let (events, code) = run(root.path(), &["docs", "import", "--ndjson"], &manifest);

    assert_eq!(code, Some(0));
    assert_eq!(events[0], json!({ "type": "plan", "total": 4 }));
    assert_eq!(file(&events, "a-good.pdf")["status"], "added");
    assert_eq!(file(&events, "b-scanned.pdf")["status"], "failed");
    assert_eq!(
        file(&events, "b-scanned.pdf")["reason"],
        "the PDF has no text layer (scanned)"
    );
    assert_eq!(file(&events, "c-malformed.pdf")["status"], "failed");
    assert_eq!(file(&events, "d-notes.md")["status"], "added");

    let summary = events.last().expect("done");
    assert_eq!(summary["type"], "done");
    assert_eq!(
        summary["counts"],
        json!({ "added": 2, "updated": 0, "unchanged": 0, "skipped": 0, "failed": 2 })
    );
    assert_eq!(summary["counter"], "approximate");

    let (listed, _) = run(root.path(), &["docs", "list", "--json", "--format", "pdf"], "");
    assert_eq!(listed[0]["total"], 1);
    assert_eq!(listed[0]["items"][0]["title"], "a-good");

    // The same import again changes nothing.
    let (again, _) = run(root.path(), &["docs", "import", "--ndjson"], &manifest);
    assert_eq!(file(&again, "a-good.pdf")["status"], "unchanged");
    assert_eq!(again.last().expect("done")["counts"]["added"], 0);
}

/// A 2 GiB decompression bomb of about 2 MB is one failed file, within the worker's memory limit, and the PDFs
/// next to it are read. Only Linux enforces the address-space limit the worker sets.
#[cfg(target_os = "linux")]
#[test]
fn a_decompression_bomb_fails_as_too_large_and_does_not_take_the_import_down() {
    let root = tempfile::tempdir().expect("temp dir");
    let docs = root.path().join("docs");
    std::fs::create_dir_all(&docs).expect("folder");

    let text = format!("Gateway handbook\n{}", "requests are routed by key ".repeat(40));
    std::fs::write(docs.join("a-good.pdf"), pdf(&[&text, &text])).expect("pdf");

    let bomb = flate_bomb(2048);
    assert!(bomb.len() < 3 << 20, "the bomb is {} bytes on disk", bomb.len());
    std::fs::write(docs.join("b-bomb.pdf"), bomb).expect("bomb");

    let manifest = json!({ "folders": [docs] }).to_string();
    let started = std::time::Instant::now();
    let (events, code) = run(root.path(), &["docs", "import", "--ndjson"], &manifest);

    assert_eq!(code, Some(0));
    assert_eq!(file(&events, "a-good.pdf")["status"], "added");
    assert_eq!(file(&events, "b-bomb.pdf")["status"], "failed");
    assert_eq!(file(&events, "b-bomb.pdf")["reason"], "the PDF is too large to read");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(50),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn the_worker_subcommand_prints_the_pages_as_json() {
    let root = tempfile::tempdir().expect("temp dir");
    let path = root.path().join("two.pdf");
    let text = format!("First page\n{}", "words ".repeat(50));
    std::fs::write(&path, pdf(&[&text, "Second page"])).expect("pdf");

    let output = command(root.path(), &["extract-worker", &path.to_string_lossy()])
        .output()
        .expect("runs");
    let reply: Value = serde_json::from_slice(&output.stdout).expect("one JSON object");

    assert_eq!(reply["pages"].as_array().map(Vec::len), Some(2));
    assert!(reply["pages"][0].as_str().expect("text").contains("First page"));

    let missing = command(root.path(), &["extract-worker", "/no/such/file.pdf"])
        .output()
        .expect("runs");
    let reply: Value = serde_json::from_slice(&missing.stdout).expect("one JSON object");
    assert!(
        reply["error"]
            .as_str()
            .is_some_and(|error| error.contains("cannot be read"))
    );
}

#[test]
fn the_command_line_is_checked_and_the_feature_is_listed() {
    let root = tempfile::tempdir().expect("temp dir");

    let features = command(root.path(), &["features"]).output().expect("runs");
    assert!(
        String::from_utf8_lossy(&features.stdout)
            .lines()
            .any(|line| line == "import-ndjson")
    );

    let (_, code) = run(root.path(), &["docs", "import"], "{}");
    assert_eq!(code, Some(2), "--ndjson is required");

    let (events, code) = run(root.path(), &["docs", "import", "--ndjson"], "not json");
    assert_eq!(code, Some(1));
    assert_eq!(events[0]["error"], "failed");

    // An empty manifest imports nothing.
    let (events, code) = run(root.path(), &["docs", "import", "--ndjson"], "{}");
    assert_eq!(code, Some(0));
    assert_eq!(events[0], json!({ "type": "plan", "total": 0 }));
    assert_eq!(events[1]["counts"]["added"], 0);
}

#[cfg(unix)]
#[test]
fn a_termination_signal_stops_between_documents_and_leaves_whole_ones() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let root = tempfile::tempdir().expect("temp dir");
    let docs = root.path().join("docs");
    std::fs::create_dir_all(&docs).expect("folder");

    // Big enough that the import is still running when the signal arrives.
    let total = 24;

    for n in 0..total {
        let paragraphs: Vec<String> = (0..1400)
            .map(|m| format!("Document {n} paragraph {m}. {}", "filler words here ".repeat(60)))
            .collect();
        std::fs::write(docs.join(format!("doc{n:02}.txt")), paragraphs.join("\n\n")).expect("write");
    }

    let manifest = json!({ "folders": [docs] }).to_string();
    let mut child = command(root.path(), &["docs", "import", "--ndjson"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("starts");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(manifest.as_bytes())
        .expect("manifest");

    let mut events: Vec<Value> = Vec::new();
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    let pid = Pid::from_raw(i32::try_from(child.id()).expect("pid"));
    let mut signalled = false;

    while let Some(Ok(line)) = lines.next() {
        let event: Value = serde_json::from_str(&line).expect("a JSON line");

        if event["type"] == "file" && !signalled {
            kill(pid, Signal::SIGTERM).expect("signals");
            signalled = true;
        }

        events.push(event);
    }

    let status = child.wait().expect("exits");
    let summary = events.last().expect("done");
    let added = summary["counts"]["added"].as_u64().expect("count");

    assert_eq!(status.code(), Some(0), "a cancel is a clean exit");
    assert_eq!(summary["type"], "done");
    assert_eq!(summary["cancelled"], true);
    assert!(added >= 1 && added < total, "stopped part way: {added} of {total}");

    // Exactly the documents reported are stored, each with all its chunks.
    let (listed, _) = run(root.path(), &["docs", "list", "--json", "--per-page", "200"], "");
    assert_eq!(listed[0]["total"], added);

    for item in listed[0]["items"].as_array().expect("items") {
        let id = item["id"].to_string();
        let (outline, _) = run(root.path(), &["docs", "get", "--json", &id, "--sections"], "");

        assert_eq!(
            outline[0]["sections"].as_array().map(Vec::len),
            item["chunkCount"].as_u64().map(|count| count as usize),
            "{} is whole",
            item["title"]
        );
    }
}

#[test]
fn team_a_database_without_pgvector_or_mongot_exits_3_with_its_problem_code() {
    let servers = [
        ("HARNESS_KNOWLEDGE_TEST_POSTGRES_PLAIN_URL", "postgres", "no-pgvector"),
        ("HARNESS_KNOWLEDGE_TEST_MONGO_PLAIN_URL", "mongodb", "no-mongot"),
    ];

    for (variable, backend, problem) in servers {
        let Some(url) = std::env::var(variable).ok().filter(|url| !url.trim().is_empty()) else {
            eprintln!("skipped: set {variable} to run the missing capability tests");
            continue;
        };

        let root = tempfile::tempdir().expect("temp dir");
        let file = root.path().join("guide.md");
        std::fs::write(&file, "# Guide\n\nHow the gateway routes requests.").expect("write");

        // `check` keeps its own JSON.
        let (checked, code) = run_on(root.path(), Some(&url), &["check", "--json"], "");
        assert_eq!(code, Some(3), "{backend}: {checked:?}");
        assert_eq!(checked[0]["ok"], false);
        assert_eq!(checked[0]["backend"], backend);
        assert_eq!(checked[0]["problem"], problem);

        // The first import needs the capability: exit 3 with the code, and no event of a file.
        let manifest = json!({ "paths": [file] }).to_string();
        let (events, code) = run_on(root.path(), Some(&url), &["docs", "import", "--ndjson"], &manifest);
        assert_eq!(code, Some(3), "{backend}: {events:?}");
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["error"], "missing");
        assert_eq!(events[0]["code"], problem);
        assert!(events[0]["message"].as_str().is_some_and(|message| !message.is_empty()));

        // Nothing was written, so a read finds an empty database like any other that nobody wrote to.
        let (listed, code) = run_on(root.path(), Some(&url), &["docs", "list", "--json"], "");
        assert_eq!((code, &listed[0]["total"]), (Some(0), &json!(0)), "{backend}");

        let (stats, code) = run_on(root.path(), Some(&url), &["stats", "--json"], "");
        assert_eq!(code, Some(0), "{backend}: {stats:?}");
        assert_eq!(stats[0]["documents"], 0);
    }
}
