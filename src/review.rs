//! The review policy of drafts and team approval: who may do what to a document that is not live,
//! and the refusals the commands print. The store records the transitions (compare-and-set on the
//! revision, see [`crate::store::review`]), and this module decides whether one may be made.
//!
//! - **Team approval** is the remote database's meta key [`APPROVAL_KEY`] (`on`, else off). It is
//!   read from the remote alone ([`KnowledgeStore::remote_meta`]): a remote that cannot be reached
//!   is `unreachable`, never this computer's answer. A document on this computer is never under
//!   approval, so it can always go live by its author.
//! - **Who acts** is the git `user.name` of the person ([`actor_of`]), self-declared: it stops
//!   honest mistakes, not attacks. Approving and requesting changes need a name that differs
//!   (trimmed, case-insensitive) from everyone who changed the content of the version since it was
//!   last live (a rename too) and from whoever last asked (see `people_of`, [`Review::approve`]). An author without a name blocks nobody. Asking needs a name
//!   while approval is on.
//! - **Refusals** carry a [`Code`] and print as `{"error": code, "message"}`. The checks run in
//!   this order: a missing comment, `readOnly`, `notFound`, `unreachable`, the approval switch
//!   (`approvalOn`, `approvalOff`), `changed` and `wrongState`, `noName`, `selfApproval`.
//!
//! | Action | Approval on | Approval off, or on this computer |
//! |---|---|---|
//! | ask | draft or changesRequested to waiting | `approvalOff` |
//! | request changes | waiting to changesRequested | `approvalOff` |
//! | approve | waiting to live (a pending version replaces its live document) | `approvalOff` |
//! | go live | `approvalOn` | draft, waiting or changesRequested to live |
//! | take back | waiting to draft, by its author or asker | the same |
//!
//! Every function takes the id as the store knows it (tagged with its place when both places are
//! in use, see [`crate::store::routed::ids`]) and the [`Place`] the document is kept in, which
//! decides whether approval applies ([`place_of`]).

use std::path::Path;

use crate::config::{Place, Routing};
use crate::import::document::now;
use crate::import::place::place_in_use;
use crate::limits::LIST_PAGE_MAX;
use crate::store::review::{self, COMMENT_BODY_MAX, EVENT_COMMENT_MAX, Gate, comment_body};
use crate::store::routed::ids;
use crate::store::{
    Detail, DocumentDetail, KnowledgeStore, ListQuery, NewComment, RepoFilter, ReviewAction, ReviewChange, ReviewReply,
    Reviewed, Sort, Status, StatusFilter, StoreError, clean,
};
use crate::support::{Env, author};

mod cache;

pub use cache::{PlaceState, WAITING_CACHE_FILE, waiting_for_cached};

/// The meta key of the remote database that switches team approval on.
pub const APPROVAL_KEY: &str = "team_approval";

/// The meta key that keeps who last switched team approval (the git `user.name`).
pub const APPROVAL_BY_KEY: &str = "team_approval_by";

/// The meta key that keeps when team approval was last switched (RFC 3339, UTC).
pub const APPROVAL_AT_KEY: &str = "team_approval_at";

/// Why a transition or a write of the review was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    NotFound,
    /// The document moved on since the revision that was expected.
    Changed,
    /// The action does not apply to a document in this state.
    WrongState,
    /// Go live is for documents that need no approval.
    ApprovalOn,
    /// Ask, request changes and approve need team approval to be on.
    ApprovalOff,
    /// The person wrote or asked for what they would approve.
    SelfApproval,
    /// Only the one who wrote or asked for a document may take its ask back.
    NotAuthor,
    /// No git `user.name` is known.
    NoName,
    /// The database is read-only for this user.
    ReadOnly,
    /// Requesting changes needs a comment.
    CommentRequired,
    /// Anything else that cannot be done (the message says why).
    Refused,
    /// The remote database cannot be reached.
    Unreachable,
    /// A team database does not take a repository that is a folder on this computer (`path:`).
    LocalPath,
}

