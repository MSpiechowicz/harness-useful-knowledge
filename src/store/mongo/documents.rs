//! Writing and finding documents: what `put_document`, `set_links`, `rename` and `delete` do to
//! the two collections, the lookups an import makes, and how a document and a chunk are stored.

use std::cell::Cell;
use std::collections::HashMap;

use mongodb::bson::{Bson, Document as Row, doc};

use super::ctx::{Ctx, Fail, long};
use super::schema::{CHUNKS, DOCUMENTS};
use super::{int, strings, terms, text};
use crate::chunk::passage;
use crate::limits::{DELETE_IDS_MAX, EMBED_BATCH_MAX};
use crate::store::{Chunk, DIMS, Document, Link, Origin, PendingChunk, StoreError};

/// The fields of a document that change with an import or a rename (everything but `created_at`).
fn fields(doc: &Document, title: &str, chunks: usize, links: &[Link]) -> Row {
    let mut fields = doc! {
        "title": title,
        "title_key": title.to_lowercase(),
        "source_name": &doc.source_name,
        "source_key": &doc.source_key,
        "format": &doc.format,
        "content_sha256": &doc.content_sha256,
        "file_sha256": &doc.file_sha256,
        "bytes": i64::try_from(doc.bytes).unwrap_or(i64::MAX),
        "chunk_count": long(chunks),
        "origin": doc.origin.as_str(),
        "warnings": &doc.warnings,
        "updated_at": doc.updated_at,
    };

    fields.extend(link_fields(links));
    fields
}

/// The links of a document and the two fields the list filters by: the repository keys and
/// whether there is none (a global document). A chunk carries the same two for the search.
pub(super) fn link_fields(links: &[Link]) -> Row {
    let stored: Vec<Row> = links
        .iter()
        .map(|link| doc! { "repo_key": &link.repo_key, "globs": &link.globs })
        .collect();
    let keys: Vec<&str> = links.iter().map(|link| link.repo_key.as_str()).collect();

    doc! { "links": stored, "repo_keys": keys, "global": links.is_empty() }
}

/// The two fields of a chunk that follow its document's links.
fn scope_fields(links: &[Link]) -> Row {
    let keys: Vec<&str> = links.iter().map(|link| link.repo_key.as_str()).collect();

    doc! { "repo_keys": keys, "global": links.is_empty() }
}

/// A [`Document`] from a stored one.
pub(super) fn document(row: &Row) -> Result<Document, StoreError> {
    let origin = text(row, "origin")
        .and_then(|origin| Origin::parse(&origin))
        .ok_or_else(|| StoreError::Backend("a stored document has an unknown origin".to_owned()))?;

    Ok(Document {
        id: int(row, "_id"),
        title: text(row, "title").unwrap_or_default(),
        source_name: text(row, "source_name").unwrap_or_default(),
        source_key: text(row, "source_key").unwrap_or_default(),
        format: text(row, "format").unwrap_or_default(),
        content_sha256: text(row, "content_sha256").unwrap_or_default(),
        file_sha256: text(row, "file_sha256").unwrap_or_default(),
        bytes: u64::try_from(int(row, "bytes")).unwrap_or(0),
        chunk_count: usize::try_from(int(row, "chunk_count")).unwrap_or(0),
        created_at: int(row, "created_at"),
        updated_at: int(row, "updated_at"),
        origin,
        warnings: strings(row, "warnings"),
    })
}

