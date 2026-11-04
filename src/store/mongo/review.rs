//! The review of drafts: transitions, promote, history, snapshots, comments and original files.
//!
//! Every function runs inside one write (see [`super::write`]): in a transaction on a replica set,
//! where a promote is all or nothing like the other stores' transactions. Without transactions the
//! steps are ordered and a failed one leaves what it wrote (a standalone server is for tests: a team
//! database always has a replica set, as `mongot` needs one).

use std::collections::HashMap;

use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{Binary, Bson, Document as Row, doc};

use super::ctx::{Ctx, Fail};
use super::documents::{RowState, document, links, not_found, record, row_state, scope_fields};
use super::schema::{CHUNKS, COMMENTS, DOCUMENTS, EVENTS, FILE_PART_BYTES, FILES, REVIEW_LIVE, SNAPSHOTS};
use super::{int, opt_int, strings, text};
use crate::store::review::{
    Authorship, COMMENTS_PER_DOC_MAX, Gate, HISTORY_MAX, SNAPSHOT_BYTES_MAX, SNAPSHOTS_PER_DOC_MAX, check_file_for,
    gate, group_sections, quote_text, read_quote, sections_bytes,
};
use crate::store::{
    Comment, EventKind, History, NewComment, Piece, ReviewAction, ReviewChange, ReviewEvent, ReviewReply, Reviewed,
    SectionText, Side, Status, StoreError, StoredFile,
};

/// What a write came to, and whether it changed anything (a refusal changes nothing, so it does not
/// count in the generation).
pub(super) type Done<T> = (T, bool);

/// A stored count or position.
fn size(row: &Row, key: &str) -> usize {
    usize::try_from(int(row, key)).unwrap_or(0)
}

/// The sections of document `id` as stored.
pub(super) fn sections_of(ctx: &mut Ctx<'_>, id: i64) -> Result<Vec<SectionText>, Fail> {
    let projection = doc! { "heading_path": 1, "text": 1, "char_start": 1, "char_end": 1 };
    let rows = ctx.find(CHUNKS, doc! { "doc_id": id }, (doc! { "ordinal": 1 }, 0, 0), projection)?;

    let pieces: Vec<Piece> = rows
        .iter()
        .map(|row| Piece {
            heading_path: text(row, "heading_path").unwrap_or_default(),
            text: text(row, "text").unwrap_or_default(),
            char_start: size(row, "char_start"),
            char_end: size(row, "char_end"),
        })
        .collect();

    Ok(group_sections(&pieces))
}

fn exists(ctx: &mut Ctx<'_>, id: i64) -> Result<bool, Fail> {
    Ok(ctx
        .find_one(DOCUMENTS, doc! { "_id": id }, doc! { "_id": 1 })?
        .is_some())
}

/// The sections of document `id`; `None` when there is no such document.
pub(super) fn section_texts(ctx: &mut Ctx<'_>, id: i64) -> Result<Option<Vec<SectionText>>, Fail> {
    if !exists(ctx, id)? {
        return Ok(None);
    }

    sections_of(ctx, id).map(Some)
}

/// The sections kept when document `id` was asked at `revision`.
pub(super) fn asked_sections(ctx: &mut Ctx<'_>, id: i64, revision: i64) -> Result<Option<Vec<SectionText>>, Fail> {
    let found = ctx.find_one(
        SNAPSHOTS,
        doc! { "doc_id": id, "revision": revision },
        doc! { "sections": 1 },
    )?;

    Ok(found.and_then(|row| {
        row.get_array("sections")
            .ok()?
            .iter()
            .map(|entry| {
                let entry = entry.as_document()?;

                Some(SectionText {
                    heading: text(entry, "heading")?,
                    text: text(entry, "text")?,
                })
            })
            .collect()
    }))
}

