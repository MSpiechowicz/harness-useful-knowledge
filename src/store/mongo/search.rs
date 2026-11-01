//! The two candidate lists of a search: keyword (`$search` on the `mongot` text index) and vector
//! (`$vectorSearch` on the vector index).
//!
//! Both read only the chunks of the documents the [`Scope`] sees. The scope is a prefilter inside
//! the index (the `repo_keys` and `global` fields every chunk carries), so a vector query asks
//! the index for its `k` nearest chunks among the visible ones rather than filtering afterwards.
//! A hit is checked against the scope once more from the stored chunk, so a document whose links
//! changed a moment ago and whose chunks the index has not caught up with is never shown to a
//! repository that can no longer see it.
//!
//! Keyword: the words of the query (see [`terms::query_words`]) are a `text` operator on each of
//! the three fields, and a chunk needs one of them in one field. The operator takes no syntax, so
//! nothing in a query is an operator. The title counts four times, the heading path twice and the
//! text once, and the score is the index's `searchScore`.
//!
//! Vector: cosine similarity, so a hit's score is `2 * vectorSearchScore - 1` (the index scales the
//! cosine to 0..1). Only the vectors of the current model compare (`model_id` in the meta
//! collection, when one is set). The index looks at `10 * k` candidates, at least 100.
//!
//! Both indexes are eventually consistent: a chunk is searchable about a second after it was
//! written.

use mongodb::bson::{Bson, Document as Row, doc};

use super::ctx::{Ctx, Fail};
use super::documents::links;
use super::schema::{CHUNKS, DOCUMENTS, KEYWORD_INDEX, VECTOR_INDEX};
use super::{int, strings, terms, text};
use crate::limits::CANDIDATES_MAX;
use crate::store::{ChunkHit, DIMS, Scope, StoreError};

/// The weights of the title, the heading path and the text in the keyword score.
const TITLE_BOOST: f64 = 4.0;
const HEADING_BOOST: f64 = 2.0;

/// The index looks at this many candidates for each chunk it returns, at least [`CANDIDATES_FLOOR`].
const CANDIDATES_PER_HIT: usize = 10;
const CANDIDATES_FLOOR: usize = 100;

/// The fields of a chunk a hit is built from.
fn hit_fields() -> Row {
    doc! {
        "doc_id": 1, "title": 1, "ordinal": 1, "heading_path": 1, "text": 1, "updated_at": 1,
        "repo_keys": 1, "global": 1,
    }
}

/// The `$search` filter keeping the chunks `scope` sees; `None` when it sees all.
pub(super) fn keyword_scope(scope: &Scope) -> Option<Row> {
    if scope.sees_everything() {
        return None;
    }

    let global = doc! { "equals": { "path": "global", "value": true } };

    let Some(repo) = scope.repo_key() else {
        return Some(global);
    };

    let linked = doc! { "equals": { "path": "repo_keys", "value": repo } };

    Some(doc! { "compound": { "should": [global, linked], "minimumShouldMatch": 1 } })
}

/// The `$vectorSearch` filter keeping the chunks `scope` sees and `model` made.
pub(super) fn vector_filter(scope: &Scope, model: Option<&str>) -> Row {
    let mut all: Vec<Row> = Vec::new();

    if let Some(model) = model {
        all.push(doc! { "model_id": model });
    }

    if !scope.sees_everything() {
        let mut visible = vec![doc! { "global": true }];

        if let Some(repo) = scope.repo_key() {
            visible.push(doc! { "repo_keys": repo });
        }

        all.push(doc! { "$or": visible });
    }

    match all.len() {
        0 => Row::new(),
        1 => all.remove(0),
        _ => doc! { "$and": all },
    }
}

/// The `$search` stage for `words` within `scope`.
pub(super) fn keyword_stage(words: &[String], scope: &Scope) -> Row {
    let field = |path: &str, boost: Option<f64>| {
        let mut operator = doc! { "query": words, "path": path };

        if let Some(value) = boost {
            operator.insert("score", doc! { "boost": { "value": value } });
        }

        doc! { "text": operator }
    };

    let mut compound = doc! {
        "should": [
            field("title", Some(TITLE_BOOST)),
            field("heading_path", Some(HEADING_BOOST)),
            field("text", None),
        ],
        "minimumShouldMatch": 1,
    };

    if let Some(filter) = keyword_scope(scope) {
        compound.insert("filter", vec![filter]);
    }

    doc! { "$search": { "index": KEYWORD_INDEX, "compound": compound } }
}

