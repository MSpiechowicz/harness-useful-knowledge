//! Writing and finding documents: the statements behind `put_document`, `set_links`, `rename`,
//! `delete` and the lookups an import makes.
//!
//! A vector goes in as the JSON text of its floats through `vector32(?)` (Rust prints an `f32`
//! with the digits that read back exactly) and comes out as the blob of little-endian `f32`s.

use std::collections::HashMap;

use super::sql::{Row, Sql, Value, args, query_row};
use crate::chunk::passage;
use crate::limits::{DELETE_IDS_MAX, EMBED_BATCH_MAX};
use crate::store::review::{RENAMED_COMMENT, actor};
use crate::store::sqlite::sql_quality::{document_columns, seen, stamp};
use crate::store::sqlite::sql_review::{EVENT_SQL, FILE_DROP_SQL, ROW_SQL, pending_versions_sql};
use crate::store::{Author, Chunk, Columns, DIMS, DocUsage, Document, EventKind, Link, Origin, PendingChunk};
use crate::store::{PendingVersion, Status, StoreError, Visibility};

/// A [`Document`] from a row of [`document_columns`].
pub(super) fn document(row: &Row) -> Result<Document, StoreError> {
    let origin: String = row.get(11)?;
    let warnings: String = row.get(12)?;

    Ok(Document {
        id: row.get(0)?,
        title: row.get(1)?,
        source_name: row.get(2)?,
        source_key: row.get(3)?,
        format: row.get(4)?,
        content_sha256: row.get(5)?,
        file_sha256: row.get(6)?,
        bytes: row.get(7)?,
        chunk_count: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        origin: Origin::parse(&origin)
            .ok_or_else(|| StoreError::Backend("unexpected value in column 11".to_owned()))?,
        warnings: serde_json::from_str(&warnings).unwrap_or_default(),
        added_by: Author::stored(row.tolerant_text(13), row.tolerant_text(14)),
        updated_by: Author::stored(row.tolerant_text(15), row.tolerant_text(16)),
        status: Status::parse(&row.get::<String>(20)?)
            .ok_or_else(|| StoreError::Backend("unexpected value in column 20".to_owned()))?,
        pending_of: row.get(21)?,
        review_rev: row.get(22)?,
    })
}

/// A [`Document`] with its usage from a row of [`document_columns`].
pub(super) fn listed(row: &Row) -> Result<(Document, Option<DocUsage>), StoreError> {
    let usage = DocUsage::stored(
        row.tolerant_counter(17),
        row.tolerant_counter(18),
        row.tolerant_counter(19),
    );

    Ok((document(row)?, usage))
}

/// Document `id` with its usage.
pub(super) fn by_id(
    db: &mut dyn Sql,
    columns: Columns,
    id: i64,
) -> Result<Option<(Document, Option<DocUsage>)>, StoreError> {
    let sql = format!(
        "SELECT {} FROM knowledge_documents d WHERE d.id = ?1",
        document_columns(columns)
    );

    query_row(db, &sql, args![id])?.map(|row| listed(&row)).transpose()
}

/// Records who stored `doc` as document `id`, which [`put`] has just written. A new document gets
/// both authors, an update keeps who added it. Nothing without the author columns.
pub(super) fn stamp_authors(db: &mut dyn Sql, columns: Columns, id: i64, doc: &Document) -> Result<(), StoreError> {
    if !columns.authors {
        return Ok(());
    }

    let (sql, name, via) = stamp(doc);
    db.execute(sql, args![id, name, via])?;

    Ok(())
}

/// The state of a row: status, the live document it is pending for, revision and format.
pub(super) struct RowState {
    pub status: Status,
    pub pending_of: Option<i64>,
    pub review_rev: i64,
    pub format: String,
}

/// The review columns of document `id`; `None` when there is no such document.
pub(super) fn row_state(db: &mut dyn Sql, id: i64) -> Result<Option<RowState>, StoreError> {
    query_row(db, ROW_SQL, args![id])?
        .map(|row| {
            Ok(RowState {
                status: Status::parse(&row.get::<String>(0)?)
                    .ok_or_else(|| StoreError::Backend("unexpected value in column status".to_owned()))?,
                pending_of: row.get(1)?,
                review_rev: row.get(2)?,
                format: row.get(3)?,
            })
        })
        .transpose()
}

/// Records an event of document `id`.
pub(super) fn record(
    db: &mut dyn Sql,
    id: i64,
    kind: EventKind,
    by: Option<&str>,
    comment: Option<&str>,
    at: i64,
) -> Result<(), StoreError> {
    db.execute(EVENT_SQL, args![id, kind.as_str(), by, comment, at])?;

    Ok(())
}

