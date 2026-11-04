//! Drafts, team approval and review through the real executable: the commands Harness Desktop
//! runs, their JSON and their refusals, on this computer's SQLite file and (when a libSQL server is
//! configured) as a team database with two people.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const BINARY: &str = env!("CARGO_BIN_EXE_harness-knowledge");

/// One computer: a home, a knowledge folder, and the team database of the run when there is one.
struct Lab {
    root: tempfile::TempDir,
    team: Option<String>,
}

/// What one run printed.
struct Ran {
    code: Option<i32>,
    lines: Vec<Value>,
}

impl Ran {
    fn json(&self) -> &Value {
        self.lines.last().expect("a JSON line")
    }
}

impl Lab {
    fn local() -> Self {
        Self {
            root: tempfile::tempdir().expect("temp dir"),
            team: None,
        }
    }

    fn team(url: &str) -> Self {
        Self {
            root: tempfile::tempdir().expect("temp dir"),
            team: Some(url.to_owned()),
        }
    }

    /// A project folder of a person: a git repository whose own `user.name` is `name`.
    fn person(&self, name: &str) -> PathBuf {
        let folder = self.root.path().join(format!("project-{}", name.to_lowercase()));
        std::fs::create_dir_all(&folder).expect("folder");

        for args in [
            &["init", "--quiet"][..],
            &["config", "user.name", name],
            &["config", "user.email", "person@example.com"],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(&folder)
                .env("HOME", self.root.path())
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?}");
        }

        dunce::canonicalize(&folder).expect("canonical")
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(BINARY);

        command
            .args(args)
            .env_clear()
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .envs(
                ["SystemRoot", "SYSTEMROOT", "windir", "PATHEXT", "TEMP", "TMP"]
                    .iter()
                    .filter_map(|name| std::env::var_os(name).map(|value| (*name, value))),
            )
            .env("HARNESS_KNOWLEDGE_DIR", self.root.path().join("knowledge"))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .current_dir(cwd);

        if let Some(url) = &self.team {
            command
                .env("HARNESS_KNOWLEDGE_DATABASE_URL", url)
                .env("HARNESS_KNOWLEDGE_REPOSITORY_DOCUMENTS", "remote")
                .env("HARNESS_KNOWLEDGE_GLOBAL_DOCUMENTS", "remote");
        }

        command
    }

    /// Runs `args` in `cwd` with `stdin`: the exit code and the stdout lines as JSON.
    fn run(&self, cwd: &Path, args: &[&str], stdin: &str) -> Ran {
        let mut child = self
            .command(cwd, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("starts");

        // The command may end before it reads everything.
        let _ = child.stdin.take().expect("stdin").write_all(stdin.as_bytes());
        let output = child.wait_with_output().expect("finishes");

        Ran {
            code: output.status.code(),
            lines: String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("{args:?}: JSON: {line:?}")))
                .collect(),
        }
    }

    /// The JSON of a command that succeeded.
    fn ok(&self, cwd: &Path, args: &[&str]) -> Value {
        let ran = self.run(cwd, args, "");
        assert_eq!(ran.code, Some(0), "{args:?}: {:?}", ran.lines);

        ran.json().clone()
    }

    /// The code of a command that was refused (exit 1).
    fn refused(&self, cwd: &Path, args: &[&str]) -> String {
        let ran = self.run(cwd, args, "");
        assert_eq!(ran.code, Some(1), "{args:?}: {:?}", ran.lines);

        ran.json()["error"].as_str().expect("an error code").to_owned()
    }

    /// Imports `manifest` in `cwd`: the events.
    fn import(&self, cwd: &Path, flags: &[&str], manifest: &Value) -> Vec<Value> {
        let mut args = vec!["docs", "import", "--ndjson"];
        args.extend_from_slice(flags);

        let ran = self.run(cwd, &args, &manifest.to_string());
        assert_eq!(ran.code, Some(0), "{args:?}: {:?}", ran.lines);

        ran.lines
    }

    fn document(&self, cwd: &Path, id: &Value) -> Value {
        self.ok(cwd, &["docs", "get", "--json", &id.to_string()])["document"].clone()
    }

    /// The text an agent's `knowledge_search` answers for `query`, over MCP.
    fn agent_search(&self, cwd: &Path, query: &str) -> String {
        let mut child = self
            .command(cwd, &["mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("starts");
        let requests = [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "e2e" } } }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "knowledge_search",
                    "arguments": { "query": query, "project_path": cwd, "scope": "all" } } }),
        ];

        let mut stdin = child.stdin.take().expect("stdin");

        for request in requests {
            writeln!(stdin, "{request}").expect("request");
        }

        drop(stdin);

        let output = child.wait_with_output().expect("finishes");
        let reply = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|reply| reply["id"] == 2)
            .expect("the search reply");

        reply["result"]["content"][0]["text"].as_str().expect("text").to_owned()
    }

    fn write(&self, name: &str, text: &str) -> String {
        let path = self.root.path().join(name);
        std::fs::write(&path, text).expect("file");

        path.to_string_lossy().into_owned()
    }
}

