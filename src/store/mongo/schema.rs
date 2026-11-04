//! The collections, their indexes, the two search indexes and the schema version.
//!
//! Version 1 is a set of idempotent steps, so servers that start together need no lock: a
//! collection or index that exists is left as it is, and the schema document is written last
//! (`$max`), which is what [`version`] reads.
//!
//! # Adding a version
//!
//! Later versions are entries of [`UPGRADES`] (entry `n` takes version `n + 1` to `n + 2`). They
//! add collections, indexes and fields, and fill the fields they add from what is stored, never
//! rewriting a field that holds what the person wrote. The rules keep an upgrade of a real-sized
//! database from failing forever:
//!
//! - An upgrade is idempotent, so one that stopped half way is simply run again.
//! - It runs in its own open ([`super::MongoStore::open_checked`] with
//!   [`crate::backend::Upgrade::Run`]), which [`crate::backend`] gives a long bound of its own and
//!   not the probe deadline of an ordinary open.
//! - It fills fields in pages of 500 through [`super::migrate::update_in_pages`], never one request
//!   per document.
//! - A change that only adds leaves a read of the older database working: queries written for the
//!   new version must still answer on the old one, or wait for the writer to upgrade it. A change
//!   that readers of the old version cannot cope with raises [`READABLE_FROM`] to its version, and
//!   a read of a database older than that says it has not been upgraded yet.
//! - Version numbers are released: a version that shipped is never edited.
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
//!   `phrase_text` is the folded words of the heading path and of the text (a newline between them)
//!   that a multi-word list filter matches with a regular expression, on the server.
//!   Indexes: unique `(doc_id, ordinal)`, `(chunk_sha, model_id)`, `terms`, `title_terms`.
//! - Search indexes on the chunks, built by `mongot` ([`KEYWORD_INDEX`] and [`VECTOR_INDEX`]).
//!
//! # Version 2: drafts and their review
//!
//! [`UPGRADES`]`[0]` adds, and fills from what is stored (every document was live):
//!
//! - `knowledge_documents`: `status` (`live`, `draft`, `waiting` or `changesRequested`),
//!   `review_rev` (raised by every content change and transition) and, on a pending version only,
//!   `pending_of` (the live document it replaces; the field is absent on every other row). Indexes:
//!   `status`, and `pending_of` unique among the rows that have one (at most one pending version
//!   per live document).
//! - `knowledge_chunks`: `review`, `live` for a live document, `pending` for the chunks of a pending
//!   version and `draft` for every other state (waiting and changes requested count as draft, so a
//!   change between those states rewrites no chunk). A search for an agent filters on `live`.
//! - `knowledge_events`: the history of a document (`doc_id`, `kind`, `by`, `at`, `comment`), `_id`
//!   from a counter so the order is exact. Own collection and not an array in the document: the
//!   total stays exact, a document has no size to outgrow and a promote moves the events by
//!   `doc_id`.
//! - `knowledge_comments`: the comments of a review (`doc_id`, `review_id`, `section`, `occurrence`,
//!   `side`, `line_from`, `line_to`, `body`, `by_name`, `at`, `parent_id`, `resolved_at`, `resolved_by`,
//!   `revision`, and `quote` for a line comment: the JSON of the lines it was written on, see
//!   [`crate::store::QuoteLine`], missing on older comments). A review number comes from a counter shared by all documents.
//! - `knowledge_files`: the original file of a document in parts of at most 8 MiB (`doc_id`,
//!   `part`; the first part also carries `name`, `size` and `sha256`), so a part stays far inside
//!   the 16 MiB limit of a document.
//! - `knowledge_snapshots`: the sections of a document as they were asked (`doc_id`, `revision`,
//!   `sections`), the latest asks only.
//! - The search indexes [`KEYWORD_INDEX`] and [`VECTOR_INDEX`] are new ones (`_2`) that also index
//!   `review`. They are created by name next to the version 1 indexes, which are dropped by the next
//!   writer once the new ones answer queries ([`retire_legacy_indexes`]), so a search is never
//!   without an index while a big collection is indexed again.

