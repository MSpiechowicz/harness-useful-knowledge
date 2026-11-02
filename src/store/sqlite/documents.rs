//! Writing and finding documents: the statements behind `put_document`, `set_links`, `rename`,
//! `delete` and the lookups an import makes.

use std::collections::HashMap;

use rusqlite::types::{Type, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use super::schema::EMBEDDING_BYTES;
use super::sql_quality::{document_columns, stamp};
use crate::chunk::passage;
use crate::limits::{DELETE_IDS_MAX, EMBED_BATCH_MAX};
use crate::store::{Author, Chunk, Columns, DocUsage, Document, Link, Origin, PendingChunk, StoreError};

/// A [`Document`] from a row of [`document_columns`].
pub(super) fn document(row: &Row) -> rusqlite::Result<Document> {
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
            .ok_or_else(|| rusqlite::Error::InvalidColumnType(11, "origin".to_owned(), Type::Text))?,
        warnings: serde_json::from_str(&warnings).unwrap_or_default(),
        added_by: Author::stored(tolerant_text(row, 13)?, tolerant_text(row, 14)?),
        updated_by: Author::stored(tolerant_text(row, 15)?, tolerant_text(row, 16)?),
    })
}

/// A [`Document`] with its usage from a row of [`document_columns`].
pub(super) fn listed(row: &Row) -> rusqlite::Result<(Document, Option<DocUsage>)> {
    let usage = DocUsage::stored(
        tolerant_counter(row, 17)?,
        tolerant_counter(row, 18)?,
        tolerant_counter(row, 19)?,
    );

    Ok((document(row)?, usage))
}

/// A text column that anybody with access to the file may have filled with something else: only
/// text is read, any other value reads as none.
fn tolerant_text(row: &Row, index: usize) -> rusqlite::Result<Option<String>> {
    Ok(match row.get_ref(index)? {
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    })
}

/// A counter column the same way: an integer as it is, a float (an overflow, or a hand edit) cut to
/// the integers, anything else as 0. NULL (a column the file lacks) stays none.
fn tolerant_counter(row: &Row, index: usize) -> rusqlite::Result<Option<i64>> {
    Ok(match row.get_ref(index)? {
        ValueRef::Null => None,
        ValueRef::Integer(count) => Some(count),
        ValueRef::Real(count) => Some(count as i64),
        _ => Some(0),
    })
}

/// Document `id` with its usage.
pub(super) fn by_id(
    conn: &Connection,
    columns: Columns,
    id: i64,
) -> Result<Option<(Document, Option<DocUsage>)>, StoreError> {
    let sql = format!(
        "SELECT {} FROM knowledge_documents d WHERE d.id = ?1",
        document_columns(columns)
    );

    Ok(conn.query_row(&sql, [id], listed).optional()?)
}

/// Records who stored `doc` as document `id`, which [`put`] has just written. A new document gets
/// both authors, an update keeps who added it. Nothing without the author columns.
pub(super) fn stamp_authors(tx: &Transaction, columns: Columns, id: i64, doc: &Document) -> Result<(), StoreError> {
    if !columns.authors {
        return Ok(());
    }

    let (sql, name, via) = stamp(doc);
    tx.execute(sql, params![id, name, via])?;

    Ok(())
}

