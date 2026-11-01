//! The collections, their indexes, the two search indexes and the schema version.
//!
//! Version 1 is a set of idempotent steps, so servers that start together need no lock: a
//! collection or index that exists is left as it is, and the schema document is written last
//! (`$max`), which is what [`version`] reads. Later versions only add collections, indexes and
//! fields, never rewrite documents.
//!
//! - `knowledge_meta`: `schema` (`version`, `dims`), `generation` (`value`, the write counter),
//!   the id counters `counter:documents` and `counter:chunks` (`value`), and the meta keys the
//!   store lets a caller set as `meta:<key>` (`value`, a string).
//! - `knowledge_documents`: one document per imported file, `_id` its id, the links as `links`
//!   (`repo_key`, `globs`), and `repo_keys` and `global` copied from them for the filters of
//!   the list. Indexes: `source_key`, `content_sha256`, `repo_keys`, `global`, `updated_at`,
//!   `bytes`, `title_key`.
//! - `knowledge_chunks`: the unit that is searched, `_id` its id. It carries the document's
//!   `title`, `repo_keys`, `global` and `updated_at` (so a search filters and answers from one
//!   collection), the folded words of its text (`terms`) and of the title (`title_terms`) for the
//!   list filter, and the `embedding` (an array of doubles) with its `model_id` once embedded.
//!   Indexes: unique `(doc_id, ordinal)`, `(chunk_sha, model_id)`, `terms`, `title_terms`.
//! - Search indexes on the chunks, built by `mongot` ([`KEYWORD_INDEX`] and [`VECTOR_INDEX`]).

use std::thread;
use std::time::{Duration, Instant};

use mongodb::IndexModel;
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::IndexOptions;
use mongodb::sync::Database;

use super::connection::{INDEX_EXISTS, NAMESPACE_EXISTS, NAMESPACE_NOT_FOUND, error_code};
use crate::store::DIMS;

/// The schema version this server writes.
pub const SCHEMA_VERSION: i64 = 1;

pub(super) const META: &str = "knowledge_meta";
pub(super) const DOCUMENTS: &str = "knowledge_documents";
pub(super) const CHUNKS: &str = "knowledge_chunks";

/// The `_id` of the schema document in [`META`]; its `version` field holds the version.
pub(super) const SCHEMA_ID: &str = "schema";

/// The `$search` index over the title, heading path and text of the chunks.
pub const KEYWORD_INDEX: &str = "knowledge_text";

/// The `$vectorSearch` index over the embeddings of the chunks.
pub const VECTOR_INDEX: &str = "knowledge_vectors";

/// How long a waiting call looks for the search indexes to be ready.
pub const INDEX_WAIT: Duration = Duration::from_secs(30);

/// Pause between looks at the state of the search indexes.
const INDEX_POLL: Duration = Duration::from_millis(250);

/// How far a search index has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    /// There is no such index.
    Missing,
    /// The index exists and is not queryable yet (or any more): `PENDING`, `BUILDING`, `FAILED`.
    Building,
    /// Queries are answered.
    Ready,
}

/// The stored schema version (0 for a new database).
pub fn version(db: &Database) -> mongodb::error::Result<i64> {
    let found = db
        .collection::<Document>(META)
        .find_one(doc! { "_id": SCHEMA_ID })
        .run()?;

    Ok(found.as_ref().map_or(0, |schema| super::int(schema, "version")))
}

/// Brings the database to [`SCHEMA_VERSION`] and returns whether it is newer than this server
/// (then nothing is changed). The search indexes are created when missing whatever the version, so
/// a dropped index comes back with the next writer.
pub(super) fn migrate(db: &Database) -> mongodb::error::Result<bool> {
    let current = version(db)?;

    if current > SCHEMA_VERSION {
        return Ok(true);
    }

    if current < 1 {
        // A server without `mongot` fails here, before anything is created.
        search_states(db)?;
        create_v1(db)?;
    }

    ensure_search_indexes(db)?;

    if current < SCHEMA_VERSION {
        let meta = db.collection::<Document>(META);

        meta.update_one(
            doc! { "_id": "generation" },
            doc! { "$setOnInsert": { "value": 0_i64 } },
        )
        .upsert(true)
        .run()?;

        meta.update_one(
            doc! { "_id": SCHEMA_ID },
            doc! { "$max": { "version": SCHEMA_VERSION }, "$setOnInsert": { "dims": i64::try_from(DIMS).unwrap_or(0) } },
        )
        .upsert(true)
        .run()?;
    }

    Ok(false)
}

