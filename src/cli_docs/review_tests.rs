//! The review commands: each prints its shape, and each refusal code is the one a command gives.
//! Approval is on in a store that is its own remote place (as the review's own tests do), and off
//! on this computer.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::review::{submit_on, transition_on};
use super::*;
use crate::config::Routing;
use crate::location::{DIR_VAR, FILE_NAME};
use crate::review::{APPROVAL_KEY, Submission, Verdict};
use crate::store::testing::{Seed, insert, store};
use crate::store::{NewComment, ReviewAction, ReviewChange, Side, SqliteStore, Status};

/// A store with team approval on, that is its own remote place.
fn team() -> SqliteStore {
    let mut db = store();
    db.set_meta(APPROVAL_KEY, "on").expect("approval");

    db
}

fn draft(db: &mut SqliteStore) -> i64 {
    insert(
        db,
        &Seed::new("Guide")
            .status(Status::Draft)
            .by("Ada", None)
            .chunk("Intro", "first line\nsecond line\nthird line")
            .chunk("Usage", "run it"),
    )
}

fn revision(db: &mut SqliteStore, id: i64) -> i64 {
    let detail = Detail {
        drafts: true,
        ..Detail::default()
    };

    db.get(id, &detail).expect("get").expect("row").doc.review_rev
}

/// A transition by `actor` on the row `id` at its current revision.
fn step(db: &mut SqliteStore, actor: Option<&str>, verb: &str, id: i64, comment: Option<&str>) -> Reply {
    let expect = revision(db, id);

    transition_on(db, Place::Remote, actor, verb, id, expect, comment)
}

fn code_of(reply: &Reply) -> String {
    match reply {
        Err(Failure::Refused { code, .. }) => (*code).to_owned(),
        Err(Failure::Unreachable(_)) => "unreachable".to_owned(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn done(reply: Reply) -> Value {
    reply.expect("done").json
}

#[test]
fn a_draft_is_asked_changes_are_requested_it_is_asked_again_and_a_teammate_approves() {
    let mut db = team();
    let id = draft(&mut db);

    let asked = done(step(&mut db, Some("Ada"), "ask", id, Some("Please look")));
    assert_eq!(
        asked,
        json!({ "id": id, "status": "waiting", "revision": 1, "promoted": false })
    );

    let changes = done(step(
        &mut db,
        Some("Grace"),
        "request-changes",
        id,
        Some("Add the table"),
    ));
    assert_eq!(changes["status"], "changesRequested");

    assert_eq!(done(step(&mut db, Some("Ada"), "ask", id, None))["status"], "waiting");

    // The author and the asker cannot approve what they wrote or asked for, whatever the case or spaces.
    assert_eq!(
        code_of(&step(&mut db, Some(" ada "), "approve", id, None)),
        "selfApproval"
    );

    let approved = done(step(&mut db, Some("Grace"), "approve", id, None));
    assert_eq!(approved["status"], "live");
    assert_eq!(approved["promoted"], false);
    assert_eq!(approved["id"], id);
}

#[test]
fn a_pending_version_is_reviewed_by_its_live_id_and_approving_it_answers_the_live_id() {
    let mut db = team();
    let live = insert(&mut db, &Seed::new("Live").source("/live").chunk("Intro", "old"));
    let pending = insert(
        &mut db,
        &Seed::new("Live v2")
            .source("/live-2")
            .by("Ada", None)
            .pending_of(live)
            .chunk("Intro", "new"),
    );

    // The live id stands for its pending version: its revision is the one to expect.
    let expect = revision(&mut db, pending);
    let asked = done(transition_on(
        &mut db,
        Place::Remote,
        Some("Ada"),
        "ask",
        live,
        expect,
        None,
    ));
    assert_eq!(asked["id"], pending);
    assert_eq!(asked["status"], "waiting");

    let expect = revision(&mut db, pending);
    let approved = done(transition_on(
        &mut db,
        Place::Remote,
        Some("Grace"),
        "approve",
        live,
        expect,
        None,
    ));
    assert_eq!(approved["id"], live, "the live id after a promote");
    assert_eq!(approved["status"], "live");
    assert_eq!(approved["promoted"], true);
}

#[test]
fn a_rename_after_the_ask_makes_the_approval_of_the_asked_revision_changed() {
    let mut db = team();
    let id = draft(&mut db);
    let asked = done(step(&mut db, Some("Ada"), "ask", id, None));
    let expect = asked["revision"].as_i64().expect("revision");

    assert!(db.rename(id, "Guide, renamed", Some("Ada"), 9).expect("rename"));

    let refused = transition_on(&mut db, Place::Remote, Some("Grace"), "approve", id, expect, None);
    assert_eq!(code_of(&refused), "changed");

    // Asked again, the new title is what a teammate approves.
    let again = done(step(&mut db, Some("Ada"), "ask", id, None));
    assert_eq!(again["status"], "waiting");
    assert_eq!(
        done(step(&mut db, Some("Grace"), "approve", id, None))["status"],
        "live"
    );
}

#[test]
fn take_back_is_a_transition_of_the_author_and_prints_the_shape_of_one() {
    let mut db = team();
    let id = draft(&mut db);
    let asked = done(step(&mut db, Some("Ada"), "ask", id, None));

    // The note has no place here, and only the author takes it back.
    assert_eq!(
        code_of(&step(&mut db, Some("Grace"), "take-back", id, None)),
        "notAuthor"
    );
    let expect = asked["revision"].as_i64().expect("revision");
    let back = done(transition_on(
        &mut db,
        Place::Remote,
        Some("Ada"),
        "take-back",
        id,
        expect,
        None,
    ));

    assert_eq!(
        back,
        json!({ "id": id, "status": "draft", "revision": expect + 1, "promoted": false })
    );
}

#[test]
fn every_refusal_has_its_code() {
    let mut db = team();
    let id = draft(&mut db);

    assert_eq!(
        code_of(&transition_on(&mut db, Place::Remote, Some("Ada"), "ask", 999, 0, None)),
        "notFound"
    );
    assert_eq!(
        code_of(&transition_on(&mut db, Place::Remote, Some("Ada"), "ask", id, 7, None)),
        "changed"
    );
    assert_eq!(
        code_of(&step(&mut db, Some("Ada"), "approve", id, None)),
        "wrongState",
        "a draft is not waiting"
    );
    assert_eq!(code_of(&step(&mut db, Some("Ada"), "go-live", id, None)), "approvalOn");
    assert_eq!(code_of(&step(&mut db, None, "ask", id, None)), "noName");
    assert_eq!(
        code_of(&step(&mut db, Some("Ada"), "request-changes", id, None)),
        "commentRequired"
    );
    assert_eq!(
        code_of(&step(&mut db, Some("Ada"), "request-changes", id, Some("  "))),
        "commentRequired"
    );

    // Approval off, as on this computer: nobody to ask, go live instead.
    let mut off = store();
    let id = draft(&mut off);
    assert_eq!(code_of(&step(&mut off, Some("Ada"), "ask", id, None)), "approvalOff");
    assert_eq!(
        code_of(&step(&mut off, Some("Grace"), "approve", id, None)),
        "approvalOff"
    );
    assert_eq!(done(step(&mut off, Some("Ada"), "go-live", id, None))["status"], "live");
}

#[test]
fn readonly_and_unreachable_are_the_codes_of_their_errors() {
    use crate::review::{Code, Error, Refusal};

    let read_only = Error::Refused(Refusal {
        code: Code::ReadOnly,
        message: "read-only".to_owned(),
    });
    assert_eq!(
        review::failure(read_only),
        Failure::Refused {
            code: "readOnly",
            message: "read-only".to_owned()
        }
    );

    let down = Error::Refused(Refusal {
        code: Code::Unreachable,
        message: "remote knowledge is unreachable right now".to_owned(),
    });
    assert_eq!(
        review::failure(down),
        Failure::Unreachable("remote knowledge is unreachable right now".to_owned())
    );
}

#[test]
fn the_arguments_of_a_transition_are_checked() {
    let lab = Lab::new();

    for args in [
        &["ask", "--json", "1"][..],
        &["ask", "--json", "--expect", "-1", "1"],
        &["ask", "--json", "--expect", "x", "1"],
        &["approve", "--json", "--expect", "0", "1", "2"],
        &["approve", "--json", "--expect", "0", "--comment", "no", "1"],
        &["ask", "--expect", "0", "1"],
        &["take-back", "--json", "--expect", "0", "--comment", "no", "1"],
    ] {
        assert_eq!(lab.run(args).map(|_| ()), Err(Failure::Usage), "{args:?}");
    }
}

#[test]
fn a_review_comments_on_lines_and_the_author_may_comment_but_not_decide() {
    let mut db = team();
    let id = draft(&mut db);
    step(&mut db, Some("Ada"), "ask", id, None).expect("asked");
    let expect = revision(&mut db, id);

    let line = NewComment::on_lines("Intro", Side::New, 2, 3, "Which one?");
    let comment_only = |body: Option<&str>| Submission {
        verdict: Verdict::Comment,
        body: body.map(str::to_owned),
        comments: vec![line.clone()],
    };

    let shown = done(submit_on(
        &mut db,
        Place::Remote,
        Some("Ada"),
        id,
        expect,
        &comment_only(Some("Overall")),
    ));
    assert_eq!(shown["status"], "waiting", "a comment changes no state");
    assert_eq!(shown["revision"], expect);
    assert_eq!(shown["commentIds"].as_array().map(Vec::len), Some(2), "{shown}");

    let decided = Submission {
        verdict: Verdict::Approve,
        body: None,
        comments: Vec::new(),
    };
    assert_eq!(
        code_of(&submit_on(&mut db, Place::Remote, Some("Ada"), id, expect, &decided)),
        "selfApproval"
    );

    let changes = Submission {
        verdict: Verdict::RequestChanges,
        body: None,
        comments: Vec::new(),
    };
    assert_eq!(
        code_of(&submit_on(&mut db, Place::Remote, Some("Grace"), id, expect, &changes)),
        "commentRequired"
    );

    let with_line = Submission {
        verdict: Verdict::Approve,
        body: Some("Looks right".to_owned()),
        comments: vec![NewComment::on_lines("Usage", Side::New, 1, 1, "Nice")],
    };
    let approved = done(submit_on(&mut db, Place::Remote, Some("Grace"), id, expect, &with_line));
    assert_eq!(approved["status"], "live");
    assert_eq!(approved["commentIds"].as_array().map(Vec::len), Some(2));
}

/// A folder with a database of this computer: its commands run through [`docs_reply`].
struct Lab {
    root: tempfile::TempDir,
    env: Env,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, root.path().join("knowledge").to_string_lossy());

        Self { root, env }
    }

    fn database(&self) -> SqliteStore {
        SqliteStore::open(&self.root.path().join("knowledge").join(FILE_NAME)).expect("database")
    }

    fn run(&self, args: &[&str]) -> Reply {
        let args: Vec<Option<&str>> = args.iter().map(|arg| Some(*arg)).collect();
        let context = Context::new(&self.env, &Backend::Sqlite, Routing::local());

        docs_reply(&context, &args, &mut std::io::empty())
    }

    fn json(&self, args: &[&str]) -> Value {
        self.run(args)
            .unwrap_or_else(|failure| panic!("{args:?}: {failure:?}"))
            .json
    }

    fn refusal(&self, args: &[&str]) -> String {
        code_of(&self.run(args))
    }

    fn private_file(&self, name: &str, text: &str) -> String {
        let path = self.root.path().join(name);
        std::fs::write(&path, text).expect("file");

        #[cfg(unix)]
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).expect("mode");

        path.to_string_lossy().into_owned()
    }
}

