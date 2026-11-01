//! The two candidate lists of a search: keyword (FTS5 BM25, the local file's statement) and
//! vector (the cosine ANN index).
//!
//! Both read only the chunks of the documents the [`Scope`] sees.
//!
//! Vector: `vector_top_k` cannot be filtered, so it is asked for the nearest chunks of the whole
//! database, and the scope and the current model are applied to those, then the closest `k` are
//! returned. A scope that holds few of the nearest chunks (a repository with little documentation
//! next to a large one) would starve, so while fewer than `k` chunks are visible the window grows
//! ([`WINDOWS`]: 200, 800, then 3200 nearest chunks), and stops early once it covers every
//! embedded chunk. The index is made with `search_l=3200` (see [`super::schema`]): its beam search
//! returns at most about `search_l` chunks whatever `k` says, 200 by default, so without it a
//! wider window would return the same 200. The score is the cosine similarity, `1 - vector_distance_cos`.
//! `vector_top_k` is approximate: the order of the candidates is recomputed exactly from the
//! distance.

use super::documents::vector_text;
use super::sql::{Row, Sql, Value, args};
use crate::limits::CANDIDATES_MAX;
use crate::store::sqlite::sql_shared::{fts_query, keyword_sql, linked_globs, visible};
use crate::store::{ChunkHit, DIMS, Scope, StoreError};

/// How many nearest chunks of the index a vector search looks at, from the first try to the last.
const WINDOWS: [usize; 3] = [CANDIDATES_MAX, CANDIDATES_MAX * 4, CANDIDATES_MAX * 16];

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
    let wanted = k.min(CANDIDATES_MAX);
    let mut embedded: Option<usize> = None;
    let mut rows = Vec::new();

    for window in WINDOWS {
        rows = db.query(&vector_sql(window), args![vector_text(vector), all, repo, limit(k)])?;

        if rows.len() >= wanted {
            break;
        }

        // A window over every embedded chunk has nothing more to show.
        let total = match embedded {
            Some(total) => total,
            None => {
                let total = embedded_chunks(db)?;
                embedded = Some(total);
                total
            }
        };

        if window >= total {
            break;
        }
    }

    rows.iter().map(hit).collect()
}

fn embedded_chunks(db: &mut dyn Sql) -> Result<usize, StoreError> {
    let rows = db.query("SELECT count(*) FROM knowledge_chunks WHERE embedding IS NOT NULL", &[])?;
    let count: i64 = rows.first().map_or(Ok(0), |row| row.get(0))?;

    Ok(usize::try_from(count).unwrap_or(0))
}

/// The `window` nearest chunks of the index for `?1`, kept when they are in the scope `?2` (all) and `?3`
/// (repository) and made by the current model (the one in the meta table, when it is set), the
/// closest `?4` first. Columns as [`keyword_sql`].
fn vector_sql(window: usize) -> String {
    format!(
        "SELECT c.id, c.doc_id, d.title, c.ordinal, c.heading_path, c.text,
                1 - vector_distance_cos(c.embedding, vector32(?1)) AS similarity, d.updated_at, {globs}
         FROM vector_top_k('knowledge_chunks_vec', vector32(?1), {window}) nearest
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::libsql::sql::Executed;

    /// A database whose index holds `total` chunks, all nearer than the visible ones: a window
    /// shows the `visible` chunks only once it reaches `reach`.
    struct Index {
        total: usize,
        reach: usize,
        visible: usize,
        windows: Vec<usize>,
    }

    impl Sql for Index {
        fn query(&mut self, sql: &str, _args: &[Value]) -> Result<Vec<Row>, StoreError> {
            if sql.contains("count(*)") {
                return Ok(vec![Row::new(vec![Value::from(self.total)])]);
            }

            let window: usize = sql
                .split("vector32(?1), ")
                .nth(1)
                .and_then(|rest| rest.split(')').next())
                .and_then(|number| number.parse().ok())
                .expect("the window in the statement");
            self.windows.push(window);

            let shown = if window >= self.reach { self.visible } else { 0 };

            Ok((0..shown)
                .map(|n| {
                    Row::new(vec![
                        Value::from(n + 1),
                        Value::from(1_usize),
                        Value::from("Doc"),
                        Value::from(n),
                        Value::from(""),
                        Value::from("text"),
                        Value::Real(0.9),
                        Value::from(1_usize),
                        Value::Null,
                    ])
                })
                .collect())
        }

        fn execute(&mut self, _sql: &str, _args: &[Value]) -> Result<Executed, StoreError> {
            unreachable!("a search writes nothing")
        }

        fn execute_many(&mut self, _sql: &str, _rows: &[Vec<Value>]) -> Result<(), StoreError> {
            unreachable!("a search writes nothing")
        }

        fn batch(&mut self, _sql: &str) -> Result<(), StoreError> {
            unreachable!("a search writes nothing")
        }
    }

    fn search(index: &mut Index, k: usize) -> usize {
        let vector = vec![0.0; DIMS];

        vector_top_k(index, &vector, &Scope::everything(), k)
            .expect("vector")
            .len()
    }

    #[test]
    fn a_scope_that_holds_none_of_the_nearest_chunks_widens_the_window_until_it_finds_k() {
        let mut index = Index {
            total: 5000,
            reach: 3000,
            visible: 5,
            windows: Vec::new(),
        };

        assert_eq!(search(&mut index, 5), 5);
        assert_eq!(index.windows, [200, 800, 3200]);
    }

    #[test]
    fn the_window_stops_growing_when_it_covers_every_embedded_chunk() {
        let mut index = Index {
            total: 300,
            reach: usize::MAX,
            visible: 0,
            windows: Vec::new(),
        };

        assert_eq!(search(&mut index, 5), 0);
        assert_eq!(index.windows, [200, 800]);
    }

    #[test]
    fn the_window_is_capped_at_the_index_search_list() {
        let mut index = Index {
            total: 1_000_000,
            reach: usize::MAX,
            visible: 0,
            windows: Vec::new(),
        };

        assert_eq!(search(&mut index, 5), 0);
        assert_eq!(index.windows, [200, 800, 3200]);
    }
}