/// Stores `doc` (whose cleaned title is `title`) with `chunks` and `links`: inserted for id 0,
/// else replacing that document's chunks and links. A row that is not live records the event.
pub(super) fn put(
    db: &mut dyn Sql,
    doc: &Document,
    title: &str,
    chunks: &[Chunk],
    links: &[Link],
) -> Result<i64, StoreError> {
    let warnings = serde_json::to_string(&doc.warnings).unwrap_or_else(|_| "[]".to_owned());
    let chunk_count = chunks.len();
    let by = actor(doc.updated_by.as_ref().map(|author| author.name.as_str()));

    let id = if doc.id == 0 {
        check_new_row(db, doc)?;

        let id = db
            .execute(
                "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256,
                     bytes, chunk_count, origin, warnings, created_at, updated_at, status, pending_of)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                args![
                    title,
                    &doc.source_name,
                    &doc.source_key,
                    &doc.format,
                    &doc.content_sha256,
                    &doc.file_sha256,
                    doc.bytes,
                    chunk_count,
                    doc.origin.as_str(),
                    warnings,
                    doc.created_at,
                    doc.updated_at,
                    doc.status.as_str(),
                    doc.pending_of,
                ],
            )?
            .last_insert_rowid;

        if !doc.status.is_live() {
            let kind = if doc.pending_of.is_some() {
                EventKind::UpdateDrafted
            } else {
                EventKind::Drafted
            };

            record(db, id, kind, by.as_deref(), None, doc.updated_at)?;
        }

        id
    } else {
        let Some(stored) = row_state(db, doc.id)? else {
            return Err(not_found(doc.id));
        };

        // The old chunks go first, so the retitle trigger has no chunks to rewrite.
        db.execute("DELETE FROM knowledge_chunks WHERE doc_id = ?1", args![doc.id])?;
        db.execute("DELETE FROM knowledge_doc_links WHERE doc_id = ?1", args![doc.id])?;
        db.execute(FILE_DROP_SQL, args![doc.id])?;

        // A change of content is a new revision, and what was waiting for approval is no longer
        // what was asked: it is a draft again.
        db.execute(
            "UPDATE knowledge_documents SET title = ?2, source_name = ?3, source_key = ?4, format = ?5,
                 content_sha256 = ?6, file_sha256 = ?7, bytes = ?8, chunk_count = ?9, origin = ?10,
                 warnings = ?11, updated_at = ?12, review_rev = review_rev + 1,
                 status = CASE WHEN status = 'waiting' THEN 'draft' ELSE status END
             WHERE id = ?1",
            args![
                doc.id,
                title,
                &doc.source_name,
                &doc.source_key,
                &doc.format,
                &doc.content_sha256,
                &doc.file_sha256,
                doc.bytes,
                chunk_count,
                doc.origin.as_str(),
                warnings,
                doc.updated_at,
            ],
        )?;

        if !stored.status.is_live() {
            record(db, doc.id, EventKind::Edited, by.as_deref(), None, doc.updated_at)?;
        }

        doc.id
    };

    insert_chunks(db, id, title, chunks)?;
    insert_links(db, id, links)?;

    Ok(id)
}

/// Refuses a new row that cannot be stored: a pending version needs a live document without one.
fn check_new_row(db: &mut dyn Sql, doc: &Document) -> Result<(), StoreError> {
    let Some(live) = doc.pending_of else {
        return Ok(());
    };

    if doc.status.is_live() {
        return Err(StoreError::Invalid("a pending version is never live".to_owned()));
    }

    match row_state(db, live)? {
        None => return Err(not_found(live)),
        Some(target) if !target.status.is_live() => {
            return Err(StoreError::Invalid(format!("document {live} is not live")));
        }
        Some(_) => {}
    }

    let taken = query_row(
        db,
        "SELECT id FROM knowledge_documents WHERE pending_of = ?1",
        args![live],
    )?;

    if taken.is_some() {
        return Err(StoreError::Invalid(format!(
            "document {live} already has a pending version"
        )));
    }

    Ok(())
}

/// The pending versions of the live documents `ids`, by live id.
pub(super) fn pending_of_many(
    db: &mut dyn Sql,
    columns: Columns,
    ids: &[i64],
) -> Result<HashMap<i64, PendingVersion>, StoreError> {
    let wanted = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
    let rows = db.query(&pending_versions_sql(columns), args![wanted])?;
    let mut found = HashMap::new();

    for row in &rows {
        let doc = document(row)?;

        if let Some(live) = doc.pending_of {
            found.insert(live, pending_version(&doc));
        }
    }

    Ok(found)
}