/// Version 1: the collections and their regular indexes.
fn create_v1(db: &Database) -> mongodb::error::Result<()> {
    db.collection::<Document>(DOCUMENTS)
        .create_indexes([
            index(doc! { "source_key": 1 }, "documents_source", false),
            index(doc! { "content_sha256": 1 }, "documents_content", false),
            index(doc! { "repo_keys": 1 }, "documents_repos", false),
            index(doc! { "global": 1 }, "documents_global", false),
            index(doc! { "updated_at": 1 }, "documents_updated", false),
            index(doc! { "bytes": 1 }, "documents_bytes", false),
            index(doc! { "title_key": 1 }, "documents_title", false),
        ])
        .run()?;

    db.collection::<Document>(CHUNKS)
        .create_indexes([
            index(doc! { "doc_id": 1, "ordinal": 1 }, "chunks_position", true),
            index(doc! { "chunk_sha": 1, "model_id": 1 }, "chunks_sha", false),
            index(doc! { "terms": 1 }, "chunks_terms", false),
            index(doc! { "title_terms": 1 }, "chunks_title_terms", false),
        ])
        .run()?;

    Ok(())
}

fn index(keys: Document, name: &str, unique: bool) -> IndexModel {
    IndexModel::builder()
        .keys(keys)
        .options(IndexOptions::builder().name(name.to_owned()).unique(unique).build())
        .build()
}

/// The definition of [`KEYWORD_INDEX`]: the three text fields analysed with ICU folding (case and
/// accents do not matter, like the local database's `unicode61 remove_diacritics 2`), and the
/// scope fields a query filters on.
pub(super) fn keyword_definition() -> Document {
    let text = || doc! { "type": "string", "analyzer": "knowledge_folded" };

    doc! {
        "analyzers": [{
            "name": "knowledge_folded",
            "tokenizer": { "type": "standard" },
            "tokenFilters": [{ "type": "icuFolding" }],
        }],
        "mappings": {
            "dynamic": false,
            "fields": {
                "title": text(),
                "heading_path": text(),
                "text": text(),
                "repo_keys": { "type": "token" },
                "global": { "type": "boolean" },
                "doc_id": { "type": "number" },
            },
        },
    }
}

/// The definition of [`VECTOR_INDEX`]: [`DIMS`] cosine dimensions, filtered by scope and model.
pub(super) fn vector_definition() -> Document {
    doc! {
        "fields": [
            { "type": "vector", "path": "embedding", "numDimensions": i64::try_from(DIMS).unwrap_or(0), "similarity": "cosine" },
            { "type": "filter", "path": "repo_keys" },
            { "type": "filter", "path": "global" },
            { "type": "filter", "path": "model_id" },
        ],
    }
}

/// The state of each search index by name, from `$listSearchIndexes`. On a server without
/// `mongot` this is the error 31082 ([`super::connection::SEARCH_NOT_ENABLED`]). A collection
/// that does not exist has none.
pub(super) fn search_states(db: &Database) -> mongodb::error::Result<Vec<(String, IndexState)>> {
    let listed = db
        .collection::<Document>(CHUNKS)
        .aggregate([doc! { "$listSearchIndexes": {} }])
        .run()
        .and_then(|cursor| cursor.collect::<Result<Vec<_>, _>>());

    let listed = match listed {
        Ok(listed) => listed,
        // Atlas Local answers an empty list for a collection nobody created, Atlas may not.
        Err(error) if error_code(&error) == Some(NAMESPACE_NOT_FOUND) => Vec::new(),
        Err(error) => return Err(error),
    };

    Ok(listed
        .iter()
        .filter_map(|index| {
            let name = index.get_str("name").ok()?.to_owned();
            let ready = index.get_bool("queryable").unwrap_or(false);

            Some((name, if ready { IndexState::Ready } else { IndexState::Building }))
        })
        .collect())
}

/// The state of the index `name` among `states`.
pub(super) fn state_of(states: &[(String, IndexState)], name: &str) -> IndexState {
    states
        .iter()
        .find(|(known, _)| known == name)
        .map_or(IndexState::Missing, |(_, state)| *state)
}

