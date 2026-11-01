//! Reading documents: the list with its filters and order, one document with its parts, and the
//! counts per repository and overall.

use postgres::GenericClient;
use postgres::types::ToSql;

use super::documents::{COLUMNS, document, links_of};
use super::error::{backend, column, count, int};
use super::{meta, schema};
use crate::limits::{LIST_PAGE_MAX, QUERY_TOKENS_MAX};
use crate::store::{Chunk, Detail, DocumentDetail, DocumentPage, DocumentRow, ListQuery, RepoCounts, RepoFilter};
use crate::store::{Section, Sort, Stats, StoreError};

/// A document of no repository (aliased `d`).
const GLOBAL: &str = "NOT EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id)";

/// A statement parameter that outlives the call that builds it.
type Param = Box<dyn ToSql + Sync>;

/// One page of the documents `query` keeps, with the total over all pages.
pub(super) fn list(client: &mut impl GenericClient, query: &ListQuery) -> Result<DocumentPage, StoreError> {
    let (filter, mut params) = conditions(query);

    let total = client
        .query_one(
            &format!("SELECT count(*) FROM knowledge_documents d WHERE {filter}"),
            &refs(&params),
        )
        .map_err(backend)?;

    // The `C` collation keeps the order the same on every server, whatever its locale.
    let order = match query.sort {
        Sort::Title => "lower(d.title) COLLATE \"C\"",
        Sort::Updated => "d.updated_at",
        Sort::Size => "d.bytes",
    };
    let direction = if query.descending { "DESC" } else { "ASC" };

    params.push(Box::new(int(query.limit.min(LIST_PAGE_MAX))));
    params.push(Box::new(int(query.offset)));

    let sql = format!(
        "SELECT {COLUMNS} FROM knowledge_documents d WHERE {filter}
         ORDER BY {order} {direction}, d.id {direction} LIMIT ${} OFFSET ${}",
        params.len() - 1,
        params.len()
    );

    let rows = client.query(&sql, &refs(&params)).map_err(backend)?;
    let docs = rows.iter().map(document).collect::<Result<Vec<_>, _>>()?;

    let ids: Vec<i64> = docs.iter().map(|doc| doc.id).collect();
    let mut links = links_of(client, &ids)?;

    let items = docs
        .into_iter()
        .map(|doc| DocumentRow {
            links: links.remove(&doc.id).unwrap_or_default(),
            doc,
        })
        .collect();

    Ok(DocumentPage {
        items,
        total: count(&total, 0)?,
    })
}

fn refs(params: &[Param]) -> Vec<&(dyn ToSql + Sync)> {
    params.iter().map(|param| &**param as &(dyn ToSql + Sync)).collect()
}