/// The links of a stored document, by repository key.
pub(super) fn links(row: &Row) -> Vec<Link> {
    let mut links: Vec<Link> = row
        .get_array("links")
        .map(|stored| {
            stored
                .iter()
                .filter_map(Bson::as_document)
                .filter_map(|link| {
                    Some(Link {
                        repo_key: text(link, "repo_key")?,
                        globs: strings(link, "globs"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    links.sort_by(|a, b| a.repo_key.cmp(&b.repo_key));
    links
}

/// A stored chunk of document `doc_id` (titled `title`, linked as `links`).
fn chunk_row(id: i64, doc_id: i64, (title, updated_at): (&str, i64), chunk: &Chunk, links: &[Link]) -> Row {
    let mut row = doc! {
        "_id": id,
        "doc_id": doc_id,
        "ordinal": long(chunk.ordinal),
        "title": title,
        "title_terms": terms::of_text(title),
        "heading_path": &chunk.heading_path,
        "text": &chunk.text,
        "terms": terms::of_text(&format!("{} {}", chunk.heading_path, chunk.text)),
        "chunk_sha": &chunk.chunk_sha,
        "tokens": long(chunk.tokens),
        "char_start": long(chunk.char_start),
        "char_end": long(chunk.char_end),
        "updated_at": updated_at,
    };

    row.extend(scope_fields(links));

    if let (Some(embedding), Some(model)) = (&chunk.embedding, &chunk.model_id) {
        let vector: Vec<f64> = embedding.iter().map(|value| f64::from(*value)).collect();
        row.insert("embedding", vector);
        row.insert("model_id", model);
    }

    row
}

/// Stores `doc` (whose cleaned title is `title`) under `id` with `chunks` and `links`: inserted
/// for a document that has no id yet, else replacing that document's fields, chunks and links
/// (`created_at` stays). The chunks of an earlier attempt of the same write are removed first, so
/// a retry outside a transaction starts clean.
pub(super) fn put(
    ctx: &mut Ctx<'_>,
    id: i64,
    doc: &Document,
    title: &str,
    (chunks, links): (&[Chunk], &[Link]),
    begun: &Cell<bool>,
) -> Result<i64, Fail> {
    let fresh = doc.id == 0;
    let update = doc! {
        "$set": fields(doc, title, chunks.len(), links),
        "$setOnInsert": { "created_at": doc.created_at },
    };

    if ctx.update_one(DOCUMENTS, doc! { "_id": id }, update, fresh)? == 0 {
        return Err(not_found(id).into());
    }

    // From here a failure outside a transaction leaves a part behind (see `undo_put`).
    begun.set(true);

    ctx.delete_many(CHUNKS, doc! { "doc_id": id })?;

    let first = match chunks.len() {
        0 => 0,
        count => ctx.reserve_ids("chunks", count)?,
    };
    let rows: Vec<Row> = chunks
        .iter()
        .zip(first..)
        .map(|(chunk, chunk_id)| chunk_row(chunk_id, id, (title, doc.updated_at), chunk, links))
        .collect();

    // One request per slice, as a document of 4000 chunks with vectors is some 15 MB.
    for slice in rows.chunks(500) {
        ctx.insert_many(CHUNKS, slice.to_vec())?;
    }

    Ok(id)
}

/// What a failed put leaves behind outside a transaction, cleaned up as far as the database
/// answers: a new document goes with its chunks, a replaced one keeps its old fields but loses
/// its content hash and its chunks, so the next import of the file stores it again.
pub(super) fn undo_put(ctx: &mut Ctx<'_>, id: i64, fresh: bool) {
    let _ = ctx.delete_many(CHUNKS, doc! { "doc_id": id });

    if fresh {
        let _ = ctx.delete_many(DOCUMENTS, doc! { "_id": id });
    } else {
        let blank = doc! { "$set": { "content_sha256": "", "file_sha256": "", "chunk_count": 0_i64 } };
        let _ = ctx.update_one(DOCUMENTS, doc! { "_id": id }, blank, false);
    }
}

/// Replaces the links of document `id`; false when there is none.
pub(super) fn set_links(ctx: &mut Ctx<'_>, id: i64, links: &[Link]) -> Result<bool, Fail> {
    let update = doc! { "$set": link_fields(links) };

    if ctx.update_one(DOCUMENTS, doc! { "_id": id }, update, false)? == 0 {
        return Ok(false);
    }

    ctx.update_many(CHUNKS, doc! { "doc_id": id }, doc! { "$set": scope_fields(links) })?;

    Ok(true)
}

/// Retitles document `id` (to the cleaned `title`) and its chunks; false when there is none.
pub(super) fn rename(ctx: &mut Ctx<'_>, id: i64, title: &str) -> Result<bool, Fail> {
    let update = doc! { "$set": { "title": title, "title_key": title.to_lowercase() } };

    if ctx.update_one(DOCUMENTS, doc! { "_id": id }, update, false)? == 0 {
        return Ok(false);
    }

    let chunks = doc! { "$set": { "title": title, "title_terms": terms::of_text(title) } };
    ctx.update_many(CHUNKS, doc! { "doc_id": id }, chunks)?;

    Ok(true)
}

/// Deletes the documents with their chunks; how many documents existed.
pub(super) fn delete(ctx: &mut Ctx<'_>, ids: &[i64]) -> Result<usize, Fail> {
    if ids.len() > DELETE_IDS_MAX {
        return Err(StoreError::Invalid(format!("at most {DELETE_IDS_MAX} documents at once")).into());
    }

    let deleted = ctx.delete_many(DOCUMENTS, doc! { "_id": { "$in": ids } })?;
    ctx.delete_many(CHUNKS, doc! { "doc_id": { "$in": ids } })?;

    Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
}

/// The oldest document whose `field` (`content_sha256` or `source_key`) is `value`.
pub(super) fn find(ctx: &mut Ctx<'_>, field: &str, value: &str) -> Result<Option<Document>, Fail> {
    let found = ctx.find(DOCUMENTS, doc! { field: value }, (doc! { "_id": 1 }, 0, 1), Row::new())?;

    Ok(found.first().map(document).transpose()?)
}

/// The embeddings of chunks made by `model_id`, by chunk hash.
pub(super) fn reusable_embeddings(
    ctx: &mut Ctx<'_>,
    model_id: &str,
    shas: &[String],
) -> Result<HashMap<String, Vec<f32>>, Fail> {
    if shas.is_empty() {
        return Ok(HashMap::new());
    }

    let filter = doc! { "model_id": model_id, "chunk_sha": { "$in": shas }, "embedding": { "$exists": true } };
    let rows = ctx.find(
        CHUNKS,
        filter,
        (doc! { "_id": 1 }, 0, 0),
        doc! { "chunk_sha": 1, "embedding": 1 },
    )?;

    let mut found: HashMap<String, Vec<f32>> = HashMap::new();

    for row in &rows {
        if let (Some(sha), Some(vector)) = (text(row, "chunk_sha"), embedding(row)) {
            found.entry(sha).or_insert(vector);
        }
    }

    Ok(found)
}

/// The next `limit` chunks after chunk `after` that have no embedding of `model_id` (a chunk
/// without an embedding has no `model_id` either, and `$ne` matches a missing field).
pub(super) fn pending_chunks(
    ctx: &mut Ctx<'_>,
    model_id: &str,
    after: i64,
    limit: usize,
) -> Result<Vec<PendingChunk>, Fail> {
    let filter = doc! { "_id": { "$gt": after }, "model_id": { "$ne": model_id } };
    let wanted = i64::try_from(limit.min(EMBED_BATCH_MAX)).unwrap_or(i64::MAX);
    let projection = doc! { "doc_id": 1, "title": 1, "heading_path": 1, "text": 1 };
    let rows = ctx.find(CHUNKS, filter, (doc! { "_id": 1 }, 0, wanted), projection)?;

    Ok(rows
        .iter()
        .map(|row| PendingChunk {
            chunk_id: int(row, "_id"),
            doc_id: int(row, "doc_id"),
            embed_text: passage(
                &text(row, "title").unwrap_or_default(),
                &text(row, "heading_path").unwrap_or_default(),
                &text(row, "text").unwrap_or_default(),
            ),
        })
        .collect())
}

/// Stores the embeddings; the number of chunks that existed.
pub(super) fn set_embeddings(ctx: &mut Ctx<'_>, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, Fail> {
    let mut found = 0;

    for (chunk_id, vector) in embeddings {
        let vector: Vec<f64> = vector.iter().map(|value| f64::from(*value)).collect();
        let update = doc! { "$set": { "embedding": vector, "model_id": model_id } };

        found += ctx.update_one(CHUNKS, doc! { "_id": *chunk_id }, update, false)?;
    }

    Ok(usize::try_from(found).unwrap_or(usize::MAX))
}

/// The embedding of a stored chunk; `None` unless it is exactly [`DIMS`] numbers.
fn embedding(row: &Row) -> Option<Vec<f32>> {
    let stored = row.get_array("embedding").ok()?;

    if stored.len() != DIMS {
        return None;
    }

    stored
        .iter()
        .map(|value| value.as_f64().map(|float| float as f32))
        .collect()
}

pub(super) fn not_found(id: i64) -> StoreError {
    StoreError::NotFound(format!("document {id} not found"))
}
