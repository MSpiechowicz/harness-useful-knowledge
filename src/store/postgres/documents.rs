//! Writing and finding documents: the statements behind `put_document`, `set_links`, `rename`,
//! `delete` and the lookups an import makes.
//!
//! PostgreSQL refuses a NUL character in text, so [`text`] drops it from everything written. The
//! chunks of a document go in with a few `unnest` statements instead of one statement each, so a
//! document of thousands of chunks costs a handful of round trips. An embedding is sent as the
//! text of its pgvector literal (`[0.1,0.2,...]`) and read back as a `real[]`.

use std::borrow::Cow;
use std::collections::HashMap;

use postgres::{GenericClient, Row};

use super::error::{backend, column, count, int};
use crate::chunk::passage;
use crate::limits::{DELETE_IDS_MAX, EMBED_BATCH_MAX};
use crate::store::{Chunk, Document, Link, Origin, PendingChunk, StoreError};

/// How many chunks one `INSERT` carries.
const CHUNK_BATCH: usize = 100;

/// The columns [`document`] reads, of a document aliased `d`.
pub(super) const COLUMNS: &str = "d.id, d.title, d.source_name, d.source_key, d.format, d.content_sha256, \
     d.file_sha256, d.bytes, d.chunk_count, d.created_at, d.updated_at, d.origin, d.warnings";

/// A [`Document`] from a row of [`COLUMNS`].
pub(super) fn document(row: &Row) -> Result<Document, StoreError> {
    let origin: String = column(row, 11)?;
    let bytes: i64 = column(row, 7)?;

    Ok(Document {
        id: column(row, 0)?,
        title: column(row, 1)?,
        source_name: column(row, 2)?,
        source_key: column(row, 3)?,
        format: column(row, 4)?,
        content_sha256: column(row, 5)?,
        file_sha256: column(row, 6)?,
        bytes: u64::try_from(bytes).map_err(|_| StoreError::Backend("a stored size is negative".to_owned()))?,
        chunk_count: count(row, 8)?,
        created_at: column(row, 9)?,
        updated_at: column(row, 10)?,
        origin: Origin::parse(&origin).ok_or_else(|| StoreError::Backend("a stored origin is unknown".to_owned()))?,
        warnings: column(row, 12)?,
    })
}

/// `value` without the NUL characters the database cannot hold.
fn text(value: &str) -> Cow<'_, str> {
    if value.contains('\0') {
        Cow::Owned(value.replace('\0', ""))
    } else {
        Cow::Borrowed(value)
    }
}

/// Stores `doc` (whose cleaned title is `title`) with `chunks` and `links`: inserted for id 0,
/// else replacing that document's chunks and links.
pub(super) fn put(
    tx: &mut impl GenericClient,
    doc: &Document,
    title: &str,
    chunks: &[Chunk],
    links: &[Link],
) -> Result<i64, StoreError> {
    let warnings: Vec<String> = doc.warnings.iter().map(|warning| text(warning).into_owned()).collect();
    let source_name = text(&doc.source_name);
    let source_key = text(&doc.source_key);
    let format = text(&doc.format);
    let bytes = i64::try_from(doc.bytes).unwrap_or(i64::MAX);
    let chunk_count = int(chunks.len());

    let id = if doc.id == 0 {
        let row = tx
            .query_one(
                "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256,
                     bytes, chunk_count, origin, warnings, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING id",
                &[
                    &title,
                    &&*source_name,
                    &&*source_key,
                    &&*format,
                    &doc.content_sha256,
                    &doc.file_sha256,
                    &bytes,
                    &chunk_count,
                    &doc.origin.as_str(),
                    &warnings,
                    &doc.created_at,
                    &doc.updated_at,
                ],
            )
            .map_err(backend)?;

        column(&row, 0)?
    } else {
        tx.execute("DELETE FROM knowledge_chunks WHERE doc_id = $1", &[&doc.id])
            .map_err(backend)?;
        tx.execute("DELETE FROM knowledge_doc_links WHERE doc_id = $1", &[&doc.id])
            .map_err(backend)?;

        let changed = tx
            .execute(
                "UPDATE knowledge_documents SET title = $2, source_name = $3, source_key = $4, format = $5,
                     content_sha256 = $6, file_sha256 = $7, bytes = $8, chunk_count = $9, origin = $10,
                     warnings = $11, updated_at = $12
                 WHERE id = $1",
                &[
                    &doc.id,
                    &title,
                    &&*source_name,
                    &&*source_key,
                    &&*format,
                    &doc.content_sha256,
                    &doc.file_sha256,
                    &bytes,
                    &chunk_count,
                    &doc.origin.as_str(),
                    &warnings,
                    &doc.updated_at,
                ],
            )
            .map_err(backend)?;

        if changed == 0 {
            return Err(not_found(doc.id));
        }

        doc.id
    };

    insert_chunks(tx, id, title, chunks)?;
    insert_links(tx, id, links)?;

    Ok(id)
}

