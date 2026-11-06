//! Reading documents: the list with its filters and order, one document with its parts, and the
//! counts per repository and overall.

use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::documents::{by_id, links_of, links_of_many, listed, pending_of_many};
use super::sql_shared::{
    GLOBAL, MATCHES_COUNT_SQL, MATCHES_SQL, author_groups_sql, list_conditions, matches_query, page_limit, page_sql,
    with_author,
};
use super::{meta, schema};
use crate::store::author_list::unhex;
use crate::store::{AuthorFacet, Section, Stats, StoreError};
use crate::store::{
    Chunk, ChunkMatch, ChunkMatches, Columns, Detail, DocumentDetail, DocumentPage, DocumentRow, ListQuery, RepoCounts,
};

/// One page of the documents `query` keeps, with the total over all pages.
pub(super) fn list(conn: &Connection, columns: Columns, query: &ListQuery) -> Result<DocumentPage, StoreError> {
    let (base, mut params) = list_conditions::<Value>(columns, query);

    // The facet honours every filter but the author one, so it is read before that filter's name is
    // bound. Only a list that asks for it pays for it.
    let authors = if query.want_authors {
        author_facet(conn, columns, &base, &params)?
    } else {
        AuthorFacet::default()
    };

    let filter = with_author(columns, query, &base, &mut params);
    let total: i64 = conn.query_row(
        &format!("SELECT count(*) FROM knowledge_documents d WHERE {filter}"),
        params_from_iter(params.iter()),
        |row| row.get(0),
    )?;

    params.push(Value::Integer(page_limit(query)));
    params.push(Value::Integer(i64::try_from(query.offset).unwrap_or(i64::MAX)));

    let sql = page_sql(columns, &filter, query, params.len());
    let mut statement = conn.prepare(&sql)?;
    let docs: Vec<_> = statement
        .query_map(params_from_iter(params.iter()), listed)?
        .collect::<Result<_, _>>()?;

    let ids: Vec<i64> = docs.iter().map(|(doc, _)| doc.id).collect();
    let mut links = links_of_many(conn, &ids)?;
    let mut pending = pending_of_many(conn, columns, &ids)?;

    let items = docs
        .into_iter()
        .map(|(doc, usage)| DocumentRow {
            links: links.remove(&doc.id).unwrap_or_default(),
            usage,
            pending: pending.remove(&doc.id),
            doc,
        })
        .collect();

    Ok(DocumentPage {
        items,
        total: usize::try_from(total).unwrap_or(0),
        authors,
    })
}

