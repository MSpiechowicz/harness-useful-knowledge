//! The review of drafts: transitions, promote, history, snapshots, comments and original files. The
//! statements are shared with the libSQL store (see [`super::sql_review`]).

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::documents::{not_found, record, row_state};
use super::sql_review::{
    Bind, COMMENT_ADD_SQL, COMMENT_COUNT_SQL, COMMENT_DELETE_SQL, COMMENT_REOPEN_SQL, COMMENT_REPLY_SQL,
    COMMENT_RESOLVE_SQL, COMMENT_THREAD_SQL, COMMENTS_DROP_SQL, COMMENTS_SQL, EXISTS_SQL, FILE_DROP_SQL, FILE_GET_SQL,
    FILE_PART_BYTES, FILE_PART_GET_SQL, FILE_PART_PUT_SQL, FILE_PUT_SQL, HISTORY_COUNT_SQL, HISTORY_SQL, PIECES_SQL,
    REVIEW_ID_SQL, SNAPSHOT_GET_SQL, SNAPSHOT_PRUNE_SQL, SNAPSHOT_PUT_SQL, TRANSITION_SQL, WRITERS_SQL, piece,
    promote_statements, side, snapshot_json, snapshot_sections,
};
use crate::store::review::{
    COMMENTS_PER_DOC_MAX, Gate, HISTORY_MAX, SNAPSHOT_BYTES_MAX, SNAPSHOTS_PER_DOC_MAX, check_file_for, gate,
    group_sections, quote_text, read_quote, sections_bytes,
};
use crate::store::{
    Columns, Comment, EventKind, History, NewComment, ReviewAction, ReviewChange, ReviewEvent, ReviewReply, Reviewed,
    SectionText, Status, StoreError, StoredFile,
};

/// What a write came to, and whether it changed anything (a refusal changes nothing, so it does not
/// count as a write).
pub(super) type Done<T> = (T, bool);

