//! The two candidate lists of a search: keyword (`ts_rank_cd`) and vector (pgvector cosine).
//!
//! Both read only the chunks of the documents the [`Scope`] sees ([`visible`]).
//!
//! Keyword: the words of the query are folded to lowercase letters and digits, quoted and OR-ed
//! (`'git' | 'cli'`) for `to_tsquery('simple')`, so `tsquery` operators, quotes and stray `*`, `!`,
//! `&` or `:` in the query are plain words and cannot break the statement. The chunk's column
//! weighs title A, heading path B, text D (see [`super::schema`]), and `ts_rank_cd` ranks by how
//! close together the matches are.
//!
//! Vector: the HNSW index orders chunks by cosine distance and the score is `1 - distance`, the
//! cosine itself (stored vectors and the query are L2-normalised). The scope is a `WHERE` filter
//! of the same scan. pgvector 0.8 and newer keep scanning the index until the filter has let `k`
//! chunks through (`hnsw.iterative_scan`, in `relaxed_order`, so a final sort puts the rows in
//! order), up to `hnsw.max_scan_tuples` (20 000 by default): a scope that keeps a tiny share of a
//! huge corpus can come back with fewer than `k`. An older pgvector filters the rows the index
//! returns, so `hnsw.ef_search` is raised to [`CANDIDATES_MAX`] and `k` of the candidates is asked
//! for ([`CANDIDATES_MAX`] when the scope hides some), which also can come back short. Only the
//! vectors of the current model are compared (`model_id` in the meta table, when one is set).

use postgres::types::ToSql;
use postgres::{Client, GenericClient, Row};

use super::error::{backend, column, count};
use super::vector::{Problem, Version};
use super::{meta, schema};
use crate::limits::{CANDIDATES_MAX, QUERY_TOKENS_MAX};
use crate::store::{ChunkHit, DIMS, Scope, StoreError};

/// True for the documents of the scope: parameter `${all}` is true for every document, else the
/// global documents (no link) and the ones linked to the repository in `${repo}` (NULL for none).
fn visible(all: usize, repo: usize) -> String {
    format!(
        "(${all}::boolean
          OR NOT EXISTS (SELECT 1 FROM knowledge_doc_links v WHERE v.doc_id = d.id)
          OR EXISTS (SELECT 1 FROM knowledge_doc_links v WHERE v.doc_id = d.id AND v.repo_key = ${repo}::text))"
    )
}

/// The globs of the document's link to the repository in `${repo}`: NULL when it is not linked.
fn linked_globs(repo: usize) -> String {
    format!("(SELECT g.globs FROM knowledge_doc_links g WHERE g.doc_id = d.id AND g.repo_key = ${repo}::text)")
}

/// The scope as its two statement parameters: whether every document is visible, and the
/// repository whose links are read.
fn bindings(scope: &Scope) -> (bool, Option<&str>) {
    (scope.sees_everything(), scope.repo_key())
}

/// The words of `text` as a `tsquery`: `'a' | 'b'`. `None` when there is no word. The words are
/// lowercase letters and digits, so nothing in the query is `tsquery` syntax. Accents are folded
/// by the statement itself ([`schema::fold`]), like the indexed text.
fn tsquery(text: &str) -> Option<String> {
    let mut words: Vec<String> = Vec::new();

    for word in text.split(|character: char| !character.is_alphanumeric()) {
        if word.is_empty() || words.len() >= QUERY_TOKENS_MAX {
            continue;
        }

        let word = word.to_lowercase();

        if !words.contains(&word) {
            words.push(word);
        }
    }

    if words.is_empty() {
        return None;
    }

    let quoted: Vec<String> = words.iter().map(|word| format!("'{word}'")).collect();
    Some(quoted.join(" | "))
}

pub(super) fn keyword_top_k(
    client: &mut impl GenericClient,
    query: &str,
    scope: &Scope,
    k: usize,
) -> Result<Vec<ChunkHit>, StoreError> {
    let Some(words) = tsquery(query) else {
        return Ok(Vec::new());
    };

    let (all, repo) = bindings(scope);
    let sql = format!(
        "WITH q AS (SELECT to_tsquery('simple'::regconfig, {words}) AS query)
         SELECT c.id, c.doc_id, d.title, c.ordinal, c.heading_path, c.text,
                ts_rank_cd(c.tsv, q.query)::float8 AS relevance, d.updated_at, {globs}
         FROM q
         CROSS JOIN knowledge_chunks c
         JOIN knowledge_documents d ON d.id = c.doc_id
         WHERE c.tsv @@ q.query AND {visible}
         ORDER BY relevance DESC, c.id
         LIMIT $4",
        words = schema::fold("$1::text"),
        globs = linked_globs(3),
        visible = visible(2, 3),
    );

    let rows = limit(k);
    let params: [&(dyn ToSql + Sync); 4] = [&words, &all, &repo, &rows];

    client.query(&sql, &params).map_err(backend)?.iter().map(hit).collect()
}