impl Code {
    /// The `error` the commands print.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "notFound",
            Self::Changed => "changed",
            Self::WrongState => "wrongState",
            Self::ApprovalOn => "approvalOn",
            Self::ApprovalOff => "approvalOff",
            Self::SelfApproval => "selfApproval",
            Self::NotAuthor => "notAuthor",
            Self::NoName => "noName",
            Self::ReadOnly => "readOnly",
            Self::CommentRequired => "commentRequired",
            Self::Refused => "refused",
            Self::Unreachable => "unreachable",
            Self::LocalPath => "localPath",
        }
    }
}

/// A refusal: the code and what to tell the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: Code,
    pub message: String,
}

impl Refusal {
    fn new(code: Code, message: &str) -> Self {
        Self {
            code,
            message: message.to_owned(),
        }
    }
}

/// Why the review did not do what was asked.
#[derive(Debug)]
pub enum Error {
    /// A refusal the command prints as `{"error": code, "message"}` (exit 1).
    Refused(Refusal),
    /// The store failed for a reason of its own (a database error).
    Store(StoreError),
}

impl From<Refusal> for Error {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<StoreError> for Error {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Unreachable(message) => Self::Refused(Refusal {
                code: Code::Unreachable,
                message,
            }),
            StoreError::NotFound(message) => Self::Refused(Refusal {
                code: Code::NotFound,
                message,
            }),
            StoreError::Invalid(message) => Self::Refused(Refusal {
                code: Code::Refused,
                message,
            }),
            StoreError::LocalPath(message) => Self::Refused(Refusal {
                code: Code::LocalPath,
                message,
            }),
            other => Self::Store(other),
        }
    }
}

fn refuse(code: Code, message: &str) -> Error {
    Error::Refused(Refusal::new(code, message))
}

/// The place of the document `id` while `routing` is in use (see [`place_in_use`]). An id that
/// names a place the routing does not use (`local:5` with everything remote) is `notFound`: there
/// is no such document, and the other place's rules must not be applied to it.
pub fn place_of(routing: Routing, id: i64) -> Result<Place, Error> {
    let place = place_in_use(routing, id);

    if !routing.places().contains(&place) {
        return Err(refuse(
            Code::NotFound,
            &format!(
                "document {} not found: that place ({}) is not in use",
                crate::store::routed::ids::show(id),
                place.at()
            ),
        ));
    }

    Ok(place)
}

/// The name the person acts under: their git `user.name` in `dir` (see [`author::resolve`]).
pub fn actor_of(env: &Env, dir: &Path) -> Option<String> {
    author::resolve(env, dir)
}

/// Whether team approval is on for documents kept in `place`: this computer is never under
/// approval, the remote database says so itself. `unreachable` while it cannot be reached, never
/// a guess from this computer's file.
pub fn approval_in(store: &mut dyn KnowledgeStore, place: Place) -> Result<bool, Error> {
    if place == Place::Local {
        return Ok(false);
    }

    let value = store.remote_meta(APPROVAL_KEY)?;

    Ok(value.is_some_and(|value| value.trim() == "on"))
}

/// What `approval get` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalState {
    pub approval: bool,
    /// `Remote` while a team database is in use, else `Local` (where approval is always off).
    pub place: Place,
    /// Whether the user may switch approval: `None` when that is not known, and for this computer.
    pub writable: Option<bool>,
    /// Who last switched it (a git `user.name`), when the database says.
    pub changed_by: Option<String>,
    /// When it was last switched (RFC 3339, UTC), when the database says.
    pub changed_at: Option<String>,
}

impl ApprovalState {
    /// Approval is off, and this computer's file has no say about it.
    pub fn local() -> Self {
        Self {
            approval: false,
            place: Place::Local,
            writable: None,
            changed_by: None,
            changed_at: None,
        }
    }

    /// Approval is off in a team database that has no tables yet.
    pub fn unset() -> Self {
        Self {
            place: Place::Remote,
            ..Self::local()
        }
    }
}

