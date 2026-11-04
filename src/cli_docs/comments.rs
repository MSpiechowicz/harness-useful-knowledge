//! `docs comments --json ID` and `docs comment reply|resolve|unresolve|delete`: the comments of reviews.
//!
//! - `docs comments --json ID` prints `{"comments": [...]}` in the order they were written. A comment
//!   is `{"id", "reviewId", "section", "occurrence", "side", "from", "to", "body", "by", "at",
//!   "parentId", "resolvedAt", "resolvedBy", "revision", "outdated", "quote"}`: `section`, `occurrence`
//!   (which of the sections that share the heading `section`, from 1), `side`, `from` and `to` are
//!   `null` for an overall comment of a review or a reply (`reviewId` is `null` for a reply, which
//!   answers `parentId`). `quote` is `[{"line", "kind": "added"|"removed"|"same", "text"}]`, the lines
//!   the comment was written on, or `null` (see [`super::quote`], which also says when a comment is
//!   `outdated`: its section is gone or its lines changed).
//!   The comments of a live document that has a pending version are the ones of both rows (the live
//!   row's first, and their line comments outdated: they were about the text now live).
//!   Going live removes the comments of the version that goes live, so a live document with no pending version has none.
//! - `docs comment reply --json ID COMMENT_ID --body TEXT` answers a comment (1 to 4000 characters,
//!   not a reply), `docs comment resolve|unresolve --json ID COMMENT_ID` marks one resolved or open
//!   again. Both print `{"comment": {...}}` as it is now. Anyone may reply and resolve, the author
//!   included.
//!
//! - `docs comment delete --json ID COMMENT_ID` deletes a comment and prints `{"deleted": [ID, ...]}`:
//!   the ids of the comment and, when it starts a thread, of its replies. Only the one who wrote the
//!   comment may (`notAuthor` otherwise, `noName` without a git `user.name`), in any state of the
//!   document. A reply deletes only itself. The history keeps its events.
//!
//! `ID` and `COMMENT_ID` are as the commands print them (`12` or `local:12`).

use serde_json::{Value, json};

use super::quote::{Current, Verdict};
use super::review::{actor, failure, missing, open_to_change, open_to_read, reviewed_row};
use super::{Context, Done, Failure, Reply, parse_id, the_id};
use crate::cli::Args;
use crate::config::Place;
use crate::review::{self, Review};
use crate::store::routed::ids;
use crate::store::{Comment, Detail, EventKind, KnowledgeStore};
use crate::support::time::rfc3339;

/// `docs comments`.
pub(super) fn comments(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json"], &[]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let id = the_id(&parsed)?;
    let mut store = open_to_read(context, id)?;

    comments_of(store.as_mut(), id)
}

/// The comments of row `id` of `store`.
pub(super) fn comments_of(store: &mut dyn KnowledgeStore, id: i64) -> Reply {
    let pending = reviewed_row(store, id).map_err(failure)?;
    let mut shown: Vec<Value> = Vec::new();

    // The rounds that went live stay on the live row: they are behind a pending version now.
    shown.extend(row_comments(store, id, pending != id)?);

    if pending != id {
        shown.extend(row_comments(store, pending, false)?);
    }

    Ok(Done::ok(json!({ "comments": shown })))
}

/// The comments of row `row` as JSON. `behind` says a pending version replaces the text they were
/// written on, so their lines are outdated whatever they say. The sections of the row are read once,
/// and only when it has a line comment.
fn row_comments(store: &mut dyn KnowledgeStore, row: i64, behind: bool) -> Result<Vec<Value>, Failure> {
    let comments = store.comments(row)?;
    let mut context = Read::default();

    comments
        .iter()
        .map(|comment| {
            let outdated = outdated(store, row, comment, behind, &mut context)?;

            Ok(json_comment(comment, outdated))
        })
        .collect()
}

/// What is read once for the comments of a row.
#[derive(Default)]
struct Read {
    current: Option<Current>,
    content: Option<i64>,
}

/// Whether `comment` of row `row` is outdated (see [`super::quote`]). `behind`: the row's text is
/// replaced by a pending version.
fn outdated(
    store: &mut dyn KnowledgeStore,
    row: i64,
    comment: &Comment,
    behind: bool,
    context: &mut Read,
) -> Result<bool, Failure> {
    if comment.section.is_none() {
        return Ok(false);
    }

    if behind {
        return Ok(true);
    }

    if context.current.is_none() {
        context.current = Some(Current::load(store, row)?);
    }

    let verdict = context.current.as_ref().map(|current| current.verdict(comment));

    match verdict {
        Some(Verdict::Known(outdated)) => Ok(outdated),
        Some(Verdict::ByRevision) => {
            let content = match context.content {
                Some(content) => content,
                None => {
                    let content = content_revision(store, row)?;
                    context.content = Some(content);

                    content
                }
            };

            Ok(comment.revision < content)
        }
        _ => Ok(false),
    }
}

