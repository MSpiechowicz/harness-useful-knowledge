//! `docs ask|request-changes|approve|go-live|take-back` and `docs review`: the transitions of a draft or a
//! pending version and the review of one (see [`crate::review`] for the policy).
//!
//! - `docs ask|request-changes|approve|go-live|take-back --json ID --expect REV [--comment TEXT]` prints
//!   `{"id", "status", "revision", "promoted"}`: the document after the transition, the live id
//!   after a pending version replaced its live document. `--expect` is the revision the person saw
//!   (`revision` of the row, `pending.revision` of a live row that has a pending version), and a
//!   document that moved on is refused as `changed`. `--comment` is the note of an `ask` or the
//!   required comment of `request-changes` (at most 500 characters).
//! - `docs review --json ID --expect REV --verdict comment|approve|requestChanges [--body TEXT]
//!   [--comments FILE]` submits one review: the overall `--body` and the line comments of FILE (a
//!   JSON array of `{"section", "occurrence"?, "side": "old"|"new", "from", "to", "body"}`, where
//!   `occurrence` is which of the sections that share the heading `section` (from 1, the first when
//!   left out)) are stored, then the
//!   verdict is carried out. It prints what a transition prints, and `commentIds`.
//!
//! `ID` is the row to review: a draft, a waiting row or a pending version, or a live row that has a
//! pending version (its pending version is meant). The author cannot approve or request changes.
//! The person is the git `user.name` of the working directory.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use super::quote::with_quotes;
use super::{Context, Done, Failure, Reply, parse_id};
use crate::backend::Access;
use crate::cli::Args;
use crate::config::Place;
use crate::limits::MANIFEST_MAX;
use crate::review::{self, Review, Submission, Verdict};
use crate::store::routed::ids;
use crate::store::{Detail, KnowledgeStore, NewComment, Reviewed, Side};
use crate::support::private::read_private;

/// The failure a refusal or an error of the review stands for: a refusal prints its code, a remote
/// that cannot be reached is `unreachable`.
pub(super) fn failure(error: review::Error) -> Failure {
    match error {
        review::Error::Refused(refusal) if refusal.code == review::Code::Unreachable => {
            Failure::Unreachable(refusal.message)
        }
        review::Error::Refused(refusal) => Failure::Refused {
            code: refusal.code.as_str(),
            message: refusal.message,
        },
        review::Error::Store(error) => Failure::from(error),
    }
}

/// A refusal that is not one of the review's own (`sinceUnavailable`, or a state that has no code of
/// its own).
pub(super) fn refused(code: &'static str, message: &str) -> Failure {
    Failure::Refused {
        code,
        message: message.to_owned(),
    }
}

/// `document ID not found`, as the code `notFound`.
pub(super) fn missing(id: i64) -> Failure {
    failure(review::Error::Refused(review::Refusal {
        code: review::Code::NotFound,
        message: format!("document {} not found", ids::show(id)),
    }))
}

/// The store of the routing for a command that changes documents.
pub(super) fn open_to_change(context: &Context, id: i64) -> Result<Box<dyn KnowledgeStore>, Failure> {
    context.open(Access::Existing)?.ok_or_else(|| missing(id))
}

/// The store of the routing for a command that only reads documents.
pub(super) fn open_to_read(context: &Context, id: i64) -> Result<Box<dyn KnowledgeStore>, Failure> {
    context.open(Access::Read)?.ok_or_else(|| missing(id))
}

/// The row a review of `id` is about: the pending version when `id` is a live row that has one,
/// else `id`. `notFound` for a row that does not exist.
pub(super) fn reviewed_row(store: &mut dyn KnowledgeStore, id: i64) -> Result<i64, review::Error> {
    let detail = Detail {
        drafts: true,
        ..Detail::default()
    };
    let Some(found) = store.get(id, &detail)? else {
        return Err(review::Error::Refused(review::Refusal {
            code: review::Code::NotFound,
            message: format!("document {} not found", ids::show(id)),
        }));
    };

    if !found.doc.status.is_live() || found.doc.pending_of.is_some() {
        return Ok(id);
    }

    Ok(store.pending_of(id)?.map_or(id, |pending| pending.id))
}

/// The person at the terminal: the git `user.name` of the working directory.
pub(super) fn actor(context: &Context) -> Option<String> {
    let dir = std::env::current_dir().ok()?;

    review::actor_of(context.env, &dir)
}

/// What `docs ask|request-changes|approve|go-live|take-back` takes.
struct Decision<'a> {
    id: i64,
    expect: i64,
    comment: Option<&'a str>,
}