#[test]
fn on_this_computer_a_draft_goes_live_and_ask_approve_and_request_changes_are_refused() {
    let lab = Lab::new();
    let id = insert(
        &mut lab.database(),
        &Seed::new("Guide").status(Status::Draft).chunk("Intro", "words"),
    )
    .to_string();

    assert_eq!(lab.refusal(&["ask", "--json", &id, "--expect", "0"]), "approvalOff");
    assert_eq!(
        lab.refusal(&["request-changes", "--json", &id, "--expect", "0", "--comment", "No"]),
        "approvalOff"
    );
    assert_eq!(lab.refusal(&["approve", "--json", &id, "--expect", "0"]), "approvalOff");
    assert_eq!(lab.refusal(&["go-live", "--json", &id, "--expect", "3"]), "changed");
    assert_eq!(lab.refusal(&["go-live", "--json", "99", "--expect", "0"]), "notFound");

    let live = lab.json(&["go-live", "--json", &id, "--expect", "0"]);
    assert_eq!(live["status"], "live");
    assert_eq!(live["id"].to_string(), id);
    assert_eq!(live["revision"], 1);

    assert_eq!(
        lab.refusal(&["go-live", "--json", &id, "--expect", "1"]),
        "wrongState",
        "a live document is not a draft"
    );
}

#[test]
fn a_review_by_file_is_stored_and_its_comments_are_listed_answered_and_resolved() {
    let lab = Lab::new();
    let id = insert(
        &mut lab.database(),
        &Seed::new("Guide")
            .status(Status::Draft)
            .chunk("Intro", "first line\nsecond line"),
    )
    .to_string();
    let file = lab.private_file(
        "comments.json",
        r#"[{"section":"Intro","side":"new","from":1,"to":2,"body":"Say more"},
            {"section":"Intro","occurrence":2,"side":"old","from":1,"to":1,"body":"The other Intro"}]"#,
    );

    let reviewed = lab.json(&[
        "review",
        "--json",
        &id,
        "--expect",
        "0",
        "--verdict",
        "comment",
        "--body",
        "Overall",
        "--comments",
        &file,
    ]);
    assert_eq!(reviewed["status"], "draft");
    assert_eq!(reviewed["commentIds"].as_array().map(Vec::len), Some(3));

    let listed = lab.json(&["comments", "--json", &id]);
    let all = listed["comments"].as_array().expect("comments");
    assert_eq!(all.len(), 3);

    let overall = &all[0];
    assert_eq!(overall["body"], "Overall");
    assert_eq!(overall["section"], Value::Null);
    assert_eq!(overall["occurrence"], Value::Null);
    assert_eq!(overall["parentId"], Value::Null);
    assert_eq!(overall["outdated"], false);

    let line = &all[1];
    assert_eq!(
        (
            &line["section"],
            &line["side"],
            &line["from"],
            &line["to"],
            &line["body"]
        ),
        (&json!("Intro"), &json!("new"), &json!(1), &json!(2), &json!("Say more"))
    );
    assert_eq!(
        line["occurrence"], 1,
        "the first section of its heading when the file names none"
    );
    assert_eq!(all[2]["occurrence"], 2);
    assert_eq!(line["reviewId"], overall["reviewId"]);
    assert_eq!(line["resolvedAt"], Value::Null);
    assert!(line["at"].as_str().is_some_and(|at| at.contains('T')));

    let line_id = line["id"].to_string();
    let replied = lab.json(&["comment", "reply", "--json", &id, &line_id, "--body", "Done"]);
    assert_eq!(replied["comment"]["body"], "Done");
    assert_eq!(replied["comment"]["parentId"], line["id"]);
    assert_eq!(replied["comment"]["reviewId"], Value::Null);

    let resolved = lab.json(&["comment", "resolve", "--json", &id, &line_id]);
    assert!(resolved["comment"]["resolvedAt"].is_string());
    let reopened = lab.json(&["comment", "unresolve", "--json", &id, &line_id]);
    assert_eq!(reopened["comment"]["resolvedAt"], Value::Null);

    // A reply is not answered, and a comment that is not there is not found.
    let reply_id = replied["comment"]["id"].to_string();
    assert_eq!(
        lab.refusal(&["comment", "reply", "--json", &id, &reply_id, "--body", "x"]),
        "notFound"
    );
    assert_eq!(lab.refusal(&["comment", "resolve", "--json", &id, "999"]), "notFound");
}

