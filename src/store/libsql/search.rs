//! The two candidate lists of a search: keyword (FTS5 BM25, the local file's statement) and
//! vector (the cosine ANN index).
//!
//! Both read only the chunks of the documents the [`Scope`] sees.
//!
//! Vector: `vector_top_k` cannot be filtered, so it is asked for [`CANDIDATES_MAX`] nearest
//! chunks of the whole database, and the scope and the current model are applied to those, then
//! the closest `k` are returned. A scope that holds few of the nearest chunks (a repository with
//! little documentation next to a large one) may therefore return fewer than `k`: the fusion
//! tolerates a short list. The score is the cosine similarity, `1 - vector_distance_cos`.
//! `vector_top_k` is approximate: the order of the candidates is recomputed exactly from the
//! distance.

use super::documents::vector_text;
use super::sql::{Row, Sql, Value, args};
use crate::limits::CANDIDATES_MAX;
use crate::store::sqlite::sql_shared::{fts_query, keyword_sql, linked_globs, visible};
use crate::store::{ChunkHit, DIMS, Scope, StoreError};

/// The scope as its two statement parameters: whether every document is visible, and the
/// repository whose links are read.
fn bindings(scope: &Scope) -> (i64, Option<&str>) {
    (i64::from(scope.sees_everything()), scope.repo_key())
}

pub(super) fn keyword_top_k(
    db: &mut dyn Sql,
    query: &str,
    scope: &Scope,
    k: usize,
) -> Result<Vec<ChunkHit>, StoreError> {
    let Some(fts) = fts_query(query) else {
        return Ok(Vec::new());
    };

    let (all, repo) = bindings(scope);
    let rows = db.query(&keyword_sql(), args![fts, all, repo, limit(k)])?;

    rows.iter().map(hit).collect()
}

pub(super) fn vector_top_k(
    db: &mut dyn Sql,
    vector: &[f32],
    scope: &Scope,
    k: usize,
) -> Result<Vec<ChunkHit>, StoreError> {
    if vector.len() != DIMS || vector.iter().any(|value| !value.is_finite()) {
        return Err(StoreError::Invalid(format!("a query vector is {DIMS} finite numbers")));
    }

    let (all, repo) = bindings(scope);
    let rows = db.query(&vector_sql(), args![vector_text(vector), all, repo, limit(k)])?;

    rows.iter().map(hit).collect()
}

/// The nearest chunks of the index for `?1`, kept when they are in the scope `?2` (all) and `?3`
/// (repository) and made by the current model (the one in the meta table, when it is set), the
/// closest `?4` first. Columns as [`keyword_sql`].
fn vector_sql() -> String {
    format!(
        "SELECT c.id, c.doc_id, d.title, c.ordinal, c.heading_path, c.text,
                1 - vector_distance_cos(c.embedding, vector32(?1)) AS similarity, d.updated_at, {globs}
         FROM vector_top_k('knowledge_chunks_vec', vector32(?1), {CANDIDATES_MAX}) nearest
         JOIN knowledge_chunks c ON c.id = nearest.id
         JOIN knowledge_documents d ON d.id = c.doc_id
         WHERE c.model_id = coalesce((SELECT value FROM knowledge_meta WHERE key = 'model_id'), c.model_id)
           AND {visible}
         ORDER BY similarity DESC, c.id
         LIMIT ?4",
        globs = linked_globs(3),
        visible = visible(2, 3),
    )
}

/// A [`ChunkHit`] from the columns the two statements share.
fn hit(row: &Row) -> Result<ChunkHit, StoreError> {
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

/// `k` capped at [`CANDIDATES_MAX`], as the SQL parameter.
fn limit(k: usize) -> Value {
    Value::from(k.min(CANDIDATES_MAX))
}
