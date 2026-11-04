//! The review of drafts: transitions, promote, history, snapshots, comments and original files. The
//! statements are the local file's (see [`crate::store::sqlite::sql_review`]).

use super::documents::{not_found, record, row_state};
use std::collections::HashMap;

use super::sql::{Sql, Value, args, query_row};
use crate::store::review::{
    Authorship, COMMENTS_PER_DOC_MAX, Gate, HISTORY_MAX, SNAPSHOT_BYTES_MAX, SNAPSHOTS_PER_DOC_MAX, check_file_for,
    gate, group_sections, quote_text, read_quote, sections_bytes,
};
use crate::store::sqlite::sql_review::{
    Bind, COMMENT_ADD_SQL, COMMENT_COUNT_SQL, COMMENT_DELETE_SQL, COMMENT_REOPEN_SQL, COMMENT_REPLY_SQL,
    COMMENT_RESOLVE_SQL, COMMENT_THREAD_SQL, COMMENTS_DROP_SQL, COMMENTS_SQL, EXISTS_SQL, FILE_DROP_SQL, FILE_GET_SQL,
    FILE_PART_BYTES, FILE_PART_GET_SQL, FILE_PART_PUT_SQL, FILE_PUT_SQL, HISTORY_COUNT_SQL, HISTORY_SQL, PIECES_SQL,
    REVIEW_ID_SQL, SNAPSHOT_GET_SQL, SNAPSHOT_PRUNE_SQL, SNAPSHOT_PUT_SQL, TRANSITION_SQL, WRITERS_SQL, piece,
    promote_statements, side, snapshot_json, snapshot_sections,
};
use crate::store::{
    Columns, Comment, EventKind, History, NewComment, ReviewAction, ReviewChange, ReviewEvent, ReviewReply, Reviewed,
    SectionText, Status, StoreError, StoredFile,
};

/// What a write came to, and whether it changed anything (a refusal changes nothing, so it does not
/// count as a write).
pub(super) type Done<T> = (T, bool);