fn kinds(history: &Value) -> Vec<&str> {
    history["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["kind"].as_str().expect("kind"))
        .collect()
}

#[test]
fn the_features_list_drafts_and_approval() {
    let lab = Lab::local();
    let output = lab.command(lab.root.path(), &["features"]).output().expect("runs");
    let text = String::from_utf8_lossy(&output.stdout);
    let features: Vec<&str> = text.lines().collect();

    assert!(
        features.contains(&"docs-drafts") && features.contains(&"docs-approval"),
        "{features:?}"
    );
}

#[test]
fn on_this_computer_a_draft_goes_live_with_its_history_comments_and_original() {
    let lab = Lab::local();
    let ada = lab.person("Ada");
    let word = "zebrafishtwo";
    let original = format!("# Runbook\n\nRestart the {word} gateway.\r\n\r\n## Steps\n\nOne\n\nTwo\n");
    let file = lab.write("runbook.md", &original);

    let events = lab.import(&ada, &["--draft"], &json!({ "paths": [file] }));
    let added = events.iter().find(|event| event["status"] == "added").expect("added");
    assert_eq!(added["draft"], true, "{added}");

    let listed = lab.ok(&ada, &["docs", "list", "--json", "--status", "draft"]);
    let id = listed["items"][0]["id"].clone();
    let doc = lab.document(&ada, &id);
    assert_eq!(
        (doc["status"].clone(), doc["revision"].clone()),
        (json!("draft"), json!(0))
    );
    assert_eq!(doc["lastRequest"], Value::Null);
    assert!(
        !lab.agent_search(&ada, word).contains("Runbook"),
        "agents never see a draft"
    );

    // A new document compares with nothing: every line is added.
    let compared = lab.ok(&ada, &["docs", "compare", "--json", &id.to_string()]);
    assert_eq!(
        (compared["live"].clone(), compared["pending"].clone()),
        (Value::Null, id.clone())
    );
    let sections = compared["sections"].as_array().expect("sections");
    assert!(!sections.is_empty());
    assert!(
        sections
            .iter()
            .all(|section| section["status"] == "added" && section["deletions"] == 0)
    );
    assert_eq!(sections[0]["hunks"][0]["lines"][0]["kind"], "add");

    // Nobody can be asked on this computer.
    for args in [
        &["docs", "ask", "--json", &id.to_string(), "--expect", "0"][..],
        &["docs", "approve", "--json", &id.to_string(), "--expect", "0"],
    ] {
        assert_eq!(lab.refused(&ada, args), "approvalOff");
    }
    assert_eq!(
        lab.refused(&ada, &["docs", "go-live", "--json", &id.to_string(), "--expect", "9"]),
        "changed"
    );

    // A comment changes nothing but is kept, with its line.
    let comments = lab.write(
        "comments.json",
        r#"[{"section":"Steps","side":"new","from":1,"to":1,"body":"Say which step first"}]"#,
    );
    #[cfg(unix)]
    std::fs::set_permissions(&comments, std::os::unix::fs::PermissionsExt::from_mode(0o600)).expect("mode");
    let reviewed = lab.ok(
        &ada,
        &[
            "docs",
            "review",
            "--json",
            &id.to_string(),
            "--expect",
            "0",
            "--verdict",
            "comment",
            "--body",
            "Notes",
            "--comments",
            &comments,
        ],
    );
    assert_eq!(reviewed["status"], "draft");
    assert_eq!(reviewed["commentIds"].as_array().map(Vec::len), Some(2));

    let all = lab.ok(&ada, &["docs", "comments", "--json", &id.to_string()]);
    let all = all["comments"].as_array().expect("comments");
    assert_eq!(all.len(), 2);
    assert!(
        all.iter().all(|comment| comment["outdated"] == false),
        "nothing changed the content that was commented"
    );

    let line_id = all[1]["id"].to_string();
    let replied = lab.ok(
        &ada,
        &[
            "docs",
            "comment",
            "reply",
            "--json",
            &id.to_string(),
            &line_id,
            "--body",
            "First",
        ],
    );
    assert_eq!(replied["comment"]["parentId"], all[1]["id"]);
    let resolved = lab.ok(
        &ada,
        &["docs", "comment", "resolve", "--json", &id.to_string(), &line_id],
    );
    assert!(resolved["comment"]["resolvedAt"].is_string());

    let live = lab.ok(&ada, &["docs", "go-live", "--json", &id.to_string(), "--expect", "0"]);
    assert_eq!(
        live,
        json!({ "id": id, "status": "live", "revision": 1, "promoted": false })
    );
    assert!(
        lab.agent_search(&ada, word).contains("Runbook"),
        "found after it went live"
    );

    let history = lab.ok(&ada, &["docs", "history", "--json", &id.to_string()]);
    assert_eq!(kinds(&history), ["wentLive", "drafted"]);
    assert_eq!(history["total"], 2);

    // Going live ends the review: its comments are gone, the history keeps its record.
    let gone = lab.ok(&ada, &["docs", "comments", "--json", &id.to_string()]);
    assert_eq!(gone, json!({ "comments": [] }));

    // The original bytes come back as imported, and a link is never written through.
    let to = lab.root.path().join("saved.md");
    let saved = lab.ok(
        &ada,
        &[
            "docs",
            "download",
            "--json",
            &id.to_string(),
            "--to",
            &to.to_string_lossy(),
        ],
    );
    assert_eq!(saved, json!({ "path": to.to_string_lossy(), "original": true }));
    assert_eq!(std::fs::read(&to).expect("saved"), original.as_bytes());

    #[cfg(unix)]
    {
        let link = lab.root.path().join("link.md");
        std::os::unix::fs::symlink(&to, &link).expect("link");
        assert_eq!(
            lab.refused(
                &ada,
                &[
                    "docs",
                    "download",
                    "--json",
                    &id.to_string(),
                    "--to",
                    &link.to_string_lossy()
                ]
            ),
            "refused"
        );
    }
}

#[test]
fn approval_upgrade_and_stats_on_this_computer() {
    let lab = Lab::local();
    let ada = lab.person("Ada");

    assert_eq!(
        lab.ok(&ada, &["approval", "get", "--json"]),
        json!({ "approval": false, "place": "local", "writable": null, "changedBy": null, "changedAt": null })
    );
    assert_eq!(lab.refused(&ada, &["approval", "set", "--json", "on"]), "refused");
    assert_eq!(
        lab.ok(&ada, &["upgrade", "--json"]),
        json!({ "upgraded": false, "waiting": false })
    );

    let file = lab.write("a.md", "# A\n\nWords.\n");
    lab.import(&ada, &["--draft"], &json!({ "paths": [file] }));

    let stats = lab.ok(&ada, &["stats", "--json"]);
    assert_eq!(stats["me"], json!({ "name": "Ada" }), "the git name of the folder");
    assert_eq!(
        lab.ok(lab.root.path(), &["stats", "--json"])["me"],
        json!({ "name": null }),
        "a folder with no name"
    );
    assert_eq!(
        (
            stats["documents"].clone(),
            stats["drafts"].clone(),
            stats["waiting"].clone(),
            stats["pending"].clone()
        ),
        (json!(1), json!(1), json!(0), json!(0))
    );
    assert_eq!(stats["approval"], Value::Null);

    // Usage errors exit 2, a document that is not there is a refusal.
    assert_eq!(lab.run(&ada, &["docs", "ask", "--json", "1"], "").code, Some(2));
    assert_eq!(lab.run(&ada, &["approval", "flip"], "").code, Some(2));
    assert_eq!(lab.refused(&ada, &["docs", "history", "--json", "99"]), "notFound");
    assert_eq!(lab.refused(&ada, &["docs", "compare", "--json", "99"]), "notFound");
    assert_eq!(lab.refused(&ada, &["docs", "comments", "--json", "99"]), "notFound");
}

/// The team libSQL server under test: no more than one test at a time uses it.
fn team_server() -> Option<(String, std::fs::File)> {
    let url = std::env::var("HARNESS_KNOWLEDGE_TEST_LIBSQL_URL")
        .ok()
        .filter(|url| !url.trim().is_empty());

    let Some(url) = url else {
        eprintln!("skipped: set HARNESS_KNOWLEDGE_TEST_LIBSQL_URL to run this case on a libSQL server");
        return None;
    };

    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::env::temp_dir().join("harness-knowledge-libsql-tests.lock"))
        .expect("lock file");
    lock.lock().expect("lock");

    Some((url, lock))
}

