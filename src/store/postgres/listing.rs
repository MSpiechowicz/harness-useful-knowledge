//! Reading documents: the list with its filters and order, one document with its parts, and the
//! counts per repository and overall.

use postgres::GenericClient;
use postgres::types::ToSql;

use super::documents::{by_id, columns_sql, links_of, listed, pending_of_many};
use super::error::{backend, column, count, int};
use super::{meta, schema};
use crate::limits::{LIST_PAGE_MAX, QUERY_TOKENS_MAX};
use crate::store::{
    Chunk, ChunkMatch, ChunkMatches, Columns, Detail, DocumentDetail, DocumentPage, DocumentRow, ListQuery, RepoCounts,
    RepoFilter,
};
use crate::store::{Section, Sort, Stats, StatusFilter, StoreError, UsefulFilter};

/// A document of no repository (aliased `d`).
const GLOBAL: &str = "NOT EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id)";

/// The condition (on a document aliased `d`) that keeps the rows `status` lists, as a live
/// document is kept by the state of its pending version.
fn status_condition(status: StatusFilter) -> String {
    let pending =
        |extra: &str| format!("EXISTS (SELECT 1 FROM knowledge_documents p WHERE p.pending_of = d.id{extra})");

    match status {
        StatusFilter::All => "d.pending_of IS NULL".to_owned(),
        StatusFilter::Live => "d.status = 'live'".to_owned(),
        StatusFilter::Pending => format!("(d.status = 'live' AND {})", pending("")),
        state => {
            let wanted = state.state().map_or("live", |state| state.as_str());

            format!(
                "((d.pending_of IS NULL AND d.status = '{wanted}') OR (d.status = 'live' AND {}))",
                pending(&format!(" AND p.status = '{wanted}'"))
            )
        }
    }
}

/// A statement parameter that outlives the call that builds it.
type Param = Box<dyn ToSql + Sync>;

/// One page of the documents `query` keeps, with the total over all pages.
pub(super) fn list(
    client: &mut impl GenericClient,
    columns: Columns,
    query: &ListQuery,
) -> Result<DocumentPage, StoreError> {
    let (filter, mut params) = conditions(columns, query);

    let total = client
        .query_one(
            &format!("SELECT count(*) FROM knowledge_documents d WHERE {filter}"),
            &refs(&params),
        )
        .map_err(backend)?;

    // The `C` collation keeps the order the same on every server, whatever its locale. A title folds
    // ASCII letters only (`lower` would also fold the letters of the server's locale), the one fold
    // every store does alike, so merged pages of two places are the pages of each one's own order.
    // Without the counters the sorts by usefulness are the default order.
    let sort = if columns.usage {
        query.sort
    } else {
        query.sort.without_usage()
    };
    let order = match sort {
        Sort::Title => "translate(d.title, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz') COLLATE \"C\"",
        Sort::Updated => "d.updated_at",
        Sort::Size => "d.bytes",
        Sort::Format => "d.format",
        Sort::Chunks => "d.chunk_count",
        Sort::Repos => {
            "coalesce((SELECT min(l.repo_key) COLLATE \"C\" FROM knowledge_doc_links l WHERE l.doc_id = d.id), '')"
        }
        Sort::Useful => "coalesce(d.useful, 0)",
        Sort::LastUseful => "coalesce(d.last_useful_at, 0)",
    };
    let direction = if query.descending { "DESC" } else { "ASC" };

    params.push(Box::new(int(query.limit.min(LIST_PAGE_MAX))));
    params.push(Box::new(int(query.offset)));

    let sql = format!(
        "SELECT {} FROM knowledge_documents d WHERE {filter}
         ORDER BY {order} {direction}, d.id {direction} LIMIT ${} OFFSET ${}",
        columns_sql(columns),
        params.len() - 1,
        params.len()
    );

    let rows = client.query(&sql, &refs(&params)).map_err(backend)?;
    let docs = rows.iter().map(listed).collect::<Result<Vec<_>, _>>()?;

    let ids: Vec<i64> = docs.iter().map(|(doc, _)| doc.id).collect();
    let mut links = links_of(client, &ids)?;
    let mut pending = pending_of_many(client, columns, &ids)?;

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
        total: count(&total, 0)?,
    })
}