use std::thread;
use std::time::{Duration, Instant};

use mongodb::IndexModel;
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::IndexOptions;
use mongodb::sync::Database;

use super::connection::{INDEX_EXISTS, NAMESPACE_EXISTS, NAMESPACE_NOT_FOUND, error_code};
use super::migrate::{PAGE, update_in_pages};
use crate::store::DIMS;

/// What takes a database from one version to the next (see the module docs).
pub(super) type Upgrade = fn(&Database) -> mongodb::error::Result<()>;

/// The upgrades after version 1, in order.
pub(super) const UPGRADES: &[Upgrade] = &[drafts];

/// The schema version this server writes: version 1 and one for every entry of [`UPGRADES`].
pub const SCHEMA_VERSION: i64 = 1 + UPGRADES.len() as i64;

/// The oldest schema version a read works with: an older database is refused to a read until a
/// writer has upgraded it. See the module docs for when to raise it.
///
/// Version 2 is not readable by version 1 code (it would show a draft as a live document), so a
/// read of a version 1 database waits for the upgrade.
pub(super) const READABLE_FROM: i64 = 2;

pub(super) const META: &str = "knowledge_meta";
pub(super) const DOCUMENTS: &str = "knowledge_documents";
pub(super) const CHUNKS: &str = "knowledge_chunks";
pub(super) const EVENTS: &str = "knowledge_events";
pub(super) const COMMENTS: &str = "knowledge_comments";
pub(super) const FILES: &str = "knowledge_files";
pub(super) const SNAPSHOTS: &str = "knowledge_snapshots";

/// The `_id` of the schema document in [`META`]; its `version` field holds the version.
pub(super) const SCHEMA_ID: &str = "schema";

/// The `$search` index over the title, heading path and text of the chunks (version 2: it also
/// knows `review`).
pub const KEYWORD_INDEX: &str = "knowledge_text_2";

/// The `$vectorSearch` index over the embeddings of the chunks (version 2: it also filters on
/// `review`).
pub const VECTOR_INDEX: &str = "knowledge_vectors_2";

/// The version 1 keyword index, dropped once [`KEYWORD_INDEX`] answers queries.
pub(super) const LEGACY_KEYWORD_INDEX: &str = "knowledge_text";

/// The version 1 vector index, dropped once [`VECTOR_INDEX`] answers queries.
pub(super) const LEGACY_VECTOR_INDEX: &str = "knowledge_vectors";

/// The most bytes of a stored file one document of [`FILES`] holds.
pub(super) const FILE_PART_BYTES: usize = 8 * 1024 * 1024;

/// The `review` of a chunk of a live document.
pub(super) const REVIEW_LIVE: &str = "live";

/// The `review` of a chunk of a draft, whatever its state (waiting and changes requested included).
pub(super) const REVIEW_DRAFT: &str = "draft";

/// The `review` of a chunk of a pending version.
pub(super) const REVIEW_PENDING: &str = "pending";

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
    migrate_with(db, UPGRADES)
}

/// [`migrate`] with `upgrades` after version 1.
pub(super) fn migrate_with(db: &Database, upgrades: &[Upgrade]) -> mongodb::error::Result<bool> {
    let target = 1 + i64::try_from(upgrades.len()).unwrap_or(i64::MAX);
    let current = version(db)?;

    if current > target {
        return Ok(true);
    }

    if current < 1 {
        // A server without `mongot` fails here, before anything is created.
        search_states(db)?;
        create_v1(db)?;
    }

    // A new database has nothing to fill, so its upgrades are quick. One that has data records
    // each version as it gets there, so a stopped upgrade resumes from the last.
    let done = usize::try_from(current.max(1) - 1).unwrap_or(0);

    for (index, upgrade) in upgrades.iter().enumerate().skip(done) {
        upgrade(db)?;

        if current >= 1 {
            record_version(db, 2 + i64::try_from(index).unwrap_or(0))?;
        }
    }

    ensure_search_indexes(db)?;

    if current < target {
        db.collection::<Document>(META)
            .update_one(
                doc! { "_id": "generation" },
                doc! { "$setOnInsert": { "value": 0_i64 } },
            )
            .upsert(true)
            .run()?;

        record_version(db, target)?;
    }

    Ok(false)
}