/// The state of team approval. `remote` is whether a team database is in use at all.
pub fn approval_state(store: &mut dyn KnowledgeStore, remote: bool) -> Result<ApprovalState, Error> {
    if !remote {
        return Ok(ApprovalState::local());
    }

    Ok(ApprovalState {
        approval: approval_in(store, Place::Remote)?,
        place: Place::Remote,
        writable: store.remote_writable(),
        changed_by: store.remote_meta(APPROVAL_BY_KEY)?.filter(|by| !by.trim().is_empty()),
        changed_at: store.remote_meta(APPROVAL_AT_KEY)?.filter(|at| !at.trim().is_empty()),
    })
}

/// Switches team approval on or off in the remote database, for a user who may write there and has
/// a name (`actor`, a git `user.name`): the switch, who made it and when are written together, so
/// the team can see who changed it. Turning it on changes no document, turning it off leaves
/// waiting rows waiting (their authors can go live). `remote` is whether a team database is in use
/// at all.
pub fn set_approval(
    store: &mut dyn KnowledgeStore,
    remote: bool,
    on: bool,
    actor: Option<&str>,
) -> Result<ApprovalState, Error> {
    if !remote {
        return Err(refuse(
            Code::Refused,
            "there is no team database: team approval is a setting of the shared database",
        ));
    }

    if store.remote_writable() == Some(false) {
        return Err(read_only());
    }

    let by = review::actor(actor).ok_or_else(|| {
        refuse(
            Code::NoName,
            "no git user.name is set: set one with `git config user.name`",
        )
    })?;

    let value = if on { "on" } else { "off" };
    let at = crate::support::time::rfc3339(now());
    store.set_metas(&[(APPROVAL_KEY, value), (APPROVAL_BY_KEY, &by), (APPROVAL_AT_KEY, &at)])?;

    // A routed store with no remote would have written to this computer's file: say so.
    let written = store.remote_meta(APPROVAL_KEY)?;

    if written.as_deref() != Some(value) {
        return Err(refuse(
            Code::Refused,
            "the team database was not changed: it could not be written",
        ));
    }

    approval_state(store, true)
}

/// Refuses to change the links of a pending version: it has none of its own, the live document's
/// win. `NotFound` for a document that does not exist.
pub fn check_links_target(store: &mut dyn KnowledgeStore, id: i64) -> Result<(), Error> {
    let found = store.get(id, &with_drafts())?;
    let Some(found) = found else {
        return Err(refuse(Code::NotFound, &format!("document {id} not found")));
    };

    if let Some(live) = found.doc.pending_of {
        return Err(refuse(
            Code::Refused,
            &format!(
                "document {id} is a pending version and has no links of its own: change the links of document {live}"
            ),
        ));
    }

    Ok(())
}

fn read_only() -> Error {
    refuse(Code::ReadOnly, "the team database is read-only for this user")
}

fn with_drafts() -> Detail {
    Detail {
        drafts: true,
        ..Detail::default()
    }
}

