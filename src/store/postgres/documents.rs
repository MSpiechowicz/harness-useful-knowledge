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
use crate::store::review::{RENAMED_COMMENT, actor};
use crate::store::{Author, Chunk, Columns, DocUsage, Document, EventKind, Link, Origin, PendingChunk, PendingVersion};
use crate::store::{Status, StoreError, Visibility};

/// How many chunks one `INSERT` carries.
const CHUNK_BATCH: usize = 100;

/// The columns of a document aliased `d` that every database has.
const BASE_COLUMNS: &str = "d.id, d.title, d.source_name, d.source_key, d.format, d.content_sha256, \
     d.file_sha256, d.bytes, d.chunk_count, d.created_at, d.updated_at, d.origin, d.warnings";

/// The columns of the review (20-22), which every database of schema version 2 has.
const REVIEW_COLUMNS: &str = "d.status, d.pending_of, d.review_rev";

/// The columns [`document`] and [`listed`] read, of a document aliased `d`: the base ones (0-12),
/// the authors (13-16), the usage (17-19) and the review (20-22). A column of the added ones the
/// database lacks is read as NULL.
pub(super) fn columns_sql(columns: Columns) -> String {
    let authors = if columns.authors {
        "d.added_by_name, d.added_by_via, d.updated_by_name, d.updated_by_via"
    } else {
        "NULL::text, NULL::text, NULL::text, NULL::text"
    };
    let usage = if columns.usage {
        "d.shown, d.useful, d.last_useful_at"
    } else {
        "NULL::bigint, NULL::bigint, NULL::bigint"
    };

    format!("{BASE_COLUMNS}, {authors}, {usage}, {REVIEW_COLUMNS}")
}

/// A [`Document`] from a row of [`columns_sql`].
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
        added_by: Author::stored(column(row, 13)?, column(row, 14)?),
        updated_by: Author::stored(column(row, 15)?, column(row, 16)?),
        status: {
            let status: String = column(row, 20)?;

            Status::parse(&status).ok_or_else(|| StoreError::Backend("a stored status is unknown".to_owned()))?
        },
        pending_of: column(row, 21)?,
        review_rev: column(row, 22)?,
    })
}

/// A [`Document`] with its usage from a row of [`columns_sql`].
pub(super) fn listed(row: &Row) -> Result<(Document, Option<DocUsage>), StoreError> {
    let usage = DocUsage::stored(column(row, 17)?, column(row, 18)?, column(row, 19)?);

    Ok((document(row)?, usage))
}

/// Document `id` with its usage.
pub(super) fn by_id(
    client: &mut impl GenericClient,
    columns: Columns,
    id: i64,
) -> Result<Option<(Document, Option<DocUsage>)>, StoreError> {
    let sql = format!(
        "SELECT {} FROM knowledge_documents d WHERE d.id = $1",
        columns_sql(columns)
    );
    let row = client.query_opt(&sql, &[&id]).map_err(backend)?;

    row.map(|row| listed(&row)).transpose()
}

/// Records who stored `doc` as document `id`, which [`put`] has just written. A new document gets
/// both authors, an update keeps who added it. Nothing without the author columns.
pub(super) fn stamp_authors(
    tx: &mut impl GenericClient,
    columns: Columns,
    id: i64,
    doc: &Document,
) -> Result<(), StoreError> {
    if !columns.authors {
        return Ok(());
    }

    let fresh = doc.id == 0;
    let author = if fresh { &doc.added_by } else { &doc.updated_by };
    let name = author.as_ref().map(|author| text(&author.name).into_owned());
    let via = author
        .as_ref()
        .and_then(|author| author.via.as_deref())
        .map(|via| text(via).into_owned());

    let sql = if fresh {
        "UPDATE knowledge_documents
         SET added_by_name = $2, added_by_via = $3, updated_by_name = $2, updated_by_via = $3 WHERE id = $1"
    } else {
        "UPDATE knowledge_documents SET updated_by_name = $2, updated_by_via = $3 WHERE id = $1"
    };
    tx.execute(sql, &[&id, &name, &via]).map_err(backend)?;

    Ok(())
}