/// The summary of a pending row.
pub(super) fn pending_version(doc: &Document) -> PendingVersion {
    PendingVersion {
        id: doc.id,
        status: doc.status,
        review_rev: doc.review_rev,
        updated_by: doc.updated_by.clone(),
        updated_at: doc.updated_at,
    }
}

fn insert_chunks(db: &mut dyn Sql, doc_id: i64, title: &str, chunks: &[Chunk]) -> Result<(), StoreError> {
    let rows: Vec<Vec<Value>> = chunks
        .iter()
        .map(|chunk| {
            vec![
                doc_id.into(),
                chunk.ordinal.into(),
                title.into(),
                (&chunk.heading_path).into(),
                (&chunk.text).into(),
                (&chunk.chunk_sha).into(),
                chunk.tokens.into(),
                chunk.char_start.into(),
                chunk.char_end.into(),
                chunk.embedding.as_deref().map(vector_text).into(),
                chunk.model_id.clone().into(),
            ]
        })
        .collect();

    db.execute_many(
        "INSERT INTO knowledge_chunks (doc_id, ordinal, title, heading_path, text, chunk_sha, tokens,
             char_start, char_end, embedding, model_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, CASE WHEN ?10 IS NULL THEN NULL ELSE vector32(?10) END, ?11)",
        &rows,
    )
}

fn insert_links(db: &mut dyn Sql, doc_id: i64, links: &[Link]) -> Result<(), StoreError> {
    let rows: Vec<Vec<Value>> = links
        .iter()
        .map(|link| {
            let globs = serde_json::to_string(&link.globs).unwrap_or_else(|_| "[]".to_owned());
            vec![doc_id.into(), (&link.repo_key).into(), globs.into()]
        })
        .collect();

    db.execute_many(
        "INSERT INTO knowledge_doc_links (doc_id, repo_key, globs) VALUES (?1, ?2, ?3)",
        &rows,
    )
}

/// How many links point at a folder of this computer (a `path:` key).
pub(super) fn count_local_paths(db: &mut dyn Sql) -> Result<usize, StoreError> {
    let row = query_row(
        db,
        "SELECT count(*) FROM knowledge_doc_links WHERE substr(repo_key, 1, 5) = 'path:'",
        args![],
    )?;
    let found: i64 = row.map_or(Ok(0), |row| row.get(0))?;

    Ok(usize::try_from(found).unwrap_or(0))
}

/// Deletes the links to folders of this computer, which a team database never keeps; the number
/// deleted. A live document that no link is left on becomes a draft (a document with no link is
/// global, which would widen who sees it), with an `edited` event and a new revision.
pub(super) fn drop_local_paths(db: &mut dyn Sql) -> Result<(usize, bool), StoreError> {
    let affected: Vec<i64> = db
        .query(
            "SELECT DISTINCT doc_id FROM knowledge_doc_links WHERE substr(repo_key, 1, 5) = 'path:' ORDER BY doc_id",
            args![],
        )?
        .iter()
        .map(|row| row.get(0))
        .collect::<Result<_, _>>()?;

    let removed = db
        .execute(
            "DELETE FROM knowledge_doc_links WHERE substr(repo_key, 1, 5) = 'path:'",
            args![],
        )?
        .changes;

    for id in affected {
        if query_row(db, "SELECT 1 FROM knowledge_doc_links WHERE doc_id = ?1", args![id])?.is_some() {
            continue;
        }

        // A pending version has no links of its own: the live document's win.
        let Some(state) = row_state(db, id)?.filter(|state| state.pending_of.is_none()) else {
            continue;
        };
        let mut comment = crate::store::clean::FOLDER_LINK_REMOVED;

        // Nothing links it, and a document with no link is global: it is a draft that is looked at
        // again (a waiting or sent-back row needs a new ask), and a pending update of a live one
        // goes, so no pending version points at a draft.
        if state.status != Status::Draft {
            db.execute(
                "UPDATE knowledge_documents SET status = 'draft', review_rev = review_rev + 1 WHERE id = ?1",
                args![id],
            )?;
        }

        if state.status == Status::Live
            && let Some(pending) = query_row(
                db,
                "SELECT id FROM knowledge_documents WHERE pending_of = ?1",
                args![id],
            )?
        {
            delete(db, &[pending.get::<i64>(0)?])?;
            comment = crate::store::clean::FOLDER_LINK_REMOVED_PENDING;
        }

        record(
            db,
            id,
            EventKind::Edited,
            None,
            Some(comment),
            crate::import::document::now(),
        )?;
    }

    Ok((removed, removed > 0))
}