/// Writes `version` as the schema version, never lowering it.
fn record_version(db: &Database, version: i64) -> mongodb::error::Result<()> {
    db.collection::<Document>(META)
        .update_one(
            doc! { "_id": SCHEMA_ID },
            doc! { "$max": { "version": version }, "$setOnInsert": { "dims": i64::try_from(DIMS).unwrap_or(0) } },
        )
        .upsert(true)
        .run()
        .map(drop)
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

/// An index over only the documents that have `field`: unique, so no two documents hold the same value.
fn unique_where_present(field: &str, name: &str) -> IndexModel {
    IndexModel::builder()
        .keys(doc! { field: 1 })
        .options(
            IndexOptions::builder()
                .name(name.to_owned())
                .unique(true)
                .partial_filter_expression(doc! { field: { "$exists": true } })
                .build(),
        )
        .build()
}

/// Creates `collection` unless it exists (a transaction cannot create one on every server).
fn ensure_collection(db: &Database, collection: &str) -> mongodb::error::Result<()> {
    match db.create_collection(collection).run() {
        Err(error) if error_code(&error) == Some(NAMESPACE_EXISTS) => Ok(()),
        other => other,
    }
}

/// Version 2: the review of drafts (see the module docs). The collections and indexes come first and
/// the fields are filled after, in pages of ids, so a stopped run is simply run again.
fn drafts(db: &Database) -> mongodb::error::Result<()> {
    for collection in [EVENTS, COMMENTS, FILES, SNAPSHOTS] {
        ensure_collection(db, collection)?;
    }

    db.collection::<Document>(DOCUMENTS)
        .create_indexes([
            index(doc! { "status": 1 }, "documents_status", false),
            unique_where_present("pending_of", "documents_pending"),
        ])
        .run()?;
    db.collection::<Document>(EVENTS)
        .create_indexes([index(doc! { "doc_id": 1, "_id": 1 }, "events_document", false)])
        .run()?;
    db.collection::<Document>(COMMENTS)
        .create_indexes([index(doc! { "doc_id": 1, "_id": 1 }, "comments_document", false)])
        .run()?;
    db.collection::<Document>(FILES)
        .create_indexes([index(doc! { "doc_id": 1, "part": 1 }, "files_part", true)])
        .run()?;
    db.collection::<Document>(SNAPSHOTS)
        .create_indexes([index(doc! { "doc_id": 1, "revision": 1 }, "snapshots_revision", true)])
        .run()?;

    // Every document stored so far is live.
    fill(
        db,
        DOCUMENTS,
        "status",
        doc! { "$set": { "status": "live", "review_rev": 0_i64 } },
    )?;
    fill(db, CHUNKS, "review", doc! { "$set": { "review": REVIEW_LIVE } })
}

/// Applies `update` to every document of `collection` that lacks `field`, a page of ids at a time
/// (by `_id`, so each page is found by the index and none is read twice).
fn fill(db: &Database, collection: &str, field: &str, update: Document) -> mongodb::error::Result<()> {
    let rows = db.collection::<Document>(collection);
    let mut after = i64::MIN;

    loop {
        let page = rows
            .find(doc! { "_id": { "$gt": after }, field: { "$exists": false } })
            .sort(doc! { "_id": 1 })
            .limit(i64::try_from(PAGE).unwrap_or(500))
            .projection(doc! { "_id": 1 })
            .run()?
            .collect::<Result<Vec<_>, _>>()?;

        let Some(last) = page.last().map(|row| super::int(row, "_id")) else {
            return Ok(());
        };

        // The guard names the field the value stands for, so a row someone filled meanwhile is kept.
        update_in_pages(
            db,
            collection,
            page.iter().map(|row| {
                (
                    doc! { "_id": super::int(row, "_id"), field: { "$exists": false } },
                    update.clone(),
                )
            }),
        )?;

        after = last;
    }
}

/// The definition of [`KEYWORD_INDEX`]: the three text fields analysed with ICU folding (case and
/// accents do not matter, like the local database's `unicode61 remove_diacritics 2`), and the
/// scope and review fields a query filters on.
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
                "review": { "type": "token" },
            },
        },
    }
}