pub(super) fn vector_top_k(
    client: &mut Client,
    pgvector: Option<Version>,
    vector: &[f32],
    scope: &Scope,
    k: usize,
) -> Result<Vec<ChunkHit>, StoreError> {
    if vector.len() != DIMS || vector.iter().any(|value| !value.is_finite()) {
        return Err(StoreError::Invalid(format!("a query vector is {DIMS} finite numbers")));
    }

    let Some(pgvector) = pgvector else {
        return Err(StoreError::Backend(Problem::NoPgvector.message()));
    };

    let (all, repo) = bindings(scope);
    let model = meta::get(client, "model_id")?;

    // The scan's own limit: `k` when the filter is part of an iterative scan or hides nothing.
    let candidates = if pgvector.has_iterative_scan() || all {
        limit(k)
    } else {
        limit(CANDIDATES_MAX)
    };

    let sql = format!(
        "WITH nearest AS MATERIALIZED (
             SELECT c.id, c.embedding <=> $1::real[]::vector AS distance
             FROM knowledge_chunks c JOIN knowledge_documents d ON d.id = c.doc_id
             WHERE c.embedding IS NOT NULL AND ($4::text IS NULL OR c.model_id = $4::text) AND {visible}
             ORDER BY c.embedding <=> $1::real[]::vector
             LIMIT $5)
         SELECT c.id, c.doc_id, d.title, c.ordinal, c.heading_path, c.text,
                (1 - n.distance)::float8, d.updated_at, {globs}
         FROM nearest n
         JOIN knowledge_chunks c ON c.id = n.id
         JOIN knowledge_documents d ON d.id = c.doc_id
         ORDER BY n.distance, c.id
         LIMIT $6",
        visible = visible(2, 3),
        globs = linked_globs(3),
    );

    let query: Vec<f32> = vector.to_vec();
    let rows = limit(k);
    let params: [&(dyn ToSql + Sync); 6] = [&query, &all, &repo, &model, &candidates, &rows];

    // The index settings last for this transaction only.
    let mut tx = client.transaction().map_err(backend)?;
    tx.batch_execute(&settings(pgvector)).map_err(backend)?;

    let hits = tx
        .query(&sql, &params)
        .map_err(backend)?
        .iter()
        .map(hit)
        .collect::<Result<_, _>>();

    tx.commit().map_err(backend)?;

    hits
}

/// The `SET LOCAL` statements of a vector search on this pgvector.
fn settings(pgvector: Version) -> String {
    let mut sql = format!("SET LOCAL hnsw.ef_search = {CANDIDATES_MAX};");

    if pgvector.has_iterative_scan() {
        sql.push_str(" SET LOCAL hnsw.iterative_scan = relaxed_order;");
    }

    sql
}

/// A [`ChunkHit`] from the columns the two statements share.
fn hit(row: &Row) -> Result<ChunkHit, StoreError> {
    Ok(ChunkHit {
        chunk_id: column(row, 0)?,
        doc_id: column(row, 1)?,
        title: column(row, 2)?,
        ordinal: count(row, 3)?,
        heading_path: column(row, 4)?,
        text: column(row, 5)?,
        score: column(row, 6)?,
        updated_at: column(row, 7)?,
        repo_globs: column(row, 8)?,
    })
}

/// `k` capped at [`CANDIDATES_MAX`], as the SQL parameter.
fn limit(k: usize) -> i64 {
    i64::try_from(k.min(CANDIDATES_MAX)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_words_are_quoted_and_or_ed() {
        assert_eq!(tsquery("Git CLI").as_deref(), Some("'git' | 'cli'"));
        assert_eq!(tsquery("git git").as_deref(), Some("'git'"));
        assert_eq!(tsquery("  ?! "), None);
        assert_eq!(tsquery(""), None);
    }

    #[test]
    fn query_syntax_is_dropped_into_plain_words() {
        let query = tsquery("title:\"x\" a&b | !c <-> d:* ' ) ( \\ -secret ^start");

        assert_eq!(
            query.as_deref(),
            Some("'title' | 'x' | 'a' | 'b' | 'c' | 'd' | 'secret' | 'start'")
        );
    }

    #[test]
    fn a_long_query_keeps_its_first_words() {
        let text: Vec<String> = (0..100).map(|n| format!("w{n}")).collect();
        let query = tsquery(&text.join(" ")).expect("query");

        assert_eq!(query.matches(" | ").count(), QUERY_TOKENS_MAX - 1);
    }

    #[test]
    fn the_index_settings_follow_the_pgvector_version() {
        assert_eq!(settings(Version(0, 5, 1)), "SET LOCAL hnsw.ef_search = 200;");
        assert!(settings(Version(0, 8, 7)).contains("hnsw.iterative_scan = relaxed_order"));
    }
}
