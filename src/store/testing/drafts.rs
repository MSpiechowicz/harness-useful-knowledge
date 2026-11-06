//! What every backend answers about drafts and their review (the same checks run on each, on an
//! empty database): agents never see a row that is not live, a transition is one compare-and-set,
//! a promote replaces the live document in one step, and comments, original files and snapshots
//! follow their document.

use std::path::PathBuf;

use super::{Seed, angled, axis};
use crate::config::Place;
use crate::digest::sha256_hex;
use crate::limits::{FILE_BYTES_MAX, PDF_BYTES_MAX};
use crate::project::ProjectKey;
use crate::review::{APPROVAL_KEY, Code, Error, Review, waiting_for};
use crate::store::review::HISTORY_MAX;
use crate::store::{
    Authorship, ChunkHit, Detail, DocumentDetail, DocumentPage, EventKind, KnowledgeStore, ListQuery, NewComment,
    QuoteKind, QuoteLine, RepoFilter, ReviewAction, ReviewChange, ReviewReply, Reviewed, Scope, SectionText, Side,
    Sort, Status, StatusFilter, StoreError, TitleScope, Visibility,
};

const REPO: &str = "git:h/o/r";

fn put(store: &mut dyn KnowledgeStore, seed: &Seed) -> i64 {
    let (doc, chunks, links) = seed.parts();

    store.put_document(&doc, &chunks, &links).expect("put document")
}

fn repo_scope() -> Scope {
    Scope::from_repo(Some(ProjectKey {
        key: REPO.to_owned(),
        root: PathBuf::new(),
        remote: None,
    }))
}

/// A document as the person reads it: drafts and pending versions included.
fn read(store: &mut dyn KnowledgeStore, id: i64) -> Option<DocumentDetail> {
    store
        .get(
            id,
            &Detail {
                sections: true,
                drafts: true,
                ..Detail::default()
            },
        )
        .expect("get")
}

fn revision(store: &mut dyn KnowledgeStore, id: i64) -> i64 {
    read(store, id).expect("the document exists").doc.review_rev
}

fn list(store: &mut dyn KnowledgeStore, status: StatusFilter) -> DocumentPage {
    store
        .list(&ListQuery {
            filter: RepoFilter::All,
            format: None,
            text: String::new(),
            useful: None,
            author: None,
            want_authors: false,
            status,
            sort: Sort::Updated,
            descending: false,
            offset: 0,
            limit: 50,
        })
        .expect("list")
}

fn listed(store: &mut dyn KnowledgeStore, status: StatusFilter) -> Vec<i64> {
    let mut ids: Vec<i64> = list(store, status).items.iter().map(|row| row.doc.id).collect();
    ids.sort_unstable();
    ids
}

fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids
}