fn insert_chunks(tx: &mut impl GenericClient, doc_id: i64, title: &str, chunks: &[Chunk]) -> Result<(), StoreError> {
    for batch in chunks.chunks(CHUNK_BATCH) {
        let ordinals: Vec<i64> = batch.iter().map(|chunk| int(chunk.ordinal)).collect();
        let headings: Vec<String> = batch
            .iter()
            .map(|chunk| text(&chunk.heading_path).into_owned())
            .collect();
        let texts: Vec<String> = batch.iter().map(|chunk| text(&chunk.text).into_owned()).collect();
        let shas: Vec<String> = batch.iter().map(|chunk| text(&chunk.chunk_sha).into_owned()).collect();
        let tokens: Vec<i64> = batch.iter().map(|chunk| int(chunk.tokens)).collect();
        let starts: Vec<i64> = batch.iter().map(|chunk| int(chunk.char_start)).collect();
        let ends: Vec<i64> = batch.iter().map(|chunk| int(chunk.char_end)).collect();
        let vectors: Vec<Option<String>> = batch
            .iter()
            .map(|chunk| chunk.embedding.as_deref().map(literal))
            .collect();
        let models: Vec<Option<String>> = batch
            .iter()
            .map(|chunk| chunk.model_id.as_deref().map(|model| text(model).into_owned()))
            .collect();

        tx.execute(
            "INSERT INTO knowledge_chunks (doc_id, ordinal, title, heading_path, text, chunk_sha, tokens,
                 char_start, char_end, embedding, model_id)
             SELECT $1, t.ordinal, $2, t.heading_path, t.text, t.chunk_sha, t.tokens, t.char_start, t.char_end,
                    t.embedding::vector, t.model_id
             FROM unnest($3::bigint[], $4::text[], $5::text[], $6::text[], $7::bigint[], $8::bigint[],
                         $9::bigint[], $10::text[], $11::text[])
                  AS t(ordinal, heading_path, text, chunk_sha, tokens, char_start, char_end, embedding, model_id)",
            &[
                &doc_id, &title, &ordinals, &headings, &texts, &shas, &tokens, &starts, &ends, &vectors, &models,
            ],
        )
        .map_err(backend)?;
    }

    Ok(())
}

fn insert_links(tx: &mut impl GenericClient, doc_id: i64, links: &[Link]) -> Result<(), StoreError> {
    for link in links {
        tx.execute(
            "INSERT INTO knowledge_doc_links (doc_id, repo_key, globs) VALUES ($1, $2, $3)",
            &[&doc_id, &link.repo_key, &link.globs],
        )
        .map_err(backend)?;
    }

    Ok(())
}

/// Replaces the links of document `id`, false when there is none.
pub(super) fn set_links(tx: &mut impl GenericClient, id: i64, links: &[Link]) -> Result<bool, StoreError> {
    let found = tx
        .query_opt("SELECT 1 FROM knowledge_documents WHERE id = $1 FOR UPDATE", &[&id])
        .map_err(backend)?;

    if found.is_none() {
        return Ok(false);
    }

    tx.execute("DELETE FROM knowledge_doc_links WHERE doc_id = $1", &[&id])
        .map_err(backend)?;
    insert_links(tx, id, links)?;

    Ok(true)
}

/// Retitles document `id` (to the cleaned `title`), false when there is none. The copies of the
/// title in the chunks follow, so the keyword index finds the new title.
pub(super) fn rename(tx: &mut impl GenericClient, id: i64, title: &str) -> Result<bool, StoreError> {
    let changed = tx
        .execute(
            "UPDATE knowledge_documents SET title = $2 WHERE id = $1",
            &[&id, &title],
        )
        .map_err(backend)?;

    if changed == 0 {
        return Ok(false);
    }

    tx.execute(
        "UPDATE knowledge_chunks SET title = $2 WHERE doc_id = $1",
        &[&id, &title],
    )
    .map_err(backend)?;

    Ok(true)
}

/// Deletes the documents, the chunks and links go with them.
pub(super) fn delete(tx: &mut impl GenericClient, ids: &[i64]) -> Result<usize, StoreError> {
    if ids.len() > DELETE_IDS_MAX {
        return Err(StoreError::Invalid(format!(
            "at most {DELETE_IDS_MAX} documents at once"
        )));
    }

    let deleted = tx
        .execute("DELETE FROM knowledge_documents WHERE id = ANY($1)", &[&ids])
        .map_err(backend)?;

    Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
}