/// Replaces the links of document `id`; false when there is none.
pub(super) fn set_links(db: &mut dyn Sql, id: i64, links: &[Link]) -> Result<bool, StoreError> {
    if query_row(db, "SELECT 1 FROM knowledge_documents WHERE id = ?1", args![id])?.is_none() {
        return Ok(false);
    }

    db.execute("DELETE FROM knowledge_doc_links WHERE doc_id = ?1", args![id])?;
    insert_links(db, id, links)?;

    Ok(true)
}

/// Retitles document `id` (to the cleaned `title`); false when there is none. The chunks follow
/// through the retitle trigger, so the keyword index finds the new title. A row that is not live
/// takes it as a change (see [`retitle`]), and so does the pending version of a live one.
pub(super) fn rename(db: &mut dyn Sql, id: i64, title: &str, by: Option<&str>, at: i64) -> Result<bool, StoreError> {
    let Some(state) = row_state(db, id)? else {
        return Ok(false);
    };

    retitle(db, id, &state, title, by, at)?;

    if !state.status.is_live() {
        return Ok(true);
    }

    let pending: Option<i64> = query_row(
        db,
        "SELECT id FROM knowledge_documents WHERE pending_of = ?1",
        args![id],
    )?
    .map(|row| row.get(0))
    .transpose()?;

    if let Some(pending) = pending
        && let Some(state) = row_state(db, pending)?
    {
        retitle(db, pending, &state, title, by, at)?;
    }

    Ok(true)
}

/// Gives the row `id`, in `state`, the title. A live row is only retitled. A row that is not live
/// has a new revision, a waiting one is a draft again (what was asked is no longer what is there)
/// and the `edited` event says what changed. Nothing when the title already is `title`.
fn retitle(
    db: &mut dyn Sql,
    id: i64,
    state: &RowState,
    title: &str,
    by: Option<&str>,
    at: i64,
) -> Result<(), StoreError> {
    // The renamer goes on the `edited` event, not on the row: who wrote the content stays as it was.
    let renamer = actor(by);

    if state.status.is_live() {
        db.execute(
            "UPDATE knowledge_documents SET title = ?2, updated_at = ?3
             WHERE id = ?1 AND title <> ?2",
            args![id, title, at],
        )?;

        return Ok(());
    }

    let done = db.execute(
        "UPDATE knowledge_documents SET title = ?2, review_rev = review_rev + 1,
             status = CASE WHEN status = 'waiting' THEN 'draft' ELSE status END, updated_at = ?3
         WHERE id = ?1 AND title <> ?2",
        args![id, title, at],
    )?;

    if done.changes > 0 {
        record(db, id, EventKind::Edited, renamer.as_deref(), Some(RENAMED_COMMENT), at)?;
    }

    Ok(())
}

/// Deletes the documents; the chunks, links and index entries go with them.
pub(super) fn delete(db: &mut dyn Sql, ids: &[i64]) -> Result<usize, StoreError> {
    if ids.len() > DELETE_IDS_MAX {
        return Err(StoreError::Invalid(format!(
            "at most {DELETE_IDS_MAX} documents at once"
        )));
    }

    let wanted = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
    let done = db.execute(
        "DELETE FROM knowledge_documents WHERE id IN (SELECT value FROM json_each(?1))",
        args![wanted],
    )?;

    Ok(done.changes)
}

/// The oldest document `visibility` lets through whose `column` (`content_sha256` or `source_key`)
/// is `value`.
pub(super) fn find(
    db: &mut dyn Sql,
    columns: Columns,
    column: &str,
    value: &str,
    visibility: Visibility,
) -> Result<Option<Document>, StoreError> {
    let sql = format!(
        "SELECT {} FROM knowledge_documents d WHERE d.{column} = ?1 AND {} ORDER BY d.id LIMIT 1",
        document_columns(columns),
        seen(visibility)
    );

    query_row(db, &sql, args![value])?.map(|row| document(&row)).transpose()
}

/// The embeddings of chunks made by `model_id`, by chunk hash.
pub(super) fn reusable_embeddings(
    db: &mut dyn Sql,
    model_id: &str,
    shas: &[String],
) -> Result<HashMap<String, Vec<f32>>, StoreError> {
    let wanted = serde_json::to_string(shas).unwrap_or_else(|_| "[]".to_owned());
    let rows = db.query(
        "SELECT chunk_sha, embedding FROM knowledge_chunks
         WHERE model_id = ?1 AND embedding IS NOT NULL AND chunk_sha IN (SELECT value FROM json_each(?2))",
        args![model_id, wanted],
    )?;

    let mut found: HashMap<String, Vec<f32>> = HashMap::new();

    for row in rows {
        let sha: String = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;

        if let Some(vector) = decode(&blob) {
            found.entry(sha).or_insert(vector);
        }
    }

    Ok(found)
}

