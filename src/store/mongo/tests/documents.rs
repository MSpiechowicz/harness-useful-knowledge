use mongodb::bson::doc;
use pretty_assertions::assert_eq;

use super::{chunk_count, eventually, insert, sections, store};
use crate::store::mongo::schema::{CHUNKS, DOCUMENTS};
use crate::store::testing::{MODEL, Seed, axis};
use crate::store::{Detail, Document, KnowledgeStore, Link, Scope, StoreError};

fn link(repo: &str, globs: &[&str]) -> Link {
    Link {
        repo_key: repo.to_owned(),
        globs: globs.iter().map(|glob| (*glob).to_owned()).collect(),
    }
}

fn keyword_ids(store: &mut impl KnowledgeStore, query: &str) -> Result<Vec<i64>, StoreError> {
    let hits = store.keyword_top_k(query, &Scope::everything(), 50)?;
    Ok(hits.iter().map(|hit| hit.chunk_id).collect())
}

#[test]
fn team_mongo_a_document_comes_back_with_its_chunks_links_and_counts() {
    let Some((_db, mut store)) = store() else { return };
    let seed = Seed::new("Gateway guide")
        .chunk("Intro", "How the gateway routes requests")
        .chunk("Intro > Limits", "Rate limits per key")
        .link("git:github.com/o/api", &["src/gateway/**"]);
    let id = insert(&mut store, &seed);

    let detail = store
        .get(
            id,
            &Detail {
                sections: true,
                chunk: None,
                drafts: false,
            },
        )
        .expect("get")
        .expect("document");

    assert_eq!(detail.doc.title, "Gateway guide");
    assert_eq!(detail.doc.chunk_count, 2);
    assert_eq!(detail.links, [link("git:github.com/o/api", &["src/gateway/**"])]);

    let sections = detail.sections.expect("sections");
    let headings: Vec<&str> = sections.iter().map(|section| section.heading_path.as_str()).collect();
    assert_eq!(headings, ["Intro", "Intro > Limits"]);
    assert_eq!(sections[1].ordinal, 1);
    assert!(detail.chunk.is_none());
}

#[test]
fn team_mongo_ids_grow_and_are_never_reused() {
    let Some((_db, mut store)) = store() else { return };
    let first = insert(&mut store, &Seed::new("First").chunk("A", "one").chunk("B", "two"));
    let second = insert(&mut store, &Seed::new("Second").chunk("A", "three"));

    assert!(second > first, "{first} then {second}");

    store.delete(&[second]).expect("delete");
    let third = insert(&mut store, &Seed::new("Third").chunk("A", "four"));
    assert!(third > second, "a deleted id is not reused: {second} then {third}");

    let first_chunks: Vec<i64> = sections(&mut store, first)
        .iter()
        .map(|section| section.chunk_id)
        .collect();
    assert_eq!(first_chunks.len(), 2);
    assert!(first_chunks[1] > first_chunks[0]);
}

#[test]
fn team_mongo_a_chunk_is_read_whole_only_from_its_own_document() {
    let Some((_db, mut store)) = store() else { return };
    let first = insert(&mut store, &Seed::new("First").chunk("A", "alpha text"));
    let second = insert(&mut store, &Seed::new("Second").chunk("B", "beta text"));

    let chunk_id = sections(&mut store, first)[0].chunk_id;

    let detail = store
        .get(
            first,
            &Detail {
                sections: false,
                chunk: Some(chunk_id),
                drafts: false,
            },
        )
        .expect("get")
        .expect("document");
    let chunk = detail.chunk.expect("chunk");
    assert_eq!((chunk.text.as_str(), chunk.heading_path.as_str()), ("alpha text", "A"));
    assert!(chunk.embedding.is_none(), "the vector stays in the database");

    let wrong = store.get(
        second,
        &Detail {
            sections: false,
            chunk: Some(chunk_id),
            drafts: false,
        },
    );
    assert!(matches!(wrong, Err(StoreError::NotFound(_))));
    assert!(store.get(999, &Detail::default()).expect("get").is_none());
}