/// The revision row `id` had when its content last changed: a comment written at an older
/// revision is outdated. Every transition (ask, request changes, approve, go live) raises the
/// revision by one without changing the content, so the transitions the history lists after the
/// latest change of content (`drafted`, `edited`, `updateDrafted`) are taken off the current
/// revision. A row whose history shows no change of content (it was made live by an import, which
/// records none) counts its current revision. `notFound` for a row that does not exist.
fn content_revision(store: &mut dyn KnowledgeStore, id: i64) -> Result<i64, Failure> {
    let detail = Detail {
        drafts: true,
        ..Detail::default()
    };
    let current = store.get(id, &detail)?.ok_or_else(|| missing(id))?.doc.review_rev;
    let history = store.history(id)?.unwrap_or_default();

    let mut transitions = 0;

    for event in &history.events {
        match event.kind {
            EventKind::Drafted | EventKind::Edited | EventKind::UpdateDrafted => {
                return Ok((current - transitions).max(0));
            }
            _ => transitions += 1,
        }
    }

    Ok(current)
}

/// A comment as JSON. `outdated` says the document changed since it was written.
fn json_comment(comment: &Comment, outdated: bool) -> Value {
    json!({
        "id": ids::json(comment.id),
        "reviewId": comment.review_id,
        "section": comment.section,
        "occurrence": comment.occurrence,
        "side": comment.side.map(|side| side.as_str()),
        "from": comment.line_from,
        "to": comment.line_to,
        "body": comment.body,
        "by": comment.by,
        "at": rfc3339(comment.at),
        "parentId": comment.parent_id.map(ids::json),
        "resolvedAt": comment.resolved_at.map(rfc3339),
        "resolvedBy": comment.resolved_by,
        "revision": comment.revision,
        "outdated": outdated,
        "quote": comment.quote.as_ref().map(|quote| {
            quote
                .iter()
                .map(|line| {
                    let mut shown = json!({ "line": line.line, "kind": line.kind.as_str(), "text": line.text });

                    if line.cut {
                        shown["cut"] = json!(true);
                    }

                    shown
                })
                .collect::<Vec<Value>>()
        }),
    })
}

/// `docs comment reply|resolve|unresolve|delete`.
pub(super) fn comment(context: &Context, verb: &str, args: &[Option<&str>]) -> Reply {
    let valued: &[&str] = if verb == "reply" { &["--body"] } else { &[] };
    let parsed = Args::parse(args, &["--json"], valued).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let [id, comment] = parsed.positional.as_slice() else {
        return Err(Failure::Usage);
    };
    let id = parse_id(id)?;
    let comment = parse_id(comment)?;
    let body = match verb {
        "reply" => Some(parsed.value("--body").ok_or(Failure::Usage)?),
        _ => None,
    };

    let place = review::place_of(context.routing, id).map_err(failure)?;
    let mut store = open_to_change(context, id)?;

    if verb == "delete" {
        return delete_on(store.as_mut(), place, actor(context).as_deref(), id, comment);
    }

    comment_on(
        store.as_mut(),
        place,
        actor(context).as_deref(),
        verb,
        id,
        comment,
        body,
    )
}

/// A reply to, or the resolving of, comment `comment` of row `id` of `store`, kept in `place`.
pub(super) fn comment_on(
    store: &mut dyn KnowledgeStore,
    place: Place,
    actor: Option<&str>,
    verb: &str,
    id: i64,
    comment: i64,
    body: Option<&str>,
) -> Reply {
    // The comments of a live row's pending version are asked for by the live id: find the row that has it.
    let row = row_of_comment(store, id, comment).map_err(failure)?;
    let mut review = Review::new(store, place, actor);

    let changed = match (verb, body) {
        ("reply", Some(body)) => review.reply(row, comment, body).map_err(failure)?,
        _ => {
            review.resolve(row, comment, verb == "resolve").map_err(failure)?;
            comment
        }
    };

    let found = store
        .comments(row)?
        .into_iter()
        .find(|stored| same_comment(stored.id, changed))
        .ok_or_else(|| missing(row))?;
    let outdated = outdated(store, row, &found, false, &mut Read::default())?;

    Ok(Done::ok(json!({ "comment": json_comment(&found, outdated) })))
}

/// The deleting of comment `comment` of row `id` of `store`, kept in `place`, by `actor`.
pub(super) fn delete_on(
    store: &mut dyn KnowledgeStore,
    place: Place,
    actor: Option<&str>,
    id: i64,
    comment: i64,
) -> Reply {
    let row = row_of_comment(store, id, comment).map_err(failure)?;
    let deleted = Review::new(store, place, actor)
        .delete_comment(row, comment)
        .map_err(failure)?;
    let shown: Vec<Value> = deleted.into_iter().map(ids::json).collect();

    Ok(Done::ok(json!({ "deleted": shown })))
}

/// The row that holds comment `comment` of the document `id`: `id` itself, or the pending version
/// of the live row `id` when the comment is there.
fn row_of_comment(store: &mut dyn KnowledgeStore, id: i64, comment: i64) -> Result<i64, review::Error> {
    let pending = reviewed_row(store, id)?;

    if pending == id {
        return Ok(id);
    }

    let on_pending = store
        .comments(pending)?
        .iter()
        .any(|stored| same_comment(stored.id, comment));

    Ok(if on_pending { pending } else { id })
}

/// Whether two ids name the same comment: a routed store tags the ids it prints, a person may write them plain.
fn same_comment(stored: i64, named: i64) -> bool {
    ids::split(stored).1 == ids::split(named).1
}