/// True when two names are the same person: equal once trimmed and lower-cased.
fn same_person(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Everyone who wrote the version of row `id` that is reviewed, or asked for its review: the one
/// rule of `selfApproval`, the take-back and [`waiting_for`]. Writers are the names on the
/// `drafted`, `edited` (a rename too) and `updateDrafted` events since the row was last live (its
/// latest `wentLive` or `approved`), `updated_by` and the latest asker. The writers come from the
/// whole history ([`KnowledgeStore::writers_since_live`]), so a writer cannot push their own events
/// out of a window. The asker is the newest `asked` event of the newest `HISTORY_MAX` ones.
pub(crate) fn people_of(
    store: &mut dyn KnowledgeStore,
    id: i64,
    updated_by: Option<&str>,
) -> Result<Vec<String>, Error> {
    let mut found: Vec<String> = updated_by.map(str::to_owned).into_iter().collect();
    found.extend(store.writers_since_live(id)?);

    // Newest first: the events before the first one that made the row live.
    found.extend(review::latest_asker(&store.history(id)?.unwrap_or_default()));

    Ok(distinct_people(found))
}

/// The people of a row in a list: `updated_by`, then the writers and the asker `authorship` names,
/// each once and at most [`review::AUTHORS_LIST_MAX`] of them, in that order. For display: the
/// rule of `selfApproval` and of the take-back reads every name ([`people_of`]).
pub(crate) fn people_listed(updated_by: Option<&str>, authorship: &review::Authorship) -> Vec<String> {
    let mut found: Vec<String> = updated_by.map(str::to_owned).into_iter().collect();
    found.extend(authorship.writers.iter().cloned());
    found.extend(authorship.asker.clone());

    let mut people = distinct_people(found);
    people.truncate(review::AUTHORS_LIST_MAX);

    people
}

/// Each person once, as the first name they were written with.
fn distinct_people(found: Vec<String>) -> Vec<String> {
    let mut people: Vec<String> = Vec::new();

    for name in found {
        if !is_own(&name, &people) {
            people.push(name);
        }
    }

    people
}

/// Whether `name` is one of `people` (see [`people_of`]).
fn is_own(name: &str, people: &[String]) -> bool {
    people.iter().any(|other| same_person(other, name))
}

/// Most rows [`waiting_for`] looks at in one database. A database with more waiting rows than this
/// counts only the first ones (ordered by id), so the number is a floor there.
pub const WAITING_SCAN_MAX: usize = 2000;

/// How many documents of `store` wait for `name` to decide on them: a waiting row, or a live row
/// whose pending version waits, that `name` could approve (not written, renamed or asked for by
/// them, the rule of `selfApproval`). `None` or a blank name is nobody and counts 0.
///
/// The rows are listed with [`StatusFilter::Waiting`] in pages of [`LIST_PAGE_MAX`] and at most
/// [`WAITING_SCAN_MAX`] of them are looked at, so the answer is exact up to that many waiting
/// documents in one database and a lower bound past it.
pub fn waiting_for(store: &mut dyn KnowledgeStore, name: Option<&str>) -> Result<usize, Error> {
    let Some(name) = name.filter(|name| !name.trim().is_empty()) else {
        return Ok(0);
    };

    let mut count = 0;
    let mut offset = 0;

    while offset < WAITING_SCAN_MAX {
        let page = store.list(&ListQuery {
            filter: RepoFilter::All,
            format: None,
            text: String::new(),
            useful: None,
            author: None,
            want_authors: false,
            status: StatusFilter::Waiting,
            sort: Sort::Updated,
            descending: false,
            offset,
            limit: LIST_PAGE_MAX.min(WAITING_SCAN_MAX - offset),
        })?;
        let seen = page.items.len();

        for row in page.items {
            // The row that is reviewed: the pending version of a live row, else the row itself.
            let (id, status, author) = match &row.pending {
                Some(pending) => (pending.id, pending.status, pending.updated_by.as_ref()),
                None => (row.doc.id, row.doc.status, row.doc.updated_by.as_ref()),
            };

            if status != Status::Waiting {
                continue;
            }

            let author = author.map(|author| author.name.as_str());

            if is_own(name, &people_of(store, id, author)?) {
                continue;
            }

            count += 1;
        }

        offset += seen;

        if seen == 0 || offset >= page.total {
            break;
        }
    }

    Ok(count)
}

/// The refusal a reply that is not a transition stands for.
fn stopped(reply: ReviewReply) -> Error {
    match reply {
        ReviewReply::NotFound => refuse(Code::NotFound, "document not found"),
        ReviewReply::Changed { revision } => refuse(
            Code::Changed,
            &format!("the document changed since it was shown (it is at revision {revision} now)"),
        ),
        ReviewReply::WrongState(status) => refuse(
            Code::WrongState,
            &format!("the document is {}, which this does not apply to", status.as_str()),
        ),
        ReviewReply::Done(_) => refuse(Code::Refused, "nothing to refuse"),
    }
}

/// What a reviewer says about a document in one submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Comments only: no state changes. The author may do this.
    Comment,
    /// Approve (promote): not for the author.
    Approve,
    /// Ask for changes: not for the author, and needs a comment.
    RequestChanges,
}