fn doc_ids(hits: &[ChunkHit]) -> Vec<i64> {
    let mut ids: Vec<i64> = hits.iter().map(|hit| hit.doc_id).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn keyword(store: &mut dyn KnowledgeStore, text: &str, scope: &Scope) -> Vec<ChunkHit> {
    store.keyword_top_k(text, scope, 50).expect("keyword search")
}

fn nearest(store: &mut dyn KnowledgeStore, vector: &[f32], scope: &Scope) -> Vec<ChunkHit> {
    store.vector_top_k(vector, scope, 50).expect("vector search")
}

fn change(action: ReviewAction, by: &str, at: i64) -> ReviewChange {
    ReviewChange::new(action, Some(by), at)
}

fn done(reply: ReviewReply) -> Reviewed {
    match reply {
        ReviewReply::Done(reviewed) => reviewed,
        other => panic!("expected the transition to be made, got {other:?}"),
    }
}

/// Asks (`draft` or `changesRequested` to `waiting`) at the current revision.
fn ask(store: &mut dyn KnowledgeStore, id: i64, by: &str, at: i64) -> Reviewed {
    let expect = revision(store, id);

    done(
        store
            .set_review(id, expect, &change(ReviewAction::Ask, by, at))
            .expect("ask"),
    )
}

fn sections(texts: &[(&str, &str)]) -> Vec<SectionText> {
    texts
        .iter()
        .map(|(heading, text)| SectionText {
            heading: (*heading).to_owned(),
            text: (*text).to_owned(),
        })
        .collect()
}

/// Agents never see a row that is not live, however the rows share words, titles, content, chunks
/// and links: keyword and vector search in every scope, `get`, `list`, the lookups an import makes,
/// the counters and the counts. The person's own scope sees drafts but never a pending version.
pub fn drafts_hidden_from_agents_contract(store: &mut dyn KnowledgeStore) {
    let live = put(
        store,
        &Seed::new("Gateway live")
            .source("/docs/live")
            .embedded("Intro", "gateway timeout live words", angled(0.0))
            .embedded("Shared", "shared gateway chunk", angled(30.0))
            .link(REPO, &[])
            .by("Ada", None),
    );
    let global = put(
        store,
        &Seed::new("Gateway global")
            .source("/docs/global")
            .embedded("Intro", "gateway global words", angled(40.0)),
    );
    let draft = put(
        store,
        &Seed::new("Gateway draft")
            .source("/docs/draft")
            .status(Status::Draft)
            .embedded("Intro", "gateway draft words", angled(1.0))
            .embedded("Shared", "shared gateway chunk", angled(31.0))
            .link(REPO, &[]),
    );
    let waiting = put(
        store,
        &Seed::new("Gateway waiting")
            .source("/docs/waiting")
            .status(Status::Draft)
            .embedded("Intro", "gateway waiting words", angled(2.0))
            .embedded("Shared", "shared gateway chunk", angled(32.0))
            .link(REPO, &[]),
    );
    ask(store, waiting, "Ada", 10);

    let requested = put(
        store,
        &Seed::new("Gateway requested")
            .source("/docs/requested")
            .status(Status::Draft)
            .embedded("Intro", "gateway requested words", angled(3.0))
            .embedded("Shared", "shared gateway chunk", angled(33.0))
            .link(REPO, &[]),
    );
    ask(store, requested, "Ada", 10);
    let expect = revision(store, requested);
    done(
        store
            .set_review(
                requested,
                expect,
                &change(ReviewAction::RequestChanges, "Bob", 11).with_comment("rework the intro"),
            )
            .expect("request changes"),
    );

    let pending = put(
        store,
        &Seed::new("Gateway live")
            .source("/docs/pending")
            .pending_of(live)
            .embedded("Intro", "gateway pending words", angled(4.0))
            .embedded("Shared", "shared gateway chunk", angled(34.0))
            .link(REPO, &[]),
    );

    let visible = sorted(vec![live, global]);
    let with_drafts = sorted(vec![live, global, draft, waiting, requested]);

    // Keyword search, in the repository's, the global and everyone's scope.
    for (name, scope) in [
        ("repo", repo_scope()),
        ("everything", Scope::everything()),
        ("global", Scope::from_repo(None)),
    ] {
        let hits = keyword(store, "gateway", &scope);
        let expected: Vec<i64> = match name {
            "global" => vec![global],
            _ => visible.clone(),
        };

        assert_eq!(doc_ids(&hits), expected, "keyword in {name}");
        assert!(
            hits.iter().all(|hit| hit.status == Status::Live),
            "keyword states in {name}"
        );

        let hits = keyword(store, "gateway", &scope.clone().with_drafts());
        let expected: Vec<i64> = match name {
            "global" => vec![global],
            _ => with_drafts.clone(),
        };

        assert_eq!(doc_ids(&hits), expected, "keyword with drafts in {name}");
        assert!(
            hits.iter().all(|hit| hit.doc_id != pending),
            "no pending version in {name}"
        );
    }

    // A word only a draft has is found by the person alone, and one only a pending version has by nobody.
    assert!(keyword(store, "draft", &Scope::everything()).is_empty());
    assert_eq!(
        doc_ids(&keyword(store, "draft", &Scope::everything().with_drafts())),
        [draft]
    );
    assert!(keyword(store, "pending", &Scope::everything()).is_empty());
    assert!(keyword(store, "pending", &Scope::everything().with_drafts()).is_empty());

    // Vector search.
    for scope in [repo_scope(), Scope::everything()] {
        let hits = nearest(store, &angled(1.0), &scope);

        assert_eq!(doc_ids(&hits), visible, "vector");
        assert!(hits.iter().all(|hit| hit.status == Status::Live));

        let hits = nearest(store, &angled(1.0), &scope.with_drafts());
        assert_eq!(doc_ids(&hits), with_drafts, "vector with drafts");
        assert!(hits.iter().all(|hit| hit.doc_id != pending));
    }

    // `get` and `list`.
    for id in [draft, waiting, requested, pending] {
        assert!(
            store.get(id, &Detail::default()).expect("get").is_none(),
            "an agent reads {id}"
        );
        assert!(read(store, id).is_some(), "the person reads {id}");
    }

    let pending_row = read(store, pending).expect("pending");
    assert_eq!(pending_row.doc.pending_of, Some(live));
    assert_eq!(pending_row.doc.status, Status::Draft);

    let live_row = read(store, live).expect("live");
    assert_eq!(live_row.pending.as_ref().map(|version| version.id), Some(pending));
    assert!(
        store
            .get(live, &Detail::default())
            .expect("get")
            .expect("live")
            .pending
            .is_none(),
        "an agent is not told about it"
    );

    assert_eq!(listed(store, StatusFilter::Live), visible);
    assert_eq!(listed(store, StatusFilter::All), with_drafts);
    assert_eq!(listed(store, StatusFilter::Draft), sorted(vec![live, draft]));
    assert_eq!(listed(store, StatusFilter::Waiting), vec![waiting]);
    assert_eq!(listed(store, StatusFilter::ChangesRequested), vec![requested]);
    assert_eq!(listed(store, StatusFilter::Pending), vec![live]);

    let page = list(store, StatusFilter::All);
    assert_eq!(page.total, with_drafts.len());
    let version = page
        .items
        .iter()
        .find(|row| row.doc.id == live)
        .and_then(|row| row.pending.clone())
        .expect("the live row carries its pending version");
    assert_eq!((version.id, version.status), (pending, Status::Draft));
    assert_eq!(store.pending_of(live).expect("pending of"), Some(version));
    assert!(store.pending_of(draft).expect("pending of").is_none());

    // The lookups of an import.
    let found = |doc: Option<crate::store::Document>| doc.map(|doc| doc.id);

    for (key, id) in [
        ("/docs/draft", draft),
        ("/docs/waiting", waiting),
        ("/docs/requested", requested),
    ] {
        assert_eq!(
            found(store.find_by_source_key(key, Visibility::Live).expect("find")),
            None
        );
        assert_eq!(
            found(store.find_by_source_key(key, Visibility::WithDrafts).expect("find")),
            Some(id)
        );

        let sha = format!("content-{key}");
        assert_eq!(
            found(store.find_by_content_sha(&sha, Visibility::Live).expect("find")),
            None
        );
        assert_eq!(
            found(store.find_by_content_sha(&sha, Visibility::WithDrafts).expect("find")),
            Some(id)
        );
    }

    for visibility in [Visibility::Live, Visibility::WithDrafts] {
        assert_eq!(
            found(store.find_by_source_key("/docs/pending", visibility).expect("find")),
            None,
            "a pending version is never found by its source"
        );
        assert_eq!(
            found(
                store
                    .find_by_content_sha("content-/docs/pending", visibility)
                    .expect("find")
            ),
            None,
            "nor by its content"
        );
    }

    let titled = |store: &mut dyn KnowledgeStore, title: &str, scope: TitleScope, visibility: Visibility| {
        sorted(
            store
                .find_by_title(title, &scope, visibility)
                .expect("find by title")
                .iter()
                .map(|doc| doc.id)
                .collect(),
        )
    };
    let repo = || TitleScope::Repo(REPO.to_owned());

    assert!(titled(store, "gateway draft", repo(), Visibility::Live).is_empty());
    assert_eq!(titled(store, "gateway draft", repo(), Visibility::WithDrafts), [draft]);
    assert_eq!(titled(store, "Gateway live", repo(), Visibility::Live), [live]);
    assert_eq!(
        titled(store, "Gateway live", repo(), Visibility::WithDrafts),
        [live],
        "the pending version has the title of its live document and is not found"
    );

    let shared = ["sha-shared gateway chunk".to_owned()];
    let count = |store: &mut dyn KnowledgeStore, visibility: Visibility| {
        let mut shared_by = store.docs_sharing_chunks(&shared, visibility).expect("sharing");
        shared_by.sort_unstable();
        shared_by
    };

    assert_eq!(count(store, Visibility::Live), [(live, 1)]);
    assert_eq!(
        count(store, Visibility::WithDrafts),
        sorted(vec![live, draft, waiting, requested])
            .into_iter()
            .map(|id| (id, 1))
            .collect::<Vec<_>>()
    );

    // Counters count live documents only.
    store.record_shown(&[live, draft, waiting, pending]).expect("shown");
    store
        .record_useful(&[live, draft, requested, pending], 77)
        .expect("useful");

    let usage = |store: &mut dyn KnowledgeStore, id: i64| read(store, id).expect("document").usage;

    assert_eq!(
        usage(store, live).map(|usage| (usage.shown, usage.useful)),
        Some((1, 1))
    );

    for id in [draft, waiting, requested, pending] {
        assert_eq!(
            usage(store, id).map(|usage| (usage.shown, usage.useful, usage.last_useful_at)),
            Some((0, 0, None)),
            "document {id} counts nothing"
        );
    }

    // The counts.
    let stats = store.stats().expect("stats");
    assert_eq!(stats.documents, 5, "pending versions are not documents");
    assert_eq!(
        (stats.drafts, stats.waiting, stats.changes_requested, stats.pending),
        (2, 1, 1, 1)
    );

    let repos = store.repos().expect("repos");
    assert_eq!(
        repos.repos,
        [(REPO.to_owned(), 4)],
        "a pending version is not counted for a repository"
    );
    assert_eq!(repos.global, 1);
}

/// A transition is one compare-and-set on the revision, the state machine refuses what does not
/// apply, every change is an event of the history, an ask keeps the sections as asked, and editing
/// what waits sends it back to draft. Every change bumps the generation, a refusal does not.
pub fn review_transitions_contract(store: &mut dyn KnowledgeStore) {
    let seed = Seed::new("Spec")
        .source("/docs/spec")
        .status(Status::Draft)
        .chunk("Intro", "alpha text")
        .chunk("Usage", "beta text")
        .by("Ada", None);
    let id = put(store, &seed);

    let drafted = read(store, id).expect("the draft");
    assert_eq!((drafted.doc.status, drafted.doc.review_rev), (Status::Draft, 0));

    let events = store.history(id).expect("history").expect("a history");
    assert_eq!(events.total, 1);
    assert_eq!(
        (events.events[0].kind, events.events[0].by.as_deref()),
        (EventKind::Drafted, Some("Ada"))
    );

    // A stale revision and the wrong state are refused, and a refusal is not a write.
    let before = store.generation().expect("generation");

    assert_eq!(
        store
            .set_review(id, 5, &change(ReviewAction::Ask, "Ada", 20))
            .expect("stale"),
        ReviewReply::Changed { revision: 0 }
    );
    assert_eq!(
        store
            .set_review(id, 0, &change(ReviewAction::RequestChanges, "Bob", 20))
            .expect("wrong state"),
        ReviewReply::WrongState(Status::Draft)
    );
    assert_eq!(
        store
            .set_review(id, 0, &change(ReviewAction::Approve, "Bob", 20))
            .expect("wrong state"),
        ReviewReply::WrongState(Status::Draft)
    );
    assert_eq!(
        store
            .set_review(987_654, 0, &change(ReviewAction::Ask, "Ada", 20))
            .expect("unknown"),
        ReviewReply::NotFound
    );
    assert_eq!(
        store.generation().expect("generation"),
        before,
        "refusals write nothing"
    );

    // Ask.
    let asked = done(
        store
            .set_review(id, 0, &change(ReviewAction::Ask, "Ada", 21))
            .expect("ask"),
    );
    assert_eq!(
        asked,
        Reviewed {
            id,
            status: Status::Waiting,
            revision: 1,
            promoted: false
        }
    );
    assert!(
        store.generation().expect("generation") > before,
        "the transition bumps the generation"
    );
    assert_eq!(
        store.asked_sections(id, 1).expect("snapshot"),
        Some(sections(&[("Intro", "alpha text"), ("Usage", "beta text")]))
    );
    assert_eq!(store.asked_sections(id, 2).expect("snapshot"), None);
    assert_eq!(
        store
            .set_review(id, 1, &change(ReviewAction::Ask, "Ada", 22))
            .expect("again"),
        ReviewReply::WrongState(Status::Waiting)
    );

    // Editing what waits makes it a draft again, a new revision and an `edited` event.
    let (mut doc, chunks, links) = Seed::new("Spec")
        .source("/docs/spec")
        .chunk("Intro", "alpha text changed")
        .chunk("Usage", "beta text")
        .updated(2_000)
        .by("Ada", None)
        .parts();
    doc.id = id;
    store.put_document(&doc, &chunks, &links).expect("edit");

    let edited = read(store, id).expect("document");
    assert_eq!((edited.doc.status, edited.doc.review_rev), (Status::Draft, 2));
    assert_eq!(
        store
            .set_review(id, 1, &change(ReviewAction::Ask, "Ada", 23))
            .expect("old revision"),
        ReviewReply::Changed { revision: 2 }
    );

    // Ask again, a reviewer asks for changes with a comment, the author edits and asks once more.
    assert_eq!(ask(store, id, "Ada", 24).revision, 3);

    let requested = done(
        store
            .set_review(
                id,
                3,
                &change(ReviewAction::RequestChanges, "Bob", 25).with_comment("  needs\n an example "),
            )
            .expect("request changes"),
    );
    assert_eq!((requested.status, requested.revision), (Status::ChangesRequested, 4));
    assert_eq!(
        store
            .set_review(id, 4, &change(ReviewAction::Approve, "Bob", 26))
            .expect("approve"),
        ReviewReply::WrongState(Status::ChangesRequested)
    );
    assert_eq!(ask(store, id, "Ada", 27).revision, 5);

    // The sections as they were asked stay as they were.
    assert_eq!(
        store.asked_sections(id, 1).expect("snapshot"),
        Some(sections(&[("Intro", "alpha text"), ("Usage", "beta text")]))
    );
    assert_eq!(
        store.asked_sections(id, 3).expect("snapshot"),
        Some(sections(&[("Intro", "alpha text changed"), ("Usage", "beta text")]))
    );

    // Approve: the document goes live under its own id.
    assert!(store.get(id, &Detail::default()).expect("get").is_none());
    let approved = done(
        store
            .set_review(id, 5, &change(ReviewAction::Approve, "Bob", 28))
            .expect("approve"),
    );
    assert_eq!(
        approved,
        Reviewed {
            id,
            status: Status::Live,
            revision: 6,
            promoted: false
        }
    );
    assert!(
        store.get(id, &Detail::default()).expect("get").is_some(),
        "agents see it now"
    );
    assert_eq!(
        store
            .set_review(id, 6, &change(ReviewAction::GoLive, "Ada", 29))
            .expect("live"),
        ReviewReply::WrongState(Status::Live)
    );
    assert_eq!(
        store
            .set_review(id, 6, &change(ReviewAction::Ask, "Ada", 29))
            .expect("live"),
        ReviewReply::WrongState(Status::Live)
    );

    // The history, newest first.
    let history = store.history(id).expect("history").expect("a history");
    let kinds: Vec<EventKind> = history.events.iter().map(|event| event.kind).collect();

    assert_eq!(
        kinds,
        [
            EventKind::Approved,
            EventKind::Asked,
            EventKind::ChangesRequested,
            EventKind::Asked,
            EventKind::Edited,
            EventKind::Asked,
            EventKind::Drafted,
        ]
    );
    assert_eq!(history.total, 7);
    assert_eq!(
        (
            history.events[2].by.as_deref(),
            history.events[2].comment.as_deref(),
            history.events[2].at
        ),
        (Some("Bob"), Some("needs an example"), 25)
    );
    assert!(store.history(987_654).expect("history").is_none());

    // A draft can go live without being asked.
    let quick = put(
        store,
        &Seed::new("Quick")
            .source("/docs/quick")
            .status(Status::Draft)
            .chunk("A", "quick text"),
    );
    let live = done(
        store
            .set_review(quick, 0, &change(ReviewAction::GoLive, "Ada", 30))
            .expect("go live"),
    );
    assert_eq!((live.status, live.promoted), (Status::Live, false));
    assert_eq!(
        store.history(quick).expect("history").expect("history").events[0].kind,
        EventKind::WentLive
    );

    // A comment is checked, and a pending version goes live through `promote` only.
    assert!(matches!(
        store.set_review(
            id,
            6,
            &ReviewChange::new(ReviewAction::Ask, Some("Ada"), 31).with_comment(&"x".repeat(501))
        ),
        Err(StoreError::Invalid(_))
    ));

    let pending = put(
        store,
        &Seed::new("Spec")
            .source("/docs/spec-v2")
            .pending_of(id)
            .chunk("Intro", "new text"),
    );
    ask(store, pending, "Ada", 32);
    let expect = revision(store, pending);

    assert!(matches!(
        store.set_review(pending, expect, &change(ReviewAction::Approve, "Bob", 33)),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store.promote(pending, expect, &change(ReviewAction::Ask, "Bob", 33)),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(
        store
            .promote(id, 6, &change(ReviewAction::Approve, "Bob", 33))
            .expect("promote a live document"),
        ReviewReply::WrongState(Status::Live)
    );

    let plain = put(
        store,
        &Seed::new("Plain")
            .source("/docs/plain")
            .status(Status::Draft)
            .chunk("A", "plain text"),
    );
    assert!(
        matches!(
            store.promote(plain, 0, &change(ReviewAction::GoLive, "Ada", 34)),
            Err(StoreError::Invalid(_))
        ),
        "only a pending version is promoted"
    );
}

/// A promote replaces the live document with its pending version in one step: the id, links,
/// `created_at`, `added_by` and counters stay, the chunks and their embeddings move, the events,
/// comments and original file follow, and the pending row is gone. Search finds the new text only
/// after it, and deleting a document takes its pending version, comments and files with it.
pub fn promote_contract(store: &mut dyn KnowledgeStore) {
    let live = put(
        store,
        &Seed::new("Gateway guide")
            .source("/docs/guide")
            .chunk("Intro", "legacy timeout words")
            .embedded("Usage", "legacy usage words", axis(0))
            .link(REPO, &["src/**"])
            .updated(1_000)
            .by("Ada", Some("claude-code")),
    );
    for _ in 0..3 {
        store.record_shown(&[live]).expect("shown");
    }
    store.record_useful(&[live], 77).expect("useful");
    assert!(store.put_file(live, "guide.md", b"legacy bytes").expect("file"));

    let pending = put(
        store,
        &Seed::new("Gateway guide v2")
            .source("/docs/guide-v2")
            .pending_of(live)
            .embedded("Intro", "modern latency words", axis(1))
            .embedded("Usage", "modern usage words", axis(2))
            .updated(2_000)
            .by("Bob", None),
    );
    assert!(store.put_file(pending, "guide-v2.md", b"modern bytes").expect("file"));

    // One pending version per live document.
    let (doc, chunks, links) = Seed::new("Gateway guide v3")
        .source("/docs/guide-v3")
        .pending_of(live)
        .chunk("Intro", "third")
        .parts();
    assert!(matches!(
        store.put_document(&doc, &chunks, &links),
        Err(StoreError::Invalid(_))
    ));

    store
        .add_comments(
            pending,
            Some("Cy"),
            5,
            0,
            &[
                NewComment::overall("looks good"),
                NewComment::on_lines("Intro", Side::New, 1, 1, "typo"),
            ],
        )
        .expect("comments");

    // The new text is not found before the promote, the old text is.
    let everything = Scope::everything();

    assert!(keyword(store, "modern", &everything.clone().with_drafts()).is_empty());
    assert_eq!(doc_ids(&keyword(store, "legacy", &everything)), [live]);

    // Only what waits for approval, at its revision, is made live.
    let expect = revision(store, pending);
    assert!(matches!(
        store
            .promote(pending, expect, &change(ReviewAction::Approve, "Cy", 40))
            .expect("promote"),
        ReviewReply::WrongState(Status::Draft)
    ));

    ask(store, pending, "Bob", 41);
    let expect = revision(store, pending);
    let before = store.generation().expect("generation");

    assert_eq!(
        store
            .promote(pending, expect - 1, &change(ReviewAction::Approve, "Cy", 42))
            .expect("stale"),
        ReviewReply::Changed { revision: expect }
    );
    assert_eq!(store.generation().expect("generation"), before);
    assert_eq!(
        doc_ids(&keyword(store, "legacy", &everything)),
        [live],
        "a refusal changes nothing"
    );

    let live_revision = revision(store, live);
    let reviewed = done(
        store
            .promote(
                pending,
                expect,
                &change(ReviewAction::Approve, "Cy", 42).with_comment("ship it"),
            )
            .expect("promote"),
    );

    assert_eq!(
        (reviewed.id, reviewed.status, reviewed.promoted),
        (live, Status::Live, true)
    );
    assert!(
        reviewed.revision > live_revision.max(expect),
        "the live revision passes both"
    );
    assert!(store.generation().expect("generation") > before);

    // The live document keeps what is its own and takes the new content.
    let now = read(store, live).expect("live");

    assert_eq!(now.doc.title, "Gateway guide v2");
    assert_eq!(
        now.doc.source_key, "/docs/guide",
        "the live document is still found by its source"
    );
    assert_eq!((now.doc.created_at, now.doc.updated_at), (1_000, 2_000));
    assert_eq!(
        now.doc.added_by.as_ref().map(|author| author.name.as_str()),
        Some("Ada")
    );
    assert_eq!(
        now.doc.updated_by.as_ref().map(|author| author.name.as_str()),
        Some("Bob")
    );
    assert_eq!(now.doc.chunk_count, 2);
    assert_eq!(now.doc.review_rev, reviewed.revision);
    assert_eq!(
        now.links
            .iter()
            .map(|link| (link.repo_key.as_str(), link.globs.clone()))
            .collect::<Vec<_>>(),
        [(REPO, vec!["src/**".to_owned()])]
    );
    assert_eq!(
        now.usage.map(|usage| (usage.shown, usage.useful, usage.last_useful_at)),
        Some((3, 1, Some(77)))
    );
    assert!(now.pending.is_none());

    assert_eq!(
        store.section_texts(live).expect("sections"),
        Some(sections(&[
            ("Intro", "modern latency words"),
            ("Usage", "modern usage words")
        ]))
    );

    // The pending row is gone, and search finds the new text under the live id and the old text no more.
    assert!(read(store, pending).is_none());
    assert!(store.pending_of(live).expect("pending of").is_none());

    let hits = keyword(store, "modern", &everything);
    assert_eq!(doc_ids(&hits), [live]);
    assert!(hits.iter().all(|hit| hit.title == "Gateway guide v2"));
    assert!(keyword(store, "legacy", &everything).is_empty());
    assert!(keyword(store, "legacy", &everything.clone().with_drafts()).is_empty());

    let hits = nearest(store, &axis(1), &everything);
    assert_eq!(
        hits.first().map(|hit| (hit.doc_id, hit.text.as_str())),
        Some((live, "modern latency words"))
    );
    assert!(
        hits.iter().all(|hit| hit.text.starts_with("modern")),
        "the old embeddings are gone: {hits:?}"
    );

    let stats = store.stats().expect("stats");
    assert_eq!((stats.documents, stats.pending, stats.chunks), (1, 0, 2));

    // The events and the original file moved to the live document, the comments are gone: the review is over.
    let history = store.history(live).expect("history").expect("history");
    assert_eq!(
        history.events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        [EventKind::Approved, EventKind::Asked, EventKind::UpdateDrafted]
    );
    assert_eq!(history.events[0].comment.as_deref(), Some("ship it"));
    assert!(store.history(pending).expect("history").is_none());

    assert!(store.comments(live).expect("comments").is_empty());
    assert!(store.comments(pending).expect("comments").is_empty());

    let file = store.file(live).expect("file").expect("the pending file");
    assert_eq!(
        (file.name.as_str(), file.bytes.as_slice()),
        ("guide-v2.md", b"modern bytes".as_slice())
    );
    assert!(store.file(pending).expect("file").is_none());

    // The one-pending rule is free again, and deleting the live document takes all of it.
    let second = put(
        store,
        &Seed::new("Gateway guide v3")
            .source("/docs/guide-v3")
            .pending_of(live)
            .chunk("Intro", "third words"),
    );
    assert!(store.put_file(second, "third.md", b"third").expect("file"));
    store
        .add_comments(second, Some("Cy"), 6, 0, &[NewComment::overall("hm")])
        .expect("comment");

    assert_eq!(store.delete(&[live]).expect("delete"), 1);

    for id in [live, second] {
        assert!(read(store, id).is_none(), "document {id} is gone");
        assert!(store.history(id).expect("history").is_none());
        assert!(store.comments(id).expect("comments").is_empty());
        assert!(store.file(id).expect("file").is_none());
    }

    let stats = store.stats().expect("stats");
    assert_eq!((stats.documents, stats.pending, stats.chunks), (0, 0, 0));
    assert!(keyword(store, "third", &everything.clone().with_drafts()).is_empty());
    assert!(keyword(store, "modern", &everything).is_empty());
}

/// Discarding a pending version (deleting it) leaves its live document as it was.
pub fn discarding_a_pending_version_contract(store: &mut dyn KnowledgeStore) {
    let live = put(
        store,
        &Seed::new("Guide").source("/docs/guide").chunk("A", "stable words"),
    );
    let pending = put(
        store,
        &Seed::new("Guide")
            .source("/docs/guide-2")
            .pending_of(live)
            .chunk("A", "draft words"),
    );

    assert_eq!(store.delete(&[pending]).expect("delete"), 1);
    assert!(store.pending_of(live).expect("pending of").is_none());
    assert_eq!(doc_ids(&keyword(store, "stable", &Scope::everything())), [live]);
    assert!(keyword(store, "draft", &Scope::everything().with_drafts()).is_empty());
    assert_eq!(store.stats().expect("stats").chunks, 1);
}

/// Taking an ask back is one compare-and-set like the other transitions: a waiting row becomes a
/// draft at the next revision, the history records `withdrawn`, and anything but a waiting row, or a
/// stale revision, is left alone.
pub fn take_back_contract(store: &mut dyn KnowledgeStore) {
    let id = put(
        store,
        &Seed::new("Spec")
            .source("/docs/spec")
            .status(Status::Draft)
            .by("Ada", None)
            .chunk("Intro", "alpha words"),
    );

    // Only what waits can be taken back.
    let expect = revision(store, id);
    assert!(matches!(
        store
            .set_review(id, expect, &change(ReviewAction::TakeBack, "Ada", 5))
            .expect("take back"),
        ReviewReply::WrongState(Status::Draft)
    ));

    let asked = ask(store, id, "Ada", 10);
    let generation = store.generation().expect("generation");

    assert!(matches!(
        store
            .set_review(id, asked.revision - 1, &change(ReviewAction::TakeBack, "Ada", 11))
            .expect("stale"),
        ReviewReply::Changed { revision } if revision == asked.revision
    ));
    assert_eq!(
        store.generation().expect("generation"),
        generation,
        "a refusal is no write"
    );

    let back = done(
        store
            .set_review(id, asked.revision, &change(ReviewAction::TakeBack, "Ada", 12))
            .expect("take back"),
    );
    assert_eq!(
        (back.status, back.revision, back.promoted),
        (Status::Draft, asked.revision + 1, false)
    );
    assert!(store.generation().expect("generation") > generation);

    let row = read(store, id).expect("row").doc;
    assert_eq!((row.status, row.review_rev), (Status::Draft, asked.revision + 1));

    let last = &store.history(id).expect("history").expect("a history").events[0];
    assert_eq!(
        (last.kind, last.by.as_deref(), last.at),
        (EventKind::Withdrawn, Some("Ada"), 12)
    );

    // Agents still see nothing, and it can be asked again.
    assert!(keyword(store, "alpha", &Scope::everything()).is_empty());
    assert_eq!(ask(store, id, "Ada", 13).status, Status::Waiting);
}

/// Several meta keys are set together or not at all, and bump the generation once.
pub fn set_metas_contract(store: &mut dyn KnowledgeStore) {
    let generation = store.generation().expect("generation");

    store
        .set_metas(&[("team_approval", "on"), ("team_approval_by", "Ada")])
        .expect("set");
    assert_eq!(store.meta("team_approval").expect("meta").as_deref(), Some("on"));
    assert_eq!(store.meta("team_approval_by").expect("meta").as_deref(), Some("Ada"));
    assert!(store.generation().expect("generation") > generation);

    // One key the store keeps for itself refuses all of them.
    let generation = store.generation().expect("generation");
    assert!(matches!(
        store.set_metas(&[("team_approval", "off"), ("generation", "9")]),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(store.meta("team_approval").expect("meta").as_deref(), Some("on"));
    assert_eq!(store.generation().expect("generation"), generation);
}

/// A rename is a change like any other for a row that is not live: it takes the next revision (so
/// an approval of the revision that was asked is refused as `changed`), sends a waiting row back to
/// draft and records an `edited` event. A live document is renamed directly, and so is its pending
/// version, which the rename changes the same way, so a promote keeps the new title.
pub fn rename_is_a_change_contract(store: &mut dyn KnowledgeStore) {
    // A waiting row.
    let id = put(
        store,
        &Seed::new("Spec")
            .source("/docs/spec")
            .status(Status::Draft)
            .by("Ada", None)
            .chunk("Intro", "alpha words"),
    );
    let asked = ask(store, id, "Ada", 10);
    assert_eq!(asked.status, Status::Waiting);
    let generation = store.generation().expect("generation");

    assert!(store.rename(id, "  Spec\nrenamed ", Some("Bob"), 20).expect("rename"));
    assert!(store.generation().expect("generation") > generation);

    let row = read(store, id).expect("row").doc;
    assert_eq!(row.title, "Spec renamed");
    assert_eq!((row.status, row.review_rev), (Status::Draft, asked.revision + 1));

    // The renamer is on the `edited` event: who wrote the content is kept as it was.
    assert_eq!(
        (row.updated_by.map(|author| (author.name, author.via)), row.updated_at),
        (Some(("Ada".to_owned(), None)), 20)
    );

    let history = store.history(id).expect("history").expect("a history");
    let last = &history.events[0];
    assert_eq!(
        (last.kind, last.by.as_deref(), last.comment.as_deref(), last.at),
        (EventKind::Edited, Some("Bob"), Some("renamed"), 20)
    );

    // What was asked is not what is there: an approval of the asked revision is refused.
    assert!(matches!(
        store
            .set_review(id, asked.revision, &change(ReviewAction::Approve, "Cy", 30))
            .expect("approve"),
        ReviewReply::Changed { revision } if revision == asked.revision + 1
    ));

    // The same title again changes nothing.
    let before = revision(store, id);
    let events = store.history(id).expect("history").expect("a history").total;
    assert!(store.rename(id, "Spec renamed", Some("Cy"), 21).expect("same"));
    assert_eq!(revision(store, id), before);
    let unchanged = read(store, id).expect("row").doc;
    assert_eq!(
        (unchanged.updated_by.map(|author| author.name), unchanged.updated_at),
        (Some("Ada".to_owned()), 20),
        "a title that already is changes nobody's authorship"
    );
    assert_eq!(store.history(id).expect("history").expect("a history").total, events);

    // A row with requested changes stays so, at the next revision.
    ask(store, id, "Ada", 40);
    let expect = revision(store, id);
    done(
        store
            .set_review(
                id,
                expect,
                &change(ReviewAction::RequestChanges, "Bob", 41).with_comment("rework"),
            )
            .expect("request changes"),
    );
    let expect = revision(store, id);
    assert!(store.rename(id, "Spec again", Some("Ada"), 42).expect("rename"));
    let row = read(store, id).expect("row").doc;
    assert_eq!((row.status, row.review_rev), (Status::ChangesRequested, expect + 1));

    assert!(!store.rename(987_654, "Nobody", None, 1).expect("missing"));

    // A live document with a pending version.
    let live = put(
        store,
        &Seed::new("Guide")
            .source("/docs/guide")
            .by("Zoe", None)
            .chunk("Intro", "legacy words"),
    );
    let pending = put(
        store,
        &Seed::new("Guide")
            .source("/docs/guide-2")
            .pending_of(live)
            .by("Ada", None)
            .chunk("Intro", "modern words"),
    );
    let live_before = revision(store, live);
    let asked = ask(store, pending, "Ada", 50);

    assert!(store.rename(live, "Guide 2", Some("Bob"), 60).expect("rename"));

    let renamed = read(store, live).expect("live").doc;
    assert_eq!(
        (renamed.title.as_str(), renamed.status, renamed.review_rev),
        ("Guide 2", Status::Live, live_before),
        "a live document is renamed directly"
    );
    assert_eq!(
        (renamed.updated_by.map(|author| author.name), renamed.updated_at),
        (Some("Zoe".to_owned()), 60),
        "and who wrote it is kept"
    );
    assert_eq!(
        store.history(live).expect("history").expect("a history").total,
        0,
        "a live row records no event"
    );

    let pending_row = read(store, pending).expect("pending").doc;
    assert_eq!(
        (pending_row.title.as_str(), pending_row.status, pending_row.review_rev),
        ("Guide 2", Status::Draft, asked.revision + 1),
        "the pending version takes the rename as a change"
    );
    assert_eq!(
        (pending_row.updated_by.map(|author| author.name), pending_row.updated_at),
        (Some("Ada".to_owned()), 60),
        "the pending version keeps who wrote it, the rename is on the event"
    );
    let last = &store.history(pending).expect("history").expect("a history").events[0];
    assert_eq!(
        (last.kind, last.by.as_deref(), last.comment.as_deref(), last.at),
        (EventKind::Edited, Some("Bob"), Some("renamed"), 60)
    );
    assert!(matches!(
        store
            .promote(pending, asked.revision, &change(ReviewAction::Approve, "Cy", 61))
            .expect("promote"),
        ReviewReply::Changed { .. }
    ));

    // Renaming the pending version itself bumps its revision.
    let expect = revision(store, pending);
    assert!(store.rename(pending, "Guide Zephyr", Some("Bob"), 62).expect("rename"));
    assert_eq!(revision(store, pending), expect + 1);

    // The title survives the promote.
    ask(store, pending, "Bob", 70);
    let expect = revision(store, pending);
    let promoted = done(
        store
            .promote(pending, expect, &change(ReviewAction::Approve, "Cy", 71))
            .expect("promote"),
    );
    assert_eq!(promoted.id, live);
    assert_eq!(read(store, live).expect("live").doc.title, "Guide Zephyr");
    assert_eq!(
        doc_ids(&keyword(store, "zephyr", &Scope::everything())),
        [live],
        "the keyword index finds the new title"
    );
}

/// Who may decide on a version: everyone who changed its content since it was last live (a
/// rename too) or asked for its approval is an author of it, whoever the row says wrote it last.
pub fn authorship_contract(store: &mut dyn KnowledgeStore) {
    store.set_meta(APPROVAL_KEY, "on").expect("approval");

    let code = |result: Result<Reviewed, Error>| match result {
        Err(Error::Refused(refusal)) => refusal.code,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let review = |store: &mut dyn KnowledgeStore, name: &str, id: i64, verb: &str| {
        let expect = revision(store, id);
        let mut review = Review::new(store, Place::Remote, Some(name));

        match verb {
            "ask" => review.ask(id, expect, None),
            "approve" => review.approve(id, expect),
            _ => review.take_back(id, expect),
        }
    };

    // Ada drafts, Bob renames, Bob asks.
    let id = put(
        store,
        &Seed::new("Spec")
            .source("/docs/spec")
            .status(Status::Draft)
            .by("Ada", Some("agent"))
            .chunk("Intro", "alpha words"),
    );
    assert!(store.rename(id, "Spec renamed", Some("Bob"), 20).expect("rename"));
    review(store, "Bob", id, "ask").expect("ask");

    let row = read(store, id).expect("row").doc;
    assert_eq!(
        row.updated_by.map(|author| (author.name, author.via)),
        Some(("Ada".to_owned(), Some("agent".to_owned()))),
        "the rename leaves who wrote the content, and through whom"
    );

    // Neither of the two may decide, nor is it waiting for either.
    assert_eq!(waiting_for(store, Some("Ada")).expect("count"), 0);
    assert_eq!(waiting_for(store, Some("Bob")).expect("count"), 0);
    assert_eq!(waiting_for(store, Some("Carol")).expect("count"), 1);
    assert_eq!(code(review(store, "Ada", id, "approve")), Code::SelfApproval);
    assert_eq!(code(review(store, "Bob", id, "approve")), Code::SelfApproval);

    // Both may take the ask back, someone else may not.
    assert_eq!(code(review(store, "Carol", id, "take")), Code::NotAuthor);
    assert_eq!(
        review(store, "Ada", id, "take").expect("take back").status,
        Status::Draft
    );
    review(store, "Bob", id, "ask").expect("ask");

    assert_eq!(
        review(store, "Carol", id, "approve").expect("approve").status,
        Status::Live
    );

    // A pending version of a live document, renamed through the live one.
    let live = id;
    let pending = put(
        store,
        &Seed::new("Spec renamed")
            .source("/docs/spec-2")
            .pending_of(live)
            .by("Ada", None)
            .chunk("Intro", "modern words"),
    );
    assert!(store.rename(live, "Spec 2", Some("Bob"), 60).expect("rename"));
    review(store, "Bob", pending, "ask").expect("ask");

    assert_eq!(waiting_for(store, Some("Ada")).expect("count"), 0);
    assert_eq!(waiting_for(store, Some("Bob")).expect("count"), 0);
    assert_eq!(waiting_for(store, Some("Carol")).expect("count"), 1);
    assert_eq!(code(review(store, "Ada", pending, "approve")), Code::SelfApproval);
    assert_eq!(code(review(store, "Bob", pending, "approve")), Code::SelfApproval);

    let promoted = review(store, "Carol", pending, "approve").expect("approve");
    assert_eq!((promoted.id, promoted.status), (live, Status::Live));
    assert_eq!(read(store, live).expect("live").doc.title, "Spec 2");

    // Once live, the next version starts afresh: Bob, who only renamed before, wrote nothing of it.
    let next = put(
        store,
        &Seed::new("Spec 2")
            .source("/docs/spec-3")
            .pending_of(live)
            .by("Dan", None)
            .chunk("Intro", "newer words"),
    );
    review(store, "Dan", next, "ask").expect("ask");
    assert_eq!(code(review(store, "Dan", next, "approve")), Code::SelfApproval);
    assert_eq!(
        review(store, "Ada", next, "approve").expect("approve").status,
        Status::Live
    );
}

/// A writer cannot push their own change out of sight: whoever renamed a version still is an author
/// of it after more events than a history lists, and the names of its writers come from every event
/// since it was last live.
pub fn authorship_window_contract(store: &mut dyn KnowledgeStore) {
    store.set_meta(APPROVAL_KEY, "on").expect("approval");

    let step = |store: &mut dyn KnowledgeStore, name: &str, id: i64, verb: &str| {
        let expect = revision(store, id);
        let mut review = Review::new(store, Place::Remote, Some(name));

        match verb {
            "ask" => review.ask(id, expect, None),
            "approve" => review.approve(id, expect),
            _ => review.take_back(id, expect),
        }
    };

    // Ada drafts, Bob renames, then Ada asks and takes the ask back over and over.
    let id = put(
        store,
        &Seed::new("Spec")
            .source("/docs/spec")
            .status(Status::Draft)
            .by("Ada", None)
            .chunk("Intro", "alpha words"),
    );
    assert!(store.rename(id, "Spec renamed", Some("Bob"), 20).expect("rename"));

    for _ in 0..=HISTORY_MAX / 2 {
        step(store, "Ada", id, "ask").expect("ask");
        step(store, "Ada", id, "take").expect("take back");
    }

    // Carol asks. The rename lies further back than a history shows.
    step(store, "Carol", id, "ask").expect("ask");

    let history = store.history(id).expect("history").expect("document");
    assert!(history.total > HISTORY_MAX, "more events than a history lists");
    assert!(
        history
            .events
            .iter()
            .all(|event| !matches!(event.kind, EventKind::Drafted | EventKind::Edited)),
        "the writes are out of the listed events"
    );

    let mut writers = store.writers_since_live(id).expect("writers");
    writers.sort();
    assert_eq!(writers, ["Ada", "Bob"]);

    for name in ["Ada", "Bob", "Carol"] {
        assert_eq!(waiting_for(store, Some(name)).expect("count"), 0, "{name}");
    }
    assert_eq!(waiting_for(store, Some("Dan")).expect("count"), 1);

    match step(store, "Bob", id, "approve") {
        Err(Error::Refused(refusal)) => assert_eq!(refusal.code, Code::SelfApproval),
        other => panic!("the renamer approved their own change: {other:?}"),
    }

    assert_eq!(step(store, "Dan", id, "approve").expect("approve").status, Status::Live);
    assert_eq!(
        store.writers_since_live(id).expect("writers"),
        Vec::<String>::new(),
        "a live row has no writers since it was live"
    );
}

/// The writers and the latest asker of many documents are read together and are what one-by-one reads
/// give (`writers_since_live`, the newest `asked` event since the version was last live), the writers
/// cut at `per_doc`.
pub fn authorship_of_many_contract(store: &mut dyn KnowledgeStore) {
    let draft = |store: &mut dyn KnowledgeStore, title: &str| {
        put(
            store,
            &Seed::new(title)
                .source(&format!("/docs/{title}"))
                .status(Status::Draft)
                .by("Ada", None)
                .chunk("Intro", "alpha"),
        )
    };

    // Written by Ada, renamed by Cy, asked by Bob.
    let asked_one = draft(store, "asked");
    assert!(store.rename(asked_one, "asked again", Some("Cy"), 20).expect("rename"));
    ask(store, asked_one, "Bob", 30);

    // Written by Ada and asked by nobody.
    let quiet = draft(store, "quiet");

    // Asked by Bob, sent back, asked by Dee: the latest asker.
    let twice = draft(store, "twice");
    ask(store, twice, "Bob", 31);
    let expect = revision(store, twice);
    done(
        store
            .set_review(twice, expect, &change(ReviewAction::RequestChanges, "Eve", 32))
            .expect("request changes"),
    );
    ask(store, twice, "Dee", 33);

    // Asked and approved: the version is live, nobody is left to name.
    let approved = draft(store, "approved");
    ask(store, approved, "Bob", 34);
    let expect = revision(store, approved);
    done(
        store
            .set_review(approved, expect, &change(ReviewAction::Approve, "Eve", 35))
            .expect("approve"),
    );

    // Many writers: only the first few are read.
    let crowded = draft(store, "crowded");
    for number in 0..30 {
        assert!(
            store
                .rename(
                    crowded,
                    &format!("crowded {number}"),
                    Some(&format!("Writer {number:02}")),
                    40 + number
                )
                .expect("rename")
        );
    }

    let ids = [asked_one, quiet, twice, approved, crowded, 987_654];
    let many = store.authorship_of(&ids, 5).expect("authorship");

    for id in [asked_one, quiet, twice, approved] {
        let each = Authorship {
            writers: store.writers_since_live(id).expect("writers"),
            asker: crate::store::review::latest_asker(&store.history(id).expect("history").expect("a history")),
        };

        assert_eq!(many.get(&id), Some(&each), "document {id}");
    }

    assert_eq!(many[&asked_one].asker.as_deref(), Some("Bob"));
    assert_eq!(many[&twice].asker.as_deref(), Some("Dee"));
    assert_eq!(many[&quiet].asker, None);
    assert_eq!(
        (many[&approved].writers.len(), many[&approved].asker.clone()),
        (0, None)
    );

    let crowd = &many[&crowded].writers;
    assert_eq!(crowd.len(), 5);
    assert_eq!(crowd[0], "Ada");
    assert_eq!(crowd[1], "Writer 00", "oldest first");

    assert!(store.authorship_of(&[], 5).expect("nothing").is_empty());
}

/// A document with a crowd of writers does not use up what the other documents of the page are given:
/// each gets its own first `per_doc` writers.
pub fn authorship_crowd_contract(store: &mut dyn KnowledgeStore) {
    let crowded = put(
        store,
        &Seed::new("crowded")
            .source("/docs/crowded")
            .status(Status::Draft)
            .by("Ada", None)
            .chunk("Intro", "alpha"),
    );
    let small = put(
        store,
        &Seed::new("small")
            .source("/docs/small")
            .status(Status::Draft)
            .by("Bea", None)
            .chunk("Intro", "beta"),
    );
    assert!(store.rename(small, "small renamed", Some("Cy"), 20).expect("rename"));

    for number in 0..140_i64 {
        assert!(
            store
                .rename(
                    crowded,
                    &format!("crowded {number}"),
                    Some(&format!("Writer {number:03}")),
                    30 + number
                )
                .expect("rename")
        );
    }

    let per_doc = crate::store::review::AUTHORS_FETCH_MAX;
    let many = store.authorship_of(&[crowded, small], per_doc).expect("authorship");

    assert_eq!(many[&crowded].writers.len(), per_doc);
    assert_eq!(many[&crowded].writers[0], "Ada");
    assert_eq!(many[&small].writers, ["Bea", "Cy"], "the crowd took nothing from it");
}

/// A team database keeps no folder of this computer: a document linked to a `path:` repository, new
/// or by `set_links`, is refused as [`StoreError::LocalPath`] before anything is written, and the
/// message names the folder and never its path. A `git:` link is kept.
pub fn local_paths_refused_contract(store: &mut dyn KnowledgeStore) {
    let key = "path:/home/secret-owner/projects/ledger";
    let before = store.stats().expect("stats");

    for seed in [
        Seed::new("Only path").source("/docs/only").link(key, &["src/**"]),
        Seed::new("Mixed").source("/docs/mixed").link(REPO, &[]).link(key, &[]),
    ] {
        let (doc, chunks, links) = seed.chunk("Intro", "alpha").parts();

        match store.put_document(&doc, &chunks, &links) {
            Err(StoreError::LocalPath(message)) => {
                assert!(message.starts_with("ledger has no git remote"), "{message}");
                assert!(!message.contains("secret-owner") && !message.contains('/'), "{message}");
            }
            other => panic!("expected the folder to be refused, got {other:?}"),
        }
    }

    let after = store.stats().expect("stats");
    assert_eq!(
        (after.documents, after.chunks, after.generation),
        (before.documents, before.chunks, before.generation),
        "a refused write leaves nothing behind"
    );

    // A document with a git link is kept, and a later set of links to a folder is refused.
    let id = put(
        store,
        &Seed::new("Linked")
            .source("/docs/linked")
            .link(REPO, &["src/**"])
            .chunk("Intro", "beta"),
    );
    let generation = store.stats().expect("stats").generation;

    assert!(matches!(
        store.set_links(
            id,
            &[crate::store::Link {
                repo_key: key.to_owned(),
                globs: Vec::new(),
            }]
        ),
        Err(StoreError::LocalPath(_))
    ));
    assert_eq!(store.stats().expect("stats").generation, generation);
    assert_eq!(
        read(store, id)
            .expect("the document")
            .links
            .iter()
            .map(|link| link.repo_key.as_str())
            .collect::<Vec<_>>(),
        [REPO]
    );

    assert!(
        store
            .set_links(
                id,
                &[crate::store::Link {
                    repo_key: "git:h/o/other".to_owned(),
                    globs: Vec::new(),
                }]
            )
            .expect("a git link")
    );
}

/// A team database is never asked for a folder of this computer: a search scoped to one finds the
/// global documents (and none that are linked to a repository), and a list or a title lookup of one
/// finds nothing, as before.
pub fn local_paths_read_contract(store: &mut dyn KnowledgeStore) {
    let key = "path:/home/secret-owner/projects/ledger";
    let global = put(
        store,
        &Seed::new("Team global")
            .source("/docs/global")
            .chunk("Intro", "gateway timeout settings"),
    );
    put(
        store,
        &Seed::new("Team linked")
            .source("/docs/linked")
            .link(REPO, &[])
            .chunk("Intro", "gateway retry budget"),
    );

    let scope = Scope::from_repo(Some(ProjectKey {
        key: key.to_owned(),
        root: PathBuf::new(),
        remote: None,
    }));

    assert_eq!(doc_ids(&keyword(store, "gateway", &scope)), [global]);

    let mut query = ListQuery {
        filter: RepoFilter::Repo(key.to_owned()),
        format: None,
        text: String::new(),
        useful: None,
        author: None,
        want_authors: false,
        status: StatusFilter::All,
        sort: Sort::Updated,
        descending: false,
        offset: 0,
        limit: 50,
    };
    let page = store.list(&query).expect("list");
    assert_eq!((page.items.len(), page.total), (0, 0));

    query.filter = RepoFilter::All;
    assert_eq!(store.list(&query).expect("list").total, 2);

    assert!(
        store
            .find_by_title("Team global", &TitleScope::Repo(key.to_owned()), Visibility::Live)
            .expect("titles")
            .is_empty()
    );

    // A source key built from a folder is not looked up, whatever it is; a team one is.
    assert!(
        store
            .find_by_source_key("path-0123456789ab/ledger/notes.md", Visibility::Live)
            .expect("lookup")
            .is_none()
    );
    assert!(
        store
            .find_by_source_key("/docs/global", Visibility::Live)
            .expect("lookup")
            .is_some()
    );
}

/// A pending version is promoted onto a live document only. A store does not keep a pending version
/// of a row that is not live, so a row that stopped being live under one (`not_live` does that below
/// the store, as leftovers of an earlier version could) refuses the promote and changes nothing.
pub fn promote_needs_a_live_target_contract(store: &mut dyn KnowledgeStore, not_live: &mut dyn FnMut(i64)) {
    let target = put(
        store,
        &Seed::new("Target").source("/docs/target").chunk("Intro", "alpha"),
    );
    let pending = put(
        store,
        &Seed::new("Update")
            .source("/docs/update")
            .pending_of(target)
            .by("Ada", None)
            .chunk("Intro", "beta"),
    );
    ask(store, pending, "Ada", 40);

    // A pending version of a draft is not stored at all.
    let draft = put(
        store,
        &Seed::new("Draft")
            .source("/docs/draft")
            .status(Status::Draft)
            .by("Ada", None)
            .chunk("Intro", "gamma"),
    );
    let (doc, chunks, links) = Seed::new("Update of a draft")
        .source("/docs/draft-update")
        .pending_of(draft)
        .by("Ada", None)
        .chunk("Intro", "delta")
        .parts();
    assert!(matches!(
        store.put_document(&doc, &chunks, &links),
        Err(StoreError::Invalid(_))
    ));

    not_live(target);

    let expect = revision(store, pending);
    let result = store.promote(pending, expect, &change(ReviewAction::Approve, "Bob", 50));

    assert!(matches!(result, Err(StoreError::Invalid(_))), "{result:?}");
    assert_eq!(read(store, target).expect("the target").doc.status, Status::Draft);
    assert_eq!(
        read(store, pending).expect("still pending").doc.pending_of,
        Some(target)
    );
}

/// The documents of the cleanup of folder links: one of every state a document can be in.
pub struct FolderCases {
    pub draft: i64,
    pub waiting: i64,
    pub changes_requested: i64,
    pub live: i64,
    /// A live document and its pending update.
    pub live_with_pending: (i64, i64),
    /// A live document that also has a git link.
    pub kept: i64,
}

impl FolderCases {
    /// The documents a `path:` link is seeded on (the pending update has no links of its own).
    pub fn linked(&self) -> [i64; 6] {
        [
            self.draft,
            self.waiting,
            self.changes_requested,
            self.live,
            self.live_with_pending.0,
            self.kept,
        ]
    }
}

/// Stores one document of every state, none with a link. The caller then seeds the `path:` links
/// below the store, as an earlier version would have left them.
pub fn folder_cases(store: &mut dyn KnowledgeStore) -> FolderCases {
    let draft = |store: &mut dyn KnowledgeStore, title: &str| {
        put(
            store,
            &Seed::new(title)
                .source(&format!("/docs/{title}"))
                .status(Status::Draft)
                .by("Ada", None)
                .chunk("Intro", "alpha"),
        )
    };

    let plain_draft = draft(store, "draft");

    let waiting = draft(store, "waiting");
    ask(store, waiting, "Ada", 10);

    let changes_requested = draft(store, "changes");
    ask(store, changes_requested, "Ada", 11);
    let expect = revision(store, changes_requested);
    done(
        store
            .set_review(
                changes_requested,
                expect,
                &change(ReviewAction::RequestChanges, "Bob", 12),
            )
            .expect("request changes"),
    );

    let live = put(store, &Seed::new("live").source("/docs/live").chunk("Intro", "beta"));

    let with_pending = put(
        store,
        &Seed::new("pending").source("/docs/pending").chunk("Intro", "gamma"),
    );
    let pending = put(
        store,
        &Seed::new("pending v2")
            .source("/docs/pending-2")
            .pending_of(with_pending)
            .by("Ada", None)
            .chunk("Intro", "delta"),
    );

    let kept = put(store, &Seed::new("kept").source("/docs/kept").chunk("Intro", "epsilon"));
    assert!(
        store
            .set_links(
                kept,
                &[crate::store::Link {
                    repo_key: REPO.to_owned(),
                    globs: Vec::new(),
                }],
            )
            .expect("a git link")
    );

    FolderCases {
        draft: plain_draft,
        waiting,
        changes_requested,
        live,
        live_with_pending: (with_pending, pending),
        kept,
    }
}

/// The revision of each document the cleanup may change, read before it runs.
pub fn folder_revisions(store: &mut dyn KnowledgeStore, cases: &FolderCases) -> Vec<(i64, i64)> {
    cases.linked().iter().map(|id| (*id, revision(store, *id))).collect()
}

/// What the cleanup of the folder links left, read after a writer opened the database: a document
/// no link is left on is a draft that is looked at again, with an `edited` event saying why, and a
/// pending update of a live one is gone with its chunks. A document that keeps a git link is as it was.
pub fn folder_cleanup_check(store: &mut dyn KnowledgeStore, cases: &FolderCases, before: &[(i64, i64)]) {
    let revision_before = |id: i64| {
        before
            .iter()
            .find(|(doc, _)| *doc == id)
            .map(|(_, rev)| *rev)
            .expect("seen")
    };
    let newest = |store: &mut dyn KnowledgeStore, id: i64| {
        let event = store.history(id).expect("history").expect("a history").events.remove(0);

        (event.kind, event.comment)
    };

    for (id, bumped) in [
        (cases.draft, false),
        (cases.waiting, true),
        (cases.changes_requested, true),
        (cases.live, true),
        (cases.live_with_pending.0, true),
    ] {
        let row = read(store, id).expect("the document").doc;

        assert_eq!(row.status, Status::Draft, "document {id}");
        assert_eq!(
            row.review_rev,
            revision_before(id) + i64::from(bumped),
            "document {id}: only a document that was not a draft has a new revision"
        );

        let wanted = if id == cases.live_with_pending.0 {
            "folder link removed, its pending update was dropped"
        } else {
            "folder link removed"
        };
        assert_eq!(
            newest(store, id),
            (EventKind::Edited, Some(wanted.to_owned())),
            "document {id}"
        );
    }

    // Out of the agents' reach, and no pending update points at a draft.
    for id in [cases.waiting, cases.live, cases.live_with_pending.0] {
        assert!(
            store.get(id, &Detail::default()).expect("get").is_none(),
            "document {id}"
        );
    }
    assert!(
        read(store, cases.live_with_pending.1).is_none(),
        "the pending update went"
    );
    assert!(
        store
            .keyword_top_k("delta", &Scope::everything().with_drafts(), 10)
            .expect("search")
            .is_empty()
    );

    let kept = read(store, cases.kept).expect("the document");
    assert_eq!(
        (kept.doc.status, kept.doc.review_rev),
        (Status::Live, revision_before(cases.kept))
    );
    assert_eq!(
        kept.links.iter().map(|link| link.repo_key.as_str()).collect::<Vec<_>>(),
        [REPO]
    );
    assert!(
        store.history(cases.kept).expect("history").is_none_or(|history| history
            .events
            .iter()
            .all(|event| event.comment.as_deref() != Some("folder link removed"))),
        "nothing happened to it"
    );
}

/// This computer's own database keeps `path:` links as it always did.
pub fn local_paths_kept_contract(store: &mut dyn KnowledgeStore) {
    let key = "path:/home/me/project";
    let id = put(
        store,
        &Seed::new("Local")
            .source("/docs/local")
            .link(key, &["src/**"])
            .chunk("Intro", "alpha"),
    );

    let links = read(store, id).expect("the document").links;
    assert_eq!(
        links.iter().map(|link| link.repo_key.as_str()).collect::<Vec<_>>(),
        [key]
    );

    assert!(
        store
            .set_links(
                id,
                &[crate::store::Link {
                    repo_key: "path:/home/me/other".to_owned(),
                    globs: Vec::new(),
                }]
            )
            .expect("set links")
    );
    assert_eq!(
        read(store, id).expect("the document").links[0].repo_key,
        "path:/home/me/other"
    );
}

/// A version that goes live has had its review: its comments, replies included, are removed with
/// the change of state, whether it is a waiting document, a draft made live without approval or a
/// pending version promoted onto its live document. The history keeps its record.
pub fn going_live_removes_the_comments_contract(store: &mut dyn KnowledgeStore) {
    let kept_threads = |store: &mut dyn KnowledgeStore, id: i64| -> i64 {
        let at = revision(store, id);
        let first = store
            .add_comments(
                id,
                Some("Bob"),
                100,
                at,
                &[
                    NewComment::overall("overall"),
                    NewComment::on_lines("Intro", Side::New, 1, 1, "line"),
                ],
            )
            .expect("review");
        store
            .reply_comment(id, first[1], Some("Ada"), 101, "answer")
            .expect("reply")
            .expect("a reply");

        first[0]
    };

    // A waiting document, approved.
    let waiting = put(
        store,
        &Seed::new("Spec")
            .source("/docs/spec")
            .status(Status::Draft)
            .chunk("Intro", "alpha"),
    );
    kept_threads(store, waiting);
    ask(store, waiting, "Ada", 110);
    assert_eq!(store.comments(waiting).expect("comments").len(), 3);
    let expect = revision(store, waiting);
    done(
        store
            .set_review(waiting, expect, &change(ReviewAction::Approve, "Bob", 120))
            .expect("approve"),
    );
    assert!(store.comments(waiting).expect("comments").is_empty());

    // A draft made live without approval.
    let draft = put(
        store,
        &Seed::new("Notes")
            .source("/docs/notes")
            .status(Status::Draft)
            .chunk("Intro", "beta"),
    );
    kept_threads(store, draft);
    let expect = revision(store, draft);
    done(
        store
            .set_review(draft, expect, &change(ReviewAction::GoLive, "Ada", 130))
            .expect("go live"),
    );
    assert!(store.comments(draft).expect("comments").is_empty());

    // Another document keeps its own comments.
    let other = put(
        store,
        &Seed::new("Other")
            .source("/docs/other")
            .status(Status::Draft)
            .chunk("Intro", "gamma"),
    );
    kept_threads(store, other);

    // A pending version promoted onto its live document: the comments of both go.
    let live = put(store, &Seed::new("Guide").source("/docs/guide").chunk("Intro", "old"));
    kept_threads(store, live);
    let pending = put(
        store,
        &Seed::new("Guide v2")
            .source("/docs/guide-v2")
            .pending_of(live)
            .by("Bob", None)
            .chunk("Intro", "new"),
    );
    kept_threads(store, pending);
    ask(store, pending, "Bob", 140);
    let expect = revision(store, pending);
    let promoted = done(
        store
            .promote(pending, expect, &change(ReviewAction::Approve, "Cy", 150))
            .expect("promote"),
    );
    assert_eq!(promoted.id, live);
    assert!(store.comments(live).expect("comments").is_empty());
    assert!(store.comments(pending).expect("comments").is_empty());

    // The history is untouched by it.
    let kinds = |store: &mut dyn KnowledgeStore, id: i64| -> Vec<EventKind> {
        store
            .history(id)
            .expect("history")
            .expect("a history")
            .events
            .iter()
            .map(|event| event.kind)
            .collect()
    };
    assert_eq!(
        kinds(store, waiting),
        [EventKind::Approved, EventKind::Asked, EventKind::Drafted]
    );
    assert_eq!(kinds(store, draft), [EventKind::WentLive, EventKind::Drafted]);
    assert_eq!(
        kinds(store, live),
        [EventKind::Approved, EventKind::Asked, EventKind::UpdateDrafted]
    );
    assert_eq!(
        store.comments(other).expect("comments").len(),
        3,
        "another document keeps its own"
    );
}

/// A line comment keeps the lines it was written on, as given and no more than the caps allow;
/// an overall comment, a reply and a comment with none read as having none.
pub fn quotes_contract(store: &mut dyn KnowledgeStore) {
    let id = put(
        store,
        &Seed::new("Spec")
            .source("/docs/spec")
            .status(Status::Draft)
            .chunk("Intro", "alpha"),
    );
    let before = revision(store, id);
    let line = |line: u32, kind: QuoteKind, text: &str| QuoteLine {
        line,
        kind,
        text: text.to_owned(),
        cut: false,
    };
    let quote = vec![
        line(2, QuoteKind::Same, "żółć, a line with accents"),
        line(3, QuoteKind::Added, "  indented \"quoted\" text"),
        line(4, QuoteKind::Removed, ""),
    ];

    // Many long lines, and one longer than a quote can be.
    let long: Vec<QuoteLine> = (1..=30)
        .map(|at| line(at, QuoteKind::Added, &"ł".repeat(150)))
        .collect();
    let huge = vec![line(1, QuoteKind::Added, &"ą".repeat(5000))];

    let added = store
        .add_comments(
            id,
            Some("Bob"),
            100,
            before,
            &[
                NewComment::overall("an overall remark"),
                NewComment::on_lines("Intro", Side::New, 2, 4, "with lines").with_quote(quote.clone()),
                NewComment::on_lines("Intro", Side::Old, 1, 1, "written before quotes"),
                NewComment::overall("an overall remark").with_quote(quote.clone()),
                NewComment::on_lines("Intro", Side::New, 1, 30, "a long range").with_quote(long),
                NewComment::on_lines("Intro", Side::New, 1, 1, "a long line").with_quote(huge),
                NewComment::on_lines("Intro", Side::New, 1, 1, "an empty quote").with_quote(Vec::new()),
            ],
        )
        .expect("review");
    let reply = store
        .reply_comment(id, added[1], Some("Ada"), 110, "thanks")
        .expect("reply")
        .expect("a reply");

    let all = store.comments(id).expect("comments");
    let quote_of = |comment: i64| {
        all.iter()
            .find(|stored| stored.id == comment)
            .expect("the comment")
            .quote
            .clone()
    };

    assert_eq!(quote_of(added[0]), None);
    assert_eq!(quote_of(added[1]), Some(quote));
    assert_eq!(quote_of(added[2]), None);
    assert_eq!(quote_of(added[3]), None, "an overall comment has no lines");
    assert_eq!(quote_of(reply), None);
    assert_eq!(quote_of(added[6]), None);

    let long = quote_of(added[4]).expect("a quote");
    assert_eq!(
        long.len(),
        14,
        "thirteen lines of 300 bytes, and the start of the fourteenth"
    );
    assert_eq!(long.iter().map(|line| line.text.len()).sum::<usize>(), 4000);
    assert_eq!(long[13].line, 14);
    assert_eq!(
        long.iter().map(|line| line.cut).collect::<Vec<_>>(),
        [vec![false; 13], vec![true]].concat(),
        "only the line that crossed the cap is marked"
    );

    let many: Vec<QuoteLine> = (1..=30).map(|at| line(at, QuoteKind::Same, "x")).collect();
    let capped = store
        .add_comments(
            id,
            Some("Bob"),
            101,
            before,
            &[NewComment::on_lines("Intro", Side::New, 1, 30, "many lines").with_quote(many)],
        )
        .expect("review");
    let stored = store.comments(id).expect("comments");
    let kept = stored
        .iter()
        .find(|stored| stored.id == capped[0])
        .expect("the comment");
    assert_eq!(kept.quote.as_ref().map(Vec::len), Some(20));

    let cut = quote_of(added[5]).expect("a quote");
    assert_eq!(cut.len(), 1);
    assert_eq!(cut[0].text, "ą".repeat(2000), "cut at a character, inside the bytes");
    assert!(cut[0].cut);
}

/// A comment is deleted with its replies, a reply alone, and only from its own document. The other
/// threads, the document and its history stay.
pub fn delete_comments_contract(store: &mut dyn KnowledgeStore) {
    let id = put(
        store,
        &Seed::new("Spec")
            .source("/docs/spec")
            .status(Status::Draft)
            .chunk("Intro", "alpha"),
    );
    let other = put(
        store,
        &Seed::new("Other")
            .source("/docs/other")
            .status(Status::Draft)
            .chunk("Intro", "beta"),
    );
    let before = revision(store, id);

    let threads = store
        .add_comments(
            id,
            Some("Bob"),
            100,
            before,
            &[
                NewComment::overall("an overall remark"),
                NewComment::on_lines("Intro", Side::New, 1, 1, "a line"),
            ],
        )
        .expect("review");
    let elsewhere = store
        .add_comments(
            other,
            Some("Bob"),
            100,
            0,
            &[NewComment::overall("on the other document")],
        )
        .expect("review of the other document");

    let first = store
        .reply_comment(id, threads[0], Some("Ada"), 110, "first reply")
        .expect("reply")
        .expect("a reply");
    let second = store
        .reply_comment(id, threads[0], Some("Cy"), 111, "second reply")
        .expect("reply")
        .expect("a reply");
    let kept = store
        .reply_comment(id, threads[1], Some("Ada"), 112, "reply to the line")
        .expect("reply")
        .expect("a reply");
    let history = store.history(id).expect("history").expect("a history").events.len();
    let ids_of = |store: &mut dyn KnowledgeStore, doc: i64| -> Vec<i64> {
        store
            .comments(doc)
            .expect("comments")
            .iter()
            .map(|comment| comment.id)
            .collect()
    };

    // Nothing to delete: an unknown comment, and a comment of another document.
    assert_eq!(store.delete_comment(id, 987_654).expect("unknown"), None);
    assert_eq!(store.delete_comment(id, elsewhere[0]).expect("another document"), None);
    assert_eq!(
        store.delete_comment(987_654, threads[0]).expect("unknown document"),
        None
    );
    assert_eq!(ids_of(store, id).len(), 5);
    assert_eq!(ids_of(store, other), elsewhere);

    // A reply deletes only itself.
    let generation = store.generation().expect("generation");
    assert_eq!(store.delete_comment(id, second).expect("delete"), Some(vec![second]));
    assert!(store.generation().expect("generation") > generation);
    assert_eq!(ids_of(store, id), [threads[0], threads[1], first, kept]);

    // A thread takes its replies with it, the root first, and not the other threads.
    assert_eq!(
        store.delete_comment(id, threads[0]).expect("delete"),
        Some(vec![threads[0], first])
    );
    assert_eq!(ids_of(store, id), [threads[1], kept]);
    assert_eq!(ids_of(store, other), elsewhere);

    // Gone is gone.
    assert_eq!(store.delete_comment(id, threads[0]).expect("again"), None);
    assert_eq!(store.delete_comment(id, first).expect("again"), None);

    // The document and its history are left as they were.
    assert_eq!(revision(store, id), before);
    assert_eq!(
        store.history(id).expect("history").expect("a history").events.len(),
        history
    );
}

/// Comments are written in reviews, answered once, resolved and reopened, and carry the revision
/// they were written at, so an older one reads as outdated.
pub fn comments_contract(store: &mut dyn KnowledgeStore) {
    let id = put(
        store,
        &Seed::new("Spec")
            .source("/docs/spec")
            .status(Status::Draft)
            .chunk("Intro", "alpha"),
    );
    let before = revision(store, id);
    let generation = store.generation().expect("generation");

    let first = store
        .add_comments(
            id,
            Some("Bob"),
            100,
            before,
            &[
                NewComment::overall("  an overall remark \r\n"),
                NewComment::on_lines("Intro", Side::Old, 2, 4, "this line"),
                NewComment::on_lines("Intro", Side::New, 1, 1, "the other Intro").in_occurrence(2),
            ],
        )
        .expect("review");
    assert_eq!(first.len(), 3);
    assert!(store.generation().expect("generation") > generation);
    assert_eq!(revision(store, id), before, "a comment is not a new revision");

    let second = store
        .add_comments(
            id,
            Some("Cy"),
            101,
            before,
            &[NewComment::on_lines("Intro", Side::New, 1, 1, "later")],
        )
        .expect("another review");

    let all = store.comments(id).expect("comments");
    assert_eq!(all.len(), 4);
    assert_eq!(
        all.iter().map(|comment| comment.id).collect::<Vec<_>>(),
        [first[0], first[1], first[2], second[0]]
    );

    // A line comment keeps which of the sections that share its heading it is on.
    assert_eq!(
        all.iter().map(|comment| comment.occurrence).collect::<Vec<_>>(),
        [None, Some(1), Some(2), Some(1)]
    );

    assert_eq!(all[0].body, "an overall remark");
    assert_eq!(
        (all[0].section.as_deref(), all[0].side, all[0].line_from, all[0].line_to),
        (None, None, None, None)
    );
    assert_eq!(
        (all[1].section.as_deref(), all[1].side, all[1].line_from, all[1].line_to),
        (Some("Intro"), Some(Side::Old), Some(2), Some(4))
    );
    assert_eq!(
        (all[0].by.as_deref(), all[0].at, all[0].revision),
        (Some("Bob"), 100, before)
    );
    assert_eq!(all[0].review_id, all[1].review_id);
    assert!(all[0].review_id.is_some());
    assert_ne!(all[0].review_id, all[3].review_id, "another review");
    assert!(
        all.iter()
            .all(|comment| comment.parent_id.is_none() && comment.resolved_at.is_none())
    );

    // A reply answers a comment, and cannot be answered.
    let reply = store
        .reply_comment(id, first[1], Some("Ada"), 110, "thanks, fixed")
        .expect("reply")
        .expect("a reply");
    assert!(
        store
            .reply_comment(id, reply, Some("Bob"), 111, "and again")
            .expect("reply")
            .is_none()
    );
    assert!(
        store
            .reply_comment(id, 987_654, Some("Bob"), 111, "nobody")
            .expect("reply")
            .is_none()
    );

    let all = store.comments(id).expect("comments");
    let answered = all.iter().find(|comment| comment.id == reply).expect("the reply");
    assert_eq!(
        (answered.parent_id, answered.body.as_str(), answered.by.as_deref()),
        (Some(first[1]), "thanks, fixed", Some("Ada"))
    );

    // Resolve and reopen: a thread can, a reply cannot.
    assert!(
        store
            .resolve_comment(id, first[1], true, Some("Ada"), 120)
            .expect("resolve")
    );
    let all = store.comments(id).expect("comments");
    let thread = all.iter().find(|comment| comment.id == first[1]).expect("thread");
    assert_eq!(
        (thread.resolved_at, thread.resolved_by.as_deref()),
        (Some(120), Some("Ada"))
    );
    assert!(
        !store
            .resolve_comment(id, reply, true, Some("Ada"), 121)
            .expect("a reply")
    );
    assert!(
        !store
            .resolve_comment(id, 987_654, true, Some("Ada"), 121)
            .expect("unknown")
    );

    assert!(
        store
            .resolve_comment(id, first[1], false, Some("Ada"), 122)
            .expect("reopen")
    );
    let all = store.comments(id).expect("comments");
    let thread = all.iter().find(|comment| comment.id == first[1]).expect("thread");
    assert_eq!((thread.resolved_at, thread.resolved_by.as_deref()), (None, None));

    // Editing the content makes the earlier comments outdated.
    let (mut doc, chunks, links) = Seed::new("Spec")
        .source("/docs/spec")
        .chunk("Intro", "alpha changed")
        .parts();
    doc.id = id;
    store.put_document(&doc, &chunks, &links).expect("edit");

    let current = revision(store, id);
    assert!(current > before);

    // A comment carries the revision that was reviewed, not the one the document is at now.
    let late = store
        .add_comments(id, Some("Cy"), 125, before, &[NewComment::overall("written late")])
        .expect("a review of the earlier revision");
    let stored = store.comments(id).expect("comments");
    assert_eq!(
        stored
            .iter()
            .find(|comment| comment.id == late[0])
            .map(|comment| comment.revision),
        Some(before)
    );
    assert!(stored.iter().all(|comment| comment.revision < current));

    // Bad input is refused, and comments belong to a document.
    for bad in [
        NewComment::overall("   "),
        NewComment::overall(&"x".repeat(4001)),
        NewComment::on_lines("Intro", Side::New, 0, 1, "zero"),
        NewComment::on_lines("Intro", Side::New, 5, 2, "backwards"),
        NewComment {
            side: Some(Side::New),
            ..NewComment::overall("a side alone")
        },
        NewComment::on_lines("Intro", Side::New, 1, 1, "no zeroth Intro").in_occurrence(0),
        NewComment::overall("an occurrence alone").in_occurrence(1),
    ] {
        assert!(matches!(
            store.add_comments(id, Some("Bob"), 130, before, &[bad]),
            Err(StoreError::Invalid(_))
        ));
    }

    assert!(matches!(
        store.add_comments(id, Some("Bob"), 130, before, &[]),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store.add_comments(987_654, Some("Bob"), 130, before, &[NewComment::overall("nobody")]),
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(store.comments(id).expect("comments").len(), 6);
}

/// The original file of an import round-trips byte for byte up to the limits, is refused over
/// them, belongs to its document and is dropped when the content changes.
pub fn files_contract(store: &mut dyn KnowledgeStore) {
    let id = put(store, &Seed::new("Guide").source("/docs/guide").chunk("A", "alpha"));

    assert!(store.file(id).expect("file").is_none());
    assert!(!store.put_file(987_654, "x.md", b"nobody").expect("unknown"));

    let bytes: Vec<u8> = (0..70_000_u32).map(|n| (n % 251) as u8).collect();
    assert!(store.put_file(id, "guide.md", &bytes).expect("put"));

    let stored = store.file(id).expect("file").expect("a file");
    assert_eq!(stored.name, "guide.md");
    assert_eq!(stored.bytes, bytes);
    assert_eq!(stored.sha256, sha256_hex(&bytes));

    // Another file replaces it.
    assert!(store.put_file(id, "guide.txt", b"short").expect("replace"));
    let stored = store.file(id).expect("file").expect("a file");
    assert_eq!(
        (stored.name.as_str(), stored.bytes.as_slice()),
        ("guide.txt", b"short".as_slice())
    );

    // Over the limits is refused, whatever the document is.
    let over = vec![0_u8; usize::try_from(FILE_BYTES_MAX).expect("limit") + 1];
    assert!(matches!(
        store.put_file(id, "big.md", &over),
        Err(StoreError::Invalid(_))
    ));
    let past = vec![0_u8; usize::try_from(PDF_BYTES_MAX).expect("limit") + 1];
    assert!(matches!(
        store.put_file(id, "big.pdf", &past),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(store.put_file(id, "  ", b"x"), Err(StoreError::Invalid(_))));
    assert_eq!(store.file(id).expect("file").expect("kept").bytes, b"short");

    // Changing the content drops it: it no longer is the original.
    let (mut doc, chunks, links) = Seed::new("Guide").source("/docs/guide").chunk("A", "alpha two").parts();
    doc.id = id;
    store.put_document(&doc, &chunks, &links).expect("edit");
    assert!(store.file(id).expect("file").is_none());
}

/// The largest files an import reads are kept whole: [`FILE_BYTES_MAX`] for a document, and
/// [`PDF_BYTES_MAX`] for a PDF.
pub fn largest_files_contract(store: &mut dyn KnowledgeStore, pdf: bool) {
    let seed = Seed::new("Big")
        .source("/docs/big")
        .format(if pdf { "pdf" } else { "markdown" })
        .chunk("A", "alpha");
    let id = put(store, &seed);
    let size = usize::try_from(if pdf { PDF_BYTES_MAX } else { FILE_BYTES_MAX }).expect("limit");
    let bytes: Vec<u8> = (0..size).map(|n| (n % 253) as u8).collect();

    assert!(store.put_file(id, "big", &bytes).expect("put"));

    let stored = store.file(id).expect("file").expect("a file");
    assert_eq!(stored.bytes.len(), size);
    assert!(stored.bytes == bytes, "the bytes come back as they went");
    assert_eq!(stored.sha256, sha256_hex(&bytes));

    if !pdf {
        let over = vec![1_u8; size + 1];
        assert!(matches!(store.put_file(id, "big", &over), Err(StoreError::Invalid(_))));
    }
}