/// The `WHERE` clause of `query`'s filters and its numbered parameters.
fn conditions(query: &ListQuery) -> (String, Vec<Param>) {
    let mut clauses: Vec<String> = vec!["TRUE".to_owned()];
    let mut params: Vec<Param> = Vec::new();

    match &query.filter {
        RepoFilter::All => {}
        RepoFilter::Global => clauses.push(GLOBAL.to_owned()),
        RepoFilter::Repo(key) => {
            params.push(Box::new(key.clone()));
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id AND l.repo_key = ${})",
                params.len()
            ));
        }
    }

    if let Some(format) = &query.format {
        params.push(Box::new(format.clone()));
        clauses.push(format!("d.format = ${}", params.len()));
    }

    let text = query.text.trim();

    if !text.is_empty() {
        params.push(Box::new(format!("%{}%", escape_like(text))));
        let mut matching = format!("d.title ILIKE ${} ESCAPE '\\'", params.len());

        if let Some(tsquery) = prefix_query(text) {
            params.push(Box::new(tsquery));
            matching.push_str(&format!(
                " OR EXISTS (SELECT 1 FROM knowledge_chunks c WHERE c.doc_id = d.id
                             AND c.tsv @@ to_tsquery('simple'::regconfig, {}))",
                schema::fold(&format!("${}", params.len()))
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

/// The words of `text` as a `tsquery` that wants all of them, each as a prefix: `'api':* & 'gat':*`.
/// The words are lowercase letters and digits, so nothing in it is `tsquery` syntax. `None` when
/// there is no word.
fn prefix_query(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(QUERY_TOKENS_MAX)
        .map(|word| format!("'{}':*", word.to_lowercase()))
        .collect();

    (!words.is_empty()).then(|| words.join(" & "))
}

/// Document `id` with its links, and what `detail` asks for.
pub(super) fn get(
    client: &mut impl GenericClient,
    id: i64,
    detail: &Detail,
) -> Result<Option<DocumentDetail>, StoreError> {
    let sql = format!("SELECT {COLUMNS} FROM knowledge_documents d WHERE d.id = $1");
    let row = client.query_opt(&sql, &[&id]).map_err(backend)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let doc = document(&row)?;

    let sections = if detail.sections {
        Some(sections(client, id)?)
    } else {
        None
    };

    let chunk = match detail.chunk {
        Some(chunk_id) => Some(chunk(client, id, chunk_id)?),
        None => None,
    };

    Ok(Some(DocumentDetail {
        links: links_of(client, &[id])?.remove(&id).unwrap_or_default(),
        doc,
        sections,
        chunk,
    }))
}

fn sections(client: &mut impl GenericClient, id: i64) -> Result<Vec<Section>, StoreError> {
    let rows = client
        .query(
            "SELECT id, ordinal, heading_path, tokens, char_start, char_end
             FROM knowledge_chunks WHERE doc_id = $1 ORDER BY ordinal",
            &[&id],
        )
        .map_err(backend)?;

    rows.iter()
        .map(|row| {
            Ok(Section {
                chunk_id: column(row, 0)?,
                ordinal: count(row, 1)?,
                heading_path: column(row, 2)?,
                tokens: count(row, 3)?,
                char_start: count(row, 4)?,
                char_end: count(row, 5)?,
            })
        })
        .collect()
}

/// Chunk `chunk_id` of document `id`, whole and without its vector.
fn chunk(client: &mut impl GenericClient, id: i64, chunk_id: i64) -> Result<Chunk, StoreError> {
    let row = client
        .query_opt(
            "SELECT id, doc_id, ordinal, heading_path, text, chunk_sha, tokens, char_start, char_end
             FROM knowledge_chunks WHERE id = $1 AND doc_id = $2",
            &[&chunk_id, &id],
        )
        .map_err(backend)?;

    let Some(row) = row else {
        return Err(StoreError::NotFound(format!(
            "chunk {chunk_id} is not in document {id}"
        )));
    };

    Ok(Chunk {
        id: column(&row, 0)?,
        doc_id: column(&row, 1)?,
        ordinal: count(&row, 2)?,
        heading_path: column(&row, 3)?,
        text: column(&row, 4)?,
        chunk_sha: column(&row, 5)?,
        tokens: count(&row, 6)?,
        char_start: count(&row, 7)?,
        char_end: count(&row, 8)?,
        embedding: None,
        model_id: None,
    })
}

pub(super) fn repos(client: &mut impl GenericClient) -> Result<RepoCounts, StoreError> {
    let global = scalar(
        client,
        &format!("SELECT count(*) FROM knowledge_documents d WHERE {GLOBAL}"),
    )?;

    let rows = client
        .query(
            "SELECT repo_key, count(*) AS docs FROM knowledge_doc_links
             GROUP BY repo_key ORDER BY docs DESC, repo_key COLLATE \"C\"",
            &[],
        )
        .map_err(backend)?;

    let repos = rows
        .iter()
        .map(|row| Ok((column(row, 0)?, count(row, 1)?)))
        .collect::<Result<_, StoreError>>()?;

    Ok(RepoCounts { global, repos })
}

pub(super) fn stats(client: &mut impl GenericClient) -> Result<Stats, StoreError> {
    let model_id = meta::get(client, "model_id")?;

    let totals = client
        .query_one(
            "SELECT count(*), coalesce(sum(bytes), 0)::bigint FROM knowledge_documents",
            &[],
        )
        .map_err(backend)?;

    let embedded = client
        .query_one(
            "SELECT count(*) FROM knowledge_chunks
             WHERE embedding IS NOT NULL AND ($1::text IS NULL OR model_id = $1)",
            &[&model_id],
        )
        .map_err(backend)?;

    let bytes: i64 = column(&totals, 1)?;

    Ok(Stats {
        documents: count(&totals, 0)?,
        chunks: scalar(client, "SELECT count(*) FROM knowledge_chunks")?,
        embedded_chunks: count(&embedded, 0)?,
        bytes: u64::try_from(bytes).unwrap_or(0),
        global_documents: scalar(
            client,
            &format!("SELECT count(*) FROM knowledge_documents d WHERE {GLOBAL}"),
        )?,
        repositories: scalar(client, "SELECT count(DISTINCT repo_key) FROM knowledge_doc_links")?,
        generation: meta::generation(client)?,
        schema_version: schema::stored_version(client)?.unwrap_or(schema::SCHEMA_VERSION),
        model_id,
    })
}

/// The one count `sql` selects.
fn scalar(client: &mut impl GenericClient, sql: &str) -> Result<usize, StoreError> {
    let row = client.query_one(sql, &[]).map_err(backend)?;

    count(&row, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_words_of_a_text_become_quoted_prefixes_that_all_must_match() {
        assert_eq!(prefix_query("API gat").as_deref(), Some("'api':* & 'gat':*"));
        assert_eq!(prefix_query("\"( OR *").as_deref(), Some("'or':*"));
        assert_eq!(prefix_query("'; DROP").as_deref(), Some("'drop':*"));
        assert_eq!(prefix_query(" ?! "), None);
    }

    #[test]
    fn like_wildcards_are_escaped() {
        assert_eq!(escape_like("100%_\\"), "100\\%\\_\\\\");
    }
}