/// The definition of [`VECTOR_INDEX`]: [`DIMS`] cosine dimensions, filtered by scope, review state
/// and model.
pub(super) fn vector_definition() -> Document {
    doc! {
        "fields": [
            { "type": "vector", "path": "embedding", "numDimensions": i64::try_from(DIMS).unwrap_or(0), "similarity": "cosine" },
            { "type": "filter", "path": "repo_keys" },
            { "type": "filter", "path": "global" },
            { "type": "filter", "path": "model_id" },
            { "type": "filter", "path": "review" },
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

/// Creates the search indexes that are missing (and the collection, which Atlas wants first), and
/// retires the version 1 indexes once the new ones answer queries.
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
        return retire_legacy_indexes(db, &states);
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

/// Drops the version 1 search indexes that are still there, but only once both new ones answer
/// queries: until then a search that is built on the old ones keeps its index. A failed drop is
/// tried again by the next writer.
pub(super) fn retire_legacy_indexes(db: &Database, states: &[(String, IndexState)]) -> mongodb::error::Result<()> {
    let ready =
        state_of(states, KEYWORD_INDEX) == IndexState::Ready && state_of(states, VECTOR_INDEX) == IndexState::Ready;

    if !ready {
        return Ok(());
    }

    for legacy in [LEGACY_KEYWORD_INDEX, LEGACY_VECTOR_INDEX] {
        if state_of(states, legacy) != IndexState::Missing {
            let _ = db.run_command(doc! { "dropSearchIndex": CHUNKS, "name": legacy }).run();
        }
    }

    Ok(())
}

/// Waits until both search indexes are queryable, at most `wait`; the state they ended in. Once they
/// are, the version 1 indexes that are left are dropped when `retire` (a writer's connection).
pub(super) fn wait_for_indexes(
    db: &Database,
    wait: Duration,
    retire: bool,
) -> mongodb::error::Result<(IndexState, IndexState)> {
    let deadline = Instant::now() + wait;

    loop {
        let states = search_states(db)?;
        let keyword = state_of(&states, KEYWORD_INDEX);
        let vector = state_of(&states, VECTOR_INDEX);
        let ready = keyword == IndexState::Ready && vector == IndexState::Ready;

        if ready && retire {
            let _ = retire_legacy_indexes(db, &states);
        }

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
            fields.get_document("review").expect("review").get_str("type"),
            Ok("token")
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
        assert_eq!(filters, ["global", "model_id", "repo_keys", "review"]);
    }

    #[test]
    fn the_search_indexes_of_version_two_have_names_of_their_own() {
        assert_eq!(
            (KEYWORD_INDEX, VECTOR_INDEX),
            ("knowledge_text_2", "knowledge_vectors_2")
        );
        assert_ne!(KEYWORD_INDEX, LEGACY_KEYWORD_INDEX);
        assert_ne!(VECTOR_INDEX, LEGACY_VECTOR_INDEX);
        assert_eq!((SCHEMA_VERSION, READABLE_FROM), (2, 2));
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