#[test]
fn the_comments_file_is_read_with_its_limits() {
    let lab = Lab::new();
    let id = insert(
        &mut lab.database(),
        &Seed::new("Guide").status(Status::Draft).chunk("A", "x"),
    )
    .to_string();
    let review = |file: &str| {
        lab.run(&[
            "review",
            "--json",
            &id,
            "--expect",
            "0",
            "--verdict",
            "comment",
            "--comments",
            file,
        ])
    };

    assert!(matches!(review("/no/such/comments.json"), Err(Failure::Failed(_))));

    let not_json = lab.private_file("bad.json", "not json");
    assert!(matches!(review(&not_json), Err(Failure::Failed(_))));

    let bad_side = lab.private_file(
        "side.json",
        r#"[{"section":"A","side":"left","from":1,"to":1,"body":"x"}]"#,
    );
    assert!(matches!(review(&bad_side), Err(Failure::Failed(message)) if message.contains("side")));

    let bad_range = lab.private_file(
        "range.json",
        r#"[{"section":"A","side":"new","from":3,"to":1,"body":"x"}]"#,
    );
    assert_eq!(code_of(&review(&bad_range)), "refused");

    #[cfg(unix)]
    {
        let target = lab.private_file("target.json", "[]");
        let link = lab.root.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).expect("link");

        assert!(matches!(review(&link.to_string_lossy()), Err(Failure::Failed(_))));
    }

    // Nothing was stored by any of these.
    assert_eq!(lab.json(&["comments", "--json", &id])["comments"], json!([]));
}

#[test]
fn history_lists_the_events_newest_first_and_a_live_row_merges_its_pending_version() {
    let mut db = team();
    let live = insert(&mut db, &Seed::new("Live").source("/live").chunk("Intro", "old"));
    let pending = insert(
        &mut db,
        &Seed::new("Live v2")
            .source("/live-2")
            .by("Ada", None)
            .pending_of(live)
            .chunk("Intro", "new"),
    );
    step(&mut db, Some("Ada"), "ask", pending, Some("Please")).expect("asked");

    let own = history::history_of(&mut db, pending).expect("history").json;
    let kinds: Vec<&str> = own["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["kind"].as_str().expect("kind"))
        .collect();
    assert_eq!(kinds, ["asked", "updateDrafted"]);
    assert_eq!(own["total"], 2);
    assert_eq!(own["events"][0]["by"], "Ada");
    assert_eq!(own["events"][0]["comment"], "Please");
    assert!(own["events"][0]["at"].as_str().is_some_and(|at| at.contains('T')));

    let merged = history::history_of(&mut db, live).expect("history").json;
    assert_eq!(merged["total"], 2, "the live row has no event of its own yet");
    assert_eq!(merged["events"][0]["kind"], "asked");

    assert_eq!(code_of(&history::history_of(&mut db, 999)), "notFound");
}

#[test]
fn compare_shows_the_sections_that_differ_as_file_diffs() {
    let mut db = team();
    let live = insert(
        &mut db,
        &Seed::new("Live")
            .source("/live")
            .chunk("Intro", "one\ntwo\nthree")
            .chunk("Gone", "bye")
            .chunk("Same", "same"),
    );
    let pending = insert(
        &mut db,
        &Seed::new("Live v2")
            .source("/live-2")
            .pending_of(live)
            .chunk("Intro", "one\n2\nthree")
            .chunk("Same", "same")
            .chunk("Fresh", "hello\nworld"),
    );

    for id in [live, pending] {
        let shown = compare::compare_on(&mut db, id, None).expect("compare").json;

        assert_eq!(
            (shown["live"].clone(), shown["pending"].clone()),
            (json!(live), json!(pending))
        );
        assert_eq!(shown["truncated"], false);
        assert_eq!(
            shown["revision"],
            revision(&mut db, pending),
            "the revision of the row that is reviewed, whichever row was asked for"
        );

        let sections = shown["sections"].as_array().expect("sections");
        let by: Vec<(&str, &str)> = sections
            .iter()
            .map(|section| {
                (
                    section["path"].as_str().expect("path"),
                    section["status"].as_str().expect("status"),
                )
            })
            .collect();
        assert_eq!(
            by,
            [("Intro", "modified"), ("Gone", "deleted"), ("Fresh", "added")],
            "the unchanged one is left out, the removed one sits at its live position"
        );

        let intro = &sections[0];
        assert_eq!(intro["oldPath"], Value::Null);
        assert_eq!(
            (intro["staged"].clone(), intro["isBinary"].clone()),
            (json!(false), json!(false))
        );
        assert_eq!(
            (intro["additions"].clone(), intro["deletions"].clone()),
            (json!(1), json!(1))
        );

        let hunk = &intro["hunks"][0];
        assert_eq!(hunk["header"], "@@ -1,3 +1,3 @@");
        let lines: Vec<(&str, &Value, &Value, &str)> = hunk["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .map(|line| {
                (
                    line["kind"].as_str().expect("kind"),
                    &line["oldNo"],
                    &line["newNo"],
                    line["text"].as_str().expect("text"),
                )
            })
            .collect();
        assert_eq!(
            lines,
            [
                ("context", &json!(1), &json!(1), "one"),
                ("remove", &json!(2), &Value::Null, "two"),
                ("add", &Value::Null, &json!(2), "2"),
                ("context", &json!(3), &json!(3), "three"),
            ]
        );

        assert_eq!(
            sections.iter().map(|s| s["occurrence"].clone()).collect::<Vec<_>>(),
            [json!(1), json!(1), json!(1)]
        );
        assert_eq!(sections[1]["hunks"][0]["header"], "@@ -1,1 +0,0 @@");
        assert_eq!(sections[2]["hunks"][0]["header"], "@@ -0,0 +1,2 @@");
    }

    // A live document with no pending version has nothing to compare.
    let plain = insert(&mut db, &Seed::new("Plain").source("/plain").chunk("A", "x"));
    assert_eq!(code_of(&compare::compare_on(&mut db, plain, None)), "wrongState");
    assert_eq!(code_of(&compare::compare_on(&mut db, 999, None)), "notFound");
}