/// The authors facet under `filter`. A name that is not valid UTF-8, or has a control character in
/// it, which somebody may have written, is no known author.
fn author_facet(
    conn: &Connection,
    columns: Columns,
    filter: &str,
    params: &[Value],
) -> Result<AuthorFacet, StoreError> {
    let mut statement = conn.prepare(&author_groups_sql(columns, filter))?;
    let groups = statement
        .query_map(params_from_iter(params.iter()), |row| {
            // The hex of the name, NULL for none: anything else cannot be a name.
            let name = match row.get_ref(0)? {
                ValueRef::Text(hex) => std::str::from_utf8(hex).ok().and_then(unhex),
                _ => None,
            };

            Ok((name, row.get::<_, usize>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(AuthorFacet::from_groups(groups))
}

/// The chunks of document `id` that match `text`, at most `limit`.
pub(super) fn matching_chunks(
    conn: &Connection,
    id: i64,
    text: &str,
    limit: usize,
) -> Result<ChunkMatches, StoreError> {
    let Some(query) = matches_query(text) else {
        return Ok(ChunkMatches::default());
    };

    let total: i64 = conn.query_row(MATCHES_COUNT_SQL, params![query, id], |row| row.get(0))?;
    let mut statement = conn.prepare(MATCHES_SQL)?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);

    let chunks = statement
        .query_map(params![query, id, limit], |row| {
            Ok(ChunkMatch {
                chunk_id: row.get(0)?,
                ordinal: row.get(1)?,
                heading_path: row.get(2)?,
                text: row.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;

    Ok(ChunkMatches {
        total: usize::try_from(total).unwrap_or(0),
        chunks,
    })
}

/// Document `id` with its links, and what `detail` asks for.
pub(super) fn get(
    conn: &Connection,
    columns: Columns,
    id: i64,
    detail: &Detail,
) -> Result<Option<DocumentDetail>, StoreError> {
    let Some((doc, usage)) = by_id(conn, columns, id)? else {
        return Ok(None);
    };

    // Agents read live documents only: a draft or a pending version is not there for them.
    if !detail.drafts && !doc.status.is_live() {
        return Ok(None);
    }

    let pending = if detail.drafts {
        pending_of_many(conn, columns, &[id])?.remove(&id)
    } else {
        None
    };

    let sections = if detail.sections {
        Some(sections(conn, id)?)
    } else {
        None
    };

    let chunk = match detail.chunk {
        Some(chunk_id) => Some(chunk(conn, id, chunk_id)?),
        None => None,
    };

    Ok(Some(DocumentDetail {
        links: links_of(conn, id)?,
        doc,
        usage,
        sections,
        chunk,
        pending,
    }))
}

fn sections(conn: &Connection, id: i64) -> Result<Vec<Section>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT id, ordinal, heading_path, tokens, char_start, char_end
         FROM knowledge_chunks WHERE doc_id = ?1 ORDER BY ordinal",
    )?;

    let rows = statement.query_map([id], |row| {
        Ok(Section {
            chunk_id: row.get(0)?,
            ordinal: row.get(1)?,
            heading_path: row.get(2)?,
            tokens: row.get(3)?,
            char_start: row.get(4)?,
            char_end: row.get(5)?,
        })
    })?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// Chunk `chunk_id` of document `id`, whole and without its vector.
fn chunk(conn: &Connection, id: i64, chunk_id: i64) -> Result<Chunk, StoreError> {
    let found = conn
        .query_row(
            "SELECT id, doc_id, ordinal, heading_path, text, chunk_sha, tokens, char_start, char_end
             FROM knowledge_chunks WHERE id = ?1 AND doc_id = ?2",
            params![chunk_id, id],
            |row| {
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
            },
        )
        .optional()?;

    found.ok_or_else(|| StoreError::NotFound(format!("chunk {chunk_id} is not in document {id}")))
}

pub(super) fn repos(conn: &Connection) -> Result<RepoCounts, StoreError> {
    let global = count(
        conn,
        &format!("SELECT count(*) FROM knowledge_documents d WHERE d.pending_of IS NULL AND {GLOBAL}"),
    )?;

    let mut statement = conn.prepare(
        "SELECT l.repo_key, count(*) AS docs FROM knowledge_doc_links l
         JOIN knowledge_documents d ON d.id = l.doc_id WHERE d.pending_of IS NULL
         GROUP BY l.repo_key ORDER BY docs DESC, l.repo_key",
    )?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?)))?;

    Ok(RepoCounts {
        global,
        repos: rows.collect::<Result<_, _>>()?,
    })
}

pub(super) fn stats(conn: &Connection) -> Result<Stats, StoreError> {
    let model_id = meta::get(conn, "model_id")?;
    let (documents, bytes): (usize, u64) = conn.query_row(
        "SELECT count(*), coalesce(sum(bytes), 0) FROM knowledge_documents WHERE pending_of IS NULL",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let embedded_chunks: usize = conn.query_row(
        "SELECT count(*) FROM knowledge_chunks WHERE embedding IS NOT NULL AND (?1 IS NULL OR model_id = ?1)",
        [&model_id],
        |row| row.get(0),
    )?;

    Ok(Stats {
        documents,
        chunks: count(conn, "SELECT count(*) FROM knowledge_chunks")?,
        embedded_chunks,
        bytes,
        global_documents: count(
            conn,
            &format!("SELECT count(*) FROM knowledge_documents d WHERE d.pending_of IS NULL AND {GLOBAL}"),
        )?,
        repositories: count(
            conn,
            "SELECT count(DISTINCT l.repo_key) FROM knowledge_doc_links l
             JOIN knowledge_documents d ON d.id = l.doc_id WHERE d.pending_of IS NULL",
        )?,
        generation: meta::generation(conn)?,
        schema_version: schema::user_version(conn)?,
        model_id,
        drafts: count(conn, "SELECT count(*) FROM knowledge_documents WHERE status = 'draft'")?,
        waiting: count(
            conn,
            "SELECT count(*) FROM knowledge_documents WHERE status = 'waiting'",
        )?,
        changes_requested: count(
            conn,
            "SELECT count(*) FROM knowledge_documents WHERE status = 'changesRequested'",
        )?,
        pending: count(
            conn,
            "SELECT count(*) FROM knowledge_documents WHERE pending_of IS NOT NULL",
        )?,
    })
}

fn count(conn: &Connection, sql: &str) -> Result<usize, StoreError> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}