/// The sections of document `id` as stored.
pub(super) fn sections_of(db: &mut dyn Sql, id: i64) -> Result<Vec<SectionText>, StoreError> {
    let pieces = db
        .query(PIECES_SQL, args![id])?
        .iter()
        .map(|row| Ok(piece(row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .collect::<Result<Vec<_>, StoreError>>()?;

    Ok(group_sections(&pieces))
}

fn exists(db: &mut dyn Sql, id: i64) -> Result<bool, StoreError> {
    Ok(query_row(db, EXISTS_SQL, args![id])?.is_some())
}

/// The sections of document `id`; `None` when there is no such document.
pub(super) fn section_texts(db: &mut dyn Sql, id: i64) -> Result<Option<Vec<SectionText>>, StoreError> {
    if !exists(db, id)? {
        return Ok(None);
    }

    sections_of(db, id).map(Some)
}

/// The sections kept when document `id` was asked at `revision`.
pub(super) fn asked_sections(db: &mut dyn Sql, id: i64, revision: i64) -> Result<Option<Vec<SectionText>>, StoreError> {
    let json = query_row(db, SNAPSHOT_GET_SQL, args![id, revision])?
        .map(|row| row.get::<String>(0))
        .transpose()?;

    Ok(json.and_then(|json| snapshot_sections(&json)))
}

/// Keeps the sections of document `id` as asked at `revision`, unless they are over the limit.
fn snapshot(db: &mut dyn Sql, id: i64, revision: i64) -> Result<(), StoreError> {
    let sections = sections_of(db, id)?;

    if sections_bytes(&sections) > SNAPSHOT_BYTES_MAX {
        return Ok(());
    }

    db.execute(SNAPSHOT_PUT_SQL, args![id, revision, snapshot_json(&sections)])?;
    db.execute(SNAPSHOT_PRUNE_SQL, args![id, SNAPSHOTS_PER_DOC_MAX])?;

    Ok(())
}

/// Applies a transition that does not make a pending version live. `by` and `comment` are the
/// cleaned ones of the change.
pub(super) fn set_review(
    db: &mut dyn Sql,
    id: i64,
    expect: i64,
    change: &ReviewChange,
    (by, comment): (Option<&str>, Option<&str>),
) -> Result<Done<ReviewReply>, StoreError> {
    let state = row_state(db, id)?;

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
    let moved = db.execute(TRANSITION_SQL, args![id, to.as_str(), expect])?;

    if moved.changes == 0 {
        return Err(not_found(id));
    }

    // A version that goes live has had its review: its comments are not kept.
    if change.action.goes_live() {
        db.execute(COMMENTS_DROP_SQL, args![id])?;
    }

    record(db, id, change.action.event(), by, comment, change.at)?;

    if change.action == ReviewAction::Ask {
        snapshot(db, id, expect + 1)?;
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

/// Makes pending version `id` live in the transaction `db` is in.
pub(super) fn promote(
    db: &mut dyn Sql,
    columns: Columns,
    id: i64,
    expect: i64,
    change: &ReviewChange,
    (by, comment): (Option<&str>, Option<&str>),
) -> Result<Done<ReviewReply>, StoreError> {
    let state = row_state(db, id)?;

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
    if row_state(db, live)?.is_none_or(|row| row.status != Status::Live) {
        return Err(StoreError::Invalid(format!(
            "document {live} is not live: its pending version {id} cannot be promoted onto it"
        )));
    }

    for (sql, bind) in promote_statements(columns) {
        match bind {
            Bind::Live => db.execute(&sql, args![live])?,
            Bind::Pending => db.execute(&sql, args![id])?,
            Bind::Both => db.execute(&sql, args![live, id])?,
        };
    }

    record(db, live, change.action.event(), by, comment, change.at)?;

    let revision = query_row(
        db,
        "SELECT review_rev FROM knowledge_documents WHERE id = ?1",
        args![live],
    )?
    .map_or(Ok(0), |row| row.get(0))?;

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
pub(super) fn history(db: &mut dyn Sql, id: i64) -> Result<Option<History>, StoreError> {
    if !exists(db, id)? {
        return Ok(None);
    }

    let rows = db.query(HISTORY_SQL, args![id, HISTORY_MAX])?;
    let mut events = Vec::new();

    for row in &rows {
        let kind: String = row.get(0)?;

        if let Some(kind) = EventKind::parse(&kind) {
            events.push(ReviewEvent {
                kind,
                by: row.get(1)?,
                comment: row.get(2)?,
                at: row.get(3)?,
            });
        }
    }

    let total = query_row(db, HISTORY_COUNT_SQL, args![id])?.map_or(Ok(0), |row| row.get(0))?;

    Ok(Some(History { events, total }))
}

/// The names that wrote document `id` since it was last live, oldest first.
pub(super) fn writers_since_live(db: &mut dyn Sql, id: i64) -> Result<Vec<String>, StoreError> {
    let rows = db.query(WRITERS_SQL, args![id])?;

    rows.iter().map(|row| row.get(0)).collect()
}

/// The writers (at most `per_doc` each) and the latest asker of the documents `ids`, in two queries.
pub(super) fn authorship_of(
    db: &mut dyn Sql,
    ids: &[i64],
    per_doc: usize,
) -> Result<HashMap<i64, Authorship>, StoreError> {
    let mut found: HashMap<i64, Authorship> = HashMap::new();

    if ids.is_empty() {
        return Ok(found);
    }

    let listed = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
    let writers = db.query(
        "SELECT doc_id, by_name FROM (
             SELECT e.doc_id, e.by_name,
                    row_number() OVER (PARTITION BY e.doc_id ORDER BY min(e.id)) AS n
             FROM knowledge_doc_events e
             WHERE e.doc_id IN (SELECT value FROM json_each(?1))
               AND e.kind IN ('drafted', 'edited', 'updateDrafted') AND e.by_name IS NOT NULL
               AND e.id > COALESCE((SELECT max(l.id) FROM knowledge_doc_events l
                                    WHERE l.doc_id = e.doc_id AND l.kind IN ('approved', 'wentLive')), 0)
             GROUP BY e.doc_id, e.by_name
         ) WHERE n <= ?2 ORDER BY doc_id, n",
        args![listed.clone(), per_doc],
    )?;

    for row in &writers {
        let doc: i64 = row.get(0)?;
        found.entry(doc).or_default().writers.push(row.get(1)?);
    }

    let askers = db.query(
        "SELECT e.doc_id, e.by_name FROM knowledge_doc_events e
         WHERE e.doc_id IN (SELECT value FROM json_each(?1)) AND e.kind = 'asked'
           AND e.id = (SELECT max(a.id) FROM knowledge_doc_events a WHERE a.doc_id = e.doc_id AND a.kind = 'asked')
           AND e.id > COALESCE((SELECT max(l.id) FROM knowledge_doc_events l
                                WHERE l.doc_id = e.doc_id AND l.kind IN ('approved', 'wentLive')), 0)",
        args![listed],
    )?;

    for row in &askers {
        let doc: i64 = row.get(0)?;
        found.entry(doc).or_default().asker = row.get(1)?;
    }

    for id in ids {
        found.entry(*id).or_default();
    }

    Ok(found)
}

/// Refuses a document that cannot take more comments.
fn room_for(db: &mut dyn Sql, id: i64, more: usize) -> Result<(), StoreError> {
    let have: usize = query_row(db, COMMENT_COUNT_SQL, args![id])?.map_or(Ok(0), |row| row.get(0))?;

    if have.saturating_add(more) > COMMENTS_PER_DOC_MAX {
        return Err(StoreError::Invalid(format!(
            "a document keeps at most {COMMENTS_PER_DOC_MAX} comments"
        )));
    }

    Ok(())
}

/// Adds the cleaned `comments` of one review to document `id` and returns their ids.
pub(super) fn add_comments(
    db: &mut dyn Sql,
    id: i64,
    by: Option<&str>,
    at: i64,
    revision: i64,
    comments: &[NewComment],
) -> Result<Vec<i64>, StoreError> {
    if row_state(db, id)?.is_none() {
        return Err(not_found(id));
    }

    room_for(db, id, comments.len())?;

    let review: i64 = query_row(db, REVIEW_ID_SQL, args![id])?.map_or(Ok(1), |row| row.get(0))?;
    let mut ids = Vec::with_capacity(comments.len());

    for comment in comments {
        let added = db.execute(
            COMMENT_ADD_SQL,
            &[
                Value::from(id),
                Value::from(review),
                Value::from(comment.section.as_deref()),
                Value::from(comment.side.map(|side| side.as_str())),
                Value::from(comment.line_from.map(i64::from)),
                Value::from(comment.line_to.map(i64::from)),
                Value::from(&comment.body),
                Value::from(by),
                Value::from(at),
                Value::from(revision),
                Value::from(comment.occurrence.map(i64::from)),
                Value::from(quote_text(comment.quote.as_deref()).as_deref()),
            ],
        )?;

        ids.push(added.last_insert_rowid);
    }

    Ok(ids)
}

/// Adds a reply to `parent`; `None` when it cannot be answered.
pub(super) fn reply_comment(
    db: &mut dyn Sql,
    id: i64,
    parent: i64,
    by: Option<&str>,
    at: i64,
    body: &str,
) -> Result<Done<Option<i64>>, StoreError> {
    let Some(state) = row_state(db, id)? else {
        return Err(not_found(id));
    };

    room_for(db, id, 1)?;

    let added = db.execute(COMMENT_REPLY_SQL, args![id, parent, body, by, at, state.review_rev])?;

    if added.changes == 0 {
        return Ok((None, false));
    }

    Ok((Some(added.last_insert_rowid), true))
}

/// Resolves or reopens `comment`; false when it cannot be.
pub(super) fn resolve_comment(
    db: &mut dyn Sql,
    id: i64,
    comment: i64,
    resolved: bool,
    by: Option<&str>,
    at: i64,
) -> Result<Done<bool>, StoreError> {
    let changed = if resolved {
        db.execute(COMMENT_RESOLVE_SQL, args![id, comment, by, at])?
    } else {
        db.execute(COMMENT_REOPEN_SQL, args![id, comment])?
    };

    Ok((changed.changes > 0, changed.changes > 0))
}

/// Deletes `comment` and, when it starts a thread, its replies: the ids deleted, the comment
/// first; `None` when the document has no such comment.
pub(super) fn delete_comment(db: &mut dyn Sql, id: i64, comment: i64) -> Result<Done<Option<Vec<i64>>>, StoreError> {
    let thread = db
        .query(COMMENT_THREAD_SQL, args![id, comment])?
        .iter()
        .map(|row| row.get::<i64>(0))
        .collect::<Result<Vec<_>, _>>()?;

    if thread.is_empty() {
        return Ok((None, false));
    }

    db.execute(COMMENT_DELETE_SQL, args![id, comment])?;

    Ok((Some(thread), true))
}

/// Every comment of document `id`.
pub(super) fn comments(db: &mut dyn Sql, id: i64) -> Result<Vec<Comment>, StoreError> {
    let rows = db.query(COMMENTS_SQL, args![id, COMMENTS_PER_DOC_MAX])?;

    rows.iter()
        .map(|row| {
            let stored: Option<String> = row.get(3)?;
            let line = |index: usize| -> Result<Option<u32>, StoreError> {
                Ok(row.get::<Option<i64>>(index)?.and_then(|line| u32::try_from(line).ok()))
            };

            Ok(Comment {
                id: row.get(0)?,
                review_id: row.get(1)?,
                section: row.get(2)?,
                side: side(stored.as_deref()),
                line_from: line(4)?,
                line_to: line(5)?,
                body: row.get(6)?,
                by: row.get(7)?,
                at: row.get(8)?,
                parent_id: row.get(9)?,
                resolved_at: row.get(10)?,
                resolved_by: row.get(11)?,
                revision: row.get(12)?,
                occurrence: line(13)?,
                quote: read_quote(row.get::<Option<String>>(14)?.as_deref()),
            })
        })
        .collect()
}

/// Keeps `bytes` as the file `name` of document `id`, replacing the one it has; false when there is
/// no such document.
pub(super) fn put_file(
    db: &mut dyn Sql,
    id: i64,
    name: &str,
    bytes: &[u8],
    sha256: &str,
) -> Result<Done<bool>, StoreError> {
    let Some(state) = row_state(db, id)? else {
        return Ok((false, false));
    };

    check_file_for(&state.format, bytes)?;

    db.execute(FILE_DROP_SQL, args![id])?;
    db.execute(FILE_PUT_SQL, args![id, name, bytes.len(), sha256])?;

    for (part, piece) in bytes.chunks(FILE_PART_BYTES).enumerate() {
        db.execute(
            FILE_PART_PUT_SQL,
            &[Value::from(id), Value::from(part), Value::Blob(piece.to_vec())],
        )?;
    }

    Ok((true, true))
}

/// The file of document `id`.
pub(super) fn file(db: &mut dyn Sql, id: i64) -> Result<Option<StoredFile>, StoreError> {
    let Some(row) = query_row(db, FILE_GET_SQL, args![id])? else {
        return Ok(None);
    };

    let (name, size, sha256): (String, usize, String) = (row.get(0)?, row.get(1)?, row.get(2)?);
    let mut bytes = Vec::with_capacity(size);

    for part in 0..size.div_ceil(FILE_PART_BYTES) {
        let piece = query_row(db, FILE_PART_GET_SQL, args![id, part])?.map_or_else(
            || Err(StoreError::Backend("a stored file is incomplete".to_owned())),
            |row| row.get::<Vec<u8>>(0),
        )?;
        bytes.extend_from_slice(&piece);
    }

    if bytes.len() != size {
        return Err(StoreError::Backend("a stored file is incomplete".to_owned()));
    }

    Ok(Some(StoredFile { name, bytes, sha256 }))
}
