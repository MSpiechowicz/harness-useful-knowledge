//! Writing and finding documents: the statements behind `put_document`, `set_links`, `rename`,
//! `delete` and the lookups an import makes.
//!
//! A vector goes in as the JSON text of its floats through `vector32(?)` (Rust prints an `f32`
//! with the digits that read back exactly) and comes out as the blob of little-endian `f32`s.

use std::collections::HashMap;

use super::sql::{Row, Sql, Value, args, query_row};
use crate::chunk::passage;
use crate::limits::{DELETE_IDS_MAX, EMBED_BATCH_MAX};
use crate::store::sqlite::sql_quality::{document_columns, stamp};
use crate::store::{Author, Chunk, Columns, DIMS, DocUsage, Document, Link, Origin, PendingChunk, StoreError};

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

/// Stores `doc` (whose cleaned title is `title`) with `chunks` and `links`: inserted for id 0,
/// else replacing that document's chunks and links.
pub(super) fn put(
    db: &mut dyn Sql,
    doc: &Document,
    title: &str,
    chunks: &[Chunk],
    links: &[Link],
) -> Result<i64, StoreError> {
    let warnings = serde_json::to_string(&doc.warnings).unwrap_or_else(|_| "[]".to_owned());
    let chunk_count = chunks.len();

    let id = if doc.id == 0 {
        db.execute(
            "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256,
                 bytes, chunk_count, origin, warnings, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
            ],
        )?
        .last_insert_rowid
    } else {
        // The old chunks go first, so the retitle trigger has no chunks to rewrite.
        db.execute("DELETE FROM knowledge_chunks WHERE doc_id = ?1", args![doc.id])?;
        db.execute("DELETE FROM knowledge_doc_links WHERE doc_id = ?1", args![doc.id])?;

        let changed = db.execute(
            "UPDATE knowledge_documents SET title = ?2, source_name = ?3, source_key = ?4, format = ?5,
                 content_sha256 = ?6, file_sha256 = ?7, bytes = ?8, chunk_count = ?9, origin = ?10,
                 warnings = ?11, updated_at = ?12
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

        if changed.changes == 0 {
            return Err(not_found(doc.id));
        }

        doc.id
    };

    insert_chunks(db, id, title, chunks)?;
    insert_links(db, id, links)?;

    Ok(id)
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
/// through the retitle trigger, so the keyword index finds the new title.
pub(super) fn rename(db: &mut dyn Sql, id: i64, title: &str) -> Result<bool, StoreError> {
    let done = db.execute(
        "UPDATE knowledge_documents SET title = ?2 WHERE id = ?1",
        args![id, title],
    )?;

    Ok(done.changes > 0)
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

/// The oldest document whose `column` (`content_sha256` or `source_key`) is `value`.
pub(super) fn find(
    db: &mut dyn Sql,
    columns: Columns,
    column: &str,
    value: &str,
) -> Result<Option<Document>, StoreError> {
    let sql = format!(
        "SELECT {} FROM knowledge_documents d WHERE d.{column} = ?1 ORDER BY d.id LIMIT 1",
        document_columns(columns)
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