#[test]
fn team_mongo_putting_a_stored_document_replaces_its_chunks_and_links_and_keeps_its_creation_time() {
    let Some((db, mut store)) = store() else { return };
    let id = insert(
        &mut store,
        &Seed::new("Guide")
            .updated(100)
            .chunk("Old", "stale words")
            .link("git:h/o/a", &[]),
    );

    let (mut doc, chunks, links) = Seed::new("Guide v2")
        .updated(200)
        .chunk("New", "fresh words")
        .link("git:h/o/b", &["docs/**"])
        .parts();
    doc.id = id;
    doc.created_at = 5;
    assert_eq!(store.put_document(&doc, &chunks, &links).expect("update"), id);

    let detail = store.get(id, &Detail::default()).expect("get").expect("document");
    assert_eq!((detail.doc.created_at, detail.doc.updated_at), (100, 200));
    assert_eq!(detail.doc.title, "Guide v2");
    assert_eq!(detail.links, [link("git:h/o/b", &["docs/**"])]);
    assert_eq!(store.stats().expect("stats").chunks, 1);
    assert_eq!(chunk_count(&db), 1);

    // The old chunk left the index (its document is gone, so the answer is empty at once), and the
    // new one joins it a moment later.
    assert!(keyword_ids(&mut store, "stale").expect("keyword").is_empty());
    let found = eventually(
        "the new chunk is indexed",
        || keyword_ids(&mut store, "fresh"),
        |ids| ids.len() == 1,
    );
    assert_eq!(found.len(), 1);

    doc.id = 4242;
    assert!(matches!(
        store.put_document(&doc, &chunks, &links),
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(
        db.count(DOCUMENTS, doc! {}),
        1,
        "a replace of a missing id stored nothing"
    );
}

#[test]
fn team_mongo_a_rejected_put_stores_nothing() {
    let Some((db, mut store)) = store() else { return };
    let (doc, mut chunks, links) = Seed::new("Guide").chunk("A", "one").chunk("B", "two").parts();
    chunks[1].ordinal = chunks[0].ordinal;

    assert!(matches!(
        store.put_document(&doc, &chunks, &links),
        Err(StoreError::Invalid(_))
    ));

    let too_long = Document {
        title: "x".repeat(301),
        ..doc.clone()
    };
    assert!(store.put_document(&too_long, &[], &[]).is_err());

    let bad_link = [link("not-a-repo", &[])];
    assert!(store.put_document(&doc, &[], &bad_link).is_err());

    let (doc, mut chunks, links) = Seed::new("Guide").embedded("A", "one", axis(0)).parts();
    chunks[0].embedding = Some(vec![0.5; 12]);
    assert!(matches!(
        store.put_document(&doc, &chunks, &links),
        Err(StoreError::Invalid(_))
    ));
    chunks[0].embedding = Some(axis(0));
    chunks[0].model_id = None;
    assert!(
        store.put_document(&doc, &chunks, &links).is_err(),
        "a vector comes with its model"
    );

    let stats = store.stats().expect("stats");
    assert_eq!((stats.documents, stats.chunks, stats.generation), (0, 0, 0));
    assert_eq!((db.count(DOCUMENTS, doc! {}), db.count(CHUNKS, doc! {})), (0, 0));
}

#[test]
fn team_mongo_documents_are_found_by_content_hash_and_by_source() {
    let Some((_db, mut store)) = store() else { return };
    let id = insert(
        &mut store,
        &Seed::new("Guide").source("/docs/guide.md").chunk("A", "text"),
    );
    insert(
        &mut store,
        &Seed::new("Other").source("/docs/other.md").chunk("A", "more"),
    );

    let by_source = store
        .find_by_source_key("/docs/guide.md", crate::store::Visibility::Live)
        .expect("find")
        .expect("found");
    assert_eq!(by_source.id, id);

    let by_content = store
        .find_by_content_sha(&by_source.content_sha256, crate::store::Visibility::Live)
        .expect("find")
        .expect("found");
    assert_eq!(by_content.id, id);

    assert!(
        store
            .find_by_source_key("/docs/missing.md", crate::store::Visibility::Live)
            .expect("find")
            .is_none()
    );
    assert!(
        store
            .find_by_content_sha("nope", crate::store::Visibility::Live)
            .expect("find")
            .is_none()
    );
}

#[test]
fn team_mongo_the_oldest_of_two_documents_with_one_source_is_found() {
    let Some((_db, mut store)) = store() else { return };
    let older = insert(
        &mut store,
        &Seed::new("Older").source("/docs/same.md").chunk("A", "one"),
    );
    insert(
        &mut store,
        &Seed::new("Newer").source("/docs/same.md").chunk("A", "two"),
    );

    let found = store
        .find_by_source_key("/docs/same.md", crate::store::Visibility::Live)
        .expect("find")
        .expect("found");
    assert_eq!(found.id, older);
}

#[test]
fn team_mongo_stored_embeddings_are_reused_by_chunk_hash_and_model() {
    let Some((_db, mut store)) = store() else { return };
    insert(
        &mut store,
        &Seed::new("Guide")
            .embedded("A", "first text", axis(2))
            .embedded("B", "second text", axis(3))
            .chunk("C", "third text"),
    );

    let shas: Vec<String> = ["sha-first text", "sha-third text", "sha-unknown"]
        .iter()
        .map(|sha| (*sha).to_owned())
        .collect();

    let found = store.reusable_embeddings(MODEL, &shas).expect("reuse");
    assert_eq!(
        found.len(),
        1,
        "an unembedded and an unknown chunk have nothing to reuse"
    );
    assert_eq!(found["sha-first text"], axis(2));

    assert!(
        store
            .reusable_embeddings("another-model", &shas)
            .expect("reuse")
            .is_empty()
    );
    assert!(store.reusable_embeddings(MODEL, &[]).expect("reuse").is_empty());
}

#[test]
fn team_mongo_setting_links_replaces_them_and_merges_repeats() {
    let Some((_db, mut store)) = store() else { return };
    let id = insert(&mut store, &Seed::new("Guide").chunk("A", "x").link("git:h/o/a", &[]));

    let applied = store
        .set_links(id, &[link("git:h/o/b", &["a/**"]), link("git:h/o/b", &["b/**"])])
        .expect("set");
    assert!(applied);

    let links = store.get(id, &Detail::default()).expect("get").expect("document").links;
    assert_eq!(links, [link("git:h/o/b", &["a/**", "b/**"])]);

    assert!(store.set_links(id, &[]).expect("clear"));
    assert!(
        store
            .get(id, &Detail::default())
            .expect("get")
            .expect("document")
            .links
            .is_empty()
    );

    assert!(!store.set_links(999, &[]).expect("missing"));
    assert!(matches!(
        store.set_links(id, &[link("github.com/o/r", &[])]),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn team_mongo_relinking_moves_the_chunks_between_scopes() {
    let Some((db, mut store)) = store() else { return };
    let id = insert(&mut store, &Seed::new("Guide").chunk("A", "x").chunk("B", "y"));
    assert_eq!(db.count(CHUNKS, doc! { "global": true }), 2);

    store.set_links(id, &[link("git:h/o/a", &[])]).expect("link");
    assert_eq!(db.count(CHUNKS, doc! { "global": false, "repo_keys": "git:h/o/a" }), 2);

    store.set_links(id, &[]).expect("unlink");
    assert_eq!(db.count(CHUNKS, doc! { "global": true, "repo_keys": [] }), 2);
}

#[test]
fn team_mongo_renaming_changes_what_the_keyword_index_finds_and_keeps_who_wrote_it() {
    let Some((_db, mut store)) = store() else { return };
    let id = insert(
        &mut store,
        &Seed::new("Alpha manual").updated(100).chunk("A", "plain body"),
    );
    eventually(
        "the title is indexed",
        || keyword_ids(&mut store, "alpha"),
        |ids| ids.len() == 1,
    );

    assert!(store.rename(id, "  Zulu\nhandbook ", Some("Zed"), 500).expect("rename"));

    eventually(
        "the new title is indexed",
        || keyword_ids(&mut store, "zulu"),
        |ids| ids.len() == 1,
    );
    eventually(
        "the old title is gone",
        || keyword_ids(&mut store, "alpha"),
        Vec::is_empty,
    );

    let doc = store.get(id, &Detail::default()).expect("get").expect("document").doc;
    assert_eq!(
        (
            doc.title.as_str(),
            doc.updated_at,
            doc.updated_by.map(|author| author.name)
        ),
        ("Zulu handbook", 500, None),
        "a rename is a change of the title and its time, not of who wrote the content"
    );

    assert!(!store.rename(999, "Anything", None, 0).expect("missing"));
    assert!(matches!(store.rename(id, " ", None, 0), Err(StoreError::Invalid(_))));
}

#[test]
fn team_mongo_deleting_cascades_to_chunks_links_and_the_search_indexes() {
    let Some((db, mut store)) = store() else { return };
    let keep = insert(&mut store, &Seed::new("Keeper").chunk("A", "shared word keeper"));
    let gone = insert(
        &mut store,
        &Seed::new("Goner")
            .chunk("A", "shared word goner")
            .embedded("B", "other goner", axis(4))
            .link("git:h/o/r", &["x/**"]),
    );
    eventually(
        "both are indexed",
        || keyword_ids(&mut store, "shared"),
        |ids| ids.len() == 2,
    );
    eventually(
        "the vector is indexed",
        || store.vector_top_k(&axis(4), &Scope::everything(), 10),
        |hits| hits.len() == 1,
    );

    assert_eq!(store.delete(&[gone, 999]).expect("delete"), 1);

    let stats = store.stats().expect("stats");
    assert_eq!((stats.documents, stats.chunks, stats.embedded_chunks), (1, 1, 0));
    assert!(store.repos().expect("repos").repos.is_empty());
    assert!(store.get(gone, &Detail::default()).expect("get").is_none());
    assert_eq!(db.count(CHUNKS, doc! { "doc_id": gone }), 0);

    // The index may still list the deleted chunks for a moment, but a hit is read from the
    // collection, so a deleted chunk is never shown.
    let ids = keyword_ids(&mut store, "shared").expect("keyword");
    assert_eq!(ids.len(), 1, "only the kept document is found");
    assert!(keyword_ids(&mut store, "goner").expect("keyword").is_empty());
    assert!(
        store
            .vector_top_k(&axis(4), &Scope::everything(), 10)
            .expect("vector")
            .is_empty()
    );

    assert_eq!(store.delete(&[keep]).expect("delete"), 1);
    assert_eq!(store.stats().expect("stats").chunks, 0);
}

#[test]
fn team_mongo_too_many_ids_are_refused_by_a_delete() {
    let Some((_db, mut store)) = store() else { return };
    let ids: Vec<i64> = (1..=1001).collect();

    assert!(matches!(store.delete(&ids), Err(StoreError::Invalid(_))));
}

#[test]
fn team_mongo_every_write_grows_the_generation_and_reads_do_not() {
    let Some((_db, mut store)) = store() else { return };
    assert_eq!(store.generation().expect("generation"), 0);

    let id = insert(&mut store, &Seed::new("Guide").chunk("A", "text"));
    assert_eq!(store.generation().expect("generation"), 1);

    store.rename(id, "Guide 2", None, 0).expect("rename");
    store.set_links(id, &[]).expect("links");
    store.set_meta("model_id", MODEL).expect("meta");
    store.delete(&[id]).expect("delete");
    assert_eq!(store.generation().expect("generation"), 5);

    store.list(&super::listing::everything()).expect("list");
    store.stats().expect("stats");
    store.keyword_top_k("guide", &Scope::everything(), 5).expect("keyword");
    assert_eq!(store.generation().expect("generation"), 5);
    assert_eq!(store.meta("generation").expect("meta").as_deref(), Some("5"));
}

#[test]
fn team_mongo_the_meta_keys_the_store_owns_cannot_be_set() {
    let Some((_db, mut store)) = store() else { return };

    for key in ["generation", "schema_version", "dims", ""] {
        assert!(
            matches!(store.set_meta(key, "1"), Err(StoreError::Invalid(_))),
            "{key:?}"
        );
    }

    store.set_meta("model_id", MODEL).expect("model");
    assert_eq!(store.meta("model_id").expect("meta").as_deref(), Some(MODEL));
    assert_eq!(store.meta("missing").expect("meta"), None);
    assert_eq!(store.meta("dims").expect("meta").as_deref(), Some("384"));
    assert_eq!(store.meta("schema_version").expect("meta").as_deref(), Some("2"));

    // A key named like an internal document cannot reach a counter or the schema.
    store.set_meta("counter:documents", "oops").expect("a plain key");
    store.set_meta("schema", "oops").expect("a plain key");
    assert_eq!(store.meta("schema_version").expect("meta").as_deref(), Some("2"));
    let id = insert(&mut store, &Seed::new("Guide").chunk("A", "text"));
    assert!(id > 0);
}

#[test]
fn team_mongo_a_document_of_the_most_chunks_with_vectors_is_stored_whole_in_one_write() {
    use crate::limits::CHUNKS_PER_DOC_MAX;
    use crate::store::testing::fake_vector;

    let Some((db, mut store)) = store() else { return };

    let mut seed = Seed::new("Handbook");
    for n in 0..CHUNKS_PER_DOC_MAX {
        seed = seed.embedded(
            &format!("Part {n}"),
            &format!("section {n} of the handbook"),
            fake_vector(n as u64),
        );
    }

    let id = insert(&mut store, &seed);

    assert_eq!(chunk_count(&db), CHUNKS_PER_DOC_MAX as u64);
    assert_eq!(store.stats().expect("stats").embedded_chunks, CHUNKS_PER_DOC_MAX);
    assert_eq!(sections(&mut store, id).len(), CHUNKS_PER_DOC_MAX);
    assert_eq!(
        store
            .get(id, &Detail::default())
            .expect("get")
            .expect("document")
            .doc
            .chunk_count,
        CHUNKS_PER_DOC_MAX
    );
}