#[test]
fn a_compare_counts_the_sections_that_share_a_heading_and_prints_the_revision_of_a_new_draft() {
    let mut db = team();
    let live = insert(
        &mut db,
        &Seed::new("Live")
            .source("/live")
            .chunk("Install", "a")
            .chunk("Other", "c")
            .chunk("Install", "b")
            .chunk("Tail", "t")
            .chunk("Install", "d"),
    );
    let pending = insert(
        &mut db,
        &Seed::new("Live v2")
            .source("/live-2")
            .pending_of(live)
            .chunk("Install", "a")
            .chunk("Other", "c")
            .chunk("Install", "b2")
            .chunk("Tail", "t")
            .chunk("Install", "d2")
            .chunk("Extra", "e")
            .chunk("Install", "f"),
    );

    let shown = compare::compare_on(&mut db, live, None).expect("compare").json;
    let by: Vec<(String, i64, String)> = shown["sections"]
        .as_array()
        .expect("sections")
        .iter()
        .map(|section| {
            (
                section["path"].as_str().expect("path").to_owned(),
                section["occurrence"].as_i64().expect("occurrence"),
                section["status"].as_str().expect("status").to_owned(),
            )
        })
        .collect();

    assert_eq!(
        by,
        [
            ("Install".to_owned(), 2, "modified".to_owned()),
            ("Install".to_owned(), 3, "modified".to_owned()),
            ("Extra".to_owned(), 1, "added".to_owned()),
            ("Install".to_owned(), 4, "added".to_owned()),
        ],
        "the occurrence counts the unchanged sections too"
    );
    assert_eq!(shown["revision"], revision(&mut db, pending));

    // A deleted section is counted among the sections of the live version.
    let gone = insert(
        &mut db,
        &Seed::new("Gone")
            .source("/gone")
            .chunk("Install", "x")
            .chunk("Middle", "m")
            .chunk("Install", "y"),
    );
    let trimmed = insert(
        &mut db,
        &Seed::new("Gone v2")
            .source("/gone-2")
            .pending_of(gone)
            .chunk("Install", "x")
            .chunk("Middle", "m"),
    );
    let sections = compare::compare_on(&mut db, trimmed, None).expect("compare").json["sections"].clone();
    assert_eq!(
        (sections[0]["status"].clone(), sections[0]["occurrence"].clone()),
        (json!("deleted"), json!(2))
    );

    // A new draft: the revision of the draft itself, one that moved on with its ask.
    let fresh = draft(&mut db);
    step(&mut db, Some("Ada"), "ask", fresh, None).expect("asked");
    let moved = revision(&mut db, fresh);
    assert!(moved > 0);

    let shown = compare::compare_on(&mut db, fresh, None).expect("compare").json;
    assert_eq!(shown["revision"], moved);
}

#[test]
fn a_new_document_compares_with_nothing_and_since_compares_with_the_version_asked() {
    let mut db = team();
    let id = draft(&mut db);

    let shown = compare::compare_on(&mut db, id, None).expect("compare").json;
    assert_eq!(shown["live"], Value::Null);
    assert_eq!(shown["pending"], id);
    assert!(
        shown["sections"]
            .as_array()
            .expect("sections")
            .iter()
            .all(|section| section["status"] == "added" && section["deletions"] == 0)
    );

    assert_eq!(
        code_of(&compare::compare_on(&mut db, id, Some(1))),
        "sinceUnavailable",
        "nothing was asked yet"
    );

    let asked = done(step(&mut db, Some("Ada"), "ask", id, None));
    let at = asked["revision"].as_i64().expect("revision");

    // The author goes on, a reviewer asks for changes, and the author edits again.
    let (mut doc, chunks, links) = Seed::new("Guide")
        .status(Status::Draft)
        .by("Ada", None)
        .chunk("Intro", "first line\nchanged line\nthird line")
        .parts();
    doc.id = id;
    db.put_document(&doc, &chunks, &links).expect("edit");

    let since = compare::compare_on(&mut db, id, Some(at)).expect("since").json;
    let sections = since["sections"].as_array().expect("sections");

    assert_eq!(
        sections.iter().map(|s| s["path"].clone()).collect::<Vec<_>>(),
        [json!("Intro"), json!("Usage")]
    );
    assert_eq!(sections[0]["status"], "modified");
    assert_eq!(sections[1]["status"], "deleted");
}

#[test]
fn download_writes_the_stored_text_when_there_is_no_original_and_the_original_when_there_is() {
    let lab = Lab::new();
    let mut db = lab.database();
    let id = insert(
        &mut db,
        &Seed::new("Guide")
            .status(Status::Draft)
            .chunk("Intro", "first")
            .chunk("Intro > Deep", "second"),
    );
    let folder = tempfile::tempdir().expect("folder");
    let to = folder.path().join("guide.pdf");
    let id_text = id.to_string();

    let text = lab.json(&["download", "--json", &id_text, "--to", &to.to_string_lossy()]);
    let written = folder.path().join("guide.md");
    assert_eq!(text["original"], false);
    assert_eq!(text["path"], written.to_string_lossy().as_ref());
    assert_eq!(
        std::fs::read_to_string(&written).expect("text"),
        "# Guide\n\n## Intro\n\nfirst\n\n### Deep\n\nsecond\n"
    );

    #[cfg(unix)]
    assert_eq!(
        std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&written).expect("metadata")) & 0o777,
        0o600
    );

    let bytes = b"# Guide\r\n\r\nthe original bytes \xff";
    assert!(db.put_file(id, "guide.md", bytes).expect("file"));

    let again = folder.path().join("copy.md");
    let original = lab.json(&["download", "--json", &id_text, "--to", &again.to_string_lossy()]);
    assert_eq!(original, json!({ "path": again.to_string_lossy(), "original": true }));
    assert_eq!(std::fs::read(&again).expect("copy"), bytes);

    // It replaces a file of its own, never a symbolic link or a folder.
    #[cfg(unix)]
    {
        let secret = folder.path().join("secret.txt");
        std::fs::write(&secret, "keep").expect("secret");
        let link = folder.path().join("link.md");
        std::os::unix::fs::symlink(&secret, &link).expect("link");

        assert_eq!(
            lab.refusal(&["download", "--json", &id_text, "--to", &link.to_string_lossy()]),
            "refused"
        );
        assert_eq!(std::fs::read_to_string(&secret).expect("secret"), "keep");
    }

    assert_eq!(
        lab.refusal(&["download", "--json", &id_text, "--to", &folder.path().to_string_lossy()]),
        "refused"
    );
    assert_eq!(
        lab.refusal(&["download", "--json", "99", "--to", &again.to_string_lossy()]),
        "notFound"
    );
}

#[test]
fn download_never_replaces_the_file_an_adjusted_extension_points_to() {
    let lab = Lab::new();
    let mut db = lab.database();
    let id = insert(
        &mut db,
        &Seed::new("Guide").status(Status::Draft).chunk("Intro", "first"),
    );
    let id_text = id.to_string();
    let folder = tempfile::tempdir().expect("folder");

    // No original: the text goes to guide.md, where the person's own file is.
    let theirs = folder.path().join("guide.md");
    std::fs::write(&theirs, "my notes").expect("theirs");
    let to = folder.path().join("guide.pdf");

    assert_eq!(
        lab.refusal(&["download", "--json", &id_text, "--to", &to.to_string_lossy()]),
        "refused"
    );
    assert_eq!(std::fs::read_to_string(&theirs).expect("kept"), "my notes");
    assert!(!to.exists());

    // The path the person confirmed is replaced as before, and a free adjusted path is written.
    let named = lab.json(&["download", "--json", &id_text, "--to", &theirs.to_string_lossy()]);
    assert_eq!(named["original"], false);
    assert!(
        std::fs::read_to_string(&theirs)
            .expect("replaced")
            .starts_with("# Guide")
    );

    let free = folder.path().join("other.pdf");
    let written = lab.json(&["download", "--json", &id_text, "--to", &free.to_string_lossy()]);
    assert_eq!(
        written["path"],
        folder.path().join("other.md").to_string_lossy().as_ref()
    );
}

