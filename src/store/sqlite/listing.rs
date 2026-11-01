//! Reading documents: the list with its filters and order, one document with its parts, and the
//! counts per repository and overall.

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::documents::{COLUMNS, document, links_of, links_of_many};
use super::{meta, schema};
use crate::limits::{LIST_PAGE_MAX, QUERY_TOKENS_MAX};
use crate::store::{Chunk, Detail, DocumentDetail, DocumentPage, DocumentRow, ListQuery, RepoCounts, RepoFilter};
use crate::store::{Section, Sort, Stats, StoreError};

/// A document of no repository (aliased `d`).
const GLOBAL: &str = "NOT EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id)";

/// One page of the documents `query` keeps, with the total over all pages.
pub(super) fn list(conn: &Connection, query: &ListQuery) -> Result<DocumentPage, StoreError> {
    let (filter, mut params) = conditions(query);
    let total: i64 = conn.query_row(
        &format!("SELECT count(*) FROM knowledge_documents d WHERE {filter}"),
        params_from_iter(params.iter()),
        |row| row.get(0),
    )?;

    let order = match query.sort {
        Sort::Title => "d.title COLLATE NOCASE",
        Sort::Updated => "d.updated_at",
        Sort::Size => "d.bytes",
    };
    let direction = if query.descending { "DESC" } else { "ASC" };

    params.push(Value::Integer(
        i64::try_from(query.limit.min(LIST_PAGE_MAX)).unwrap_or(0),
    ));
    params.push(Value::Integer(i64::try_from(query.offset).unwrap_or(i64::MAX)));

    let sql = format!(
        "SELECT {COLUMNS} FROM knowledge_documents d WHERE {filter}
         ORDER BY {order} {direction}, d.id {direction} LIMIT ?{} OFFSET ?{}",
        params.len() - 1,
        params.len()
    );

    let mut statement = conn.prepare(&sql)?;
    let docs: Vec<_> = statement
        .query_map(params_from_iter(params.iter()), document)?
        .collect::<Result<_, _>>()?;

    let ids: Vec<i64> = docs.iter().map(|doc| doc.id).collect();
    let mut links = links_of_many(conn, &ids)?;

    let items = docs
        .into_iter()
        .map(|doc| DocumentRow {
            links: links.remove(&doc.id).unwrap_or_default(),
            doc,
        })
        .collect();

    Ok(DocumentPage {
        items,
        total: usize::try_from(total).unwrap_or(0),
    })
}

/// The `WHERE` clause of `query`'s filters and its numbered parameters.
fn conditions(query: &ListQuery) -> (String, Vec<Value>) {
    let mut clauses: Vec<String> = vec!["1 = 1".to_owned()];
    let mut params: Vec<Value> = Vec::new();

    match &query.filter {
        RepoFilter::All => {}
        RepoFilter::Global => clauses.push(GLOBAL.to_owned()),
        RepoFilter::Repo(key) => {
            params.push(Value::Text(key.clone()));
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id AND l.repo_key = ?{})",
                params.len()
            ));
        }
    }

    if let Some(format) = &query.format {
        params.push(Value::Text(format.clone()));
        clauses.push(format!("d.format = ?{}", params.len()));
    }

    let text = query.text.trim();

    if !text.is_empty() {
        params.push(Value::Text(format!("%{}%", escape_like(text))));
        let mut matching = format!("d.title LIKE ?{} ESCAPE '\\'", params.len());

        if let Some(fts) = prefix_query(text) {
            params.push(Value::Text(fts));
            matching.push_str(&format!(
                " OR d.id IN (SELECT c.doc_id FROM knowledge_chunks_fts
                              JOIN knowledge_chunks c ON c.id = knowledge_chunks_fts.rowid
                              WHERE knowledge_chunks_fts MATCH ?{})",
                params.len()
            ));
        }

        clauses.push(format!("({matching})"));
    }

    (clauses.join(" AND "), params)
}

/// `text` with the `LIKE` wildcards written out, for `ESCAPE '\'`.
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// The words of `text` as an FTS5 query that wants all of them, each as a prefix:
/// `"api"* "gat"*`. `None` when there is no word.
fn prefix_query(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(QUERY_TOKENS_MAX)
        .map(|word| format!("\"{}\"*", word.to_lowercase()))
        .collect();

    (!words.is_empty()).then(|| words.join(" "))
}

/// Document `id` with its links, and what `detail` asks for.
pub(super) fn get(conn: &Connection, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError> {
    let sql = format!("SELECT {COLUMNS} FROM knowledge_documents d WHERE d.id = ?1");
    let Some(doc) = conn.query_row(&sql, [id], document).optional()? else {
        return Ok(None);
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
        sections,
        chunk,
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
        &format!("SELECT count(*) FROM knowledge_documents d WHERE {GLOBAL}"),
    )?;

    let mut statement = conn.prepare(
        "SELECT repo_key, count(*) AS docs FROM knowledge_doc_links GROUP BY repo_key ORDER BY docs DESC, repo_key",
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
        "SELECT count(*), coalesce(sum(bytes), 0) FROM knowledge_documents",
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
            &format!("SELECT count(*) FROM knowledge_documents d WHERE {GLOBAL}"),
        )?,
        repositories: count(conn, "SELECT count(DISTINCT repo_key) FROM knowledge_doc_links")?,
        generation: meta::generation(conn)?,
        schema_version: schema::user_version(conn)?,
        model_id,
    })
}

fn count(conn: &Connection, sql: &str) -> Result<usize, StoreError> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}