/// Keeps the sections of document `id` as asked at `revision`, unless they are over the limit.
fn snapshot(ctx: &mut Ctx<'_>, id: i64, revision: i64) -> Result<(), Fail> {
    let sections = sections_of(ctx, id)?;

    if sections_bytes(&sections) > SNAPSHOT_BYTES_MAX {
        return Ok(());
    }

    let kept: Vec<Row> = sections
        .iter()
        .map(|section| doc! { "heading": &section.heading, "text": &section.text })
        .collect();

    ctx.update_one(
        SNAPSHOTS,
        doc! { "doc_id": id, "revision": revision },
        doc! { "$set": { "sections": kept } },
        true,
    )?;

    // Only the latest asks are kept.
    let old = ctx.find(
        SNAPSHOTS,
        doc! { "doc_id": id },
        (
            doc! { "revision": -1 },
            u64::try_from(SNAPSHOTS_PER_DOC_MAX).unwrap_or(u64::MAX),
            0,
        ),
        doc! { "_id": 1 },
    )?;

    if !old.is_empty() {
        let ids: Vec<Bson> = old.iter().filter_map(|row| row.get("_id").cloned()).collect();

        ctx.delete_many(SNAPSHOTS, doc! { "_id": { "$in": ids } })?;
    }

    Ok(())
}

/// The gate of `action` on the row `state` as the caller saw it at `expect`.
fn decide(state: Option<&RowState>, expect: i64, action: ReviewAction) -> Gate {
    gate(state.map(|row| (row.status, row.review_rev)), expect, action)
}

/// Applies a transition that does not make a pending version live. `by` and `comment` are the
/// cleaned ones of the change.
pub(super) fn set_review(
    ctx: &mut Ctx<'_>,
    id: i64,
    expect: i64,
    change: &ReviewChange,
    (by, comment): (Option<&str>, Option<&str>),
) -> Result<Done<ReviewReply>, Fail> {
    let state = row_state(ctx, id)?;

    if let Gate::Stop(reply) = decide(state.as_ref(), expect, change.action) {
        return Ok((reply, false));
    }

    let Some(state) = state else {
        return Ok((ReviewReply::NotFound, false));
    };

    if state.pending_of.is_some() && change.action.goes_live() {
        return Err(StoreError::Invalid(format!("document {id} is a pending version: promote it instead")).into());
    }

    let to = change.action.to();

    // The compare-and-set: it matches only a row that still is at the revision that was decided on.
    let moved = ctx.update_one(
        DOCUMENTS,
        doc! { "_id": id, "review_rev": expect },
        doc! { "$set": { "status": to.as_str() }, "$inc": { "review_rev": 1_i64 } },
        false,
    )?;

    if moved == 0 {
        // Someone else got there first: say what the row is now.
        let now = row_state(ctx, id)?;

        return match decide(now.as_ref(), expect, change.action) {
            Gate::Stop(reply) => Ok((reply, false)),
            Gate::Go => Err(StoreError::Busy.into()),
        };
    }

    // A draft that goes live gets chunks an agent's search sees.
    if change.action.goes_live() {
        ctx.update_many(
            CHUNKS,
            doc! { "doc_id": id },
            doc! { "$set": { "review": REVIEW_LIVE } },
        )?;

        // Its review is over: the comments are not kept.
        ctx.delete_many(COMMENTS, doc! { "doc_id": id })?;
    }

    record(ctx, id, change.action.event(), (by, comment), change.at)?;

    if change.action == ReviewAction::Ask {
        snapshot(ctx, id, expect + 1)?;
    }

    Ok((
        ReviewReply::Done(Reviewed {
            id,
            status: to,
            revision: expect + 1,
            promoted: false,
        }),
        true,
    ))
}

/// The fields of a document a promote copies from the pending row to the live one.
const PROMOTED: [&str; 13] = [
    "title",
    "title_key",
    "source_name",
    "format",
    "content_sha256",
    "file_sha256",
    "bytes",
    "chunk_count",
    "origin",
    "warnings",
    "updated_at",
    "updated_by_name",
    "updated_by_via",
];