/// `value` without the NUL characters the database cannot hold.
fn text(value: &str) -> Cow<'_, str> {
    if value.contains('\0') {
        Cow::Owned(value.replace('\0', ""))
    } else {
        Cow::Borrowed(value)
    }
}

/// The state of a row: status, the live document it is pending for, revision and format.
pub(super) struct RowState {
    pub status: Status,
    pub pending_of: Option<i64>,
    pub review_rev: i64,
    pub format: String,
}

/// The review columns of document `id`; `None` when there is no such document.
pub(super) fn row_state(client: &mut impl GenericClient, id: i64) -> Result<Option<RowState>, StoreError> {
    let row = client
        .query_opt(
            "SELECT status, pending_of, review_rev, format FROM knowledge_documents WHERE id = $1",
            &[&id],
        )
        .map_err(backend)?;

    row.map(|row| {
        let status: String = column(&row, 0)?;

        Ok(RowState {
            status: Status::parse(&status)
                .ok_or_else(|| StoreError::Backend("a stored status is unknown".to_owned()))?,
            pending_of: column(&row, 1)?,
            review_rev: column(&row, 2)?,
            format: column(&row, 3)?,
        })
    })
    .transpose()
}

/// Records an event of document `id`.
pub(super) fn record(
    tx: &mut impl GenericClient,
    id: i64,
    kind: EventKind,
    by: Option<&str>,
    comment: Option<&str>,
    at: i64,
) -> Result<(), StoreError> {
    let by = by.map(|name| text(name).into_owned());
    let comment = comment.map(|comment| text(comment).into_owned());

    tx.execute(
        "INSERT INTO knowledge_doc_events (doc_id, kind, by_name, comment, at) VALUES ($1, $2, $3, $4, $5)",
        &[&id, &kind.as_str(), &by, &comment, &at],
    )
    .map_err(backend)?;

    Ok(())
}

/// Stores `doc` (whose cleaned title is `title`) with `chunks` and `links`: inserted for id 0,
/// else replacing that document's chunks and links. A row that is not live records the event.
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
    let by = actor(doc.updated_by.as_ref().map(|author| author.name.as_str()));

    let id = if doc.id == 0 {
        check_new_row(tx, doc)?;

        let row = tx
            .query_one(
                "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256,
                     bytes, chunk_count, origin, warnings, created_at, updated_at, status, pending_of)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING id",
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
                    &doc.status.as_str(),
                    &doc.pending_of,
                ],
            )
            .map_err(backend)?;
        let id: i64 = column(&row, 0)?;

        if !doc.status.is_live() {
            let kind = if doc.pending_of.is_some() {
                EventKind::UpdateDrafted
            } else {
                EventKind::Drafted
            };

            record(tx, id, kind, by.as_deref(), None, doc.updated_at)?;
        }

        id
    } else {
        let Some(stored) = row_state(tx, doc.id)? else {
            return Err(not_found(doc.id));
        };

        tx.execute("DELETE FROM knowledge_chunks WHERE doc_id = $1", &[&doc.id])
            .map_err(backend)?;
        tx.execute("DELETE FROM knowledge_doc_links WHERE doc_id = $1", &[&doc.id])
            .map_err(backend)?;
        tx.execute("DELETE FROM knowledge_doc_files WHERE doc_id = $1", &[&doc.id])
            .map_err(backend)?;

        // A change of content is a new revision, and what was waiting for approval is no longer
        // what was asked: it is a draft again.
        tx.execute(
            "UPDATE knowledge_documents SET title = $2, source_name = $3, source_key = $4, format = $5,
                 content_sha256 = $6, file_sha256 = $7, bytes = $8, chunk_count = $9, origin = $10,
                 warnings = $11, updated_at = $12, review_rev = review_rev + 1,
                 status = CASE WHEN status = 'waiting' THEN 'draft' ELSE status END
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

        if !stored.status.is_live() {
            record(tx, doc.id, EventKind::Edited, by.as_deref(), None, doc.updated_at)?;
        }

        doc.id
    };

    insert_chunks(tx, id, title, chunks)?;
    insert_links(tx, id, links)?;

    Ok(id)
}