/// The `$vectorSearch` stage for `vector` within `scope`.
pub(super) fn vector_stage(vector: &[f32], scope: &Scope, model: Option<&str>, limit: usize) -> Row {
    let query: Vec<f64> = vector.iter().map(|value| f64::from(*value)).collect();
    let candidates = (limit * CANDIDATES_PER_HIT).max(CANDIDATES_FLOOR);
    let mut stage = doc! {
        "index": VECTOR_INDEX,
        "path": "embedding",
        "queryVector": query,
        "numCandidates": i64::try_from(candidates).unwrap_or(i64::MAX),
        "limit": i64::try_from(limit).unwrap_or(0),
    };

    let filter = vector_filter(scope, model);

    if !filter.is_empty() {
        stage.insert("filter", filter);
    }

    doc! { "$vectorSearch": stage }
}

/// `k` capped at [`CANDIDATES_MAX`].
fn limit(k: usize) -> usize {
    k.min(CANDIDATES_MAX)
}

pub(super) fn keyword_top_k(ctx: &mut Ctx<'_>, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, Fail> {
    let words = terms::query_words(query);

    if words.is_empty() || limit(k) == 0 {
        return Ok(Vec::new());
    }

    let mut projection = hit_fields();
    projection.insert("score", doc! { "$meta": "searchScore" });

    let pipeline = vec![
        keyword_stage(&words, scope),
        doc! { "$limit": i64::try_from(limit(k)).unwrap_or(0) },
        doc! { "$project": projection },
    ];

    let rows = ctx.aggregate(CHUNKS, pipeline)?;

    hits(ctx, &rows, scope, |score| score)
}

/// Fails unless `vector` is [`DIMS`] finite numbers.
pub(super) fn check_vector(vector: &[f32]) -> Result<(), StoreError> {
    if vector.len() != DIMS || vector.iter().any(|value| !value.is_finite()) {
        return Err(StoreError::Invalid(format!("a query vector is {DIMS} finite numbers")));
    }

    Ok(())
}

pub(super) fn vector_top_k(
    ctx: &mut Ctx<'_>,
    vector: &[f32],
    scope: &Scope,
    model: Option<&str>,
    k: usize,
) -> Result<Vec<ChunkHit>, Fail> {
    if limit(k) == 0 {
        return Ok(Vec::new());
    }

    let mut projection = hit_fields();
    projection.insert("score", doc! { "$meta": "vectorSearchScore" });

    let pipeline = vec![
        vector_stage(vector, scope, model, limit(k)),
        doc! { "$project": projection },
    ];
    let rows = ctx.aggregate(CHUNKS, pipeline)?;

    // The index scores the cosine from 0 (opposite) to 1 (the same).
    hits(ctx, &rows, scope, |score| 2.0 * score - 1.0)
}

/// The hits of `rows` the scope sees, with the globs of each document's link to the scope's
/// repository. `score` turns the index's score into the one a hit carries.
fn hits(ctx: &mut Ctx<'_>, rows: &[Row], scope: &Scope, score: impl Fn(f64) -> f64) -> Result<Vec<ChunkHit>, Fail> {
    let visible: Vec<&Row> = rows.iter().filter(|row| sees(scope, row)).collect();

    let ids: Vec<Bson> = visible.iter().map(|row| Bson::Int64(int(row, "doc_id"))).collect();
    let globs = match scope.repo_key() {
        Some(repo) if !ids.is_empty() => globs_of(ctx, &ids, repo)?,
        _ => Vec::new(),
    };

    Ok(visible
        .iter()
        .map(|row| {
            let doc_id = int(row, "doc_id");

            ChunkHit {
                chunk_id: int(row, "_id"),
                doc_id,
                title: text(row, "title").unwrap_or_default(),
                ordinal: usize::try_from(int(row, "ordinal")).unwrap_or(0),
                heading_path: text(row, "heading_path").unwrap_or_default(),
                text: text(row, "text").unwrap_or_default(),
                score: score(row.get_f64("score").unwrap_or(0.0)),
                updated_at: int(row, "updated_at"),
                repo_globs: globs
                    .iter()
                    .find(|(id, _)| *id == doc_id)
                    .map(|(_, globs)| globs.clone()),
            }
        })
        .collect())
}

/// True when `scope` sees the chunk `row` (from the chunk's own `global` and `repo_keys`).
fn sees(scope: &Scope, row: &Row) -> bool {
    if scope.sees_everything() || row.get_bool("global").unwrap_or(false) {
        return true;
    }

    scope
        .repo_key()
        .is_some_and(|repo| strings(row, "repo_keys").iter().any(|key| key == repo))
}