/// Puts the team database back as it was: the documents of the run are deleted, approval is as it was.
struct Cleanup<'a> {
    lab: &'a Lab,
    cwd: PathBuf,
    ids: Vec<Value>,
    approval: bool,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        for id in &self.ids {
            self.lab
                .run(&self.cwd, &["docs", "delete", "--json", &id.to_string()], "");
        }

        let value = if self.approval { "on" } else { "off" };
        self.lab.run(&self.cwd, &["approval", "set", "--json", value], "");
    }
}

#[test]
fn team_two_people_draft_ask_review_and_update_a_document_agents_see_only_after_approval() {
    let Some((url, _lock)) = team_server() else {
        return;
    };

    let lab = Lab::team(&url);
    let ada = lab.person("Ada");
    let grace = lab.person("Grace");
    let word = format!("zebrafish{}", std::process::id());

    let before = lab.ok(&ada, &["approval", "get", "--json"]);
    assert_eq!(before["place"], "remote");

    let mut cleanup = Cleanup {
        lab: &lab,
        cwd: ada.clone(),
        ids: Vec::new(),
        approval: before["approval"] == true,
    };

    let on = lab.ok(&ada, &["approval", "set", "--json", "on"]);
    assert_eq!(on["approval"], true);
    assert_eq!(on["changedBy"], "Ada", "who switched it is recorded with the switch");
    assert!(
        on["changedAt"]
            .as_str()
            .is_some_and(|at| at.contains('T') && at.ends_with('Z')),
        "{on}"
    );
    assert_eq!(lab.ok(&grace, &["approval", "get", "--json"])["changedBy"], "Ada");
    assert_eq!(lab.ok(&grace, &["stats", "--json"])["approval"], true);

    // Nobody changes the switch without a name the team can see: this folder has none.
    assert_eq!(
        lab.refused(lab.root.path(), &["approval", "set", "--json", "off"]),
        "noName"
    );
    assert_eq!(lab.ok(&ada, &["approval", "get", "--json"])["approval"], true);

    // Approval on: every import is a draft, and an agent finds nothing.
    let first = format!("# Runbook\n\nRestart the {word} gateway.\n\n## Steps\n\nOne\n");
    let path = lab.write("runbook.md", &first);
    let events = lab.import(&ada, &[], &json!({ "paths": [path] }));
    let added = events.iter().find(|event| event["status"] == "added").expect("added");
    assert_eq!(added["draft"], true, "{added}");

    let id = added["docId"].clone();
    cleanup.ids.push(id.clone());
    let id_text = id.to_string();
    assert!(
        !lab.agent_search(&grace, &word).contains("Runbook"),
        "a draft is not found"
    );

    let doc = lab.document(&ada, &id);
    assert_eq!(doc["status"], "draft");
    let at = doc["revision"].as_i64().expect("revision").to_string();

    let waiting = lab.ok(
        &ada,
        &[
            "docs",
            "ask",
            "--json",
            &id_text,
            "--expect",
            &at,
            "--comment",
            "Please look",
        ],
    );
    assert_eq!(waiting["status"], "waiting");
    let at = waiting["revision"].as_i64().expect("revision").to_string();

    assert_eq!(
        lab.refused(&ada, &["docs", "approve", "--json", &id_text, "--expect", &at]),
        "selfApproval"
    );
    assert_eq!(
        lab.refused(&ada, &["docs", "go-live", "--json", &id_text, "--expect", &at]),
        "approvalOn"
    );
    assert_eq!(
        lab.refused(
            &grace,
            &["docs", "request-changes", "--json", &id_text, "--expect", &at]
        ),
        "commentRequired"
    );

    // Only the author takes an ask back, and it can be asked again.
    assert_eq!(
        lab.refused(&grace, &["docs", "take-back", "--json", &id_text, "--expect", &at]),
        "notAuthor"
    );
    let back = lab.ok(&ada, &["docs", "take-back", "--json", &id_text, "--expect", &at]);
    assert_eq!(back["status"], "draft");
    let back_at = back["revision"].as_i64().expect("revision").to_string();
    let asked_again = lab.ok(&ada, &["docs", "ask", "--json", &id_text, "--expect", &back_at]);
    assert_eq!(asked_again["status"], "waiting");
    let at = asked_again["revision"].as_i64().expect("revision").to_string();

    let changes = lab.ok(
        &grace,
        &[
            "docs",
            "request-changes",
            "--json",
            &id_text,
            "--expect",
            &at,
            "--comment",
            "Add the second step",
        ],
    );
    assert_eq!(changes["status"], "changesRequested");
    let doc = lab.document(&ada, &id);
    assert_eq!(doc["lastChange"]["by"], "Grace");
    assert_eq!(doc["lastChange"]["comment"], "Add the second step");

    // Replace and ask again in one step.
    let second = format!("# Runbook\n\nRestart the {word} gateway.\n\n## Steps\n\nOne\n\nTwo\n");
    let again = lab.write("runbook-2.md", &second);
    let replaced = lab.import(
        &ada,
        &["--ask", "--note", "Added step two"],
        &json!({ "paths": [again], "replacing": { again.clone(): id } }),
    );
    assert!(replaced.iter().any(|event| event["draft"] == true), "{replaced:?}");
    let doc = lab.document(&ada, &id);
    assert_eq!(doc["status"], "waiting");
    let at = doc["revision"].as_i64().expect("revision").to_string();

    // A reviewer's approval with a line comment makes it live.
    let comments = lab.write(
        "comments.json",
        r#"[{"section":"Steps","side":"new","from":3,"to":3,"body":"Good"}]"#,
    );
    #[cfg(unix)]
    std::fs::set_permissions(&comments, std::os::unix::fs::PermissionsExt::from_mode(0o600)).expect("mode");
    let approved = lab.ok(
        &grace,
        &[
            "docs",
            "review",
            "--json",
            &id_text,
            "--expect",
            &at,
            "--verdict",
            "approve",
            "--body",
            "Looks right",
            "--comments",
            &comments,
        ],
    );
    assert_eq!(
        (approved["status"].clone(), approved["id"].clone()),
        (json!("live"), id.clone())
    );
    assert!(
        lab.agent_search(&grace, &word).contains("Runbook"),
        "found only after the approval"
    );

    let history = lab.ok(&grace, &["docs", "history", "--json", &id_text]);
    assert_eq!(
        kinds(&history),
        [
            "approved",
            "asked",
            "edited",
            "changesRequested",
            "asked",
            "withdrawn",
            "asked",
            "drafted"
        ],
        "{history}"
    );
    assert_eq!(
        lab.ok(&ada, &["docs", "comments", "--json", &id_text])["comments"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "approving ended the review and removed its comments"
    );

    // The original comes back as the second file.
    let to = lab.root.path().join("saved.md");
    let saved = lab.ok(
        &grace,
        &["docs", "download", "--json", &id_text, "--to", &to.to_string_lossy()],
    );
    assert_eq!(saved["original"], true);
    assert_eq!(std::fs::read(&to).expect("saved"), second.as_bytes());

    // An update of a live document is a pending version: agents keep the old text until it is approved.
    let third = format!("# Runbook\n\nRestart the {word} gateway twice.\n\n## Steps\n\nOne\n\nTwo\n");
    let update = lab.write("runbook-3.md", &third);
    let events = lab.import(
        &ada,
        &["--ask"],
        &json!({ "paths": [update], "replacing": { update.clone(): id } }),
    );
    assert!(events.iter().any(|event| event["status"] == "pending"), "{events:?}");

    let live = lab.document(&grace, &id);
    assert_eq!(live["pending"]["status"], "waiting");
    let pending_at = live["pending"]["revision"].as_i64().expect("revision").to_string();
    assert!(
        !lab.agent_search(&grace, "twice").contains("twice"),
        "the old text is still live"
    );

    let compared = lab.ok(&grace, &["docs", "compare", "--json", &id_text]);
    assert_eq!(compared["live"], id);
    assert_eq!(compared["sections"][0]["status"], "modified");

    let promoted = lab.ok(
        &grace,
        &["docs", "approve", "--json", &id_text, "--expect", &pending_at],
    );
    assert_eq!(
        (promoted["id"].clone(), promoted["promoted"].clone()),
        (id.clone(), json!(true))
    );
    assert!(
        lab.agent_search(&grace, "twice").contains("Runbook"),
        "the new text is live"
    );

    drop(cleanup);
    assert_eq!(
        lab.ok(&ada, &["approval", "get", "--json"])["approval"],
        before["approval"]
    );
}