/// Makes pending version `id` live: the live chunks are replaced by the pending ones (with their
/// embeddings, titled and scoped as the live document), the live row takes the pending row's
/// content and keeps its id, links, `created_at`, `added_by` and counters, the events, comments and
/// file move over, and the pending row is deleted.
pub(super) fn promote(
    ctx: &mut Ctx<'_>,
    id: i64,
    expect: i64,
    change: &ReviewChange,
    (by, comment): (Option<&str>, Option<&str>),
) -> Result<Done<ReviewReply>, Fail> {
    let state = row_state(ctx, id)?;

    if let Gate::Stop(reply) = decide(state.as_ref(), expect, change.action) {
        return Ok((reply, false));
    }

    let Some(live) = state.and_then(|row| row.pending_of) else {
        return Err(StoreError::Invalid(format!("document {id} is not a pending version")).into());
    };

    // A pending version replaces a live document; onto a row that is not live it would make it live
    // without a review.
    if row_state(ctx, live)?.is_none_or(|row| row.status != Status::Live) {
        return Err(StoreError::Invalid(format!(
            "document {live} is not live: its pending version {id} cannot be promoted onto it"
        ))
        .into());
    }

    let Some(pending_row) = ctx.find_one(DOCUMENTS, doc! { "_id": id }, Row::new())? else {
        return Ok((ReviewReply::NotFound, false));
    };
    let Some(live_row) = ctx.find_one(DOCUMENTS, doc! { "_id": live }, Row::new())? else {
        return Err(not_found(live).into());
    };

    let pending = document(&pending_row)?;
    let held = document(&live_row)?;
    let revision = held.review_rev.max(pending.review_rev) + 1;

    // The live chunks go first: a document has one chunk at each position.
    ctx.delete_many(CHUNKS, doc! { "doc_id": live })?;

    let mut set = Row::new();

    for key in PROMOTED {
        set.insert(key, pending_row.get(key).cloned().unwrap_or(Bson::Null));
    }

    set.insert("review_rev", revision);
    ctx.update_one(DOCUMENTS, doc! { "_id": live }, doc! { "$set": set }, false)?;

    // The pending chunks are the live document's now, titled and scoped like it.
    let mut chunks = scope_fields(&links(&live_row));
    chunks.insert("doc_id", live);
    chunks.insert("review", REVIEW_LIVE);
    ctx.update_many(CHUNKS, doc! { "doc_id": id }, doc! { "$set": chunks })?;

    ctx.update_many(EVENTS, doc! { "doc_id": id }, doc! { "$set": { "doc_id": live } })?;

    // The review is over: the comments of both versions go, the history keeps its record.
    ctx.delete_many(COMMENTS, doc! { "doc_id": { "$in": [live, id] } })?;

    // What was stored with the live document was its own original: the new content has its own.
    ctx.delete_many(FILES, doc! { "doc_id": live })?;
    ctx.update_many(FILES, doc! { "doc_id": id }, doc! { "$set": { "doc_id": live } })?;
    ctx.delete_many(SNAPSHOTS, doc! { "doc_id": id })?;

    record(ctx, live, change.action.event(), (by, comment), change.at)?;

    ctx.delete_many(DOCUMENTS, doc! { "_id": id })?;

    Ok((
        ReviewReply::Done(Reviewed {
            id: live,
            status: Status::Live,
            revision,
            promoted: true,
        }),
        true,
    ))
}

/// The newest events of document `id`; `None` when there is no such document.
pub(super) fn history(ctx: &mut Ctx<'_>, id: i64) -> Result<Option<History>, Fail> {
    if !exists(ctx, id)? {
        return Ok(None);
    }

    let limit = i64::try_from(HISTORY_MAX).unwrap_or(i64::MAX);
    let rows = ctx.find(
        EVENTS,
        doc! { "doc_id": id },
        (doc! { "_id": -1 }, 0, limit),
        Row::new(),
    )?;

    let events = rows
        .iter()
        .filter_map(|row| {
            Some(ReviewEvent {
                kind: EventKind::parse(&text(row, "kind")?)?,
                by: text(row, "by"),
                at: int(row, "at"),
                comment: text(row, "comment"),
            })
        })
        .collect();

    let total = ctx.count(EVENTS, doc! { "doc_id": id })?;

    Ok(Some(History {
        events,
        total: usize::try_from(total).unwrap_or(usize::MAX),
    }))
}

