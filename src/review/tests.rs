//! The review policy: each refusal code, each transition, a review submitted, the comments, the
//! approval setting, a remote that cannot be reached, and a team libSQL server.

use std::collections::HashMap;

use pretty_assertions::assert_eq;

use super::spy::Spy;
use super::*;
use crate::backend::OpenError;
use crate::config::{Routing, Secrets, Where};
use crate::store::routed::{Patience, Remote};
use crate::store::testing::{Seed, insert, store};
use crate::store::{
    Chunk, ChunkHit, ChunkMatches, Comment, Document, DocumentPage, EventKind, History, Link, ListQuery, PendingChunk,
    PendingVersion, Reach, RepoCounts, RoutedStore, Scope, SectionText, Side, SqliteStore, Stats, Status, StoredFile,
    TitleScope, Visibility,
};

/// A store whose remote refuses writes of this user, like a team database with a read-only role.
struct ReadOnly(SqliteStore);

macro_rules! delegate {
    ($( fn $name:ident(&mut self $(, $arg:ident: $ty:ty)*) -> $ret:ty; )*) => {
        $( fn $name(&mut self $(, $arg: $ty)*) -> $ret { self.0.$name($($arg),*) } )*
    };
}

impl KnowledgeStore for ReadOnly {
    delegate! {
        fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError>;
        fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError>;
        fn set_metas(&mut self, entries: &[(&str, &str)]) -> Result<(), StoreError>;
        fn generation(&mut self) -> Result<i64, StoreError>;
        fn put_document(&mut self, doc: &Document, chunks: &[Chunk], links: &[Link]) -> Result<i64, StoreError>;
        fn find_by_content_sha(&mut self, sha: &str, visibility: Visibility) -> Result<Option<Document>, StoreError>;
        fn find_by_source_key(&mut self, key: &str, visibility: Visibility) -> Result<Option<Document>, StoreError>;
        fn reusable_embeddings(&mut self, model_id: &str, chunk_shas: &[String]) -> Result<HashMap<String, Vec<f32>>, StoreError>;
        fn pending_chunks(&mut self, model_id: &str, after: i64, limit: usize) -> Result<Vec<PendingChunk>, StoreError>;
        fn set_chunk_embeddings(&mut self, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StoreError>;
        fn find_by_title(&mut self, title: &str, scope: &TitleScope, visibility: Visibility) -> Result<Vec<Document>, StoreError>;
        fn docs_sharing_chunks(&mut self, chunk_shas: &[String], visibility: Visibility) -> Result<Vec<(i64, usize)>, StoreError>;
        fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError>;
        fn rename(&mut self, id: i64, title: &str, by: Option<&str>, at: i64) -> Result<bool, StoreError>;
        fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError>;
        fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError>;
        fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError>;
        fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError>;
        fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError>;
        fn matching_chunks(&mut self, doc_id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError>;
        fn pending_of(&mut self, id: i64) -> Result<Option<PendingVersion>, StoreError>;
        fn set_review(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError>;
        fn promote(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError>;
        fn history(&mut self, id: i64) -> Result<Option<History>, StoreError>;
        fn writers_since_live(&mut self, id: i64) -> Result<Vec<String>, StoreError>;
        fn section_texts(&mut self, id: i64) -> Result<Option<Vec<SectionText>>, StoreError>;
        fn asked_sections(&mut self, id: i64, revision: i64) -> Result<Option<Vec<SectionText>>, StoreError>;
        fn add_comments(&mut self, id: i64, by: Option<&str>, at: i64, revision: i64, comments: &[NewComment]) -> Result<Vec<i64>, StoreError>;
        fn reply_comment(&mut self, id: i64, parent: i64, by: Option<&str>, at: i64, body: &str) -> Result<Option<i64>, StoreError>;
        fn resolve_comment(&mut self, id: i64, comment: i64, resolved: bool, by: Option<&str>, at: i64) -> Result<bool, StoreError>;
        fn delete_comment(&mut self, id: i64, comment: i64) -> Result<Option<Vec<i64>>, StoreError>;
        fn comments(&mut self, id: i64) -> Result<Vec<Comment>, StoreError>;
        fn put_file(&mut self, id: i64, name: &str, bytes: &[u8]) -> Result<bool, StoreError>;
        fn file(&mut self, id: i64) -> Result<Option<StoredFile>, StoreError>;
        fn repos(&mut self) -> Result<RepoCounts, StoreError>;
        fn stats(&mut self) -> Result<Stats, StoreError>;
    }

    fn remote_writable(&mut self) -> Option<bool> {
        Some(false)
    }
}

/// A store; with `approval` its team approval is on (the store is its own remote place).
fn db(approval: bool) -> SqliteStore {
    let mut db = store();

    if approval {
        db.set_meta(APPROVAL_KEY, "on").expect("approval");
    }

    db
}

fn draft(db: &mut SqliteStore, by: Option<&str>) -> i64 {
    let mut seed = Seed::new("Guide")
        .status(Status::Draft)
        .chunk("Intro", "first words")
        .chunk("Usage", "second words");

    if let Some(by) = by {
        seed = seed.by(by, None);
    }

    insert(db, &seed)
}

fn live(db: &mut SqliteStore) -> i64 {
    insert(db, &Seed::new("Live guide").source("/live").chunk("Intro", "old words"))
}

fn pending(db: &mut SqliteStore, live: i64, by: &str) -> i64 {
    insert(
        db,
        &Seed::new("Live guide v2")
            .source("/live-2")
            .by(by, None)
            .pending_of(live)
            .chunk("Intro", "new words"),
    )
}

fn rev(db: &mut SqliteStore, id: i64) -> i64 {
    row(db, id).review_rev
}

fn row(db: &mut SqliteStore, id: i64) -> Document {
    db.get(id, &with_drafts()).expect("get").expect("the row").doc
}

fn refused<T: std::fmt::Debug>(result: Result<T, Error>) -> Code {
    match result {
        Err(Error::Refused(refusal)) => refusal.code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn review<'a>(db: &'a mut dyn KnowledgeStore, name: Option<&str>) -> Review<'a> {
    Review::new(db, Place::Remote, name)
}

#[test]
fn the_refusal_codes_are_the_names_the_commands_print() {
    let codes = [
        (Code::NotFound, "notFound"),
        (Code::Changed, "changed"),
        (Code::WrongState, "wrongState"),
        (Code::ApprovalOn, "approvalOn"),
        (Code::ApprovalOff, "approvalOff"),
        (Code::SelfApproval, "selfApproval"),
        (Code::NoName, "noName"),
        (Code::ReadOnly, "readOnly"),
        (Code::CommentRequired, "commentRequired"),
        (Code::NotAuthor, "notAuthor"),
        (Code::Refused, "refused"),
        (Code::Unreachable, "unreachable"),
        (Code::LocalPath, "localPath"),
    ];

    for (code, name) in codes {
        assert_eq!(code.as_str(), name);
    }
}

#[test]
fn a_draft_is_asked_then_approved_by_a_teammate_and_goes_live() {
    let mut db = db(true);
    let id = draft(&mut db, Some("Ada"));
    let generation = db.generation().expect("generation");

    let expect = rev(&mut db, id);
    let asked = review(&mut db, Some("Ada"))
        .ask(id, expect, Some("Please check"))
        .expect("ask");
    assert_eq!((asked.status, asked.promoted), (Status::Waiting, false));
    assert!(db.generation().expect("generation") > generation, "the pulse moves");

    let approved = review(&mut db, Some("Bob"))
        .approve(id, asked.revision)
        .expect("approve");
    assert_eq!(
        (approved.id, approved.status, approved.promoted),
        (id, Status::Live, false)
    );

    let events = db.history(id).expect("history").expect("history").events;
    let kinds: Vec<EventKind> = events.iter().map(|event| event.kind).collect();
    assert_eq!(kinds, [EventKind::Approved, EventKind::Asked, EventKind::Drafted]);
    assert_eq!(events[1].comment.as_deref(), Some("Please check"));
    assert_eq!(events[0].by.as_deref(), Some("Bob"));
}

#[test]
fn approving_a_pending_version_promotes_it_to_the_live_id() {
    let mut db = db(true);
    let live = live(&mut db);
    let pending = pending(&mut db, live, "Ada");

    let expect = rev(&mut db, pending);
    let asked = review(&mut db, Some("Ada")).ask(pending, expect, None).expect("ask");
    let done = review(&mut db, Some("Bob"))
        .approve(pending, asked.revision)
        .expect("approve");

    assert_eq!((done.id, done.promoted, done.status), (live, true, Status::Live));
    assert_eq!(row(&mut db, live).title, "Live guide v2");
    assert!(db.get(pending, &with_drafts()).expect("get").is_none());
}

#[test]
fn requesting_changes_needs_a_comment_and_sends_the_row_back_to_its_author() {
    let mut db = db(true);
    let id = draft(&mut db, Some("Ada"));
    let expect = rev(&mut db, id);
    let asked = review(&mut db, Some("Ada")).ask(id, expect, None).expect("ask");

    assert_eq!(
        refused(review(&mut db, Some("Bob")).request_changes(id, asked.revision, "  ")),
        Code::CommentRequired
    );

    let sent = review(&mut db, Some("Bob"))
        .request_changes(id, asked.revision, "Cover the limits")
        .expect("request");
    assert_eq!(sent.status, Status::ChangesRequested);

    let latest = db.history(id).expect("history").expect("history").events.remove(0);
    assert_eq!(
        (latest.kind, latest.comment.as_deref()),
        (EventKind::ChangesRequested, Some("Cover the limits"))
    );

    // The author asks again.
    assert_eq!(
        review(&mut db, Some("Ada"))
            .ask(id, sent.revision, None)
            .expect("ask again")
            .status,
        Status::Waiting
    );
}

#[test]
fn a_missing_document_is_not_found_and_a_stale_revision_is_changed() {
    let mut db = db(true);
    assert_eq!(refused(review(&mut db, Some("Ada")).ask(99, 0, None)), Code::NotFound);

    let id = draft(&mut db, Some("Ada"));
    let expect = rev(&mut db, id);

    assert_eq!(
        refused(review(&mut db, Some("Ada")).ask(id, expect + 5, None)),
        Code::Changed
    );
    assert_eq!(
        refused(review(&mut db, Some("Ada")).go_live(id, expect + 1)),
        Code::ApprovalOn
    );
}

#[test]
fn an_action_that_does_not_fit_the_state_is_a_wrong_state() {
    let mut db = db(true);
    let id = draft(&mut db, Some("Ada"));
    let expect = rev(&mut db, id);

    // A draft is not waiting for an approval, and a waiting row is not asked twice.
    assert_eq!(
        refused(review(&mut db, Some("Bob")).approve(id, expect)),
        Code::WrongState
    );
    assert_eq!(
        refused(review(&mut db, Some("Bob")).request_changes(id, expect, "no")),
        Code::WrongState
    );

    let asked = review(&mut db, Some("Ada")).ask(id, expect, None).expect("ask");
    assert_eq!(
        refused(review(&mut db, Some("Ada")).ask(id, asked.revision, None)),
        Code::WrongState
    );

    // A live document has nothing to decide.
    let live = live(&mut db);
    let expect = rev(&mut db, live);
    assert_eq!(
        refused(review(&mut db, Some("Bob")).approve(live, expect)),
        Code::WrongState
    );
}

#[test]
fn go_live_is_for_documents_that_need_no_approval_and_the_rest_need_it() {
    // On: go live is refused, the draft stays.
    let mut on = db(true);
    let id = draft(&mut on, Some("Ada"));
    let expect = rev(&mut on, id);
    assert_eq!(
        refused(review(&mut on, Some("Ada")).go_live(id, expect)),
        Code::ApprovalOn
    );
    assert_eq!(row(&mut on, id).status, Status::Draft);

    // Off: ask, request changes and approve are refused, and go live works for a draft, a waiting
    // row (approval was turned off meanwhile) and a pending version.
    let mut off = db(false);
    let id = draft(&mut off, Some("Ada"));
    let expect = rev(&mut off, id);
    assert_eq!(
        refused(review(&mut off, Some("Ada")).ask(id, expect, None)),
        Code::ApprovalOff
    );
    assert_eq!(
        refused(review(&mut off, Some("Bob")).approve(id, expect)),
        Code::ApprovalOff
    );
    assert_eq!(
        refused(review(&mut off, Some("Bob")).request_changes(id, expect, "x")),
        Code::ApprovalOff
    );

    let done = review(&mut off, None)
        .go_live(id, expect)
        .expect("go live needs no name");
    assert_eq!((done.status, done.promoted), (Status::Live, false));
    assert_eq!(
        off.history(id).expect("history").expect("history").events[0].kind,
        EventKind::WentLive
    );

    let base = live(&mut off);
    let pending = pending(&mut off, base, "Ada");
    let expect = rev(&mut off, pending);
    let done = review(&mut off, Some("Ada")).go_live(pending, expect).expect("go live");
    assert_eq!((done.id, done.promoted), (base, true));
}

#[test]
fn a_document_on_this_computer_is_never_under_approval() {
    // This computer's file may even hold the key: it is not the team's.
    let mut local = db(true);
    let id = draft(&mut local, Some("Ada"));
    let expect = rev(&mut local, id);

    let mut review = Review::new(&mut local, Place::Local, Some("Ada"));
    assert_eq!(refused(review.ask(id, expect, None)), Code::ApprovalOff);
    assert_eq!(review.go_live(id, expect).expect("go live").status, Status::Live);
}

#[test]
fn the_author_and_the_asker_cannot_approve_what_they_wrote_or_asked_for() {
    let mut db = db(true);
    let id = draft(&mut db, Some("Ada"));
    let expect = rev(&mut db, id);

    // Bob asks for Ada's draft: Ada (the author) and Bob (the asker) may not approve it.
    let asked = review(&mut db, Some("Bob"))
        .ask(id, expect, None)
        .expect("anyone may ask");

    assert_eq!(
        refused(review(&mut db, Some(" ada ")).approve(id, asked.revision)),
        Code::SelfApproval
    );
    assert_eq!(
        refused(review(&mut db, Some("BOB")).approve(id, asked.revision)),
        Code::SelfApproval
    );
    assert_eq!(
        refused(review(&mut db, Some("ada")).request_changes(id, asked.revision, "mine")),
        Code::SelfApproval
    );

    let done = review(&mut db, Some("Cy"))
        .approve(id, asked.revision)
        .expect("a third person");
    assert_eq!(done.status, Status::Live);
}

#[test]
fn an_author_without_a_name_blocks_nobody_and_a_reviewer_needs_one() {
    let mut db = db(true);
    let id = draft(&mut db, None);
    let expect = rev(&mut db, id);

    assert_eq!(
        refused(review(&mut db, None).ask(id, expect, None)),
        Code::NoName,
        "asking needs a name"
    );
    assert_eq!(refused(review(&mut db, Some("  ")).ask(id, expect, None)), Code::NoName);

    let asked = review(&mut db, Some("Ada")).ask(id, expect, None).expect("ask");
    assert_eq!(refused(review(&mut db, None).approve(id, asked.revision)), Code::NoName);
    assert_eq!(
        refused(review(&mut db, None).request_changes(id, asked.revision, "x")),
        Code::NoName
    );

    // The author had no name, the asker was Ada: Bob may approve.
    assert_eq!(
        review(&mut db, Some("Bob"))
            .approve(id, asked.revision)
            .expect("approve")
            .status,
        Status::Live
    );
}

#[test]
fn a_read_only_user_can_change_nothing_in_the_team_database() {
    let mut inner = db(true);
    let id = draft(&mut inner, Some("Ada"));
    let expect = rev(&mut inner, id);
    let mut read_only = ReadOnly(inner);

    assert_eq!(
        refused(review(&mut read_only, Some("Ada")).ask(id, expect, None)),
        Code::ReadOnly
    );
    assert_eq!(
        refused(review(&mut read_only, Some("Ada")).go_live(id, expect)),
        Code::ReadOnly
    );
    assert_eq!(
        refused(review(&mut read_only, Some("Bob")).reply(id, 1, "hi")),
        Code::ReadOnly
    );
    assert_eq!(
        refused(review(&mut read_only, Some("Bob")).resolve(id, 1, true)),
        Code::ReadOnly
    );
    assert_eq!(
        refused(review(&mut read_only, Some("Bob")).submit(id, expect, &comment_only("hi"))),
        Code::ReadOnly
    );
    assert_eq!(
        refused(set_approval(&mut read_only, true, false, Some("Ada"))),
        Code::ReadOnly
    );
    assert_eq!(
        approval_state(&mut read_only, true).expect("state").writable,
        Some(false)
    );
    assert_eq!(row(&mut read_only.0, id).status, Status::Draft);
}

fn comment_only(body: &str) -> Submission {
    Submission {
        verdict: Verdict::Comment,
        body: Some(body.to_owned()),
        comments: Vec::new(),
    }
}

#[test]
fn a_review_stores_its_comments_and_carries_out_its_verdict() {
    let mut db = db(true);
    let live = live(&mut db);
    let pending = pending(&mut db, live, "Ada");
    let expect = rev(&mut db, pending);
    let asked = review(&mut db, Some("Ada")).ask(pending, expect, None).expect("ask");

    // The author may comment, and only comment.
    let by_author = review(&mut db, Some("Ada"))
        .submit(
            pending,
            asked.revision,
            &comment_only("Context: the old text was wrong"),
        )
        .expect("the author comments");
    assert_eq!((by_author.comment_ids.len(), by_author.transition), (1, None));

    let approving = Submission {
        verdict: Verdict::Approve,
        body: None,
        comments: Vec::new(),
    };
    assert_eq!(
        refused(review(&mut db, Some("Ada")).submit(pending, asked.revision, &approving)),
        Code::SelfApproval
    );
    assert_eq!(
        db.comments(pending).expect("comments").len(),
        1,
        "a refused review stores nothing"
    );

    // A request for changes needs a body or a comment.
    let empty = Submission {
        verdict: Verdict::RequestChanges,
        body: None,
        comments: Vec::new(),
    };
    assert_eq!(
        refused(review(&mut db, Some("Bob")).submit(pending, asked.revision, &empty)),
        Code::CommentRequired
    );

    let requesting = Submission {
        verdict: Verdict::RequestChanges,
        body: Some("Almost\nthere".to_owned()),
        comments: vec![NewComment::on_lines("Intro", Side::New, 1, 2, "Why this wording?")],
    };
    let sent = review(&mut db, Some("Bob"))
        .submit(pending, asked.revision, &requesting)
        .expect("request");

    assert_eq!(sent.comment_ids.len(), 2, "the body first, then the line comment");
    assert_eq!(
        sent.transition.as_ref().map(|done| done.status),
        Some(Status::ChangesRequested)
    );

    let comments = db.comments(pending).expect("comments");
    assert_eq!(comments.len(), 3);
    assert_eq!(comments[1].body, "Almost\nthere");
    assert!(comments[1].section.is_none());
    assert_eq!(
        (comments[2].section.as_deref(), comments[2].side),
        (Some("Intro"), Some(Side::New))
    );
    assert_eq!(comments[2].by.as_deref(), Some("Bob"));
    assert_eq!(comments[1].review_id, comments[2].review_id);

    let latest = db.history(pending).expect("history").expect("history").events.remove(0);
    assert_eq!(latest.comment.as_deref(), Some("Almost there"), "one line");

    // Asked again and approved with a verdict: the pending version goes live.
    let revision = sent.transition.expect("transition").revision;
    let asked = review(&mut db, Some("Ada"))
        .ask(pending, revision, None)
        .expect("ask again");
    let done = review(&mut db, Some("Bob"))
        .submit(pending, asked.revision, &approving)
        .expect("approve")
        .transition
        .expect("transition");
    assert_eq!((done.id, done.promoted), (live, true));
    assert!(
        db.comments(live).expect("comments").is_empty(),
        "the review is over, its comments are not kept"
    );
}

#[test]
fn a_review_of_a_stale_revision_or_with_a_bad_comment_stores_nothing() {
    let mut db = db(true);
    let id = draft(&mut db, Some("Ada"));
    let expect = rev(&mut db, id);
    let asked = review(&mut db, Some("Ada")).ask(id, expect, None).expect("ask");

    assert_eq!(
        refused(review(&mut db, Some("Bob")).submit(id, asked.revision - 1, &comment_only("late"))),
        Code::Changed
    );

    let bad = Submission {
        verdict: Verdict::Comment,
        body: None,
        comments: vec![NewComment::on_lines("Intro", Side::New, 3, 2, "backwards")],
    };
    assert_eq!(
        refused(review(&mut db, Some("Bob")).submit(id, asked.revision, &bad)),
        Code::Refused
    );
    assert_eq!(
        refused(review(&mut db, Some("Bob")).submit(
            id,
            asked.revision,
            &Submission {
                body: None,
                ..comment_only("")
            }
        )),
        Code::Refused,
        "a comment review with nothing in it"
    );
    assert!(db.comments(id).expect("comments").is_empty());
}

#[test]
fn comments_are_answered_and_resolved_by_anyone_and_a_reply_is_not_answered_itself() {
    let mut db = db(true);
    let id = draft(&mut db, Some("Ada"));
    let expect = rev(&mut db, id);
    let ids = review(&mut db, Some("Bob"))
        .submit(
            id,
            expect,
            &Submission {
                verdict: Verdict::Comment,
                body: None,
                comments: vec![NewComment::on_lines("Intro", Side::New, 1, 1, "Typo")],
            },
        )
        .expect("submit")
        .comment_ids;
    let root = ids[0];

    let reply = review(&mut db, Some("Ada"))
        .reply(id, root, "Fixed")
        .expect("the author answers");
    assert_eq!(
        refused(review(&mut db, Some("Bob")).reply(id, reply, "ok")),
        Code::NotFound
    );
    assert_eq!(
        refused(review(&mut db, Some("Bob")).reply(id, 999, "ok")),
        Code::NotFound
    );
    assert_eq!(
        refused(review(&mut db, Some("Bob")).reply(id, root, "  ")),
        Code::Refused
    );

    review(&mut db, Some("Ada")).resolve(id, root, true).expect("resolve");
    let comments = db.comments(id).expect("comments");
    assert_eq!(comments[0].resolved_by.as_deref(), Some("Ada"));
    assert_eq!(comments[1].parent_id, Some(root));

    review(&mut db, Some("Ada"))
        .resolve(id, root, false)
        .expect("unresolve");
    assert!(db.comments(id).expect("comments")[0].resolved_at.is_none());
    assert_eq!(
        refused(review(&mut db, Some("Ada")).resolve(id, reply, true)),
        Code::NotFound
    );
}

/// Document `id` with a thread by Bob (`.0`), a reply to it by Ada (`.1`) and a thread by Cy (`.2`).
fn commented(db: &mut SqliteStore) -> (i64, i64, i64, i64) {
    let id = draft(db, Some("Ada"));
    let expect = rev(db, id);
    let submission = |text: &str| Submission {
        verdict: Verdict::Comment,
        body: None,
        comments: vec![NewComment::on_lines("Intro", Side::New, 1, 1, text)],
    };

    let root = review(db, Some("Bob"))
        .submit(id, expect, &submission("Typo"))
        .expect("submit")
        .comment_ids[0];
    let other = review(db, Some("Cy"))
        .submit(id, expect, &submission("Unclear"))
        .expect("submit")
        .comment_ids[0];
    let reply = review(db, Some("Ada")).reply(id, root, "Fixed").expect("reply");

    (id, root, reply, other)
}

fn comment_ids(db: &mut SqliteStore, id: i64) -> Vec<i64> {
    db.comments(id)
        .expect("comments")
        .iter()
        .map(|comment| comment.id)
        .collect()
}

#[test]
fn only_the_author_of_a_comment_deletes_it_and_a_thread_takes_its_replies_along() {
    for approval in [true, false] {
        let mut db = db(approval);
        let (id, root, reply, other) = commented(&mut db);

        // Not the author of the thread, even when the one who answered it, or the document's author.
        assert_eq!(
            refused(review(&mut db, Some("Ada")).delete_comment(id, root)),
            Code::NotAuthor
        );
        assert_eq!(
            refused(review(&mut db, Some("Bob")).delete_comment(id, reply)),
            Code::NotAuthor
        );
        assert_eq!(comment_ids(&mut db, id), [root, other, reply]);

        // A reply goes alone, by its author.
        assert_eq!(
            review(&mut db, Some("Ada")).delete_comment(id, reply).expect("delete"),
            [reply]
        );
        assert_eq!(comment_ids(&mut db, id), [root, other]);

        // A thread goes with its replies; the other thread stays.
        let answered = review(&mut db, Some("Cy")).reply(id, root, "More").expect("reply");
        assert_eq!(
            review(&mut db, Some("Bob")).delete_comment(id, root).expect("delete"),
            [root, answered]
        );
        assert_eq!(comment_ids(&mut db, id), [other]);

        assert_eq!(
            refused(review(&mut db, Some("Bob")).delete_comment(id, root)),
            Code::NotFound
        );
        assert_eq!(
            refused(review(&mut db, Some("Bob")).delete_comment(id, 999)),
            Code::NotFound
        );
        assert_eq!(
            refused(review(&mut db, Some("Bob")).delete_comment(999, other)),
            Code::NotFound
        );
    }
}

#[test]
fn a_comment_is_deleted_in_every_state_and_its_history_stays() {
    let mut db = db(true);
    let (id, root, _, other) = commented(&mut db);
    let expect = rev(&mut db, id);

    // Waiting for approval, then sent back for changes.
    review(&mut db, Some("Ada")).ask(id, expect, None).expect("ask");
    assert_eq!(row(&mut db, id).status, Status::Waiting);
    let events = db.history(id).expect("history").expect("a history").events.len();

    review(&mut db, Some("Cy"))
        .delete_comment(id, other)
        .expect("delete on a waiting row");

    let asked = rev(&mut db, id);
    review(&mut db, Some("Bob"))
        .request_changes(id, asked, "Please fix")
        .expect("request changes");
    assert_eq!(row(&mut db, id).status, Status::ChangesRequested);
    review(&mut db, Some("Bob")).delete_comment(id, root).expect("delete");

    assert!(comment_ids(&mut db, id).is_empty());
    assert_eq!(
        db.history(id).expect("history").expect("a history").events.len(),
        events + 1,
        "only the request for changes was recorded"
    );
}

#[test]
fn the_names_of_a_comment_are_compared_trimmed_and_without_case() {
    let mut db = db(true);
    let (id, root, _, other) = commented(&mut db);

    review(&mut db, Some("  bOB "))
        .delete_comment(id, root)
        .expect("the author");
    review(&mut db, Some("CY"))
        .delete_comment(id, other)
        .expect("the author");
    assert!(comment_ids(&mut db, id).is_empty());
}

#[test]
fn deleting_a_comment_needs_a_name_and_write_access() {
    let mut db = db(true);
    let (id, root, _, _) = commented(&mut db);

    assert_eq!(refused(review(&mut db, None).delete_comment(id, root)), Code::NoName);
    assert_eq!(
        refused(review(&mut db, Some("  ")).delete_comment(id, root)),
        Code::NoName
    );
    assert_eq!(comment_ids(&mut db, id).len(), 3);

    let mut read_only = ReadOnly(db);

    assert_eq!(
        refused(review(&mut read_only, Some("Bob")).delete_comment(id, root)),
        Code::ReadOnly
    );
    assert_eq!(
        refused(review(&mut read_only, None).delete_comment(id, root)),
        Code::ReadOnly,
        "read-only is checked before the name"
    );
    assert_eq!(comment_ids(&mut read_only.0, id).len(), 3);
}

#[test]
fn deleting_a_comment_of_an_unreachable_remote_is_unreachable() {
    let mut local = db(true);
    let (id, root, _, _) = commented(&mut local);
    let mut routed = remote_down(local);

    assert_eq!(
        refused(review(&mut routed, Some("Bob")).delete_comment(id, root)),
        Code::Unreachable
    );
}

#[test]
fn the_approval_setting_is_read_and_written_in_the_remote_only() {
    let mut db = db(false);

    assert_eq!(
        approval_state(&mut db, true).expect("state"),
        ApprovalState {
            writable: Some(true),
            ..ApprovalState::unset()
        },
        "nobody has switched it yet"
    );

    let generation = db.generation().expect("generation");
    let on = set_approval(&mut db, true, true, Some("Ada")).expect("on");
    assert!(on.approval);
    assert!(db.generation().expect("generation") > generation, "the pulse moves");
    assert_eq!(db.meta(APPROVAL_KEY).expect("meta").as_deref(), Some("on"));

    // Who and when are written with the switch, and read back.
    assert_eq!(on.changed_by.as_deref(), Some("Ada"));
    let at = on.changed_at.clone().expect("a time");
    assert!(at.ends_with('Z') && at.contains('T'), "{at}");
    assert_eq!(db.meta(APPROVAL_BY_KEY).expect("meta").as_deref(), Some("Ada"));
    assert_eq!(
        approval_state(&mut db, true).expect("state").changed_by.as_deref(),
        Some("Ada")
    );

    // Turning it on changed no document, and turning it off leaves a waiting row waiting.
    let id = draft(&mut db, Some("Ada"));
    let expect = rev(&mut db, id);
    review(&mut db, Some("Ada")).ask(id, expect, None).expect("ask");

    let off = set_approval(&mut db, true, false, Some("  Bob ")).expect("off");
    assert!(!off.approval);
    assert_eq!(
        off.changed_by.as_deref(),
        Some("Bob"),
        "the last switch is the one shown"
    );
    assert_eq!(row(&mut db, id).status, Status::Waiting);

    // Without a team database there is no such setting.
    assert_eq!(approval_state(&mut db, false).expect("state"), ApprovalState::local());
    assert_eq!(refused(set_approval(&mut db, false, true, Some("Ada"))), Code::Refused);
}

#[test]
fn switching_approval_needs_a_name_and_changes_nothing_without_one() {
    let mut db = db(false);
    let generation = db.generation().expect("generation");

    for actor in [None, Some("  "), Some("\n")] {
        assert_eq!(refused(set_approval(&mut db, true, true, actor)), Code::NoName);
    }

    assert_eq!(db.meta(APPROVAL_KEY).expect("meta"), None);
    assert_eq!(db.meta(APPROVAL_BY_KEY).expect("meta"), None);
    assert_eq!(db.generation().expect("generation"), generation);
}

#[test]
fn a_value_other_than_on_is_off() {
    let mut db = store();

    for (value, expected) in [
        ("on", true),
        (" on ", true),
        ("off", false),
        ("ON", false),
        ("yes", false),
        ("", false),
    ] {
        db.set_meta(APPROVAL_KEY, value).expect("meta");
        assert_eq!(
            approval_in(&mut db, Place::Remote).expect("approval"),
            expected,
            "{value:?}"
        );
    }
}

#[test]
fn the_links_of_a_pending_version_are_refused_and_the_live_ones_are_not() {
    let mut db = db(true);
    let live = live(&mut db);
    let pending = pending(&mut db, live, "Ada");

    assert_eq!(refused(check_links_target(&mut db, pending)), Code::Refused);
    assert_eq!(refused(check_links_target(&mut db, 999)), Code::NotFound);
    check_links_target(&mut db, live).expect("the live document's links stay direct");
    let plain = draft_id(&mut db);
    check_links_target(&mut db, plain).expect("a plain draft has its own links");
}

fn draft_id(db: &mut SqliteStore) -> i64 {
    insert(
        db,
        &Seed::new("Other")
            .source("/other")
            .status(Status::Draft)
            .chunk("A", "words"),
    )
}

#[test]
fn deleting_a_live_document_deletes_its_pending_version() {
    let mut db = db(true);
    let live = live(&mut db);
    let pending = pending(&mut db, live, "Ada");

    assert_eq!(db.delete(&[live]).expect("delete"), 1);
    assert!(db.get(pending, &with_drafts()).expect("get").is_none());
    assert_eq!(db.stats().expect("stats").pending, 0);
}

/// A routed store whose remote cannot be reached, over this computer's file with the key set.
fn remote_down(local: SqliteStore) -> RoutedStore {
    let opener = |_: Patience| Err(OpenError::Unreachable("the connection was lost".to_owned()));
    let remote = Remote::lazy(
        opener,
        Reach::default(),
        std::time::Duration::from_secs(1),
        Secrets::default(),
    );

    RoutedStore::new(
        Some(Box::new(local)),
        None,
        Routing::new(Where::RemoteLocal, Where::RemoteLocal),
    )
    .with_remote(remote)
}

#[test]
fn a_remote_that_cannot_be_reached_is_unreachable_and_never_this_computers_answer() {
    let mut local = db(true);
    let id = draft(&mut local, Some("Ada"));
    let mut routed = remote_down(local);

    // The ordinary read falls back to this computer's file, the team's setting does not.
    assert_eq!(routed.meta(APPROVAL_KEY).expect("fallback").as_deref(), Some("on"));
    assert_eq!(refused(approval_in(&mut routed, Place::Remote)), Code::Unreachable);
    assert_eq!(refused(approval_state(&mut routed, true)), Code::Unreachable);
    assert_eq!(
        refused(set_approval(&mut routed, true, true, Some("Ada"))),
        Code::Unreachable
    );

    // Every transition of a remote document says so (a plain id is the remote's).
    assert_eq!(
        refused(review(&mut routed, Some("Ada")).ask(id, 0, None)),
        Code::Unreachable
    );
    assert_eq!(
        refused(review(&mut routed, Some("Bob")).approve(id, 0)),
        Code::Unreachable
    );
    assert_eq!(
        refused(review(&mut routed, Some("Ada")).go_live(id, 0)),
        Code::Unreachable
    );
    assert_eq!(
        refused(review(&mut routed, Some("Bob")).request_changes(id, 0, "no")),
        Code::Unreachable
    );

    // This computer's own documents are not under approval and still go live.
    let mut here = Review::new(&mut routed, Place::Local, Some("Ada"));
    let local_id = crate::store::routed::ids::tag(Place::Local, id);
    let expect = 0;
    assert_eq!(refused(here.ask(local_id, expect, None)), Code::ApprovalOff);
}

#[test]
fn an_error_of_the_store_that_is_not_a_refusal_stays_an_error() {
    assert!(matches!(
        Error::from(StoreError::Backend("disk".to_owned())),
        Error::Store(StoreError::Backend(_))
    ));
    assert!(matches!(Error::from(StoreError::Busy), Error::Store(StoreError::Busy)));
}

fn put(store: &mut dyn KnowledgeStore, seed: &Seed) -> i64 {
    let (doc, chunks, links) = seed.parts();

    store.put_document(&doc, &chunks, &links).expect("put document")
}

/// A team libSQL server, when one is configured: the whole flow against a real database.
#[test]
fn team_the_review_flow_runs_on_a_libsql_server() {
    use std::fs::File;

    let Some(url) = std::env::var("HARNESS_KNOWLEDGE_TEST_LIBSQL_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
    else {
        eprintln!("skipped: set HARNESS_KNOWLEDGE_TEST_LIBSQL_URL to run this case on a libSQL server");
        return;
    };

    // The same lock the store's own team cases take, so they never share the database.
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::env::temp_dir().join("harness-knowledge-libsql-tests.lock"))
        .expect("lock file");
    lock.lock().expect("lock");

    let mut team = crate::store::libsql::LibsqlStore::connect(&url, None).expect("connect");
    let unique = format!("review-{}", crate::import::document::now());

    let before = approval_in(&mut team, Place::Remote).expect("approval");
    assert!(set_approval(&mut team, true, true, Some("Ada")).expect("on").approval);

    let live = put(
        &mut team,
        &Seed::new("Team guide")
            .source(&format!("/{unique}"))
            .chunk("Intro", "old words"),
    );
    let pending = put(
        &mut team,
        &Seed::new("Team guide v2")
            .source(&format!("/{unique}-2"))
            .by("Ada", None)
            .pending_of(live)
            .chunk("Intro", "new words"),
    );
    let expect = team
        .get(pending, &with_drafts())
        .expect("get")
        .expect("row")
        .doc
        .review_rev;

    assert_eq!(
        refused(review(&mut team, Some("Ada")).go_live(pending, expect)),
        Code::ApprovalOn
    );

    let asked = review(&mut team, Some("Ada"))
        .ask(pending, expect, Some("Please"))
        .expect("ask");
    assert_eq!(
        refused(review(&mut team, Some(" ADA ")).approve(pending, asked.revision)),
        Code::SelfApproval
    );
    assert_eq!(
        refused(review(&mut team, Some("Bob")).approve(pending, asked.revision + 1)),
        Code::Changed
    );

    let done = review(&mut team, Some("Bob"))
        .approve(pending, asked.revision)
        .expect("approve");
    assert_eq!((done.id, done.promoted), (live, true));

    // Clean up what this case made and restore the setting.
    team.delete(&[live]).expect("delete");
    set_approval(&mut team, true, before, Some("Ada")).expect("restore");
}

/// A draft with its own source key by `author`, asked by `asker`.
fn waiting(db: &mut SqliteStore, source: &str, author: &str, asker: &str) -> i64 {
    let id = insert(
        db,
        &Seed::new(source)
            .source(source)
            .status(Status::Draft)
            .by(author, None)
            .chunk("Intro", source),
    );
    let expect = rev(db, id);

    review(db, Some(asker)).ask(id, expect, None).expect("ask");

    id
}

#[test]
fn waiting_for_counts_what_this_person_could_approve() {
    let mut db = db(true);

    waiting(&mut db, "/ada-own", "Ada", "Ada");
    waiting(&mut db, "/bob-own", "Bob", "Bob");
    // Written by Ada, asked for by Bob: neither of them may decide on it.
    waiting(&mut db, "/ada-asked-by-bob", "Ada", "Bob");
    // A draft that nobody asked for does not wait.
    draft(&mut db, Some("Bob"));

    // A live document whose pending version Ada wrote and asked for.
    let live = live(&mut db);
    let version = pending(&mut db, live, "Ada");
    let expect = rev(&mut db, version);
    review(&mut db, Some("Ada")).ask(version, expect, None).expect("ask");

    let count = |db: &mut SqliteStore, name: Option<&str>| waiting_for(db, name).expect("count");

    assert_eq!(count(&mut db, Some("Ada")), 1, "only Bob's own draft");
    assert_eq!(
        count(&mut db, Some("Bob")),
        2,
        "Ada's own draft and her pending version"
    );
    assert_eq!(count(&mut db, Some(" bOb ")), 2, "trimmed and case-insensitive");
    assert_eq!(count(&mut db, Some("Cara")), 4, "everything waits for a third person");
    assert_eq!(count(&mut db, None), 0, "no name");
    assert_eq!(count(&mut db, Some("  ")), 0, "a blank name");
}

#[test]
fn waiting_for_pages_through_more_rows_than_one_page_holds() {
    let mut db = db(true);

    for index in 0..(LIST_PAGE_MAX + 5) {
        waiting(&mut db, &format!("/doc-{index}"), "Ada", "Ada");
    }

    assert_eq!(waiting_for(&mut db, Some("Bob")).expect("count"), LIST_PAGE_MAX + 5);
    assert_eq!(waiting_for(&mut db, Some("Ada")).expect("count"), 0);
}

fn state_of(db: &mut SqliteStore) -> PlaceState {
    let stats = db.stats().expect("stats");

    PlaceState {
        place: Place::Remote,
        generation: stats.generation,
        waiting: stats.waiting,
    }
}

#[test]
fn the_waiting_count_is_read_again_only_when_the_generation_or_the_name_changes() {
    let folder = tempfile::tempdir().expect("folder");
    let mut spy = Spy::new(db(true));

    waiting(&mut spy.store, "/ada", "Ada", "Ada");
    waiting(&mut spy.store, "/bob", "Bob", "Bob");

    let state = state_of(&mut spy.store);
    let count = |spy: &mut Spy, state: PlaceState, name: &str| {
        waiting_for_cached(spy, Some(folder.path()), state, Some(name)).expect("count")
    };

    assert_eq!(count(&mut spy, state, "Cara"), 2);
    assert!(spy.reads > 0, "the first count reads the waiting rows");

    // The same generation and name: nothing is read.
    spy.reads = 0;
    assert_eq!(count(&mut spy, state, " cara "), 2);
    assert_eq!(spy.reads, 0, "an unchanged generation does no list or history read");

    // Another name is worked out, and both answers stay cached.
    assert_eq!(count(&mut spy, state, "Ada"), 1);
    assert!(spy.reads > 0);
    spy.reads = 0;
    assert_eq!(count(&mut spy, state, "Ada"), 1);
    assert_eq!(count(&mut spy, state, "Cara"), 2);
    assert_eq!(spy.reads, 0);

    // A change moves the generation, so the count is worked out again.
    waiting(&mut spy.store, "/dan", "Dan", "Dan");
    let moved = state_of(&mut spy.store);
    assert_ne!(moved.generation, state.generation);

    spy.reads = 0;
    assert_eq!(count(&mut spy, moved, "Cara"), 3);
    assert!(spy.reads > 0);

    // The file is private, and one that cannot be read is only a cache that missed.
    let file = folder.path().join(WAITING_CACHE_FILE);
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&file).expect("file").permissions()) & 0o777,
        0o600
    );
    std::fs::write(&file, "not json").expect("garbage");
    spy.reads = 0;
    assert_eq!(count(&mut spy, moved, "Cara"), 3);
    assert!(spy.reads > 0);
}

#[test]
fn the_waiting_count_has_no_cache_without_a_data_folder_or_a_name() {
    let mut spy = Spy::new(db(true));
    waiting(&mut spy.store, "/ada", "Ada", "Ada");
    let state = state_of(&mut spy.store);

    assert_eq!(
        waiting_for_cached(&mut spy, None, state, Some("Bob")).expect("count"),
        1
    );
    spy.reads = 0;
    assert_eq!(
        waiting_for_cached(&mut spy, None, state, Some("Bob")).expect("count"),
        1
    );
    assert!(spy.reads > 0, "worked out each time");

    let missing = std::path::Path::new("/nonexistent/harness-knowledge-test");
    assert_eq!(
        waiting_for_cached(&mut spy, Some(missing), state, Some("Bob")).expect("count"),
        1
    );
    assert_eq!(waiting_for_cached(&mut spy, None, state, None).expect("count"), 0);
}

#[test]
fn a_review_stamps_its_comments_with_the_revision_that_was_reviewed() {
    let mut spy = Spy::new(db(true));
    let id = waiting(&mut spy.store, "/guide", "Ada", "Ada");
    let reviewed = rev(&mut spy.store, id);

    // A change lands between the check and the write of the comments.
    spy.race = Some(Box::new(move |store| {
        assert!(store.rename(id, "Moved on", Some("Ada"), 99).expect("rename"));
    }));

    review(&mut spy, Some("Grace"))
        .submit(id, reviewed, &comment_only("Looks fine"))
        .expect("review");

    assert!(rev(&mut spy.store, id) > reviewed, "the document moved on");

    let stored = spy.store.comments(id).expect("comments");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].revision, reviewed, "the revision the reviewer saw");
}