/// Creates the search indexes that are missing (and the collection, which Atlas wants first).
fn ensure_search_indexes(db: &Database) -> mongodb::error::Result<()> {
    match db.create_collection(CHUNKS).run() {
        Err(error) if error_code(&error) == Some(NAMESPACE_EXISTS) => {}
        other => other?,
    }

    let states = search_states(db)?;
    let wanted = [
        (KEYWORD_INDEX, "search", keyword_definition()),
        (VECTOR_INDEX, "vectorSearch", vector_definition()),
    ];

    let missing: Vec<Bson> = wanted
        .into_iter()
        .filter(|(name, _, _)| state_of(&states, name) == IndexState::Missing)
        .map(|(name, kind, definition)| Bson::Document(doc! { "name": name, "type": kind, "definition": definition }))
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let created = db
        .run_command(doc! { "createSearchIndexes": CHUNKS, "indexes": missing })
        .run();

    // Another server created it between the listing and the command.
    match created {
        Err(error) if error_code(&error) == Some(INDEX_EXISTS) => Ok(()),
        other => other.map(drop),
    }
}

/// Waits until both search indexes are queryable, at most `wait`; the state they ended in.
pub(super) fn wait_for_indexes(db: &Database, wait: Duration) -> mongodb::error::Result<(IndexState, IndexState)> {
    let deadline = Instant::now() + wait;

    loop {
        let states = search_states(db)?;
        let keyword = state_of(&states, KEYWORD_INDEX);
        let vector = state_of(&states, VECTOR_INDEX);
        let ready = keyword == IndexState::Ready && vector == IndexState::Ready;

        if ready || Instant::now() >= deadline {
            return Ok((keyword, vector));
        }

        thread::sleep(INDEX_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyword_index_maps_the_three_text_fields_and_the_scope_fields() {
        let definition = keyword_definition();
        let fields = definition
            .get_document("mappings")
            .and_then(|mappings| mappings.get_document("fields"))
            .expect("fields");

        for text in ["title", "heading_path", "text"] {
            let field = fields.get_document(text).expect(text);
            assert_eq!(field.get_str("type"), Ok("string"), "{text}");
            assert_eq!(field.get_str("analyzer"), Ok("knowledge_folded"), "{text}");
        }

        assert_eq!(
            fields.get_document("repo_keys").expect("repos").get_str("type"),
            Ok("token")
        );
        assert_eq!(
            fields.get_document("global").expect("global").get_str("type"),
            Ok("boolean")
        );
        assert_eq!(
            fields.get_document("doc_id").expect("doc").get_str("type"),
            Ok("number")
        );
        assert_eq!(
            definition
                .get_document("mappings")
                .expect("mappings")
                .get_bool("dynamic"),
            Ok(false)
        );
    }

    #[test]
    fn the_vector_index_has_384_cosine_dimensions_and_the_filter_fields() {
        let definition = vector_definition();
        let fields = definition.get_array("fields").expect("fields");
        let documents: Vec<&Document> = fields.iter().filter_map(Bson::as_document).collect();

        let vector = documents
            .iter()
            .find(|field| field.get_str("type") == Ok("vector"))
            .expect("the vector field");
        assert_eq!(vector.get_str("path"), Ok("embedding"));
        assert_eq!(vector.get_i64("numDimensions"), Ok(384));
        assert_eq!(vector.get_str("similarity"), Ok("cosine"));

        let mut filters: Vec<&str> = documents
            .iter()
            .filter(|field| field.get_str("type") == Ok("filter"))
            .filter_map(|field| field.get_str("path").ok())
            .collect();
        filters.sort_unstable();
        assert_eq!(filters, ["global", "model_id", "repo_keys"]);
    }

    #[test]
    fn a_missing_index_is_told_from_a_building_and_a_ready_one() {
        let states = vec![
            ("a".to_owned(), IndexState::Ready),
            ("b".to_owned(), IndexState::Building),
        ];

        assert_eq!(state_of(&states, "a"), IndexState::Ready);
        assert_eq!(state_of(&states, "b"), IndexState::Building);
        assert_eq!(state_of(&states, "c"), IndexState::Missing);
    }
}
