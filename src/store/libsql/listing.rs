//! Reading documents: the list with its filters and order, one document with its parts, and the
//! counts per repository and overall. The statements are the local file's (see
//! [`crate::store::sqlite::sql_shared`]).

use super::documents::{by_id, links_of, links_of_many, listed, pending_of_many};
use super::meta;
use super::sql::{Sql, Value, args, query_row};
use crate::store::author_list::unhex;
use crate::store::sqlite::sql_shared::{
    GLOBAL, MATCHES_COUNT_SQL, MATCHES_SQL, author_groups_sql, list_conditions, matches_query, page_limit, page_sql,
    with_author,
};
use crate::store::{AuthorFacet, Section, Stats, StoreError};
use crate::store::{
    Chunk, ChunkMatch, ChunkMatches, Columns, Detail, DocumentDetail, DocumentPage, DocumentRow, ListQuery, RepoCounts,
};

/// One page of the documents `query` keeps, with the total over all pages.
pub(super) fn list(db: &mut dyn Sql, columns: Columns, query: &ListQuery) -> Result<DocumentPage, StoreError> {
    let (base, mut params) = list_conditions::<Value>(columns, query);

    // The facet honours every filter but the author one, so it is read before that filter's name is
    // bound. Only a list that asks for it pays for it.
    let authors = if query.want_authors {
        let groups = db
            .query(&author_groups_sql(columns, &base), &params)?
            .iter()
            .map(|row| Ok((row.tolerant_text(0).and_then(|hex| unhex(&hex)), row.get::<usize>(1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;

        AuthorFacet::from_groups(groups)
    } else {
        AuthorFacet::default()
    };

    let filter = with_author(columns, query, &base, &mut params);
    let total = db.query(
        &format!("SELECT count(*) FROM knowledge_documents d WHERE {filter}"),
        &params,
    )?;
    let total: usize = total.first().map_or(Ok(0), |row| row.get(0))?;

    params.push(Value::Integer(page_limit(query)));
    params.push(Value::Integer(i64::try_from(query.offset).unwrap_or(i64::MAX)));

    let sql = page_sql(columns, &filter, query, params.len());
    let docs = db
        .query(&sql, &params)?
        .iter()
        .map(listed)
        .collect::<Result<Vec<_>, _>>()?;

    let ids: Vec<i64> = docs.iter().map(|(doc, _)| doc.id).collect();
    let mut links = links_of_many(db, &ids)?;
    let mut pending = pending_of_many(db, columns, &ids)?;

    let items = docs
        .into_iter()
        .map(|(doc, usage)| DocumentRow {
            links: links.remove(&doc.id).unwrap_or_default(),
            usage,
            pending: pending.remove(&doc.id),
            doc,
        })
        .collect();

    Ok(DocumentPage { items, total, authors })
}

/// The chunks of document `id` that match `text`, at most `limit`.
pub(super) fn matching_chunks(db: &mut dyn Sql, id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError> {
    let Some(query) = matches_query(text) else {
        return Ok(ChunkMatches::default());
    };

    let total = query_row(db, MATCHES_COUNT_SQL, args![query.clone(), id])?;
    let total: usize = total.map_or(Ok(0), |row| row.get(0))?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);

    let chunks = db
        .query(MATCHES_SQL, args![query, id, limit])?
        .iter()
        .map(|row| {
            Ok(ChunkMatch {
                chunk_id: row.get(0)?,
                ordinal: row.get(1)?,
                heading_path: row.get(2)?,
                text: row.get(3)?,
            })
        })
        .collect::<Result<_, StoreError>>()?;

    Ok(ChunkMatches { total, chunks })
}

/// Document `id` with its links, and what `detail` asks for.
pub(super) fn get(
    db: &mut dyn Sql,
    columns: Columns,
    id: i64,
    detail: &Detail,
) -> Result<Option<DocumentDetail>, StoreError> {
    let Some((doc, usage)) = by_id(db, columns, id)? else {
        return Ok(None);
    };

    // Agents read live documents only: a draft or a pending version is not there for them.
    if !detail.drafts && !doc.status.is_live() {
        return Ok(None);
    }

    let pending = if detail.drafts {
        pending_of_many(db, columns, &[id])?.remove(&id)
    } else {
        None
    };

    let sections = if detail.sections { Some(sections(db, id)?) } else { None };

    let chunk = match detail.chunk {
        Some(chunk_id) => Some(chunk(db, id, chunk_id)?),
        None => None,
    };

    Ok(Some(DocumentDetail {
        links: links_of(db, id)?,
        doc,
        usage,
        sections,
        chunk,
        pending,
    }))
}

fn sections(db: &mut dyn Sql, id: i64) -> Result<Vec<Section>, StoreError> {
    let rows = db.query(
        "SELECT id, ordinal, heading_path, tokens, char_start, char_end
         FROM knowledge_chunks WHERE doc_id = ?1 ORDER BY ordinal",
        args![id],
    )?;

    rows.iter()
        .map(|row| {
            Ok(Section {
                chunk_id: row.get(0)?,
                ordinal: row.get(1)?,
                heading_path: row.get(2)?,
                tokens: row.get(3)?,
                char_start: row.get(4)?,
                char_end: row.get(5)?,
            })
        })
        .collect()
}

/// Chunk `chunk_id` of document `id`, whole and without its vector.
fn chunk(db: &mut dyn Sql, id: i64, chunk_id: i64) -> Result<Chunk, StoreError> {
    let found = query_row(
        db,
        "SELECT id, doc_id, ordinal, heading_path, text, chunk_sha, tokens, char_start, char_end
         FROM knowledge_chunks WHERE id = ?1 AND doc_id = ?2",
        args![chunk_id, id],
    )?;

    let Some(row) = found else {
        return Err(StoreError::NotFound(format!(
            "chunk {chunk_id} is not in document {id}"
        )));
    };

    Ok(Chunk {
        id: row.get(0)?,
        doc_id: row.get(1)?,
        ordinal: row.get(2)?,
        heading_path: row.get(3)?,
        text: row.get(4)?,
        chunk_sha: row.get(5)?,
        tokens: row.get(6)?,
        char_start: row.get(7)?,
        char_end: row.get(8)?,
        embedding: None,
        model_id: None,
    })
}

pub(super) fn repos(db: &mut dyn Sql) -> Result<RepoCounts, StoreError> {
    let global = count(
        db,
        &format!("SELECT count(*) FROM knowledge_documents d WHERE d.pending_of IS NULL AND {GLOBAL}"),
    )?;

    let rows = db.query(
        "SELECT l.repo_key, count(*) AS docs FROM knowledge_doc_links l
         JOIN knowledge_documents d ON d.id = l.doc_id WHERE d.pending_of IS NULL
         GROUP BY l.repo_key ORDER BY docs DESC, l.repo_key",
        args![],
    )?;

    let repos = rows
        .iter()
        .map(|row| Ok((row.get::<String>(0)?, row.get::<usize>(1)?)))
        .collect::<Result<_, StoreError>>()?;

    Ok(RepoCounts { global, repos })
}

pub(super) fn stats(db: &mut dyn Sql) -> Result<Stats, StoreError> {
    let model_id = meta::get(db, "model_id")?;

    let totals = query_row(
        db,
        "SELECT count(*), coalesce(sum(bytes), 0) FROM knowledge_documents WHERE pending_of IS NULL",
        args![],
    )?;
    let (documents, bytes) = match totals {
        Some(row) => (row.get(0)?, row.get(1)?),
        None => (0, 0),
    };

    let embedded = query_row(
        db,
        "SELECT count(*) FROM knowledge_chunks WHERE embedding IS NOT NULL AND (?1 IS NULL OR model_id = ?1)",
        args![model_id.clone()],
    )?;

    Ok(Stats {
        documents,
        chunks: count(db, "SELECT count(*) FROM knowledge_chunks")?,
        embedded_chunks: embedded.map_or(Ok(0), |row| row.get(0))?,
        bytes,
        global_documents: count(
            db,
            &format!("SELECT count(*) FROM knowledge_documents d WHERE d.pending_of IS NULL AND {GLOBAL}"),
        )?,
        repositories: count(
            db,
            "SELECT count(DISTINCT l.repo_key) FROM knowledge_doc_links l
             JOIN knowledge_documents d ON d.id = l.doc_id WHERE d.pending_of IS NULL",
        )?,
        generation: meta::generation(db)?,
        schema_version: meta::get(db, "schema_version")?
            .and_then(|version| version.parse().ok())
            .unwrap_or(0),
        model_id,
        drafts: count(db, "SELECT count(*) FROM knowledge_documents WHERE status = 'draft'")?,
        waiting: count(db, "SELECT count(*) FROM knowledge_documents WHERE status = 'waiting'")?,
        changes_requested: count(
            db,
            "SELECT count(*) FROM knowledge_documents WHERE status = 'changesRequested'",
        )?,
        pending: count(
            db,
            "SELECT count(*) FROM knowledge_documents WHERE pending_of IS NOT NULL",
        )?,
    })
}

fn count(db: &mut dyn Sql, sql: &str) -> Result<usize, StoreError> {
    query_row(db, sql, args![])?.map_or(Ok(0), |row| row.get(0))
}