/// The globs of each of the documents `ids` to its link to `repo`; none for a document not
/// linked to it.
fn globs_of(ctx: &mut Ctx<'_>, ids: &[Bson], repo: &str) -> Result<Vec<(i64, Vec<String>)>, Fail> {
    let rows = ctx.find(
        DOCUMENTS,
        doc! { "_id": { "$in": ids } },
        (Row::new(), 0, 0),
        doc! { "links": 1 },
    )?;

    Ok(rows
        .iter()
        .filter_map(|row| {
            let link = links(row).into_iter().find(|link| link.repo_key == repo)?;
            Some((int(row, "_id"), link.globs))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::project::ProjectKey;

    fn from(key: &str) -> Scope {
        Scope::from_repo(Some(ProjectKey {
            key: key.to_owned(),
            root: PathBuf::new(),
            remote: None,
        }))
    }

    #[test]
    fn a_scope_filters_the_keyword_query_by_global_and_by_repository() {
        assert_eq!(keyword_scope(&Scope::everything()), None);
        assert_eq!(keyword_scope(&Scope::from_repo(Some(ProjectKey::every()))), None);

        let only_global = keyword_scope(&Scope::from_repo(None)).expect("filter");
        assert_eq!(only_global, doc! { "equals": { "path": "global", "value": true } });

        let from_a = keyword_scope(&from("git:h/o/a")).expect("filter");
        let should = from_a
            .get_document("compound")
            .and_then(|compound| compound.get_array("should"))
            .expect("should");
        assert_eq!(should.len(), 2);
        assert_eq!(
            should[1],
            Bson::Document(doc! { "equals": { "path": "repo_keys", "value": "git:h/o/a" } })
        );
    }

    #[test]
    fn the_keyword_stage_weights_title_heading_and_text_and_takes_plain_words() {
        let words = vec!["a".to_owned(), "or".to_owned()];
        let stage = keyword_stage(&words, &Scope::everything());
        let compound = stage
            .get_document("$search")
            .and_then(|search| search.get_document("compound"))
            .expect("compound");

        let should = compound.get_array("should").expect("should");
        let boosts: Vec<Option<f64>> = should
            .iter()
            .map(|operator| {
                operator
                    .as_document()?
                    .get_document("text")
                    .ok()?
                    .get_document("score")
                    .ok()?
                    .get_document("boost")
                    .ok()?
                    .get_f64("value")
                    .ok()
            })
            .collect();
        assert_eq!(boosts, [Some(4.0), Some(2.0), None]);

        let queried = should[0]
            .as_document()
            .and_then(|operator| operator.get_document("text").ok())
            .and_then(|text| text.get_array("query").ok())
            .expect("query");
        assert_eq!(queried.len(), 2, "the words are a list, never a query string");
        assert!(compound.get("filter").is_none(), "everything needs no filter");
    }

    #[test]
    fn the_vector_stage_prefilters_by_scope_and_model_and_over_fetches() {
        let vector = vec![0.5_f32; DIMS];

        let stage = vector_stage(&vector, &from("git:h/o/a"), Some("m1"), 50);
        let search = stage.get_document("$vectorSearch").expect("stage");
        assert_eq!(search.get_i64("numCandidates"), Ok(500), "ten candidates for each hit");
        assert_eq!(search.get_i64("limit"), Ok(50));
        assert_eq!(
            search.get_document("filter").expect("filter"),
            &doc! { "$and": [{ "model_id": "m1" }, { "$or": [{ "global": true }, { "repo_keys": "git:h/o/a" }] }] }
        );

        let small = vector_stage(&vector, &Scope::everything(), None, 3);
        let search = small.get_document("$vectorSearch").expect("stage");
        assert_eq!(search.get_i64("numCandidates"), Ok(100), "at least 100 candidates");
        assert!(search.get("filter").is_none(), "no scope and no model need no filter");

        assert_eq!(
            vector_filter(&Scope::from_repo(None), None),
            doc! { "$or": [{ "global": true }] }
        );
    }

    #[test]
    fn a_hit_the_scope_cannot_see_is_dropped_even_when_the_index_returned_it() {
        let linked = doc! { "global": false, "repo_keys": ["git:h/o/a"] };
        let global = doc! { "global": true, "repo_keys": [] };

        assert!(sees(&from("git:h/o/a"), &linked));
        assert!(!sees(&from("git:h/o/b"), &linked));
        assert!(!sees(&Scope::from_repo(None), &linked));
        assert!(sees(&Scope::from_repo(None), &global));
        assert!(sees(&Scope::everything(), &linked));
    }
}