/// The next `limit` chunks after chunk `after` that have no embedding of `model_id`.
pub(super) fn pending_chunks(
    db: &mut dyn Sql,
    model_id: &str,
    after: i64,
    limit: usize,
) -> Result<Vec<PendingChunk>, StoreError> {
    let rows = db.query(
        "SELECT id, doc_id, title, heading_path, text FROM knowledge_chunks
         WHERE id > ?1 AND (model_id IS NULL OR model_id != ?2) ORDER BY id LIMIT ?3",
        args![after, model_id, limit.min(EMBED_BATCH_MAX)],
    )?;

    rows.iter()
        .map(|row| {
            let (title, heading, body): (String, String, String) = (row.get(2)?, row.get(3)?, row.get(4)?);

            Ok(PendingChunk {
                chunk_id: row.get(0)?,
                doc_id: row.get(1)?,
                embed_text: passage(&title, &heading, &body),
            })
        })
        .collect()
}

/// Stores the embeddings (each updates the ANN graph); the number of chunks that existed. The
/// keyword index is not touched (its trigger fires on title, heading and text only).
pub(super) fn set_embeddings(
    db: &mut dyn Sql,
    model_id: &str,
    embeddings: &[(i64, Vec<f32>)],
) -> Result<usize, StoreError> {
    let rows: Vec<Vec<Value>> = embeddings
        .iter()
        .map(|(chunk_id, vector)| vec![(*chunk_id).into(), vector_text(vector).into(), model_id.into()])
        .collect();

    db.execute_many(
        "UPDATE knowledge_chunks SET embedding = vector32(?2), model_id = ?3 WHERE id = ?1",
        &rows,
    )?;

    let ids: Vec<i64> = embeddings.iter().map(|(chunk_id, _)| *chunk_id).collect();
    let wanted = serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_owned());
    let found = query_row(
        db,
        "SELECT count(*) FROM knowledge_chunks WHERE model_id = ?2 AND id IN (SELECT value FROM json_each(?1))",
        args![wanted, model_id],
    )?;

    found.map_or(Ok(0), |row| row.get(0))
}

/// The links of document `id`, by repository key.
pub(super) fn links_of(db: &mut dyn Sql, id: i64) -> Result<Vec<Link>, StoreError> {
    let rows = db.query(
        "SELECT repo_key, globs FROM knowledge_doc_links WHERE doc_id = ?1 ORDER BY repo_key",
        args![id],
    )?;

    rows.iter()
        .map(|row| Ok(link(row.get(0)?, &row.get::<String>(1)?)))
        .collect()
}

/// The links of each of `ids`.
pub(super) fn links_of_many(db: &mut dyn Sql, ids: &[i64]) -> Result<HashMap<i64, Vec<Link>>, StoreError> {
    let wanted = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
    let rows = db.query(
        "SELECT doc_id, repo_key, globs FROM knowledge_doc_links
         WHERE doc_id IN (SELECT value FROM json_each(?1)) ORDER BY doc_id, repo_key",
        args![wanted],
    )?;

    let mut by_document: HashMap<i64, Vec<Link>> = HashMap::new();

    for row in &rows {
        let id: i64 = row.get(0)?;
        by_document
            .entry(id)
            .or_default()
            .push(link(row.get(1)?, &row.get::<String>(2)?));
    }

    Ok(by_document)
}

fn link(repo_key: String, globs: &str) -> Link {
    Link {
        repo_key,
        globs: serde_json::from_str(globs).unwrap_or_default(),
    }
}

pub(super) fn not_found(id: i64) -> StoreError {
    StoreError::NotFound(format!("document {id} not found"))
}

/// The JSON array of `vector`, which `vector32` reads.
pub(super) fn vector_text(vector: &[f32]) -> String {
    let numbers: Vec<String> = vector.iter().map(f32::to_string).collect();

    format!("[{}]", numbers.join(","))
}

/// The vector in `bytes` (little-endian `f32`s); `None` unless they are exactly one embedding.
fn decode(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() != DIMS * 4 {
        return None;
    }

    let (floats, _) = bytes.as_chunks::<4>();

    Some(floats.iter().map(|four| f32::from_le_bytes(*four)).collect())
}