/// The names that wrote document `id` since it was last live, oldest first.
pub(super) fn writers_since_live(ctx: &mut Ctx<'_>, id: i64) -> Result<Vec<String>, Fail> {
    let live = ctx.find(
        EVENTS,
        doc! { "doc_id": id, "kind": { "$in": ["approved", "wentLive"] } },
        (doc! { "_id": -1 }, 0, 1),
        doc! { "_id": 1 },
    )?;
    let boundary = live.first().map_or(0, |row| int(row, "_id"));

    let rows = ctx.find(
        EVENTS,
        doc! {
            "doc_id": id,
            "_id": { "$gt": boundary },
            "kind": { "$in": ["drafted", "edited", "updateDrafted"] },
            "by": { "$type": "string" },
        },
        (doc! { "_id": 1 }, 0, 0),
        doc! { "by": 1 },
    )?;

    let mut names: Vec<String> = Vec::new();

    for name in rows.iter().filter_map(|row| text(row, "by")) {
        if !names.contains(&name) {
            names.push(name);
        }
    }

    Ok(names)
}

/// The writers (at most `per_doc` each) and the latest asker of the documents `ids`, in a few
/// queries (not one per document).
pub(super) fn authorship_of(ctx: &mut Ctx<'_>, ids: &[i64], per_doc: usize) -> Result<HashMap<i64, Authorship>, Fail> {
    let mut found: HashMap<i64, Authorship> = ids.iter().map(|id| (*id, Authorship::default())).collect();

    if ids.is_empty() {
        return Ok(found);
    }

    // Where each document was last made live: what came after is its current version. One row a
    // document, however many times it went live.
    let mut boundary: HashMap<i64, i64> = HashMap::new();
    let lives = ctx.aggregate(
        EVENTS,
        vec![
            doc! { "$match": { "doc_id": { "$in": ids }, "kind": { "$in": ["approved", "wentLive"] } } },
            doc! { "$group": { "_id": "$doc_id", "last": { "$max": "$_id" } } },
        ],
    )?;

    for row in &lives {
        boundary.insert(int(row, "_id"), int(row, "last"));
    }

    let after: Vec<Bson> = ids
        .iter()
        .map(|id| Bson::Document(doc! { "doc_id": *id, "_id": { "$gt": boundary.get(id).copied().unwrap_or(0) } }))
        .collect();

    // Each document's first `per_doc` writers, cut inside the pipeline: a document with a crowd of
    // writers cannot use up what another document of the page is given.
    let writers = ctx.aggregate(
        EVENTS,
        vec![
            doc! { "$match": {
                "$or": after,
                "kind": { "$in": ["drafted", "edited", "updateDrafted"] },
                "by": { "$type": "string" },
            } },
            doc! { "$group": { "_id": { "d": "$doc_id", "n": "$by" }, "first": { "$min": "$_id" } } },
            doc! { "$sort": { "first": 1 } },
            doc! { "$group": { "_id": "$_id.d", "names": { "$push": "$_id.n" } } },
            doc! { "$project": { "names": { "$slice": ["$names", i64::try_from(per_doc).unwrap_or(i64::MAX)] } } },
        ],
    )?;

    for row in &writers {
        if let Some(entry) = found.get_mut(&int(row, "_id")) {
            entry.writers = strings(row, "names");
        }
    }

    // The newest request of each, if it came after the version was last made live.
    let asked = ctx.aggregate(
        EVENTS,
        vec![
            doc! { "$match": { "doc_id": { "$in": ids }, "kind": "asked" } },
            doc! { "$sort": { "_id": -1 } },
            doc! { "$group": { "_id": "$doc_id", "event": { "$first": "$_id" }, "by": { "$first": "$by" } } },
        ],
    )?;

    for row in &asked {
        let doc = int(row, "_id");

        if int(row, "event") > boundary.get(&doc).copied().unwrap_or(0)
            && let Some(entry) = found.get_mut(&doc)
        {
            entry.asker = text(row, "by");
        }
    }

    Ok(found)
}

/// Refuses a document that cannot take more comments.
fn room_for(ctx: &mut Ctx<'_>, id: i64, more: usize) -> Result<(), Fail> {
    let have = ctx.count(COMMENTS, doc! { "doc_id": id })?;

    if usize::try_from(have).unwrap_or(usize::MAX).saturating_add(more) > COMMENTS_PER_DOC_MAX {
        return Err(StoreError::Invalid(format!("a document keeps at most {COMMENTS_PER_DOC_MAX} comments")).into());
    }

    Ok(())
}