#[test]
fn an_id_naming_a_place_the_routing_does_not_use_is_not_found() {
    let local = crate::store::routed::ids::tag(Place::Local, 5);
    let remote_only = Routing::new(Where::Remote, Where::Remote);

    assert_eq!(refused(place_of(remote_only, local)), Code::NotFound);
    assert_eq!(place_of(remote_only, 5).expect("a plain id"), Place::Remote);

    let both = Routing::new(Where::RemoteLocal, Where::RemoteLocal);
    assert_eq!(place_of(both, local).expect("in use"), Place::Local);
    assert_eq!(place_of(both, 5).expect("plain is the remote's"), Place::Remote);
    assert_eq!(place_of(Routing::local(), local).expect("the only place"), Place::Local);
}

#[test]
fn only_the_author_or_the_asker_takes_an_ask_back_and_it_is_a_draft_again() {
    let mut db = db(true);
    // Written by Ada, asked for by Bob (who may ask for a draft that is not his).
    let id = draft(&mut db, Some("Ada"));
    let expect = rev(&mut db, id);
    let asked = review(&mut db, Some("Bob")).ask(id, expect, None).expect("ask");

    assert_eq!(
        refused(review(&mut db, Some("Cara")).take_back(id, asked.revision)),
        Code::NotAuthor
    );
    assert_eq!(
        refused(review(&mut db, None).take_back(id, asked.revision)),
        Code::NoName
    );
    assert_eq!(row(&mut db, id).status, Status::Waiting, "refusals change nothing");

    // A stale revision is `changed`, and the author may be written any way a name can be.
    assert_eq!(
        refused(review(&mut db, Some(" ada ")).take_back(id, asked.revision - 1)),
        Code::Changed
    );

    let generation = db.generation().expect("generation");
    let back = review(&mut db, Some(" ada "))
        .take_back(id, asked.revision)
        .expect("take back");
    assert_eq!(
        (back.status, back.revision, back.promoted),
        (Status::Draft, asked.revision + 1, false)
    );
    assert!(db.generation().expect("generation") > generation);

    let history = db.history(id).expect("history").expect("a history");
    assert_eq!(history.events[0].kind, EventKind::Withdrawn);
    assert_eq!(history.events[0].by.as_deref(), Some("ada"));

    // Only what waits can be taken back.
    assert_eq!(
        refused(review(&mut db, Some("Ada")).take_back(id, back.revision)),
        Code::WrongState
    );

    // The asker may take it back too.
    let again = review(&mut db, Some("Bob"))
        .ask(id, back.revision, None)
        .expect("ask again");
    assert_eq!(
        review(&mut db, Some("Bob"))
            .take_back(id, again.revision)
            .expect("asker")
            .status,
        Status::Draft
    );
}