/// The arguments of a transition. `--comment` is for `ask` and `request-changes`.
fn decision<'a>(verb: &str, args: &'a [Option<&'a str>]) -> Result<Decision<'a>, Failure> {
    let parsed = Args::parse(args, &["--json"], &["--expect", "--comment"]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let comment = parsed.value("--comment");

    if comment.is_some() && !matches!(verb, "ask" | "request-changes") {
        return Err(Failure::Usage);
    }

    Ok(Decision {
        id: the_id(&parsed)?,
        expect: expected(&parsed)?,
        comment,
    })
}

fn the_id(parsed: &Args) -> Result<i64, Failure> {
    match parsed.positional.as_slice() {
        [id] => parse_id(id),
        _ => Err(Failure::Usage),
    }
}

/// `--expect`, a revision: a number that is not negative.
fn expected(parsed: &Args) -> Result<i64, Failure> {
    parsed
        .value("--expect")
        .and_then(|raw| raw.parse::<i64>().ok())
        .filter(|revision| *revision >= 0)
        .ok_or(Failure::Usage)
}

/// What a transition printed: `{"id", "status", "revision", "promoted"}`.
fn reviewed(done: &Reviewed) -> Value {
    json!({
        "id": ids::json(done.id),
        "status": done.status.as_str(),
        "revision": done.revision,
        "promoted": done.promoted,
    })
}

/// `docs ask|request-changes|approve|go-live|take-back`.
pub(super) fn transition(context: &Context, verb: &str, args: &[Option<&str>]) -> Reply {
    let Decision { id, expect, comment } = decision(verb, args)?;
    let place = review::place_of(context.routing, id).map_err(failure)?;
    let mut store = open_to_change(context, id)?;

    transition_on(
        store.as_mut(),
        place,
        actor(context).as_deref(),
        verb,
        id,
        expect,
        comment,
    )
}

/// A transition of the row `id` of `store`, kept in `place`, by `actor`.
pub(super) fn transition_on(
    store: &mut dyn KnowledgeStore,
    place: Place,
    actor: Option<&str>,
    verb: &str,
    id: i64,
    expect: i64,
    comment: Option<&str>,
) -> Reply {
    let row = reviewed_row(store, id).map_err(failure)?;
    let mut review = Review::new(store, place, actor);

    let done = match verb {
        "ask" => review.ask(row, expect, comment),
        "request-changes" => review.request_changes(row, expect, comment.unwrap_or_default()),
        "approve" => review.approve(row, expect),
        "take-back" => review.take_back(row, expect),
        _ => review.go_live(row, expect),
    }
    .map_err(failure)?;

    Ok(Done::ok(reviewed(&done)))
}

/// One line comment of the FILE of `docs review`.
#[derive(Deserialize)]
struct LineComment {
    section: String,
    /// Which of the sections that share the heading (from 1, the first when left out).
    #[serde(default)]
    occurrence: Option<u32>,
    side: String,
    from: u32,
    to: u32,
    body: String,
}

/// The line comments in the JSON of `file`: private to this user and at most [`MANIFEST_MAX`] bytes.
fn read_comments(file: &str) -> Result<Vec<NewComment>, Failure> {
    let bytes = read_private(Path::new(file), usize::try_from(MANIFEST_MAX).unwrap_or(usize::MAX))
        .map_err(Failure::Failed)?
        .ok_or_else(|| Failure::Failed("the comments file does not exist".to_owned()))?;

    let comments: Vec<LineComment> = serde_json::from_slice(&bytes).map_err(|_| {
        Failure::Failed(
            "the comments must be a JSON array of {\"section\", \"occurrence\"?, \"side\": \"old\"|\"new\", \"from\", \"to\", \"body\"}"
                .to_owned(),
        )
    })?;

    comments
        .iter()
        .map(|comment| {
            let side = Side::parse(&comment.side)
                .ok_or_else(|| Failure::Failed("the side of a comment is old or new".to_owned()))?;

            Ok(
                NewComment::on_lines(&comment.section, side, comment.from, comment.to, &comment.body)
                    .in_occurrence(comment.occurrence.unwrap_or(1)),
            )
        })
        .collect()
}

/// `docs review`.
pub(super) fn submit(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed =
        Args::parse(args, &["--json"], &["--expect", "--verdict", "--body", "--comments"]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let id = the_id(&parsed)?;
    let expect = expected(&parsed)?;
    let verdict = parsed
        .value("--verdict")
        .and_then(Verdict::parse)
        .ok_or(Failure::Usage)?;
    let comments = match parsed.value("--comments") {
        Some(file) => read_comments(file)?,
        None => Vec::new(),
    };
    let submission = Submission {
        verdict,
        body: parsed.value("--body").map(str::to_owned),
        comments,
    };

    let place = review::place_of(context.routing, id).map_err(failure)?;
    let mut store = open_to_change(context, id)?;

    submit_on(
        store.as_mut(),
        place,
        actor(context).as_deref(),
        id,
        expect,
        &submission,
    )
}

/// A review of the row `id` of `store`, kept in `place`, by `actor`.
pub(super) fn submit_on(
    store: &mut dyn KnowledgeStore,
    place: Place,
    actor: Option<&str>,
    id: i64,
    expect: i64,
    submission: &Submission,
) -> Reply {
    let row = reviewed_row(store, id).map_err(failure)?;
    let quoted = Submission {
        comments: with_quotes(store, row, &submission.comments),
        body: submission.body.clone(),
        verdict: submission.verdict,
    };
    let submitted = Review::new(store, place, actor)
        .submit(row, expect, &quoted)
        .map_err(failure)?;

    let mut shown = match &submitted.transition {
        Some(done) => reviewed(done),
        None => unchanged(store, row)?,
    };
    shown["commentIds"] = submitted.comment_ids.iter().map(|id| ids::json(*id)).collect();

    Ok(Done::ok(shown))
}

/// The state of row `id` after a review that changed none.
fn unchanged(store: &mut dyn KnowledgeStore, id: i64) -> Result<Value, Failure> {
    let detail = Detail {
        drafts: true,
        ..Detail::default()
    };
    let found = store.get(id, &detail)?.ok_or_else(|| missing(id))?;

    Ok(reviewed(&Reviewed {
        id: found.doc.id,
        status: found.doc.status,
        revision: found.doc.review_rev,
        promoted: false,
    }))
}