#[test]
fn a_live_row_with_a_pending_version_downloads_the_pending_file_and_live_asks_for_its_own() {
    let lab = Lab::new();
    let mut db = lab.database();
    let live = insert(&mut db, &Seed::new("Live").source("/live").chunk("Intro", "old"));
    let pending = insert(
        &mut db,
        &Seed::new("Live v2")
            .source("/live-2")
            .pending_of(live)
            .chunk("Intro", "new"),
    );
    db.put_file(live, "live.md", b"live bytes").expect("live file");
    db.put_file(pending, "pending.md", b"pending bytes")
        .expect("pending file");

    let folder = tempfile::tempdir().expect("folder");
    let read = |id: i64, live_flag: bool| {
        let to = folder.path().join(format!("{id}-{live_flag}.md"));
        let mut args = vec![
            "download".to_owned(),
            "--json".to_owned(),
            id.to_string(),
            "--to".to_owned(),
        ];
        args.push(to.to_string_lossy().into_owned());

        if live_flag {
            args.push("--live".to_owned());
        }

        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        lab.json(&args);

        std::fs::read(&to).expect("written")
    };

    assert_eq!(read(live, false), b"pending bytes");
    assert_eq!(read(live, true), b"live bytes");
    assert_eq!(read(pending, false), b"pending bytes");
    assert_eq!(read(pending, true), b"live bytes");
}

