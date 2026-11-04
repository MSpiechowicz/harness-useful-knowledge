//! The review of drafts: transitions, promote, history, snapshots, comments and original files. The
//! semantics are the local file's (see [`crate::store::sqlite`]'s `review` module) and the checks
//! before a write are shared with it ([`crate::store::review`]).
//!
//! Every function runs in the write transaction of [`super::PostgresStore::write_if`] (or on a plain
//! client for a read) and says whether it changed anything, so a refusal is no write and does not
//! count in the generation.

use std::collections::HashMap;

use postgres::GenericClient;

use super::documents::{not_found, record, row_state};
use super::error::{backend, column, count, int};
use crate::store::review::{
    Authorship, COMMENTS_PER_DOC_MAX, Gate, HISTORY_MAX, SNAPSHOT_BYTES_MAX, SNAPSHOTS_PER_DOC_MAX, check_file_for,
    gate, group_sections, quote_text, read_quote, sections_bytes,
};
use crate::store::sqlite::sql_review::{piece, side, snapshot_json, snapshot_sections};
use crate::store::{
    Columns, Comment, EventKind, History, NewComment, ReviewAction, ReviewChange, ReviewEvent, ReviewReply, Reviewed,
    SectionText, Status, StoreError, StoredFile,
};

/// What a write came to, and whether it changed anything.
pub(super) type Done<T> = (T, bool);

/// The sections of document `id` as stored.
pub(super) fn sections_of(client: &mut impl GenericClient, id: i64) -> Result<Vec<SectionText>, StoreError> {
    let rows = client
        .query(
            "SELECT heading_path, text, char_start, char_end FROM knowledge_chunks WHERE doc_id = $1 ORDER BY ordinal",
            &[&id],
        )
        .map_err(backend)?;

    let pieces = rows
        .iter()
        .map(|row| Ok(piece(column(row, 0)?, column(row, 1)?, count(row, 2)?, count(row, 3)?)))
        .collect::<Result<Vec<_>, StoreError>>()?;

    Ok(group_sections(&pieces))
}

fn exists(client: &mut impl GenericClient, id: i64) -> Result<bool, StoreError> {
    let found = client
        .query_opt("SELECT 1 FROM knowledge_documents WHERE id = $1", &[&id])
        .map_err(backend)?;

    Ok(found.is_some())
}

/// The sections of document `id`; `None` when there is no such document.
pub(super) fn section_texts(client: &mut impl GenericClient, id: i64) -> Result<Option<Vec<SectionText>>, StoreError> {
    if !exists(client, id)? {
        return Ok(None);
    }

    sections_of(client, id).map(Some)
}

/// The sections kept when document `id` was asked at `revision`.
pub(super) fn asked_sections(
    client: &mut impl GenericClient,
    id: i64,
    revision: i64,
) -> Result<Option<Vec<SectionText>>, StoreError> {
    let row = client
        .query_opt(
            "SELECT sections FROM knowledge_doc_snapshots WHERE doc_id = $1 AND revision = $2",
            &[&id, &revision],
        )
        .map_err(backend)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let json: String = column(&row, 0)?;

    Ok(snapshot_sections(&json))
}

/// Keeps the sections of document `id` as asked at `revision`, unless they are over the limit.
fn snapshot(tx: &mut impl GenericClient, id: i64, revision: i64) -> Result<(), StoreError> {
    let sections = sections_of(tx, id)?;

    if sections_bytes(&sections) > SNAPSHOT_BYTES_MAX {
        return Ok(());
    }

    tx.execute(
        "INSERT INTO knowledge_doc_snapshots (doc_id, revision, sections) VALUES ($1, $2, $3)
         ON CONFLICT (doc_id, revision) DO UPDATE SET sections = excluded.sections",
        &[&id, &revision, &snapshot_json(&sections)],
    )
    .map_err(backend)?;

    tx.execute(
        "DELETE FROM knowledge_doc_snapshots WHERE doc_id = $1 AND revision NOT IN
             (SELECT revision FROM knowledge_doc_snapshots WHERE doc_id = $1 ORDER BY revision DESC LIMIT $2)",
        &[&id, &int(SNAPSHOTS_PER_DOC_MAX)],
    )
    .map_err(backend)?;

    Ok(())
}