fn refs(params: &[Param]) -> Vec<&(dyn ToSql + Sync)> {
    params.iter().map(|param| &**param as &(dyn ToSql + Sync)).collect()
}

/// The `WHERE` clause of `query`'s filters and its numbered parameters.
fn conditions(columns: Columns, query: &ListQuery) -> (String, Vec<Param>) {
    let mut clauses: Vec<String> = vec!["TRUE".to_owned()];
    let mut params: Vec<Param> = Vec::new();

    match &query.filter {
        RepoFilter::All => {}
        RepoFilter::Global => clauses.push(GLOBAL.to_owned()),
        RepoFilter::Linked => clauses.push(format!("NOT ({GLOBAL})")),
        RepoFilter::Repo(key) => {
            params.push(Box::new(key.clone()));
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id AND l.repo_key = ${})",
                params.len()
            ));
        }
    }

    clauses.push(status_condition(query.status));

    if let Some(format) = &query.format {
        params.push(Box::new(format.clone()));
        clauses.push(format!("d.format = ${}", params.len()));
    }

    if let Some(useful) = UsefulFilter::condition(query.useful, ("d.shown", "d.useful"), columns.usage) {
        clauses.push(useful);
    }

    let text = query.text.trim();

    if !text.is_empty() {
        params.push(Box::new(format!("%{}%", escape_like(text))));
        let mut matching = format!("d.title ILIKE ${} ESCAPE '\\'", params.len());

        if let Some(tsquery) = phrase_query(text) {
            params.push(Box::new(tsquery.text));
            let wanted = format!(
                "to_tsquery('simple'::regconfig, {})",
                schema::fold(&format!("${}", params.len()))
            );
            let found = if tsquery.phrase {
                phrase_in_a_field(&wanted)
            } else {
                format!("c.tsv @@ {wanted}")
            };

            matching.push_str(&format!(
                " OR EXISTS (SELECT 1 FROM knowledge_chunks c WHERE c.doc_id = d.id AND {found})"
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

/// A `tsquery` for the words of a text.
struct PhraseQuery {
    /// The words next to each other in that order, the last as a prefix, `'api' <-> 'gat':*`. The
    /// words are lowercase letters and digits, so nothing in it is `tsquery` syntax.
    text: String,
    /// There is more than one word, so the order and the adjacency matter.
    phrase: bool,
}

/// The `tsquery` of the words of `text`. `None` when there is no word.
fn phrase_query(text: &str) -> Option<PhraseQuery> {
    let words: Vec<String> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(QUERY_TOKENS_MAX)
        .map(str::to_lowercase)
        .collect();

    let last = words.len().checked_sub(1)?;
    let terms: Vec<String> = words
        .iter()
        .enumerate()
        .map(|(index, word)| format!("'{word}'{}", if index == last { ":*" } else { "" }))
        .collect();

    Some(PhraseQuery {
        text: terms.join(" <-> "),
        phrase: words.len() > 1,
    })
}

/// The condition that a chunk `c` has the `tsquery` as a phrase in its heading path or in its text,
/// never across the two (each has a `tsvector` of its own).
fn phrase_in_a_field(tsquery: &str) -> String {
    format!("(c.tsv_heading @@ {tsquery} OR c.tsv_text @@ {tsquery})")
}

/// The chunks of document `id` that match `text` in their heading path or text, at most `limit`.
pub(super) fn matching_chunks(
    client: &mut impl GenericClient,
    id: i64,
    text: &str,
    limit: usize,
) -> Result<ChunkMatches, StoreError> {
    let Some(phrase) = phrase_query(text) else {
        return Ok(ChunkMatches::default());
    };
    let tsquery = phrase.text;

    let condition = format!(
        "c.doc_id = $1 AND {}",
        phrase_in_a_field(&format!("to_tsquery('simple'::regconfig, {})", schema::fold("$2")))
    );
    let total = client
        .query_one(
            &format!("SELECT count(*) FROM knowledge_chunks c WHERE {condition}"),
            &[&id, &tsquery],
        )
        .map_err(backend)?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = client
        .query(
            &format!(
                "SELECT c.id, c.ordinal, c.heading_path, c.text FROM knowledge_chunks c
                 WHERE {condition} ORDER BY c.ordinal LIMIT $3"
            ),
            &[&id, &tsquery, &limit],
        )
        .map_err(backend)?;

    let chunks = rows
        .iter()
        .map(|row| {
            Ok(ChunkMatch {
                chunk_id: column(row, 0)?,
                ordinal: count(row, 1)?,
                heading_path: column(row, 2)?,
                text: column(row, 3)?,
            })
        })
        .collect::<Result<_, StoreError>>()?;

    Ok(ChunkMatches {
        total: count(&total, 0)?,
        chunks,
    })
}

/// Document `id` with its links, and what `detail` asks for.
pub(super) fn get(
    client: &mut impl GenericClient,
    columns: Columns,
    id: i64,
    detail: &Detail,
) -> Result<Option<DocumentDetail>, StoreError> {
    let Some((doc, usage)) = by_id(client, columns, id)? else {
        return Ok(None);
    };

    // Agents read live documents only: a draft or a pending version is not there for them.
    if !detail.drafts && !doc.status.is_live() {
        return Ok(None);
    }

    let pending = if detail.drafts {
        pending_of_many(client, columns, &[id])?.remove(&id)
    } else {
        None
    };

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
        usage,
        sections,
        chunk,
        pending,
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
        &format!("SELECT count(*) FROM knowledge_documents d WHERE d.pending_of IS NULL AND {GLOBAL}"),
    )?;

    let rows = client
        .query(
            "SELECT l.repo_key, count(*) AS docs FROM knowledge_doc_links l
             JOIN knowledge_documents d ON d.id = l.doc_id WHERE d.pending_of IS NULL
             GROUP BY l.repo_key ORDER BY docs DESC, l.repo_key COLLATE \"C\"",
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
            "SELECT count(*), coalesce(sum(bytes), 0)::bigint FROM knowledge_documents WHERE pending_of IS NULL",
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
            &format!("SELECT count(*) FROM knowledge_documents d WHERE d.pending_of IS NULL AND {GLOBAL}"),
        )?,
        repositories: scalar(
            client,
            "SELECT count(DISTINCT l.repo_key) FROM knowledge_doc_links l
             JOIN knowledge_documents d ON d.id = l.doc_id WHERE d.pending_of IS NULL",
        )?,
        generation: meta::generation(client)?,
        schema_version: schema::stored_version(client)?.unwrap_or(schema::SCHEMA_VERSION),
        model_id,
        drafts: scalar(
            client,
            "SELECT count(*) FROM knowledge_documents WHERE status = 'draft'",
        )?,
        waiting: scalar(
            client,
            "SELECT count(*) FROM knowledge_documents WHERE status = 'waiting'",
        )?,
        changes_requested: scalar(
            client,
            "SELECT count(*) FROM knowledge_documents WHERE status = 'changesRequested'",
        )?,
        pending: scalar(
            client,
            "SELECT count(*) FROM knowledge_documents WHERE pending_of IS NOT NULL",
        )?,
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
    fn the_words_of_a_text_become_a_quoted_phrase_whose_last_word_is_a_prefix() {
        assert_eq!(
            phrase_query("API gat").map(|query| query.text).as_deref(),
            Some("'api' <-> 'gat':*")
        );
        assert_eq!(
            phrase_query("one two three").map(|query| query.text).as_deref(),
            Some("'one' <-> 'two' <-> 'three':*")
        );
        assert_eq!(
            phrase_query("\"( OR *").map(|query| query.text).as_deref(),
            Some("'or':*")
        );
        assert_eq!(
            phrase_query("'; DROP").map(|query| query.text).as_deref(),
            Some("'drop':*")
        );
        assert!(phrase_query(" ?! ").is_none());
        assert!(phrase_query("Version 1").is_some_and(|query| query.phrase));
        assert!(phrase_query("Version").is_some_and(|query| !query.phrase));
    }

    #[test]
    fn like_wildcards_are_escaped() {
        assert_eq!(escape_like("100%_\\"), "100\\%\\_\\\\");
    }
}