#[test]
fn approval_is_local_and_off_without_a_team_database_and_set_is_refused() {
    let lab = Lab::new();
    let context = Context::new(&lab.env, &Backend::Sqlite, Routing::local());
    let args = |words: &[&'static str]| -> Vec<Option<&'static str>> { words.iter().map(|word| Some(*word)).collect() };

    let state = approval_reply(&context, &args(&["get", "--json"])).expect("get").json;
    assert_eq!(
        state,
        json!({ "approval": false, "place": "local", "writable": null, "changedBy": null, "changedAt": null })
    );

    assert_eq!(
        code_of(&approval_reply(&context, &args(&["set", "--json", "on"]))),
        "refused"
    );

    for bad in [
        &["set", "--json", "maybe"][..],
        &["set", "--json"],
        &["get"],
        &["flip", "--json"],
    ] {
        assert_eq!(
            approval_reply(&context, &args(bad)).map(|_| ()),
            Err(Failure::Usage),
            "{bad:?}"
        );
    }
}

#[test]
fn upgrade_leaves_a_current_or_missing_database_alone() {
    let lab = Lab::new();
    let context = Context::new(&lab.env, &Backend::Sqlite, Routing::local());
    let run = || upgrade_reply(&context, &[Some("--json")]).expect("upgrade").json;

    assert_eq!(run(), json!({ "upgraded": false, "waiting": false }), "no database");

    drop(lab.database());
    assert_eq!(run(), json!({ "upgraded": false, "waiting": false }), "a current one");

    assert_eq!(upgrade_reply(&context, &[]).map(|_| ()), Err(Failure::Usage));
}

#[test]
fn a_database_whose_upgrade_breaks_is_a_failure_and_the_reads_say_what_to_do() {
    let lab = Lab::new();
    drop(lab.database());

    // A file that claims to be of version 1 but already has the version 2 columns: reading it is
    // refused as not upgraded yet, and the upgrade breaks. That is no permission a teammate could
    // give, so it is not a wait: the message is shown.
    let path = lab.root.path().join("knowledge").join(FILE_NAME);
    rusqlite::Connection::open(&path)
        .expect("open")
        .execute_batch("PRAGMA user_version = 1")
        .expect("version");

    let context = Context::new(&lab.env, &Backend::Sqlite, Routing::local());

    match upgrade_reply(&context, &[Some("--json")]) {
        Err(Failure::Failed(message)) => assert!(!message.is_empty()),
        other => panic!("expected a failure, got {other:?}"),
    }

    // The commands that read say what to do.
    match stats_reply(&context, &[Some("--json")]) {
        Err(Failure::Failed(message)) => {
            assert!(
                message.contains("not upgraded yet") && message.contains("harness-knowledge upgrade"),
                "{message}"
            );
        }
        other => panic!("expected the not-upgraded answer, got {other:?}"),
    }

    let checked = check_reply(&context, &[Some("--json")]).expect("check");
    assert_eq!(
        (checked.code, checked.json["problem"].clone()),
        (3, json!("not-upgraded"))
    );
    assert!(
        checked.json["message"]
            .as_str()
            .is_some_and(|message| message.contains("harness-knowledge upgrade"))
    );
}

#[test]
fn stats_counts_drafts_waiting_and_pending_and_says_no_approval_without_a_team_database() {
    let lab = Lab::new();
    let mut db = lab.database();
    let live = insert(&mut db, &Seed::new("Live").source("/live").chunk("Intro", "old"));
    insert(
        &mut db,
        &Seed::new("Draft")
            .source("/draft")
            .status(Status::Draft)
            .chunk("A", "x"),
    );
    let waiting = insert(
        &mut db,
        &Seed::new("Waiting")
            .source("/waiting")
            .status(Status::Draft)
            .chunk("A", "y"),
    );
    insert(
        &mut db,
        &Seed::new("Live v2")
            .source("/live-2")
            .pending_of(live)
            .chunk("Intro", "new"),
    );
    db.set_review(
        waiting,
        0,
        &crate::store::ReviewChange::new(crate::store::ReviewAction::Ask, Some("Ada"), 5),
    )
    .expect("ask");

    let context = Context::new(&lab.env, &Backend::Sqlite, Routing::local());
    let stats = stats_reply(&context, &[Some("--json")]).expect("stats").json;

    assert_eq!(stats["documents"], 3, "pending versions are not documents of their own");
    assert_eq!(
        (
            stats["drafts"].clone(),
            stats["waiting"].clone(),
            stats["pending"].clone(),
            stats["approval"].clone()
        ),
        (json!(2), json!(1), json!(1), Value::Null)
    );
}

#[test]
fn stats_counts_waiting_for_you_by_the_name_of_the_person() {
    let lab = Lab::new();
    let mut db = lab.database();

    for (source, author, asker) in [("/ada", "Ada", "Ada"), ("/bob", "Bob", "Bob")] {
        let id = insert(
            &mut db,
            &Seed::new(source)
                .source(source)
                .status(Status::Draft)
                .by(author, None)
                .chunk("A", "x"),
        );
        db.set_review(
            id,
            0,
            &crate::store::ReviewChange::new(crate::store::ReviewAction::Ask, Some(asker), 5),
        )
        .expect("ask");
    }

    let context = Context::new(&lab.env, &Backend::Sqlite, Routing::local());
    let for_you = |actor: Option<&str>| {
        let stats = overview::stats_as(&context, &[Some("--json")], actor)
            .expect("stats")
            .json;

        (stats["waiting"].clone(), stats["waitingForYou"].clone())
    };

    assert_eq!(
        for_you(Some("Ada")),
        (json!(2), json!(1)),
        "the teammate's, not her own"
    );
    assert_eq!(for_you(Some("bob ")), (json!(2), json!(1)));
    assert_eq!(for_you(Some("Cara")), (json!(2), json!(2)));
    assert_eq!(for_you(None), (json!(2), json!(0)), "no name, nothing waits for you");

    // The answers are kept next to the database, and a second ask gives the same ones.
    assert!(
        lab.root
            .path()
            .join("knowledge")
            .join(crate::review::WAITING_CACHE_FILE)
            .is_file()
    );
    assert_eq!(for_you(Some("Cara")), (json!(2), json!(2)), "from the cache");
}

#[test]
fn a_comment_is_outdated_once_the_content_changed_and_not_by_the_transitions_around_it() {
    let mut db = team();
    let id = draft(&mut db);
    step(&mut db, Some("Ada"), "ask", id, None).expect("asked");
    let expect = revision(&mut db, id);
    let changes = Submission {
        verdict: Verdict::RequestChanges,
        body: None,
        comments: vec![NewComment::on_lines("Intro", Side::New, 1, 1, "Longer")],
    };
    submit_on(&mut db, Place::Remote, Some("Grace"), id, expect, &changes).expect("review");

    let outdated = |db: &mut SqliteStore| -> Vec<Value> {
        comments::comments_of(db, id).expect("comments").json["comments"]
            .as_array()
            .expect("comments")
            .iter()
            .map(|comment| comment["outdated"].clone())
            .collect()
    };

    assert_eq!(
        outdated(&mut db),
        [json!(false)],
        "the request raised the revision, not the content"
    );

    // The author edits the draft: what was commented is gone.
    let (mut doc, chunks, links) = Seed::new("Guide")
        .status(Status::Draft)
        .by("Ada", None)
        .chunk("Intro", "a longer first line")
        .parts();
    doc.id = id;
    db.put_document(&doc, &chunks, &links).expect("edit");
    assert_eq!(outdated(&mut db), [json!(true)]);

    // A reply made now is about the new content.
    let comment = comments::comments_of(&mut db, id).expect("comments").json["comments"][0]["id"].clone();
    let reply = comments::comment_on(
        &mut db,
        Place::Remote,
        Some("Ada"),
        "reply",
        id,
        comment.as_i64().expect("id"),
        Some("Done"),
    )
    .expect("reply");
    assert_eq!(reply.json["comment"]["outdated"], false);
}

#[test]
fn deleting_a_comment_prints_the_ids_it_deleted_and_is_for_its_author() {
    let mut db = team();
    let id = draft(&mut db);
    let first = db
        .add_comments(
            id,
            Some("Bob"),
            100,
            0,
            &[NewComment::overall("one"), NewComment::overall("two")],
        )
        .expect("comments");
    let reply = db
        .reply_comment(id, first[0], Some("Ada"), 101, "answer")
        .expect("reply")
        .expect("a reply");
    let delete = |db: &mut SqliteStore, actor: Option<&str>, comment: i64| {
        comments::delete_on(db, Place::Remote, actor, id, comment)
    };

    assert_eq!(code_of(&delete(&mut db, Some("Ada"), first[0])), "notAuthor");
    assert_eq!(code_of(&delete(&mut db, None, first[0])), "noName");
    assert_eq!(code_of(&delete(&mut db, Some("Bob"), 999)), "notFound");

    // A reply alone, then a thread (the reply is gone with it, so only its own id is printed).
    assert_eq!(
        done(delete(&mut db, Some(" ada "), reply)),
        json!({ "deleted": [reply] })
    );
    let answered = db
        .reply_comment(id, first[0], Some("Cy"), 102, "again")
        .expect("reply")
        .expect("a reply");
    assert_eq!(
        done(delete(&mut db, Some("BOB"), first[0])),
        json!({ "deleted": [first[0], answered] })
    );

    let left = comments::comments_of(&mut db, id).expect("comments").json;
    assert_eq!(left["comments"].as_array().map(Vec::len), Some(1));
    assert_eq!(left["comments"][0]["id"], json!(first[1]));
}

#[test]
fn deleting_a_comment_is_a_json_command_that_names_a_document_and_a_comment() {
    let lab = Lab::new();

    for args in [
        &["comment", "delete", "1", "1"][..],
        &["comment", "delete", "--json", "1"],
        &["comment", "delete", "--json", "1", "1", "2"],
        &["comment", "delete", "--json", "1", "1", "--body", "x"],
    ] {
        assert!(matches!(lab.run(args), Err(Failure::Usage)), "{args:?}");
    }
}

#[test]
fn a_comment_deleted_on_this_computer_is_printed_with_its_place() {
    let mut local = team();
    let id = draft(&mut local);
    let comment = local
        .add_comments(id, Some("Bob"), 100, 0, &[NewComment::overall("one")])
        .expect("comments")[0];
    let mut routed = crate::store::routed::RoutedStore::new(Some(Box::new(local)), None, Routing::local());
    let tagged = |id: i64| crate::store::routed::ids::tag(Place::Local, id);

    let deleted = comments::delete_on(&mut routed, Place::Local, Some("Bob"), tagged(id), tagged(comment))
        .expect("delete")
        .json;

    assert_eq!(deleted, json!({ "deleted": [format!("local:{comment}")] }));
}

fn comments_json(db: &mut SqliteStore, id: i64) -> Vec<Value> {
    comments::comments_of(db, id).expect("comments").json["comments"]
        .as_array()
        .expect("comments")
        .clone()
}

fn review_with(db: &mut SqliteStore, id: i64, comments: Vec<NewComment>) {
    let expect = revision(db, id);
    let submission = Submission {
        verdict: Verdict::Comment,
        body: None,
        comments,
    };

    submit_on(db, Place::Remote, Some("Bob"), id, expect, &submission).expect("review");
}

/// Replaces the content of row `id` with `chunks`.
fn rewrite(db: &mut SqliteStore, id: i64, chunks: &[(&str, &str)]) {
    let mut seed = Seed::new("Guide").status(Status::Draft).by("Ada", None);

    for (heading, text) in chunks {
        seed = seed.chunk(heading, text);
    }

    let (mut doc, parts, links) = seed.parts();
    doc.id = id;
    db.put_document(&doc, &parts, &links).expect("edit");
}

#[test]
fn a_line_comment_keeps_the_lines_it_was_written_on() {
    let mut db = team();
    let id = draft(&mut db);

    review_with(
        &mut db,
        id,
        vec![
            NewComment::on_lines("Intro", Side::New, 2, 3, "Both"),
            NewComment::on_lines("Usage", Side::New, 1, 1, "One"),
            NewComment::on_lines("Intro", Side::New, 5, 6, "Beyond what was shown"),
            NewComment::on_lines("Intro", Side::Old, 1, 1, "No old side in a new document"),
            NewComment::on_lines("Gone", Side::New, 1, 1, "No such section"),
        ],
    );
    let first = comments_json(&mut db, id);

    assert_eq!(
        first[0]["quote"],
        json!([
            { "line": 2, "kind": "added", "text": "second line" },
            { "line": 3, "kind": "added", "text": "third line" },
        ])
    );
    assert_eq!(
        first[1]["quote"],
        json!([{ "line": 1, "kind": "added", "text": "run it" }])
    );
    assert!(first[2..].iter().all(|comment| comment["quote"].is_null()), "{first:?}");

    // A reply has none, nor has an overall comment.
    let reply = comments::comment_on(
        &mut db,
        Place::Remote,
        Some("Ada"),
        "reply",
        id,
        first[0]["id"].as_i64().expect("id"),
        Some("Done"),
    )
    .expect("reply");
    assert_eq!(reply.json["comment"]["quote"], Value::Null);
    assert_eq!(reply.json["comment"]["outdated"], false);

    let expect = revision(&mut db, id);
    let overall = Submission {
        verdict: Verdict::Comment,
        body: Some("Overall".to_owned()),
        comments: Vec::new(),
    };
    submit_on(&mut db, Place::Remote, Some("Bob"), id, expect, &overall).expect("review");
    let all = comments_json(&mut db, id);
    assert_eq!(all.last().expect("the overall comment")["quote"], Value::Null);
}

#[test]
fn the_lines_of_both_sides_of_a_pending_version_are_kept_with_their_kinds() {
    let mut db = team();
    let live = insert(
        &mut db,
        &Seed::new("Live")
            .source("/live")
            .chunk("Intro", "keep\nold line\nkeep too"),
    );
    let pending = insert(
        &mut db,
        &Seed::new("Live v2")
            .source("/live-2")
            .by("Ada", None)
            .pending_of(live)
            .chunk("Intro", "keep\nnew line\nkeep too"),
    );

    review_with(
        &mut db,
        live,
        vec![
            NewComment::on_lines("Intro", Side::Old, 1, 2, "Why drop it"),
            NewComment::on_lines("Intro", Side::New, 2, 3, "Fine"),
        ],
    );
    let shown = comments_json(&mut db, pending);

    assert_eq!(
        shown.last().expect("comments")["quote"],
        json!([
            { "line": 2, "kind": "added", "text": "new line" },
            { "line": 3, "kind": "same", "text": "keep too" },
        ])
    );
    assert_eq!(
        shown[shown.len() - 2]["quote"],
        json!([
            { "line": 1, "kind": "same", "text": "keep" },
            { "line": 2, "kind": "removed", "text": "old line" },
        ])
    );
}

#[test]
fn a_line_comment_is_outdated_when_its_section_is_gone_or_its_lines_changed() {
    let mut db = team();
    let id = draft(&mut db);
    review_with(
        &mut db,
        id,
        vec![
            NewComment::on_lines("Intro", Side::New, 2, 2, "On Intro"),
            NewComment::on_lines("Usage", Side::New, 1, 1, "On Usage"),
        ],
    );
    let first = comments_json(&mut db, id)[0]["id"].as_i64().expect("id");
    let replied =
        comments::comment_on(&mut db, Place::Remote, Some("Ada"), "reply", id, first, Some("Done")).expect("reply");
    assert_eq!(replied.json["comment"]["outdated"], false);

    let flags = |db: &mut SqliteStore| -> Vec<Value> {
        comments_json(db, id)
            .iter()
            .map(|comment| comment["outdated"].clone())
            .collect()
    };
    assert_eq!(flags(&mut db), [json!(false), json!(false), json!(false)]);

    // Another section changes: the comment on this one still is about what it was.
    rewrite(
        &mut db,
        id,
        &[
            ("Intro", "first line\nsecond line\nthird line"),
            ("Usage", "run it twice"),
        ],
    );
    assert_eq!(flags(&mut db), [json!(false), json!(true), json!(false)]);

    // The commented line changes (and a line above it moves what is on line 2).
    rewrite(
        &mut db,
        id,
        &[
            ("Intro", "first line\nsecond line, longer\nthird line"),
            ("Usage", "run it twice"),
        ],
    );
    assert_eq!(flags(&mut db), [json!(true), json!(true), json!(false)]);

    // The section is gone altogether, and a replacement with the same lines is not outdated.
    rewrite(&mut db, id, &[("Other", "first line\nsecond line\nthird line")]);
    assert_eq!(flags(&mut db), [json!(true), json!(true), json!(false)]);
    rewrite(
        &mut db,
        id,
        &[("Intro", "first line\nsecond line\nthird line"), ("Usage", "run it")],
    );
    assert_eq!(flags(&mut db), [json!(false), json!(false), json!(false)]);
}

#[test]
fn a_line_cut_at_the_cap_is_compared_by_its_start() {
    let mut db = team();
    let id = draft(&mut db);
    let long = |tail: &str| format!("{}{tail}", "a".repeat(4500));

    rewrite(&mut db, id, &[("Intro", &long("end"))]);
    review_with(
        &mut db,
        id,
        vec![NewComment::on_lines("Intro", Side::New, 1, 1, "Too long")],
    );

    let shown = comments_json(&mut db, id);
    let quote = shown[0]["quote"].as_array().expect("a quote");
    assert_eq!(quote[0]["cut"], true);
    assert_eq!(quote[0]["text"].as_str().map(str::len), Some(4000));
    assert_eq!(shown[0]["outdated"], false, "right after it was written");

    // Past what was kept nothing is known to have changed, inside it something did.
    rewrite(&mut db, id, &[("Intro", &long("other end"))]);
    assert_eq!(comments_json(&mut db, id)[0]["outdated"], false);
    rewrite(&mut db, id, &[("Intro", &format!("b{}", "a".repeat(4499)))]);
    assert_eq!(comments_json(&mut db, id)[0]["outdated"], true);
}

#[test]
fn a_comment_written_before_quotes_is_outdated_without_its_section_or_after_the_content_changed() {
    let mut db = team();
    let id = draft(&mut db);
    let written = revision(&mut db, id);

    // As an older server wrote them: no lines kept.
    db.add_comments(
        id,
        Some("Bob"),
        100,
        written,
        &[
            NewComment::on_lines("Intro", Side::New, 1, 1, "On Intro"),
            NewComment::on_lines("Nowhere", Side::New, 1, 1, "On nothing"),
        ],
    )
    .expect("comments");

    let flags = |db: &mut SqliteStore| -> Vec<Value> {
        comments_json(db, id)
            .iter()
            .map(|comment| comment["outdated"].clone())
            .collect()
    };
    assert_eq!(flags(&mut db), [json!(false), json!(true)]);
    assert!(
        comments_json(&mut db, id)
            .iter()
            .all(|comment| comment["quote"].is_null())
    );

    rewrite(&mut db, id, &[("Intro", "something else")]);
    assert_eq!(flags(&mut db), [json!(true), json!(true)], "the content changed since");
}

/// `docs list` carries `authors` on a row that waits for approval, and on the `pending` object of a
/// live row whose pending version waits, the same as `docs get` prints, and on no other row.
pub(crate) fn list_authors_contract(store: &mut dyn KnowledgeStore) {
    let ask = |store: &mut dyn KnowledgeStore, id: i64, by: &str| {
        let expect = revision_of(store, id);
        let change = ReviewChange::new(ReviewAction::Ask, Some(by), 50);

        store.set_review(id, expect, &change).expect("ask");
    };

    // A waiting row: written by Ada, renamed by Cy, asked by Bob.
    let waiting = put_seed(
        store,
        &Seed::new("Billing glossary")
            .source("/billing")
            .status(Status::Draft)
            .by("Ada", None)
            .chunk("Terms", "invoice"),
    );
    assert!(store.rename(waiting, "Billing terms", Some("Cy"), 20).expect("rename"));
    ask(store, waiting, "Bob");

    // A draft, a plain live row, a live row with a draft pending version, and one whose pending version waits.
    let draft = put_seed(
        store,
        &Seed::new("Notes")
            .source("/notes")
            .status(Status::Draft)
            .by("Ada", None)
            .chunk("Intro", "notes"),
    );
    let plain = put_seed(store, &Seed::new("Plain").source("/plain").chunk("Intro", "plain"));
    let quiet = put_seed(store, &Seed::new("Quiet").source("/quiet").chunk("Intro", "old"));
    put_seed(
        store,
        &Seed::new("Quiet v2")
            .source("/quiet-2")
            .pending_of(quiet)
            .by("Ada", None)
            .chunk("Intro", "new"),
    );
    let deploy = put_seed(
        store,
        &Seed::new("Deploy runbook").source("/deploy").chunk("Intro", "old"),
    );
    let proposal = put_seed(
        store,
        &Seed::new("Deploy runbook v2")
            .source("/deploy-2")
            .pending_of(deploy)
            .by("Dee", None)
            .chunk("Intro", "new"),
    );
    ask(store, proposal, "Eve");

    let (query, _) = super::listing::parse_list(&[Some("--json")]).expect("a query");
    let page = super::list_on(store, None, &query).expect("list");
    let items = page["items"].as_array().expect("items");
    let item = |id: i64| items.iter().find(|item| item["id"] == json!(id)).expect("a listed row");
    let mut expected = |id: i64| {
        let found = store
            .get(
                id,
                &Detail {
                    drafts: true,
                    ..Detail::default()
                },
            )
            .expect("get")
            .expect("a row");
        let by = found.doc.updated_by.as_ref().map(|author| author.name.clone());

        json!(crate::review::people_of(store, id, by.as_deref()).expect("people"))
    };

    let sorted = |names: &Value| -> Vec<String> {
        let mut names: Vec<String> = names
            .as_array()
            .expect("a list")
            .iter()
            .map(|name| name.as_str().expect("a name").to_owned())
            .collect();
        names.sort();

        names
    };

    // What `docs get` prints for the same rows.
    assert_eq!(item(waiting)["authors"], expected(waiting));
    assert_eq!(sorted(&item(waiting)["authors"]), ["Ada", "Bob", "Cy"]);
    assert_eq!(item(deploy)["pending"]["authors"], expected(proposal));
    assert_eq!(sorted(&item(deploy)["pending"]["authors"]), ["Dee", "Eve"]);

    // No other row has the key.
    for id in [draft, plain, quiet, deploy] {
        assert!(item(id).get("authors").is_none(), "{id}: {}", item(id));
    }
    assert!(item(quiet)["pending"].is_object());
    assert!(item(quiet)["pending"].get("authors").is_none());
    assert!(item(plain)["pending"].is_null());
}

fn put_seed(store: &mut dyn KnowledgeStore, seed: &Seed) -> i64 {
    let (doc, chunks, links) = seed.parts();

    store.put_document(&doc, &chunks, &links).expect("put document")
}

#[test]
fn a_list_names_at_most_twenty_people_per_row_and_a_get_names_them_all() {
    let mut db = team();
    let id = draft(&mut db);

    for number in 0..30 {
        assert!(
            db.rename(
                id,
                &format!("Guide {number}"),
                Some(&format!("Writer {number:02}")),
                20 + number
            )
            .expect("rename")
        );
    }
    step(&mut db, Some("Ada"), "ask", id, None).expect("asked");

    let (query, _) = super::listing::parse_list(&[Some("--json")]).expect("a query");
    let listed = super::list_on(&mut db, None, &query).expect("list");
    let names = listed["items"][0]["authors"].as_array().expect("authors");

    assert_eq!(names.len(), crate::store::review::AUTHORS_LIST_MAX);
    assert_eq!(
        names[0], "Ada",
        "the row's updatedBy comes first, then the writers in order"
    );
    assert_eq!(names[1], "Writer 00");

    // The rule itself reads every name: what `docs get` prints, and what selfApproval counts.
    let people = crate::review::people_of(&mut db, id, Some("Ada")).expect("people");
    assert!(
        people.len() > crate::store::review::AUTHORS_LIST_MAX,
        "{}",
        people.len()
    );
    assert!(people.contains(&"Writer 05".to_owned()) && people.contains(&"Ada".to_owned()));
}

fn revision_of(store: &mut dyn KnowledgeStore, id: i64) -> i64 {
    let detail = Detail {
        drafts: true,
        ..Detail::default()
    };

    store.get(id, &detail).expect("get").expect("a row").doc.review_rev
}

#[test]
fn a_list_carries_authors_where_a_reviewer_needs_them() {
    list_authors_contract(&mut team());
}

#[test]
fn stats_names_the_person_at_the_terminal() {
    let lab = Lab::new();
    let context = Context::new(&lab.env, &Backend::Sqlite, Routing::local());
    let me = |actor: Option<&str>| {
        overview::stats_as(&context, &[Some("--json")], actor)
            .expect("stats")
            .json["me"]
            .clone()
    };

    assert_eq!(me(Some(" Ada Lovelace ")), json!({ "name": "Ada Lovelace" }));
    assert_eq!(me(None), json!({ "name": null }), "no git name, no name");
    assert_eq!(me(Some("  ")), json!({ "name": null }));
}

#[test]
fn a_rename_makes_the_renamer_an_author_who_cannot_approve_it() {
    let mut db = team();
    let id = draft(&mut db);

    // Ada drafts, Bob renames, Bob asks.
    assert!(db.rename(id, "Guide renamed", Some("Bob"), 100).expect("rename"));
    done(step(&mut db, Some("Bob"), "ask", id, None));

    let row = db
        .get(
            id,
            &Detail {
                drafts: true,
                ..Detail::default()
            },
        )
        .expect("get")
        .expect("row");
    assert_eq!(
        row.doc.updated_by.map(|author| author.name),
        Some("Ada".to_owned()),
        "the rename does not take the authorship of the row"
    );

    // Ada wrote it, Bob renamed and asked: neither may give the approval.
    assert_eq!(
        code_of(&step(&mut db, Some("Ada"), "approve", id, None)),
        "selfApproval"
    );
    assert_eq!(
        code_of(&step(&mut db, Some("Bob"), "approve", id, None)),
        "selfApproval"
    );

    assert_eq!(
        done(step(&mut db, Some("Carol"), "approve", id, None))["status"],
        "live"
    );
}

#[test]
fn get_names_the_authors_of_a_row_that_is_reviewed_and_of_the_pending_version_of_a_live_one() {
    let lab = Lab::new();
    let mut db = lab.database();

    // Ada drafts, Bob renames, Carol asks, and Ada's name is on the row too: each person once.
    let draft = insert(
        &mut db,
        &Seed::new("Guide")
            .status(Status::Draft)
            .by("Ada", None)
            .chunk("Intro", "words"),
    );
    assert!(db.rename(draft, "Guide renamed", Some("bob"), 20).expect("rename"));
    assert!(db.rename(draft, "Guide again", Some("Bob"), 21).expect("rename"));
    let expect = revision(&mut db, draft);
    db.set_review(draft, expect, &ReviewChange::new(ReviewAction::Ask, Some("Carol"), 30))
        .expect("ask");

    let got = lab.json(&["get", "--json", &draft.to_string()]);
    assert_eq!(got["document"]["authors"], json!(["Ada", "bob", "Carol"]));

    // A live document has no authors of its own. Its pending version has: the writer and the renamer.
    let live = insert(&mut db, &Seed::new("Live").source("/live").chunk("Intro", "old"));
    let got = lab.json(&["get", "--json", &live.to_string()]);
    assert_eq!(got["document"]["pending"], Value::Null);
    assert!(got["document"].get("authors").is_none());

    insert(
        &mut db,
        &Seed::new("Live v2")
            .source("/live-2")
            .by("Dan", None)
            .pending_of(live)
            .chunk("Intro", "new"),
    );
    assert!(db.rename(live, "Live renamed", Some("Erin"), 40).expect("rename"));

    let got = lab.json(&["get", "--json", &live.to_string()]);
    assert!(got["document"].get("authors").is_none());
    assert_eq!(got["document"]["pending"]["authors"], json!(["Dan", "Erin"]));
}