/// Applies a transition that does not make a pending version live. `by` and `comment` are the
/// cleaned ones of the change.
pub(super) fn set_review(
    tx: &mut impl GenericClient,
    id: i64,
    expect: i64,
    change: &ReviewChange,
    (by, comment): (Option<&str>, Option<&str>),
) -> Result<Done<ReviewReply>, StoreError> {
    let state = row_state(tx, id)?;

    if let Gate::Stop(reply) = gate(
        state.as_ref().map(|row| (row.status, row.review_rev)),
        expect,
        change.action,
    ) {
        return Ok((reply, false));
    }

    if state.is_some_and(|row| row.pending_of.is_some()) && change.action.goes_live() {
        return Err(StoreError::Invalid(format!(
            "document {id} is a pending version: promote it instead"
        )));
    }

    let to = change.action.to();
    let moved = tx
        .execute(
            "UPDATE knowledge_documents SET status = $2, review_rev = review_rev + 1 WHERE id = $1 AND review_rev = $3",
            &[&id, &to.as_str(), &expect],
        )
        .map_err(backend)?;

    if moved == 0 {
        return Err(not_found(id));
    }

    // A version that goes live has had its review: its comments are not kept.
    if change.action.goes_live() {
        tx.execute("DELETE FROM knowledge_doc_comments WHERE doc_id = $1", &[&id])
            .map_err(backend)?;
    }

    record(tx, id, change.action.event(), by, comment, change.at)?;

    if change.action == ReviewAction::Ask {
        snapshot(tx, id, expect + 1)?;
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

/// Makes pending version `id` live in the transaction `tx`.
pub(super) fn promote(
    tx: &mut impl GenericClient,
    columns: Columns,
    id: i64,
    expect: i64,
    change: &ReviewChange,
    (by, comment): (Option<&str>, Option<&str>),
) -> Result<Done<ReviewReply>, StoreError> {
    let state = row_state(tx, id)?;

    if let Gate::Stop(reply) = gate(
        state.as_ref().map(|row| (row.status, row.review_rev)),
        expect,
        change.action,
    ) {
        return Ok((reply, false));
    }

    let Some(live) = state.and_then(|row| row.pending_of) else {
        return Err(StoreError::Invalid(format!("document {id} is not a pending version")));
    };

    // A pending version replaces a live document; onto a row that is not live it would make it live
    // without a review.
    if row_state(tx, live)?.is_none_or(|row| row.status != Status::Live) {
        return Err(StoreError::Invalid(format!(
            "document {live} is not live: its pending version {id} cannot be promoted onto it"
        )));
    }

    // The live chunks go first (the pending ones take their ordinals), then the live row takes the
    // pending row's content, and the pending chunks, events, comments and file move over.
    tx.execute("DELETE FROM knowledge_chunks WHERE doc_id = $1", &[&live])
        .map_err(backend)?;

    let authors = if columns.authors {
        ", updated_by_name = p.updated_by_name, updated_by_via = p.updated_by_via"
    } else {
        ""
    };
    let row = tx
        .query_one(
            &format!(
                "UPDATE knowledge_documents l SET title = p.title, source_name = p.source_name, format = p.format,
                     content_sha256 = p.content_sha256, file_sha256 = p.file_sha256, bytes = p.bytes,
                     chunk_count = p.chunk_count, origin = p.origin, warnings = p.warnings,
                     updated_at = p.updated_at{authors},
                     review_rev = GREATEST(l.review_rev, p.review_rev) + 1
                 FROM knowledge_documents p WHERE p.id = $2 AND l.id = $1
                 RETURNING l.review_rev"
            ),
            &[&live, &id],
        )
        .map_err(backend)?;
    let revision: i64 = column(&row, 0)?;

    for sql in [
        "UPDATE knowledge_chunks SET doc_id = $1 WHERE doc_id = $2",
        "UPDATE knowledge_doc_events SET doc_id = $1 WHERE doc_id = $2",
    ] {
        tx.execute(sql, &[&live, &id]).map_err(backend)?;
    }

    // The review is over: the comments of both versions go, the history keeps its record.
    tx.execute(
        "DELETE FROM knowledge_doc_comments WHERE doc_id IN ($1, $2)",
        &[&live, &id],
    )
    .map_err(backend)?;

    // The pending file is the original of the new content.
    tx.execute("DELETE FROM knowledge_doc_files WHERE doc_id = $1", &[&live])
        .map_err(backend)?;
    tx.execute(
        "UPDATE knowledge_doc_files SET doc_id = $1 WHERE doc_id = $2",
        &[&live, &id],
    )
    .map_err(backend)?;

    // What is left of the pending row (its links and snapshots) goes with it.
    tx.execute("DELETE FROM knowledge_documents WHERE id = $1", &[&id])
        .map_err(backend)?;

    record(tx, live, change.action.event(), by, comment, change.at)?;

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
pub(super) fn history(client: &mut impl GenericClient, id: i64) -> Result<Option<History>, StoreError> {
    if !exists(client, id)? {
        return Ok(None);
    }

    let rows = client
        .query(
            "SELECT kind, by_name, comment, at FROM knowledge_doc_events WHERE doc_id = $1 ORDER BY id DESC LIMIT $2",
            &[&id, &int(HISTORY_MAX)],
        )
        .map_err(backend)?;

    let mut events = Vec::new();

    for row in &rows {
        let kind: String = column(row, 0)?;

        if let Some(kind) = EventKind::parse(&kind) {
            events.push(ReviewEvent {
                kind,
                by: column(row, 1)?,
                at: column(row, 3)?,
                comment: column(row, 2)?,
            });
        }
    }

    let total = client
        .query_one("SELECT count(*) FROM knowledge_doc_events WHERE doc_id = $1", &[&id])
        .map_err(backend)?;

    Ok(Some(History {
        events,
        total: count(&total, 0)?,
    }))
}

/// The names that wrote document `id` since it was last live, oldest first.
pub(super) fn writers_since_live(client: &mut impl GenericClient, id: i64) -> Result<Vec<String>, StoreError> {
    let rows = client
        .query(
            "SELECT by_name FROM knowledge_doc_events
             WHERE doc_id = $1 AND kind IN ('drafted', 'edited', 'updateDrafted') AND by_name IS NOT NULL
               AND id > COALESCE((SELECT max(id) FROM knowledge_doc_events
                                  WHERE doc_id = $1 AND kind IN ('approved', 'wentLive')), 0)
             GROUP BY by_name ORDER BY min(id)",
            &[&id],
        )
        .map_err(backend)?;

    rows.iter().map(|row| column(row, 0)).collect()
}

/// The writers (at most `per_doc` each) and the latest asker of the documents `ids`, in two queries.
pub(super) fn authorship_of(
    client: &mut impl GenericClient,
    ids: &[i64],
    per_doc: usize,
) -> Result<HashMap<i64, Authorship>, StoreError> {
    let mut found: HashMap<i64, Authorship> = HashMap::new();

    if ids.is_empty() {
        return Ok(found);
    }

    let ids = ids.to_vec();
    let writers = client
        .query(
            "SELECT doc_id, by_name FROM (
                 SELECT e.doc_id, e.by_name,
                        row_number() OVER (PARTITION BY e.doc_id ORDER BY min(e.id)) AS n
                 FROM knowledge_doc_events e
                 WHERE e.doc_id = ANY($1) AND e.kind IN ('drafted', 'edited', 'updateDrafted')
                   AND e.by_name IS NOT NULL
                   AND e.id > COALESCE((SELECT max(l.id) FROM knowledge_doc_events l
                                        WHERE l.doc_id = e.doc_id AND l.kind IN ('approved', 'wentLive')), 0)
                 GROUP BY e.doc_id, e.by_name
             ) t WHERE n <= $2 ORDER BY doc_id, n",
            &[&ids, &int(per_doc)],
        )
        .map_err(backend)?;

    for row in &writers {
        let doc: i64 = column(row, 0)?;
        found.entry(doc).or_default().writers.push(column(row, 1)?);
    }

    let askers = client
        .query(
            "SELECT e.doc_id, e.by_name FROM knowledge_doc_events e
             WHERE e.doc_id = ANY($1) AND e.kind = 'asked'
               AND e.id = (SELECT max(a.id) FROM knowledge_doc_events a WHERE a.doc_id = e.doc_id AND a.kind = 'asked')
               AND e.id > COALESCE((SELECT max(l.id) FROM knowledge_doc_events l
                                    WHERE l.doc_id = e.doc_id AND l.kind IN ('approved', 'wentLive')), 0)",
            &[&ids],
        )
        .map_err(backend)?;

    for row in &askers {
        let doc: i64 = column(row, 0)?;
        found.entry(doc).or_default().asker = column(row, 1)?;
    }

    // A document that exists but has neither is a document with nobody: only the missing ones are
    // left out (the caller asked for rows it listed).
    for id in ids {
        found.entry(id).or_default();
    }

    Ok(found)
}

/// Refuses a document that cannot take more comments.
fn room_for(tx: &mut impl GenericClient, id: i64, more: usize) -> Result<(), StoreError> {
    let row = tx
        .query_one("SELECT count(*) FROM knowledge_doc_comments WHERE doc_id = $1", &[&id])
        .map_err(backend)?;

    if count(&row, 0)?.saturating_add(more) > COMMENTS_PER_DOC_MAX {
        return Err(StoreError::Invalid(format!(
            "a document keeps at most {COMMENTS_PER_DOC_MAX} comments"
        )));
    }

    Ok(())
}

/// Adds the cleaned `comments` of one review to document `id` and returns their ids.
pub(super) fn add_comments(
    tx: &mut impl GenericClient,
    id: i64,
    by: Option<&str>,
    at: i64,
    revision: i64,
    comments: &[NewComment],
) -> Result<Vec<i64>, StoreError> {
    if row_state(tx, id)?.is_none() {
        return Err(not_found(id));
    }

    room_for(tx, id, comments.len())?;

    let row = tx
        .query_one(
            "SELECT coalesce(max(review_id), 0) + 1 FROM knowledge_doc_comments WHERE doc_id = $1",
            &[&id],
        )
        .map_err(backend)?;
    let review: i64 = column(&row, 0)?;
    let mut ids = Vec::with_capacity(comments.len());

    for comment in comments {
        let side = comment.side.map(|side| side.as_str());
        let from = comment.line_from.map(i64::from);
        let to = comment.line_to.map(i64::from);
        let occurrence = comment.occurrence.map(i64::from);
        let quote = quote_text(comment.quote.as_deref());

        let row = tx
            .query_one(
                "INSERT INTO knowledge_doc_comments
                     (doc_id, review_id, section, side, line_from, line_to, body, by_name, at, revision,
                      occurrence, quote)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING id",
                &[
                    &id,
                    &review,
                    &comment.section,
                    &side,
                    &from,
                    &to,
                    &comment.body,
                    &by,
                    &at,
                    &revision,
                    &occurrence,
                    &quote,
                ],
            )
            .map_err(backend)?;

        ids.push(column(&row, 0)?);
    }

    Ok(ids)
}

/// Adds a reply to `parent`; `None` when it cannot be answered.
pub(super) fn reply_comment(
    tx: &mut impl GenericClient,
    id: i64,
    parent: i64,
    by: Option<&str>,
    at: i64,
    body: &str,
) -> Result<Done<Option<i64>>, StoreError> {
    let Some(state) = row_state(tx, id)? else {
        return Err(not_found(id));
    };

    room_for(tx, id, 1)?;

    let row = tx
        .query_opt(
            "INSERT INTO knowledge_doc_comments (doc_id, parent_id, body, by_name, at, revision)
             SELECT $1::bigint, c.id, $3::text, $4::text, $5::bigint, $6::bigint FROM knowledge_doc_comments c
             WHERE c.id = $2 AND c.doc_id = $1 AND c.parent_id IS NULL
             RETURNING id",
            &[&id, &parent, &body, &by, &at, &state.review_rev],
        )
        .map_err(backend)?;

    match row {
        Some(row) => Ok((Some(column(&row, 0)?), true)),
        None => Ok((None, false)),
    }
}

/// Resolves or reopens `comment`; false when it cannot be.
pub(super) fn resolve_comment(
    tx: &mut impl GenericClient,
    id: i64,
    comment: i64,
    resolved: bool,
    by: Option<&str>,
    at: i64,
) -> Result<Done<bool>, StoreError> {
    let changed = if resolved {
        tx.execute(
            "UPDATE knowledge_doc_comments SET resolved_at = $4, resolved_by = $3
             WHERE id = $2 AND doc_id = $1 AND parent_id IS NULL",
            &[&id, &comment, &by, &at],
        )
    } else {
        tx.execute(
            "UPDATE knowledge_doc_comments SET resolved_at = NULL, resolved_by = NULL
             WHERE id = $2 AND doc_id = $1 AND parent_id IS NULL",
            &[&id, &comment],
        )
    }
    .map_err(backend)?;

    Ok((changed > 0, changed > 0))
}

/// Deletes `comment` and, when it starts a thread, its replies: the ids deleted, the comment
/// first; `None` when the document has no such comment.
pub(super) fn delete_comment(
    tx: &mut impl GenericClient,
    id: i64,
    comment: i64,
) -> Result<Done<Option<Vec<i64>>>, StoreError> {
    let rows = tx
        .query(
            "SELECT id FROM knowledge_doc_comments
             WHERE doc_id = $1 AND (id = $2 OR parent_id = $2) ORDER BY id",
            &[&id, &comment],
        )
        .map_err(backend)?;
    let thread = rows
        .iter()
        .map(|row| column(row, 0))
        .collect::<Result<Vec<i64>, StoreError>>()?;

    if thread.is_empty() {
        return Ok((None, false));
    }

    tx.execute(
        "DELETE FROM knowledge_doc_comments WHERE doc_id = $1 AND (id = $2 OR parent_id = $2)",
        &[&id, &comment],
    )
    .map_err(backend)?;

    Ok((Some(thread), true))
}

/// A stored line number, none when it does not fit.
fn line(row: &postgres::Row, index: usize) -> Result<Option<u32>, StoreError> {
    let stored: Option<i64> = column(row, index)?;

    Ok(stored.and_then(|number| u32::try_from(number).ok()))
}

/// Every comment of document `id`.
pub(super) fn comments(client: &mut impl GenericClient, id: i64) -> Result<Vec<Comment>, StoreError> {
    let rows = client
        .query(
            "SELECT id, review_id, section, side, line_from, line_to, body, by_name, at,
                    parent_id, resolved_at, resolved_by, revision, occurrence, quote
             FROM knowledge_doc_comments WHERE doc_id = $1 ORDER BY id LIMIT $2",
            &[&id, &int(COMMENTS_PER_DOC_MAX)],
        )
        .map_err(backend)?;

    rows.iter()
        .map(|row| {
            let stored: Option<String> = column(row, 3)?;

            Ok(Comment {
                id: column(row, 0)?,
                review_id: column(row, 1)?,
                section: column(row, 2)?,
                side: side(stored.as_deref()),
                line_from: line(row, 4)?,
                line_to: line(row, 5)?,
                body: column(row, 6)?,
                by: column(row, 7)?,
                at: column(row, 8)?,
                parent_id: column(row, 9)?,
                resolved_at: column(row, 10)?,
                resolved_by: column(row, 11)?,
                revision: column(row, 12)?,
                occurrence: line(row, 13)?,
                quote: read_quote(column::<Option<String>>(row, 14)?.as_deref()),
            })
        })
        .collect()
}

/// Keeps `bytes` as the file `name` of document `id`, replacing the one it has; false when there is
/// no such document.
pub(super) fn put_file(
    tx: &mut impl GenericClient,
    id: i64,
    name: &str,
    bytes: &[u8],
    sha256: &str,
) -> Result<Done<bool>, StoreError> {
    let Some(state) = row_state(tx, id)? else {
        return Ok((false, false));
    };

    check_file_for(&state.format, bytes)?;

    tx.execute(
        "INSERT INTO knowledge_doc_files (doc_id, name, sha256, bytes) VALUES ($1, $2, $3, $4)
         ON CONFLICT (doc_id) DO UPDATE SET name = excluded.name, sha256 = excluded.sha256, bytes = excluded.bytes",
        &[&id, &name, &sha256, &bytes],
    )
    .map_err(backend)?;

    Ok((true, true))
}

/// The file of document `id`.
pub(super) fn file(client: &mut impl GenericClient, id: i64) -> Result<Option<StoredFile>, StoreError> {
    let row = client
        .query_opt(
            "SELECT name, bytes, sha256 FROM knowledge_doc_files WHERE doc_id = $1",
            &[&id],
        )
        .map_err(backend)?;

    row.map(|row| {
        Ok(StoredFile {
            name: column(&row, 0)?,
            bytes: column(&row, 1)?,
            sha256: column(&row, 2)?,
        })
    })
    .transpose()
}