/// Refuses a new row that cannot be stored: a pending version needs a live document without one.
fn check_new_row(tx: &mut impl GenericClient, doc: &Document) -> Result<(), StoreError> {
    let Some(live) = doc.pending_of else {
        return Ok(());
    };

    if doc.status.is_live() {
        return Err(StoreError::Invalid("a pending version is never live".to_owned()));
    }

    match row_state(tx, live)? {
        None => return Err(not_found(live)),
        Some(target) if !target.status.is_live() => {
            return Err(StoreError::Invalid(format!("document {live} is not live")));
        }
        Some(_) => {}
    }

    let taken = tx
        .query_opt("SELECT id FROM knowledge_documents WHERE pending_of = $1", &[&live])
        .map_err(backend)?;

    if taken.is_some() {
        return Err(StoreError::Invalid(format!(
            "document {live} already has a pending version"
        )));
    }

    Ok(())
}

/// The pending versions of the live documents `ids`, by live id.
pub(super) fn pending_of_many(
    client: &mut impl GenericClient,
    columns: Columns,
    ids: &[i64],
) -> Result<HashMap<i64, PendingVersion>, StoreError> {
    let sql = format!(
        "SELECT {} FROM knowledge_documents d WHERE d.pending_of = ANY($1)",
        columns_sql(columns)
    );
    let rows = client.query(&sql, &[&ids]).map_err(backend)?;
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

/// How many links point at a folder of this computer (a `path:` key).
pub(super) fn count_local_paths(client: &mut impl GenericClient) -> Result<usize, StoreError> {
    let row = client
        .query_one(
            "SELECT count(*) FROM knowledge_doc_links WHERE left(repo_key, 5) = 'path:'",
            &[],
        )
        .map_err(backend)?;

    count(&row, 0)
}

/// Deletes the links to folders of this computer, which a team database never keeps; the number
/// deleted. A live document that no link is left on becomes a draft (a document with no link is
/// global, which would widen who sees it), with an `edited` event and a new revision.
pub(super) fn drop_local_paths(tx: &mut impl GenericClient) -> Result<(usize, bool), StoreError> {
    let affected: Vec<i64> = tx
        .query(
            "SELECT DISTINCT doc_id FROM knowledge_doc_links WHERE left(repo_key, 5) = 'path:' ORDER BY doc_id",
            &[],
        )
        .map_err(backend)?
        .iter()
        .map(|row| column(row, 0))
        .collect::<Result<_, _>>()?;

    let removed = tx
        .execute("DELETE FROM knowledge_doc_links WHERE left(repo_key, 5) = 'path:'", &[])
        .map_err(backend)?;
    let removed = usize::try_from(removed).unwrap_or(usize::MAX);

    for id in affected {
        let left = tx
            .query_opt("SELECT 1 FROM knowledge_doc_links WHERE doc_id = $1", &[&id])
            .map_err(backend)?;

        if left.is_some() {
            continue;
        }

        // A pending version has no links of its own: the live document's win.
        let Some(state) = row_state(tx, id)?.filter(|state| state.pending_of.is_none()) else {
            continue;
        };
        let mut comment = crate::store::clean::FOLDER_LINK_REMOVED;

        // Nothing links it, and a document with no link is global: it is a draft that is looked at
        // again (a waiting or sent-back row needs a new ask), and a pending update of a live one
        // goes, so no pending version points at a draft.
        if state.status != Status::Draft {
            tx.execute(
                "UPDATE knowledge_documents SET status = 'draft', review_rev = review_rev + 1 WHERE id = $1",
                &[&id],
            )
            .map_err(backend)?;
        }

        if state.status == Status::Live
            && let Some(pending) = tx
                .query_opt("SELECT id FROM knowledge_documents WHERE pending_of = $1", &[&id])
                .map_err(backend)?
        {
            delete(tx, &[column::<i64>(&pending, 0)?])?;
            comment = crate::store::clean::FOLDER_LINK_REMOVED_PENDING;
        }

        record(
            tx,
            id,
            EventKind::Edited,
            None,
            Some(comment),
            crate::import::document::now(),
        )?;
    }

    Ok((removed, removed > 0))
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
/// title in the chunks follow, so the keyword index finds the new title. A row that is not live
/// takes it as a change (see [`retitle`]), and so does the pending version of a live one.
pub(super) fn rename(
    tx: &mut impl GenericClient,
    id: i64,
    title: &str,
    by: Option<&str>,
    at: i64,
) -> Result<bool, StoreError> {
    let Some(state) = row_state(tx, id)? else {
        return Ok(false);
    };

    retitle(tx, id, &state, title, by, at)?;

    if !state.status.is_live() {
        return Ok(true);
    }

    let pending = tx
        .query_opt("SELECT id FROM knowledge_documents WHERE pending_of = $1", &[&id])
        .map_err(backend)?
        .map(|row| column::<i64>(&row, 0))
        .transpose()?;

    if let Some(pending) = pending
        && let Some(state) = row_state(tx, pending)?
    {
        retitle(tx, pending, &state, title, by, at)?;
    }

    Ok(true)
}

/// Gives the row `id`, in `state`, the title and its chunks. A live row is only retitled. A row that
/// is not live has a new revision, a waiting one is a draft again (what was asked is no longer what
/// is there) and the `edited` event says what changed. Nothing when the title already is `title`.
fn retitle(
    tx: &mut impl GenericClient,
    id: i64,
    state: &RowState,
    title: &str,
    by: Option<&str>,
    at: i64,
) -> Result<(), StoreError> {
    // The renamer goes on the `edited` event, not on the row: who wrote the content stays as it was.
    let renamer = actor(by);

    let changed = if state.status.is_live() {
        tx.execute(
            "UPDATE knowledge_documents SET title = $2, updated_at = $3
             WHERE id = $1 AND title <> $2",
            &[&id, &title, &at],
        )
    } else {
        tx.execute(
            "UPDATE knowledge_documents SET title = $2, review_rev = review_rev + 1,
                 status = CASE WHEN status = 'waiting' THEN 'draft' ELSE status END, updated_at = $3
             WHERE id = $1 AND title <> $2",
            &[&id, &title, &at],
        )
    }
    .map_err(backend)?;

    if changed == 0 {
        return Ok(());
    }

    tx.execute(
        "UPDATE knowledge_chunks SET title = $2 WHERE doc_id = $1",
        &[&id, &title],
    )
    .map_err(backend)?;

    if !state.status.is_live() {
        record(tx, id, EventKind::Edited, renamer.as_deref(), Some(RENAMED_COMMENT), at)?;
    }

    Ok(())
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

/// The condition (on a document aliased `d`) that keeps the rows `visibility` lets a lookup see.
pub(super) fn seen(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Live => "d.status = 'live'",
        Visibility::WithDrafts => "d.pending_of IS NULL",
    }
}

/// The oldest document `visibility` lets through whose `column` (`content_sha256` or `source_key`)
/// is `value`.
pub(super) fn find(
    client: &mut impl GenericClient,
    columns: Columns,
    column_name: &str,
    value: &str,
    visibility: Visibility,
) -> Result<Option<Document>, StoreError> {
    let sql = format!(
        "SELECT {} FROM knowledge_documents d WHERE d.{column_name} = $1 AND {} ORDER BY d.id LIMIT 1",
        columns_sql(columns),
        seen(visibility)
    );
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
