//! The two candidate lists of a search: keyword (FTS5 BM25) and vector (brute-force cosine).
//!
//! Both read only the chunks of the documents the [`Scope`] sees ([`VISIBLE`]).
//!
//! Keyword: the words of the query are double-quoted and OR-ed (`"git" OR "cli"`), so FTS5
//! operators, column filters and quotes in the query are plain words and cannot break the
//! statement. Chunks rank by `bm25(knowledge_chunks_fts, 4, 2, 1)`: a word in the title counts
//! four times, in the heading path twice, in the text once.
//!
//! Vector: the scope is a SQL prefilter, then the dot product of every stored vector with the
//! query, in Rust. Stored vectors and the query are L2-normalised, so it is the cosine. Only the
//! vectors of the current model are compared (`model_id` in the meta table, when one is set).

use rusqlite::{Connection, OptionalExtension, params};

use super::documents::decode;
use super::meta;
use crate::limits::{CANDIDATES_MAX, QUERY_TOKENS_MAX};
use crate::store::{ChunkHit, DIMS, Scope, StoreError};

/// True for the documents of the scope: parameter `?{all}` is 1 for every document, else the
/// global documents (no link) and the ones linked to the repository in `?{repo}` (NULL for none).
pub(super) fn visible(all: usize, repo: usize) -> String {
    format!(
        "(?{all} = 1
          OR NOT EXISTS (SELECT 1 FROM knowledge_doc_links v WHERE v.doc_id = d.id)
          OR EXISTS (SELECT 1 FROM knowledge_doc_links v WHERE v.doc_id = d.id AND v.repo_key = ?{repo}))"
    )
}

/// The globs of the document's link to the repository in `?{repo}`: NULL when it is not linked.
fn linked_globs(repo: usize) -> String {
    format!("(SELECT g.globs FROM knowledge_doc_links g WHERE g.doc_id = d.id AND g.repo_key = ?{repo})")
}

/// The scope as its two statement parameters: whether every document is visible, and the
/// repository whose links are read.
fn bindings(scope: &Scope) -> (i64, Option<&str>) {
    (i64::from(scope.sees_everything()), scope.repo_key())
}

/// The words of `text` as an FTS5 query: `"a" OR "b"`. `None` when there is no word. The words
/// are lowercase letters and digits, so nothing in the query is FTS5 syntax.
fn fts_query(text: &str) -> Option<String> {
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

    let quoted: Vec<String> = words.iter().map(|word| format!("\"{word}\"")).collect();
    Some(quoted.join(" OR "))
}

pub(super) fn keyword_top_k(
    conn: &Connection,
    query: &str,
    scope: &Scope,
    k: usize,
) -> Result<Vec<ChunkHit>, StoreError> {
    let Some(fts) = fts_query(query) else {
        return Ok(Vec::new());
    };

    let (all, repo) = bindings(scope);
    let sql = format!(
        "SELECT c.id, c.doc_id, d.title, c.ordinal, c.heading_path, c.text,
                -bm25(knowledge_chunks_fts, 4, 2, 1) AS relevance, d.updated_at, {globs}
         FROM knowledge_chunks_fts
         JOIN knowledge_chunks c ON c.id = knowledge_chunks_fts.rowid
         JOIN knowledge_documents d ON d.id = c.doc_id
         WHERE knowledge_chunks_fts MATCH ?1 AND {visible}
         ORDER BY relevance DESC, c.id
         LIMIT ?4",
        globs = linked_globs(3),
        visible = visible(2, 3),
    );

    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params![fts, all, repo, limit(k)], hit)?;

    Ok(rows.collect::<Result<_, _>>()?)
}

pub(super) fn vector_top_k(
    conn: &Connection,
    vector: &[f32],
    scope: &Scope,
    k: usize,
) -> Result<Vec<ChunkHit>, StoreError> {
    if vector.len() != DIMS || vector.iter().any(|value| !value.is_finite()) {
        return Err(StoreError::Invalid(format!("a query vector is {DIMS} finite numbers")));
    }

    let (all, repo) = bindings(scope);
    let model = meta::get(conn, "model_id")?;

    // Only the vectors are read here: the text of the few that win is read afterwards.
    let mut scan = conn.prepare(&format!(
        "SELECT c.id, c.embedding
         FROM knowledge_chunks c JOIN knowledge_documents d ON d.id = c.doc_id
         WHERE c.embedding IS NOT NULL AND (?3 IS NULL OR c.model_id = ?3) AND {visible}",
        visible = visible(1, 2),
    ))?;

    let mut rows = scan.query(params![all, repo, model])?;
    let mut scored: Vec<(f64, i64)> = Vec::new();

    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;

        if let Some(stored) = decode(&blob) {
            scored.push((dot(vector, &stored), id));
        }
    }

    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(limit(k).try_into().unwrap_or(0));

    scored
        .into_iter()
        .filter_map(|(score, id)| chunk_hit(conn, id, score, repo).transpose())
        .collect()
}

/// The chunk `id` as a hit with `score`; `None` when it is gone since the scan.
fn chunk_hit(conn: &Connection, id: i64, score: f64, repo: Option<&str>) -> Result<Option<ChunkHit>, StoreError> {
    let sql = format!(
        "SELECT c.id, c.doc_id, d.title, c.ordinal, c.heading_path, c.text, ?3, d.updated_at, {globs}
         FROM knowledge_chunks c JOIN knowledge_documents d ON d.id = c.doc_id
         WHERE c.id = ?1",
        globs = linked_globs(2),
    );

    Ok(conn.query_row(&sql, params![id, repo, score], hit).optional()?)
}

/// A [`ChunkHit`] from the columns the two statements share.
fn hit(row: &rusqlite::Row) -> rusqlite::Result<ChunkHit> {
    let globs: Option<String> = row.get(8)?;

    Ok(ChunkHit {
        chunk_id: row.get(0)?,
        doc_id: row.get(1)?,
        title: row.get(2)?,
        ordinal: row.get(3)?,
        heading_path: row.get(4)?,
        text: row.get(5)?,
        score: row.get(6)?,
        updated_at: row.get(7)?,
        repo_globs: globs.map(|globs| serde_json::from_str(&globs).unwrap_or_default()),
    })
}

/// The dot product of two vectors of the same size.
fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| f64::from(*x) * f64::from(*y)).sum()
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
        assert_eq!(fts_query("Git CLI").as_deref(), Some("\"git\" OR \"cli\""));
        assert_eq!(fts_query("git git").as_deref(), Some("\"git\""));
        assert_eq!(fts_query("  ?! "), None);
        assert_eq!(fts_query(""), None);
    }

    #[test]
    fn query_syntax_is_dropped_into_plain_words() {
        let query = fts_query("title:\"x\" NEAR(a b) OR NOT * -secret ^start");

        assert_eq!(
            query.as_deref(),
            Some("\"title\" OR \"x\" OR \"near\" OR \"a\" OR \"b\" OR \"or\" OR \"not\" OR \"secret\" OR \"start\"")
        );
    }

    #[test]
    fn a_long_query_keeps_its_first_words() {
        let text: Vec<String> = (0..100).map(|n| format!("w{n}")).collect();
        let query = fts_query(&text.join(" ")).expect("query");

        assert_eq!(query.matches(" OR ").count(), QUERY_TOKENS_MAX - 1);
    }
}