/// The oldest document whose `column` (`content_sha256` or `source_key`) is `value`.
pub(super) fn find(
    client: &mut impl GenericClient,
    column_name: &str,
    value: &str,
) -> Result<Option<Document>, StoreError> {
    let sql = format!("SELECT {COLUMNS} FROM knowledge_documents d WHERE d.{column_name} = $1 ORDER BY d.id LIMIT 1");
    let row = client.query_opt(&sql, &[&value]).map_err(backend)?;

    row.map(|row| document(&row)).transpose()
}

/// The embeddings of chunks made by `model_id`, by chunk hash.
pub(super) fn reusable_embeddings(
    client: &mut impl GenericClient,
    model_id: &str,
    shas: &[String],
) -> Result<HashMap<String, Vec<f32>>, StoreError> {
    let rows = client
        .query(
            "SELECT DISTINCT ON (chunk_sha) chunk_sha, embedding::real[] FROM knowledge_chunks
             WHERE model_id = $1 AND embedding IS NOT NULL AND chunk_sha = ANY($2)
             ORDER BY chunk_sha, id",
            &[&model_id, &shas],
        )
        .map_err(backend)?;

    let mut found = HashMap::new();

    for row in &rows {
        let vector: Vec<f32> = column(row, 1)?;
        found.insert(column(row, 0)?, vector);
    }

    Ok(found)
}

/// The next `limit` chunks after chunk `after` that have no embedding of `model_id`.
pub(super) fn pending_chunks(
    client: &mut impl GenericClient,
    model_id: &str,
    after: i64,
    limit: usize,
) -> Result<Vec<PendingChunk>, StoreError> {
    let limit = int(limit.min(EMBED_BATCH_MAX));
    let rows = client
        .query(
            "SELECT id, doc_id, title, heading_path, text FROM knowledge_chunks
             WHERE id > $1 AND model_id IS DISTINCT FROM $2 ORDER BY id LIMIT $3",
            &[&after, &model_id, &limit],
        )
        .map_err(backend)?;

    rows.iter()
        .map(|row| {
            let (title, heading, body): (String, String, String) = (column(row, 2)?, column(row, 3)?, column(row, 4)?);

            Ok(PendingChunk {
                chunk_id: column(row, 0)?,
                doc_id: column(row, 1)?,
                embed_text: passage(&title, &heading, &body),
            })
        })
        .collect()
}

/// Stores the embeddings in one statement; the number of chunks that existed. The generated
/// keyword column does not depend on the embedding.
pub(super) fn set_embeddings(
    tx: &mut impl GenericClient,
    model_id: &str,
    embeddings: &[(i64, Vec<f32>)],
) -> Result<usize, StoreError> {
    let ids: Vec<i64> = embeddings.iter().map(|(id, _)| *id).collect();
    let vectors: Vec<String> = embeddings.iter().map(|(_, vector)| literal(vector)).collect();

    let changed = tx
        .execute(
            "UPDATE knowledge_chunks c SET embedding = t.embedding::vector, model_id = $3
             FROM unnest($1::bigint[], $2::text[]) AS t(id, embedding) WHERE c.id = t.id",
            &[&ids, &vectors, &model_id],
        )
        .map_err(backend)?;

    Ok(usize::try_from(changed).unwrap_or(usize::MAX))
}

/// The links of each of `ids`, by document.
pub(super) fn links_of(client: &mut impl GenericClient, ids: &[i64]) -> Result<HashMap<i64, Vec<Link>>, StoreError> {
    let rows = client
        .query(
            "SELECT doc_id, repo_key, globs FROM knowledge_doc_links
             WHERE doc_id = ANY($1) ORDER BY doc_id, repo_key COLLATE \"C\"",
            &[&ids],
        )
        .map_err(backend)?;

    let mut by_document: HashMap<i64, Vec<Link>> = HashMap::new();

    for row in &rows {
        by_document.entry(column(row, 0)?).or_default().push(Link {
            repo_key: column(row, 1)?,
            globs: column(row, 2)?,
        });
    }

    Ok(by_document)
}

pub(super) fn not_found(id: i64) -> StoreError {
    StoreError::NotFound(format!("document {id} not found"))
}

/// The pgvector literal of `vector`: `[0.1,0.2]`.
pub(super) fn literal(vector: &[f32]) -> String {
    let numbers: Vec<String> = vector.iter().map(f32::to_string).collect();

    format!("[{}]", numbers.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vector_is_written_as_a_pgvector_literal() {
        assert_eq!(literal(&[1.0, 0.5, -0.25]), "[1,0.5,-0.25]");
        assert_eq!(literal(&[1e-10]), "[0.0000000001]");
        assert_eq!(literal(&[]), "[]");
    }

    #[test]
    fn nul_characters_are_dropped_and_other_text_is_kept() {
        assert_eq!(text("a\0b"), "ab");
        assert_eq!(text("plain ł ✓"), "plain ł ✓");
    }
}
