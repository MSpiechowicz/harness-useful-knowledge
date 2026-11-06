//! Who wrote a document, through the real executable: the git `user.name` of the person who ran the
//! import, shown by `docs list`, `docs get` and `docs similar`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const BINARY: &str = env!("CARGO_BIN_EXE_harness-knowledge");

/// Runs `args` with its own home and knowledge folder, in `root` (so no repository of this
/// machine gives a name), and returns the stdout lines as JSON.
fn run(root: &Path, args: &[&str], stdin: &str) -> Vec<Value> {
    let mut child = Command::new(BINARY)
        .args(args)
        .env_clear()
        .env("HOME", root)
        // Windows finds the home folder through USERPROFILE, and git does not start without SystemRoot.
        .env("USERPROFILE", root)
        .envs(
            ["SystemRoot", "SYSTEMROOT", "windir", "PATHEXT", "TEMP", "TMP"]
                .iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (*name, value))),
        )
        .env("HARNESS_KNOWLEDGE_DIR", root.join("knowledge"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(root)
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
    assert!(output.status.success(), "{args:?}: {:?}", output.status);

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect()
}

fn set_git_name(root: &Path, name: &str) {
    std::fs::write(
        root.join(".gitconfig"),
        format!("[user]\n\tname = {name}\n\temail = {name}@example.com\n"),
    )
    .expect("gitconfig");
}

fn document(root: &Path, id: u32) -> Value {
    run(root, &["docs", "get", "--json", &id.to_string()], "")[0]["document"].clone()
}

#[test]
fn an_import_is_by_the_git_name_and_a_replacement_keeps_who_added_the_document() {
    let root = tempfile::tempdir().expect("temp dir");
    let root = root.path();
    let first = root.join("first.md");
    let second = root.join("second.md");
    std::fs::write(&first, "# Guide\n\nHow the gateway routes requests.").expect("write");
    std::fs::write(&second, "# Guide v2\n\nHow the gateway routes requests now.").expect("write");

    set_git_name(root, "Ada");
    run(
        root,
        &["docs", "import", "--ndjson"],
        &json!({ "paths": [first] }).to_string(),
    );

    let listed = run(root, &["docs", "list", "--json"], "");
    let ada = json!({ "name": "Ada", "via": null });

    assert_eq!(listed[0]["items"][0]["addedBy"], ada);
    assert_eq!(listed[0]["items"][0]["updatedBy"], ada);

    // Bob replaces the document with another file: Ada stays the one who added it.
    set_git_name(root, "Bob");
    let manifest = json!({ "paths": [second], "replacing": { second.to_string_lossy(): 1 } });
    let events = run(root, &["docs", "import", "--ndjson"], &manifest.to_string());

    assert_eq!(events[1]["status"], "updated");
    assert_eq!(events[1]["docId"], 1);

    let doc = document(root, 1);
    assert_eq!(doc["title"], "Guide v2");
    assert_eq!(doc["addedBy"], ada);
    assert_eq!(doc["updatedBy"], json!({ "name": "Bob", "via": null }));

    // The same file is found again, with its authors.
    let similar = run(root, &["docs", "similar", "--json", &second.to_string_lossy()], "");
    let found = &similar[0]["similar"][0]["match"];

    assert_eq!(found["id"], 1);
    assert_eq!(found["reason"], "sourceKey");
    assert_eq!(found["addedBy"], ada);
    assert_eq!(found["updatedBy"], json!({ "name": "Bob", "via": null }));
}

#[test]
fn without_a_git_name_the_authors_are_null() {
    let root = tempfile::tempdir().expect("temp dir");
    let root = root.path();
    let file = root.join("guide.md");
    std::fs::write(&file, "# Guide\n\nHow the gateway routes requests.").expect("write");

    run(
        root,
        &["docs", "import", "--ndjson"],
        &json!({ "paths": [file] }).to_string(),
    );

    let doc = document(root, 1);
    assert_eq!(
        (doc["addedBy"].clone(), doc["updatedBy"].clone()),
        (Value::Null, Value::Null)
    );
}

/// The exit code of `args` run the way [`run`] does.
fn exit_code(root: &Path, args: &[&str]) -> Option<i32> {
    Command::new(BINARY)
        .args(args)
        .env_clear()
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("HARNESS_KNOWLEDGE_DIR", root.join("knowledge"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("runs")
        .code()
}

fn import(root: &Path, name: &str, text: &str) {
    let file = root.join(format!("{}.md", name.to_lowercase()));
    std::fs::write(&file, text).expect("write");

    run(
        root,
        &["docs", "import", "--ndjson"],
        &json!({ "paths": [file] }).to_string(),
    );
}

fn titles(listed: &Value) -> Vec<&str> {
    listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["title"].as_str().expect("title"))
        .collect()
}

#[test]
fn an_empty_database_lists_nobody_and_the_list_filters_and_sorts_by_who_added_the_documents() {
    let root = tempfile::tempdir().expect("temp dir");
    let root = root.path();
    let empty = json!({ "people": [], "unknown": 0 });

    // Without a database, and with one that holds nothing yet.
    let listed = run(root, &["docs", "list", "--json", "--author", "Ada"], "");
    assert_eq!(listed[0], json!({ "items": [], "total": 0, "authors": empty }));

    set_git_name(root, "Ada");
    import(root, "Alpha", "# Alpha\n\nThe gateway routes requests.");
    import(root, "Gamma", "# Gamma\n\nThe gateway retries requests.");
    set_git_name(root, "Bob");
    import(root, "Beta", "# Beta\n\nThe gateway times requests out.");
    std::fs::remove_file(root.join(".gitconfig")).expect("no name");
    import(root, "Delta", "# Delta\n\nThe gateway logs requests.");

    let list = |extra: &[&str]| -> Value {
        let mut args = vec!["docs", "list", "--json"];
        args.extend_from_slice(extra);

        run(root, &args, "").remove(0)
    };

    assert_eq!(
        titles(&list(&["--author", "Ada", "--sort", "title"])),
        ["Alpha", "Gamma"]
    );
    assert_eq!(titles(&list(&["--author", "ada"])), Vec::<&str>::new());
    assert_eq!(titles(&list(&["--author-unknown"])), ["Delta"]);
    assert_eq!(
        titles(&list(&["--sort", "addedBy"])),
        ["Alpha", "Gamma", "Beta", "Delta"]
    );
    assert_eq!(
        titles(&list(&["--sort", "addedBy", "--desc"])),
        ["Beta", "Gamma", "Alpha", "Delta"]
    );

    let people = json!({ "people": [{ "name": "Ada", "count": 2 }, { "name": "Bob", "count": 1 }], "unknown": 1 });
    assert_eq!(list(&["--author", "Bob"])["authors"], people);
    assert_eq!(list(&["--author", "Bob"])["total"], 1);

    // A name that is not one, or two filters that exclude each other, are usage errors.
    for args in [
        &["docs", "list", "--json", "--author", " "][..],
        &["docs", "list", "--json", "--author", "Ada", "--author-unknown"],
        &["docs", "list", "--json", "--sort", "addedby"],
    ] {
        assert_eq!(exit_code(root, args), Some(2), "{args:?}");
    }
}