/// The sections of document `id` as stored.
pub(super) fn sections_of(conn: &Connection, id: i64) -> Result<Vec<SectionText>, StoreError> {
    let mut statement = conn.prepare(PIECES_SQL)?;
    let pieces = statement
        .query_map([id], |row| {
            Ok(piece(row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(group_sections(&pieces))
}

fn exists(conn: &Connection, id: i64) -> Result<bool, StoreError> {
    let found: Option<i64> = conn.query_row(EXISTS_SQL, [id], |row| row.get(0)).optional()?;

    Ok(found.is_some())
}

/// The sections of document `id`; `None` when there is no such document.
pub(super) fn section_texts(conn: &Connection, id: i64) -> Result<Option<Vec<SectionText>>, StoreError> {
    if !exists(conn, id)? {
        return Ok(None);
    }

    sections_of(conn, id).map(Some)
}

/// The sections kept when document `id` was asked at `revision`.
pub(super) fn asked_sections(
    conn: &Connection,
    id: i64,
    revision: i64,
) -> Result<Option<Vec<SectionText>>, StoreError> {
    let json: Option<String> = conn
        .query_row(SNAPSHOT_GET_SQL, params![id, revision], |row| row.get(0))
        .optional()?;

    Ok(json.and_then(|json| snapshot_sections(&json)))
}

/// Keeps the sections of document `id` as asked at `revision`, unless they are over the limit.
fn snapshot(tx: &Transaction, id: i64, revision: i64) -> Result<(), StoreError> {
    let sections = sections_of(tx, id)?;

    if sections_bytes(&sections) > SNAPSHOT_BYTES_MAX {
        return Ok(());
    }

    tx.execute(SNAPSHOT_PUT_SQL, params![id, revision, snapshot_json(&sections)])?;
    tx.execute(SNAPSHOT_PRUNE_SQL, params![id, SNAPSHOTS_PER_DOC_MAX])?;

    Ok(())
}

/// Applies a transition that does not make a pending version live. `by` and `comment` are the
/// cleaned ones of the change.
pub(super) fn set_review(
    tx: &Transaction,
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
    let moved = tx.execute(TRANSITION_SQL, params![id, to.as_str(), expect])?;

    if moved == 0 {
        return Err(not_found(id));
    }

    // A version that goes live has had its review: its comments are not kept.
    if change.action.goes_live() {
        tx.execute(COMMENTS_DROP_SQL, params![id])?;
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
    tx: &Transaction,
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

    for (sql, bind) in promote_statements(columns) {
        match bind {
            Bind::Live => tx.execute(&sql, [live])?,
            Bind::Pending => tx.execute(&sql, [id])?,
            Bind::Both => tx.execute(&sql, [live, id])?,
        };
    }

    record(tx, live, change.action.event(), by, comment, change.at)?;

    let revision: i64 = tx.query_row(
        "SELECT review_rev FROM knowledge_documents WHERE id = ?1",
        [live],
        |row| row.get(0),
    )?;

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
pub(super) fn history(conn: &Connection, id: i64) -> Result<Option<History>, StoreError> {
    if !exists(conn, id)? {
        return Ok(None);
    }

    let limit = i64::try_from(HISTORY_MAX).unwrap_or(i64::MAX);
    let mut statement = conn.prepare(HISTORY_SQL)?;
    let rows = statement.query_map(params![id, limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;

    let mut events = Vec::new();

    for row in rows {
        let (kind, by, comment, at) = row?;

        if let Some(kind) = EventKind::parse(&kind) {
            events.push(ReviewEvent { kind, by, at, comment });
        }
    }

    let total: i64 = conn.query_row(HISTORY_COUNT_SQL, [id], |row| row.get(0))?;

    Ok(Some(History {
        events,
        total: usize::try_from(total).unwrap_or(0),
    }))
}

/// The names that wrote document `id` since it was last live, oldest first.
pub(super) fn writers_since_live(conn: &Connection, id: i64) -> Result<Vec<String>, StoreError> {
    let mut statement = conn.prepare(WRITERS_SQL)?;
    let names = statement
        .query_map([id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(names)
}

/// Refuses a document that cannot take more comments.
fn room_for(tx: &Transaction, id: i64, more: usize) -> Result<(), StoreError> {
    let have: i64 = tx.query_row(COMMENT_COUNT_SQL, [id], |row| row.get(0))?;

    if usize::try_from(have).unwrap_or(usize::MAX).saturating_add(more) > COMMENTS_PER_DOC_MAX {
        return Err(StoreError::Invalid(format!(
            "a document keeps at most {COMMENTS_PER_DOC_MAX} comments"
        )));
    }

    Ok(())
}

/// Adds the cleaned `comments` of one review to document `id` and returns their ids.
pub(super) fn add_comments(
    tx: &Transaction,
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

    let review: i64 = tx.query_row(REVIEW_ID_SQL, [id], |row| row.get(0))?;
    let mut ids = Vec::with_capacity(comments.len());

    for comment in comments {
        tx.execute(
            COMMENT_ADD_SQL,
            params![
                id,
                review,
                comment.section,
                comment.side.map(|side| side.as_str()),
                comment.line_from,
                comment.line_to,
                comment.body,
                by,
                at,
                revision,
                comment.occurrence,
                quote_text(comment.quote.as_deref()),
            ],
        )?;
        ids.push(tx.last_insert_rowid());
    }

    Ok(ids)
}

/// Adds a reply to `parent`; `None` when it cannot be answered.
pub(super) fn reply_comment(
    tx: &Transaction,
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

    let added = tx.execute(COMMENT_REPLY_SQL, params![id, parent, body, by, at, state.review_rev])?;

    if added == 0 {
        return Ok((None, false));
    }

    Ok((Some(tx.last_insert_rowid()), true))
}

/// Resolves or reopens `comment`; false when it cannot be.
pub(super) fn resolve_comment(
    tx: &Transaction,
    id: i64,
    comment: i64,
    resolved: bool,
    by: Option<&str>,
    at: i64,
) -> Result<Done<bool>, StoreError> {
    let changed = if resolved {
        tx.execute(COMMENT_RESOLVE_SQL, params![id, comment, by, at])?
    } else {
        tx.execute(COMMENT_REOPEN_SQL, params![id, comment])?
    };

    Ok((changed > 0, changed > 0))
}

/// Deletes `comment` and, when it starts a thread, its replies: the ids deleted, the comment
/// first; `None` when the document has no such comment.
pub(super) fn delete_comment(tx: &Transaction, id: i64, comment: i64) -> Result<Done<Option<Vec<i64>>>, StoreError> {
    let mut statement = tx.prepare(COMMENT_THREAD_SQL)?;
    let thread = statement
        .query_map(params![id, comment], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    if thread.is_empty() {
        return Ok((None, false));
    }

    tx.execute(COMMENT_DELETE_SQL, params![id, comment])?;

    Ok((Some(thread), true))
}

/// Every comment of document `id`.
pub(super) fn comments(conn: &Connection, id: i64) -> Result<Vec<Comment>, StoreError> {
    let limit = i64::try_from(COMMENTS_PER_DOC_MAX).unwrap_or(i64::MAX);
    let mut statement = conn.prepare(COMMENTS_SQL)?;
    let rows = statement.query_map(params![id, limit], |row| {
        let stored: Option<String> = row.get(3)?;

        Ok(Comment {
            id: row.get(0)?,
            review_id: row.get(1)?,
            section: row.get(2)?,
            side: side(stored.as_deref()),
            line_from: row.get(4)?,
            line_to: row.get(5)?,
            body: row.get(6)?,
            by: row.get(7)?,
            at: row.get(8)?,
            parent_id: row.get(9)?,
            resolved_at: row.get(10)?,
            resolved_by: row.get(11)?,
            revision: row.get(12)?,
            occurrence: row
                .get::<_, Option<i64>>(13)?
                .and_then(|count| u32::try_from(count).ok()),
            quote: read_quote(row.get::<_, Option<String>>(14)?.as_deref()),
        })
    })?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// Keeps `bytes` as the file `name` of document `id`, replacing the one it has; false when there is
/// no such document.
pub(super) fn put_file(
    tx: &Transaction,
    id: i64,
    name: &str,
    bytes: &[u8],
    sha256: &str,
) -> Result<Done<bool>, StoreError> {
    let Some(state) = row_state(tx, id)? else {
        return Ok((false, false));
    };

    check_file_for(&state.format, bytes)?;

    tx.execute(FILE_DROP_SQL, [id])?;
    tx.execute(FILE_PUT_SQL, params![id, name, bytes.len(), sha256])?;

    for (part, piece) in bytes.chunks(FILE_PART_BYTES).enumerate() {
        tx.execute(FILE_PART_PUT_SQL, params![id, part, piece])?;
    }

    Ok((true, true))
}

/// The file of document `id`.
pub(super) fn file(conn: &Connection, id: i64) -> Result<Option<StoredFile>, StoreError> {
    let found = conn
        .query_row(FILE_GET_SQL, [id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, usize>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .optional()?;

    let Some((name, size, sha256)) = found else {
        return Ok(None);
    };

    let mut bytes = Vec::with_capacity(size);

    for part in 0..size.div_ceil(FILE_PART_BYTES) {
        let piece: Vec<u8> = conn.query_row(FILE_PART_GET_SQL, params![id, part], |row| row.get(0))?;
        bytes.extend_from_slice(&piece);
    }

    if bytes.len() != size {
        return Err(StoreError::Backend("a stored file is incomplete".to_owned()));
    }

    Ok(Some(StoredFile { name, bytes, sha256 }))
}