/// Adds the cleaned `comments` of one review to document `id` and returns their ids.
pub(super) fn add_comments(
    ctx: &mut Ctx<'_>,
    id: i64,
    by: Option<&str>,
    at: i64,
    revision: i64,
    comments: &[NewComment],
) -> Result<Vec<i64>, Fail> {
    if row_state(ctx, id)?.is_none() {
        return Err(not_found(id).into());
    }

    room_for(ctx, id, comments.len())?;

    // Review numbers and comment ids come from counters shared by every document, so they never repeat.
    let review = ctx.reserve_ids("reviews", 1)?;
    let first = ctx.reserve_ids("comments", comments.len())?;

    let rows: Vec<Row> = comments
        .iter()
        .zip(first..)
        .map(|(comment, comment_id)| {
            let mut row = doc! {
                "_id": comment_id,
                "doc_id": id,
                "review_id": review,
                "body": &comment.body,
                "by_name": by,
                "at": at,
                "revision": revision,
            };

            if let Some(section) = &comment.section {
                row.insert("section", section);
            }

            if let Some(occurrence) = comment.occurrence {
                row.insert("occurrence", i64::from(occurrence));
            }

            if let Some(quote) = quote_text(comment.quote.as_deref()) {
                row.insert("quote", quote);
            }

            if let Some(side) = comment.side {
                row.insert("side", side.as_str());
            }

            if let Some(from) = comment.line_from {
                row.insert("line_from", i64::from(from));
            }

            if let Some(to) = comment.line_to {
                row.insert("line_to", i64::from(to));
            }

            row
        })
        .collect();

    ctx.insert_many(COMMENTS, rows)?;

    Ok((first..).take(comments.len()).collect())
}

/// Adds a reply to `parent`; `None` when it cannot be answered.
pub(super) fn reply_comment(
    ctx: &mut Ctx<'_>,
    id: i64,
    parent: i64,
    by: Option<&str>,
    at: i64,
    body: &str,
) -> Result<Done<Option<i64>>, Fail> {
    let Some(state) = row_state(ctx, id)? else {
        return Err(not_found(id).into());
    };

    room_for(ctx, id, 1)?;

    // A reply answers a comment of this document that is not itself a reply.
    let found = ctx.find_one(
        COMMENTS,
        doc! { "_id": parent, "doc_id": id, "parent_id": Bson::Null },
        doc! { "_id": 1 },
    )?;

    if found.is_none() {
        return Ok((None, false));
    }

    let comment_id = ctx.reserve_ids("comments", 1)?;
    let row = doc! {
        "_id": comment_id,
        "doc_id": id,
        "parent_id": parent,
        "body": body,
        "by_name": by,
        "at": at,
        "revision": state.review_rev,
    };

    ctx.insert_many(COMMENTS, vec![row])?;

    Ok((Some(comment_id), true))
}

/// Resolves or reopens `comment`; false when it cannot be.
pub(super) fn resolve_comment(
    ctx: &mut Ctx<'_>,
    id: i64,
    comment: i64,
    resolved: bool,
    by: Option<&str>,
    at: i64,
) -> Result<Done<bool>, Fail> {
    let update = if resolved {
        doc! { "$set": { "resolved_at": at, "resolved_by": by } }
    } else {
        doc! { "$unset": { "resolved_at": "", "resolved_by": "" } }
    };

    let matched = ctx.update_one(
        COMMENTS,
        doc! { "_id": comment, "doc_id": id, "parent_id": Bson::Null },
        update,
        false,
    )?;

    Ok((matched > 0, matched > 0))
}

/// Deletes `comment` and, when it starts a thread, its replies: the ids deleted, the comment
/// first; `None` when the document has no such comment.
pub(super) fn delete_comment(ctx: &mut Ctx<'_>, id: i64, comment: i64) -> Result<Done<Option<Vec<i64>>>, Fail> {
    let thread = doc! { "doc_id": id, "$or": [{ "_id": comment }, { "parent_id": comment }] };
    let rows = ctx.find(COMMENTS, thread.clone(), (doc! { "_id": 1 }, 0, 0), Row::new())?;

    if rows.is_empty() {
        return Ok((None, false));
    }

    ctx.delete_many(COMMENTS, thread)?;

    Ok((Some(rows.iter().map(|row| int(row, "_id")).collect()), true))
}