#[test]
fn an_ask_is_taken_back_whether_approval_is_on_or_off() {
    let mut team = db(true);
    let id = draft(&mut team, Some("Ada"));
    let expect = rev(&mut team, id);
    let asked = review(&mut team, Some("Ada")).ask(id, expect, None).expect("ask");

    // Approval is switched off: the row is left waiting, and its author can still take it back.
    set_approval(&mut team, true, false, Some("Bob")).expect("off");
    let back = review(&mut team, Some("Ada"))
        .take_back(id, asked.revision)
        .expect("take back");
    assert_eq!(back.status, Status::Draft);

    // On this computer nothing is under approval and a waiting row can still be taken back.
    let mut local = db(false);
    let id = draft(&mut local, Some("Ada"));
    let waiting_rev = rev(&mut local, id);
    local
        .set_review(id, waiting_rev, &ReviewChange::new(ReviewAction::Ask, Some("Ada"), 5))
        .expect("ask");
    let expect = rev(&mut local, id);
    let back = Review::new(&mut local, Place::Local, Some("Ada"))
        .take_back(id, expect)
        .expect("take back");
    assert_eq!(back.status, Status::Draft);
}

#[test]
fn a_pending_version_is_taken_back_by_its_live_id_row() {
    let mut db = db(true);
    let live = live(&mut db);
    let version = pending(&mut db, live, "Ada");
    let expect = rev(&mut db, version);
    let asked = review(&mut db, Some("Ada")).ask(version, expect, None).expect("ask");

    let back = review(&mut db, Some("Ada"))
        .take_back(version, asked.revision)
        .expect("take back");
    assert_eq!((back.id, back.status), (version, Status::Draft));
    assert_eq!(
        row(&mut db, live).status,
        Status::Live,
        "the live document is as it was"
    );
}