impl Verdict {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "comment" => Some(Self::Comment),
            "approve" => Some(Self::Approve),
            "requestChanges" => Some(Self::RequestChanges),
            _ => None,
        }
    }
}

/// One review: a verdict, an overall body and line comments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub verdict: Verdict,
    pub body: Option<String>,
    /// Line comments (see [`NewComment::on_lines`]).
    pub comments: Vec<NewComment>,
}

/// What a submitted review came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submitted {
    /// The ids of the comments stored: the overall body first when there is one.
    pub comment_ids: Vec<i64>,
    /// The transition the verdict made (none for `comment`).
    pub transition: Option<Reviewed>,
}

/// The review of documents kept in one place, by one person.
pub struct Review<'a> {
    store: &'a mut dyn KnowledgeStore,
    place: Place,
    actor: Option<String>,
}

impl<'a> Review<'a> {
    /// A review of documents kept in `place` (see [`place_of`]) by `actor`, a git `user.name`
    /// (`None`, or a name that is not fit to store, is no name).
    pub fn new(store: &'a mut dyn KnowledgeStore, place: Place, actor: Option<&str>) -> Self {
        Self {
            store,
            place,
            actor: review::actor(actor),
        }
    }

    /// Asks for approval: a draft (or a row with requested changes) becomes `waiting`. The note
    /// goes into the history. Needs a name.
    pub fn ask(&mut self, id: i64, expect: i64, note: Option<&str>) -> Result<Reviewed, Error> {
        let note = review::event_comment(note)?;

        self.transition(id, expect, ReviewAction::Ask, note)
    }

    /// Asks the author for changes: `waiting` becomes `changesRequested`. The comment is required.
    pub fn request_changes(&mut self, id: i64, expect: i64, comment: &str) -> Result<Reviewed, Error> {
        let comment = review::event_comment(Some(comment))?
            .ok_or_else(|| refuse(Code::CommentRequired, "requesting changes needs a comment"))?;

        self.transition(id, expect, ReviewAction::RequestChanges, Some(comment))
    }

    /// Approves a waiting row: it goes live, or its pending version replaces the live document
    /// (the reply's id is the live one). Needs a name that is not the author's or the asker's.
    pub fn approve(&mut self, id: i64, expect: i64) -> Result<Reviewed, Error> {
        self.transition(id, expect, ReviewAction::Approve, None)
    }

    /// Takes the ask back: a `waiting` row becomes a draft again. Only for the one who wrote what
    /// waits or asked for it (the rule of `selfApproval` turned round), and whether approval is on
    /// or off: a row left waiting when approval was switched off can be taken back too. Needs a name.
    pub fn take_back(&mut self, id: i64, expect: i64) -> Result<Reviewed, Error> {
        self.transition(id, expect, ReviewAction::TakeBack, None)
    }

    /// Makes a row live without a reviewer, while no approval is needed.
    pub fn go_live(&mut self, id: i64, expect: i64) -> Result<Reviewed, Error> {
        self.transition(id, expect, ReviewAction::GoLive, None)
    }

    /// Submits one review: its comments are stored first (they belong to the revision that was
    /// reviewed, `expect`), then the verdict is carried out. `comment` changes no state.
    /// `approve` and `requestChanges` are for a reviewer, never the author, and need approval to
    /// be on. A request for changes needs the overall body or a line comment.
    pub fn submit(&mut self, id: i64, expect: i64, submission: &Submission) -> Result<Submitted, Error> {
        let body = match &submission.body {
            Some(body) if !body.trim().is_empty() => Some(comment_body(body, COMMENT_BODY_MAX)?),
            _ => None,
        };

        let mut comments: Vec<NewComment> = body.iter().map(|body| NewComment::overall(body)).collect();
        comments.extend(submission.comments.iter().cloned());

        let action = match submission.verdict {
            Verdict::Comment => None,
            Verdict::Approve => Some(ReviewAction::Approve),
            Verdict::RequestChanges => Some(ReviewAction::RequestChanges),
        };

        if comments.is_empty() && submission.verdict != Verdict::Approve {
            return Err(refuse(
                if submission.verdict == Verdict::RequestChanges {
                    Code::CommentRequired
                } else {
                    Code::Refused
                },
                "a review needs a comment",
            ));
        }

        self.writable()?;

        let doc = self.document(id)?;

        // Everything that can be refused is checked before the first comment is written.
        if let Some(action) = action {
            self.check(&doc, expect, action)?;
        } else if doc.doc.review_rev != expect {
            return Err(stopped(ReviewReply::Changed {
                revision: doc.doc.review_rev,
            }));
        }

        let comment_ids = if comments.is_empty() {
            Vec::new()
        } else {
            self.store
                .add_comments(id, self.actor.as_deref(), now(), expect, &comments)?
        };

        let Some(action) = action else {
            return Ok(Submitted {
                comment_ids,
                transition: None,
            });
        };

        let summary = summary(body.as_deref(), submission.comments.first());
        let transition = self.apply(&doc, expect, action, summary)?;

        Ok(Submitted {
            comment_ids,
            transition: Some(transition),
        })
    }