/// Stores `doc` (whose cleaned title is `title`) with `chunks` and `links`: inserted for id 0,
/// else replacing that document's chunks and links.
pub(super) fn put(
    tx: &Transaction,
    doc: &Document,
    title: &str,
    chunks: &[Chunk],
    links: &[Link],
) -> Result<i64, StoreError> {
    let warnings = serde_json::to_string(&doc.warnings).unwrap_or_else(|_| "[]".to_owned());
    let chunk_count = i64::try_from(chunks.len()).unwrap_or(i64::MAX);

    let id = if doc.id == 0 {
        tx.query_row(
            "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256,
                 bytes, chunk_count, origin, warnings, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) RETURNING id",
            params![
                title,
                doc.source_name,
                doc.source_key,
                doc.format,
                doc.content_sha256,
                doc.file_sha256,
                doc.bytes,
                chunk_count,
                doc.origin.as_str(),
                warnings,
                doc.created_at,
                doc.updated_at,
            ],
            |row| row.get(0),
        )?
    } else {
        // The old chunks go first, so the retitle trigger has no chunks to rewrite.
        tx.execute("DELETE FROM knowledge_chunks WHERE doc_id = ?1", [doc.id])?;
        tx.execute("DELETE FROM knowledge_doc_links WHERE doc_id = ?1", [doc.id])?;

        let changed = tx.execute(
            "UPDATE knowledge_documents SET title = ?2, source_name = ?3, source_key = ?4, format = ?5,
                 content_sha256 = ?6, file_sha256 = ?7, bytes = ?8, chunk_count = ?9, origin = ?10,
                 warnings = ?11, updated_at = ?12
             WHERE id = ?1",
            params![
                doc.id,
                title,
                doc.source_name,
                doc.source_key,
                doc.format,
                doc.content_sha256,
                doc.file_sha256,
                doc.bytes,
                chunk_count,
                doc.origin.as_str(),
                warnings,
                doc.updated_at,
            ],
        )?;

        if changed == 0 {
            return Err(not_found(doc.id));
        }

        doc.id
    };

    insert_chunks(tx, id, title, chunks)?;
    insert_links(tx, id, links)?;

    Ok(id)
}