/// A stored comment.
fn comment(row: &Row) -> Comment {
    Comment {
        id: int(row, "_id"),
        review_id: opt_int(row, "review_id"),
        section: text(row, "section"),
        occurrence: opt_int(row, "occurrence").and_then(|count| u32::try_from(count).ok()),
        side: text(row, "side").as_deref().and_then(Side::parse),
        line_from: opt_int(row, "line_from").and_then(|line| u32::try_from(line).ok()),
        line_to: opt_int(row, "line_to").and_then(|line| u32::try_from(line).ok()),
        body: text(row, "body").unwrap_or_default(),
        by: text(row, "by_name"),
        at: int(row, "at"),
        parent_id: opt_int(row, "parent_id"),
        resolved_at: opt_int(row, "resolved_at"),
        resolved_by: text(row, "resolved_by"),
        revision: int(row, "revision"),
        quote: read_quote(text(row, "quote").as_deref()),
    }
}

/// Every comment of document `id`, in the order they were written.
pub(super) fn comments(ctx: &mut Ctx<'_>, id: i64) -> Result<Vec<Comment>, Fail> {
    let limit = i64::try_from(COMMENTS_PER_DOC_MAX).unwrap_or(i64::MAX);
    let rows = ctx.find(
        COMMENTS,
        doc! { "doc_id": id },
        (doc! { "_id": 1 }, 0, limit),
        Row::new(),
    )?;

    Ok(rows.iter().map(comment).collect())
}

/// Keeps `bytes` as the file `name` of document `id` (hash `sha256`), replacing the one it has, in
/// parts of at most [`FILE_PART_BYTES`]; false when there is no such document.
pub(super) fn put_file(ctx: &mut Ctx<'_>, id: i64, name: &str, bytes: &[u8], sha256: &str) -> Result<Done<bool>, Fail> {
    let Some(state) = row_state(ctx, id)? else {
        return Ok((false, false));
    };

    check_file_for(&state.format, bytes)?;

    ctx.delete_many(FILES, doc! { "doc_id": id })?;

    // An empty file is one empty part, so its name and hash are kept.
    let parts: Vec<&[u8]> = if bytes.is_empty() {
        vec![bytes]
    } else {
        bytes.chunks(FILE_PART_BYTES).collect()
    };

    for (part, piece) in parts.into_iter().enumerate() {
        let mut row = doc! {
            "doc_id": id,
            "part": i64::try_from(part).unwrap_or(i64::MAX),
            "bytes": Bson::Binary(Binary { subtype: BinarySubtype::Generic, bytes: piece.to_vec() }),
        };

        if part == 0 {
            row.insert("name", name);
            row.insert("size", i64::try_from(bytes.len()).unwrap_or(i64::MAX));
            row.insert("sha256", sha256);
        }

        // One request per part: a part is up to 8 MiB.
        ctx.insert_many(FILES, vec![row])?;
    }

    Ok((true, true))
}

/// The file of document `id`.
pub(super) fn file(ctx: &mut Ctx<'_>, id: i64) -> Result<Option<StoredFile>, Fail> {
    let rows = ctx.find(FILES, doc! { "doc_id": id }, (doc! { "part": 1 }, 0, 0), Row::new())?;

    let Some(first) = rows.first() else {
        return Ok(None);
    };

    let incomplete = || StoreError::Backend("a stored file is incomplete".to_owned());
    let (Some(name), Some(sha256)) = (text(first, "name"), text(first, "sha256")) else {
        return Err(incomplete().into());
    };
    let expected = size(first, "size");
    let mut bytes = Vec::with_capacity(expected);

    for (index, row) in rows.iter().enumerate() {
        match row.get("bytes") {
            Some(Bson::Binary(part)) if usize::try_from(int(row, "part")) == Ok(index) => {
                bytes.extend_from_slice(&part.bytes);
            }
            _ => return Err(incomplete().into()),
        }
    }

    if bytes.len() != expected {
        return Err(incomplete().into());
    }

    Ok(Some(StoredFile { name, bytes, sha256 }))
}