    /// Answers comment `parent` of document `id`; returns the reply's id.
    pub fn reply(&mut self, id: i64, parent: i64, body: &str) -> Result<i64, Error> {
        let body = comment_body(body, COMMENT_BODY_MAX)?;

        self.writable()?;

        self.store
            .reply_comment(id, parent, self.actor.as_deref(), now(), &body)?
            .ok_or_else(|| {
                refuse(
                    Code::NotFound,
                    &format!("comment {parent} not found on document {id} (a reply cannot be answered)"),
                )
            })
    }

    /// Marks comment `comment` of document `id` resolved, or open again.
    pub fn resolve(&mut self, id: i64, comment: i64, resolved: bool) -> Result<(), Error> {
        self.writable()?;

        if self
            .store
            .resolve_comment(id, comment, resolved, self.actor.as_deref(), now())?
        {
            return Ok(());
        }

        Err(refuse(
            Code::NotFound,
            &format!("comment {comment} not found on document {id} (a reply cannot be resolved)"),
        ))
    }

    /// Deletes comment `comment` of document `id`, and its replies when it starts a thread: the ids
    /// deleted, the comment first. Only the one who wrote the comment may (see [`same_person`]),
    /// whatever the state of the document and whether approval is on. The history is not touched.
    pub fn delete_comment(&mut self, id: i64, comment: i64) -> Result<Vec<i64>, Error> {
        self.writable()?;

        let name = self.named()?.to_owned();
        let found = self
            .store
            .comments(id)?
            .into_iter()
            .find(|stored| ids::split(stored.id).1 == ids::split(comment).1)
            .ok_or_else(|| {
                refuse(
                    Code::NotFound,
                    &format!("comment {} not found on document {}", ids::show(comment), ids::show(id)),
                )
            })?;

        if !found.by.as_deref().is_some_and(|by| same_person(by, &name)) {
            return Err(refuse(
                Code::NotAuthor,
                "only the one who wrote a comment can delete it",
            ));
        }

        self.store.delete_comment(id, comment)?.ok_or_else(|| {
            refuse(
                Code::NotFound,
                &format!("comment {} not found on document {}", ids::show(comment), ids::show(id)),
            )
        })
    }

    fn transition(
        &mut self,
        id: i64,
        expect: i64,
        action: ReviewAction,
        comment: Option<String>,
    ) -> Result<Reviewed, Error> {
        self.writable()?;

        let doc = self.document(id)?;

        self.check(&doc, expect, action)?;
        self.apply(&doc, expect, action, comment)
    }

    /// Refuses a write to a team database the user may not write to.
    fn writable(&mut self) -> Result<(), Error> {
        if self.place == Place::Remote && self.store.remote_writable() == Some(false) {
            return Err(read_only());
        }

        Ok(())
    }

    fn document(&mut self, id: i64) -> Result<DocumentDetail, Error> {
        self.store
            .get(id, &with_drafts())?
            .ok_or_else(|| refuse(Code::NotFound, &format!("document {id} not found")))
    }