fn insert_chunks(tx: &Transaction, doc_id: i64, title: &str, chunks: &[Chunk]) -> Result<(), StoreError> {
    let mut insert = tx.prepare(
        "INSERT INTO knowledge_chunks (doc_id, ordinal, title, heading_path, text, chunk_sha, tokens,
             char_start, char_end, embedding, model_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;

    for chunk in chunks {
        insert.execute(params![
            doc_id,
            chunk.ordinal,
            title,
            chunk.heading_path,
            chunk.text,
            chunk.chunk_sha,
            chunk.tokens,
            chunk.char_start,
            chunk.char_end,
            chunk.embedding.as_deref().map(encode),
            chunk.model_id,
        ])?;
    }

    Ok(())
}

fn insert_links(tx: &Transaction, doc_id: i64, links: &[Link]) -> Result<(), StoreError> {
    let mut insert = tx.prepare("INSERT INTO knowledge_doc_links (doc_id, repo_key, globs) VALUES (?1, ?2, ?3)")?;

    for link in links {
        let globs = serde_json::to_string(&link.globs).unwrap_or_else(|_| "[]".to_owned());
        insert.execute(params![doc_id, link.repo_key, globs])?;
    }

    Ok(())
}

/// Replaces the links of document `id`; false when there is none.
pub(super) fn set_links(tx: &Transaction, id: i64, links: &[Link]) -> Result<bool, StoreError> {
    if !exists(tx, id)? {
        return Ok(false);
    }

    tx.execute("DELETE FROM knowledge_doc_links WHERE doc_id = ?1", [id])?;
    insert_links(tx, id, links)?;

    Ok(true)
}

/// Retitles document `id` (to the cleaned `title`); false when there is none. The chunks follow
/// through the retitle trigger, so the keyword index finds the new title.
pub(super) fn rename(tx: &Transaction, id: i64, title: &str) -> Result<bool, StoreError> {
    let changed = tx.execute(
        "UPDATE knowledge_documents SET title = ?2 WHERE id = ?1",
        params![id, title],
    )?;

    Ok(changed > 0)
}

/// Deletes the documents; the chunks, links and index entries go with them.
pub(super) fn delete(tx: &Transaction, ids: &[i64]) -> Result<usize, StoreError> {
    if ids.len() > DELETE_IDS_MAX {
        return Err(StoreError::Invalid(format!(
            "at most {DELETE_IDS_MAX} documents at once"
        )));
    }

    let mut deleted = 0;

    for id in ids {
        deleted += tx.execute("DELETE FROM knowledge_documents WHERE id = ?1", [id])?;
    }

    Ok(deleted)
}

/// The oldest document whose `column` (`content_sha256` or `source_key`) is `value`.
pub(super) fn find(
    conn: &Connection,
    columns: Columns,
    column: &str,
    value: &str,
) -> Result<Option<Document>, StoreError> {
    let sql = format!(
        "SELECT {} FROM knowledge_documents d WHERE d.{column} = ?1 ORDER BY d.id LIMIT 1",
        document_columns(columns)
    );

    Ok(conn.query_row(&sql, [value], document).optional()?)
}

/// The embeddings of chunks made by `model_id`, by chunk hash.
pub(super) fn reusable_embeddings(
    conn: &Connection,
    model_id: &str,
    shas: &[String],
) -> Result<HashMap<String, Vec<f32>>, StoreError> {
    let wanted = serde_json::to_string(shas).unwrap_or_else(|_| "[]".to_owned());
    let mut statement = conn.prepare(
        "SELECT chunk_sha, embedding FROM knowledge_chunks
         WHERE model_id = ?1 AND embedding IS NOT NULL AND chunk_sha IN (SELECT value FROM json_each(?2))",
    )?;

    let mut rows = statement.query(params![model_id, wanted])?;
    let mut found: HashMap<String, Vec<f32>> = HashMap::new();

    while let Some(row) = rows.next()? {
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
    conn: &Connection,
    model_id: &str,
    after: i64,
    limit: usize,
) -> Result<Vec<PendingChunk>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT id, doc_id, title, heading_path, text FROM knowledge_chunks
         WHERE id > ?1 AND (model_id IS NULL OR model_id != ?2) ORDER BY id LIMIT ?3",
    )?;
    let limit = limit.min(EMBED_BATCH_MAX);
    let rows = statement.query_map(params![after, model_id, limit], |row| {
        let (title, heading, text): (String, String, String) = (row.get(2)?, row.get(3)?, row.get(4)?);

        Ok(PendingChunk {
            chunk_id: row.get(0)?,
            doc_id: row.get(1)?,
            embed_text: passage(&title, &heading, &text),
        })
    })?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// Stores the embeddings; the number of chunks that existed. The keyword index is not touched (its
/// trigger fires on title, heading and text only).
pub(super) fn set_embeddings(
    tx: &Transaction,
    model_id: &str,
    embeddings: &[(i64, Vec<f32>)],
) -> Result<usize, StoreError> {
    let mut update = tx.prepare("UPDATE knowledge_chunks SET embedding = ?2, model_id = ?3 WHERE id = ?1")?;
    let mut changed = 0;

    for (chunk_id, vector) in embeddings {
        changed += update.execute(params![chunk_id, encode(vector), model_id])?;
    }

    Ok(changed)
}

/// The links of document `id`, by repository key.
pub(super) fn links_of(conn: &Connection, id: i64) -> Result<Vec<Link>, StoreError> {
    let mut statement =
        conn.prepare("SELECT repo_key, globs FROM knowledge_doc_links WHERE doc_id = ?1 ORDER BY repo_key")?;
    let rows = statement.query_map([id], |row| Ok(link(row.get(0)?, &row.get::<_, String>(1)?)))?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// The links of each of `ids`.
pub(super) fn links_of_many(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, Vec<Link>>, StoreError> {
    let wanted = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
    let mut statement = conn.prepare(
        "SELECT doc_id, repo_key, globs FROM knowledge_doc_links
         WHERE doc_id IN (SELECT value FROM json_each(?1)) ORDER BY doc_id, repo_key",
    )?;
    let rows = statement.query_map([wanted], |row| {
        Ok((row.get::<_, i64>(0)?, link(row.get(1)?, &row.get::<_, String>(2)?)))
    })?;

    let mut by_document: HashMap<i64, Vec<Link>> = HashMap::new();

    for row in rows {
        let (id, link) = row?;
        by_document.entry(id).or_default().push(link);
    }

    Ok(by_document)
}

fn link(repo_key: String, globs: &str) -> Link {
    Link {
        repo_key,
        globs: serde_json::from_str(globs).unwrap_or_default(),
    }
}

fn exists(tx: &Transaction, id: i64) -> Result<bool, StoreError> {
    let found: Option<i64> = tx
        .query_row("SELECT 1 FROM knowledge_documents WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?;

    Ok(found.is_some())
}

pub(super) fn not_found(id: i64) -> StoreError {
    StoreError::NotFound(format!("document {id} not found"))
}

/// The little-endian bytes of `vector`.
pub(super) fn encode(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|value| value.to_le_bytes()).collect()
}

/// The vector in `bytes`; `None` unless they are exactly one embedding.
pub(super) fn decode(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() != EMBEDDING_BYTES {
        return None;
    }

    let (floats, _) = bytes.as_chunks::<4>();

    Some(floats.iter().map(|four| f32::from_le_bytes(*four)).collect())
}