    /// Whether `action` may be made on `doc`, which the caller saw at revision `expect`.
    fn check(&mut self, doc: &DocumentDetail, expect: i64, action: ReviewAction) -> Result<(), Error> {
        // Taking an ask back is the author's own business: approval has no say.
        let approval = match action {
            ReviewAction::TakeBack => true,
            _ => approval_in(self.store, self.place)?,
        };

        match (action, approval) {
            (ReviewAction::GoLive, true) => {
                return Err(refuse(
                    Code::ApprovalOn,
                    "the team's documents are approved by a teammate: ask for approval instead",
                ));
            }
            (ReviewAction::GoLive, false) => {}
            (_, false) => {
                return Err(refuse(
                    Code::ApprovalOff,
                    "team approval is off, so there is nobody to ask: go live instead",
                ));
            }
            (_, true) => {}
        }

        let state = Some((doc.doc.status, doc.doc.review_rev));

        if let Gate::Stop(reply) = review::gate(state, expect, action) {
            return Err(stopped(reply));
        }

        match action {
            ReviewAction::GoLive => Ok(()),
            ReviewAction::TakeBack => self.the_author(doc),
            ReviewAction::Ask => self.named().map(drop),
            ReviewAction::RequestChanges | ReviewAction::Approve => self.not_the_author(doc),
        }
    }

    /// The name the person acts under.
    fn named(&self) -> Result<&str, Error> {
        self.actor.as_deref().ok_or_else(|| {
            refuse(
                Code::NoName,
                "no git user.name is set: set one with `git config user.name`",
            )
        })
    }

    /// Refuses a reviewer who wrote what they would decide on, or asked for it.
    fn not_the_author(&mut self, doc: &DocumentDetail) -> Result<(), Error> {
        let name = self.named()?.to_owned();
        let people = self.people(doc)?;

        if is_own(&name, &people) {
            return Err(refuse(
                Code::SelfApproval,
                "a teammate other than the one who wrote or asked for this must decide on it",
            ));
        }

        Ok(())
    }

    /// Refuses a person who neither wrote what waits nor asked for it.
    fn the_author(&mut self, doc: &DocumentDetail) -> Result<(), Error> {
        let name = self.named()?.to_owned();
        let people = self.people(doc)?;

        if !is_own(&name, &people) {
            return Err(refuse(
                Code::NotAuthor,
                "only the one who wrote this or asked for its approval can take the ask back",
            ));
        }

        Ok(())
    }

    /// Everyone who wrote the reviewed version or asked for its review (see [`people_of`]).
    fn people(&mut self, doc: &DocumentDetail) -> Result<Vec<String>, Error> {
        let author = doc.doc.updated_by.as_ref().map(|author| author.name.as_str());

        people_of(self.store, doc.doc.id, author)
    }

    /// Makes the transition: a pending version that goes live is promoted.
    fn apply(
        &mut self,
        doc: &DocumentDetail,
        expect: i64,
        action: ReviewAction,
        comment: Option<String>,
    ) -> Result<Reviewed, Error> {
        let mut change = ReviewChange::new(action, self.actor.as_deref(), now());
        change.comment = comment;

        let id = doc.doc.id;
        let reply = if action.goes_live() && doc.doc.pending_of.is_some() {
            self.store.promote(id, expect, &change)?
        } else {
            self.store.set_review(id, expect, &change)?
        };

        match reply {
            ReviewReply::Done(done) => Ok(done),
            other => Err(stopped(other)),
        }
    }
}

/// The event comment of a review: the start of its overall body, else of its first line comment, on
/// one line of at most [`EVENT_COMMENT_MAX`] characters.
fn summary(body: Option<&str>, first: Option<&NewComment>) -> Option<String> {
    let text = body.or_else(|| first.map(|comment| comment.body.as_str()))?;
    let line = clean::line(text);
    let short: String = line.chars().take(EVENT_COMMENT_MAX).collect();
    let short = short.trim().to_owned();

    (!short.is_empty()).then_some(short)
}

#[cfg(test)]
mod spy;
#[cfg(test)]
mod tests;
