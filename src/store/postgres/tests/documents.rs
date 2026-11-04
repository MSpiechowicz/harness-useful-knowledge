use pretty_assertions::assert_eq;

use super::support::{database, insert};
use crate::store::testing::{MODEL, Seed, axis};
use crate::store::{Detail, KnowledgeStore, Link, Scope, StoreError};

fn link(repo: &str, globs: &[&str]) -> Link {
    Link {
        repo_key: repo.to_owned(),
        globs: globs.iter().map(|glob| (*glob).to_owned()).collect(),
    }
}

fn keyword_ids(store: &mut impl KnowledgeStore, query: &str) -> Vec<i64> {
    let hits = store.keyword_top_k(query, &Scope::everything(), 50).expect("keyword");
    hits.iter().map(|hit| hit.chunk_id).collect()
}

#[test]
fn team_a_document_comes_back_with_its_chunks_links_and_counts() {
    let Some(db) = database() else { return };
    let mut store = db.open();
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
fn team_a_chunk_is_read_whole_only_from_its_own_document() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let first = insert(&mut store, &Seed::new("First").chunk("A", "alpha text"));
    let second = insert(&mut store, &Seed::new("Second").chunk("B", "beta text"));

    let sections = store
        .get(
            first,
            &Detail {
                sections: true,
                chunk: None,
                drafts: false,
            },
        )
        .expect("get")
        .expect("document")
        .sections
        .expect("sections");
    let chunk_id = sections[0].chunk_id;

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
    assert!(store.get(999_999, &Detail::default()).expect("get").is_none());
}

#[test]
fn team_putting_a_stored_document_replaces_its_chunks_and_links_and_keeps_its_creation_time() {
    let Some(db) = database() else { return };
    let mut store = db.open();
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

    assert!(
        keyword_ids(&mut store, "stale").is_empty(),
        "the old chunk left the index"
    );
    assert_eq!(keyword_ids(&mut store, "fresh").len(), 1);
    assert_eq!(store.stats().expect("stats").chunks, 1);

    doc.id = 4_242_424;
    assert!(matches!(
        store.put_document(&doc, &chunks, &links),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn team_a_rejected_put_stores_nothing() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let (doc, mut chunks, links) = Seed::new("Guide").chunk("A", "one").chunk("B", "two").parts();
    chunks[1].ordinal = chunks[0].ordinal;

    assert!(matches!(
        store.put_document(&doc, &chunks, &links),
        Err(StoreError::Invalid(_))
    ));

    let too_long = crate::store::Document {
        title: "x".repeat(301),
        ..doc.clone()
    };
    assert!(store.put_document(&too_long, &[], &[]).is_err());

    let bad_link = [link("not-a-repo", &[])];
    assert!(store.put_document(&doc, &[], &bad_link).is_err());

    let stats = store.stats().expect("stats");
    assert_eq!((stats.documents, stats.chunks, stats.generation), (0, 0, 0));
}

#[test]
fn team_a_put_the_database_refuses_midway_rolls_back_whole() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let keep = insert(&mut store, &Seed::new("Keeper").chunk("A", "kept words"));

    // The second chunk has more distinct words than a `tsvector` can hold (1 MB), which the
    // database refuses after the document row and the first chunk went in.
    let words: Vec<String> = (0..200_000).map(|n| format!("w{n}")).collect();
    let (doc, chunks, links) = Seed::new("Too much")
        .chunk("A", "fine")
        .chunk("B", &words.join(" "))
        .parts();

    match store.put_document(&doc, &chunks, &links) {
        Err(StoreError::Backend(message)) => assert!(message.contains("SQLSTATE 54000"), "{message}"),
        other => panic!("expected the database to refuse the text: {other:?}"),
    }

    // The same document replacing a stored one leaves that one as it was.
    let (mut replacing, _, _) = Seed::new("Too much").parts();
    replacing.id = keep;
    assert!(store.put_document(&replacing, &chunks, &links).is_err());

    let stats = store.stats().expect("stats");
    assert_eq!((stats.documents, stats.chunks, stats.generation), (1, 1, 1));
    assert_eq!(keyword_ids(&mut store, "kept").len(), 1);
    assert_eq!(
        store
            .get(keep, &Detail::default())
            .expect("get")
            .expect("document")
            .doc
            .title,
        "Keeper"
    );
}

#[test]
fn team_documents_are_found_by_content_hash_and_by_source() {
    let Some(db) = database() else { return };
    let mut store = db.open();
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
fn team_stored_embeddings_are_reused_by_chunk_hash_and_model() {
    let Some(db) = database() else { return };
    let mut store = db.open();
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
fn team_setting_links_replaces_them_and_merges_repeats() {
    let Some(db) = database() else { return };
    let mut store = db.open();
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

    assert!(!store.set_links(999_999, &[]).expect("missing"));
    assert!(matches!(
        store.set_links(id, &[link("github.com/o/r", &[])]),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn team_renaming_changes_what_the_keyword_index_finds_and_keeps_who_wrote_it() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let id = insert(
        &mut store,
        &Seed::new("Alpha manual").updated(100).chunk("A", "plain body"),
    );
    assert_eq!(keyword_ids(&mut store, "alpha").len(), 1, "the title is indexed");

    assert!(store.rename(id, "  Zulu\nhandbook ", Some("Zed"), 500).expect("rename"));

    assert!(keyword_ids(&mut store, "alpha").is_empty());
    assert_eq!(keyword_ids(&mut store, "zulu").len(), 1);

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

    assert!(!store.rename(999_999, "Anything", None, 0).expect("missing"));
    assert!(matches!(store.rename(id, " ", None, 0), Err(StoreError::Invalid(_))));
}

#[test]
fn team_deleting_cascades_to_chunks_links_and_the_keyword_index() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let keep = insert(&mut store, &Seed::new("Keeper").chunk("A", "shared word keeper"));
    let gone = insert(
        &mut store,
        &Seed::new("Goner")
            .chunk("A", "shared word goner")
            .embedded("B", "other goner", axis(4))
            .link("git:h/o/r", &["x/**"]),
    );

    assert_eq!(store.delete(&[gone, 999_999]).expect("delete"), 1);

    let stats = store.stats().expect("stats");
    assert_eq!((stats.documents, stats.chunks, stats.embedded_chunks), (1, 1, 0));
    assert!(store.repos().expect("repos").repos.is_empty());
    assert!(store.get(gone, &Detail::default()).expect("get").is_none());

    let ids = keyword_ids(&mut store, "shared");
    assert_eq!(ids.len(), 1, "only the kept document is found");
    assert!(keyword_ids(&mut store, "goner").is_empty());
    assert!(
        store
            .vector_top_k(&axis(4), &Scope::everything(), 10)
            .expect("vector")
            .is_empty()
    );

    // The cascade left no row of the deleted document in any table.
    let mut admin = db.admin();
    for table in ["knowledge_chunks", "knowledge_doc_links"] {
        let rows: i64 = admin
            .query_one(&format!("SELECT count(*) FROM {table} WHERE doc_id = $1"), &[&gone])
            .expect("count")
            .get(0);
        assert_eq!(rows, 0, "{table}");
    }

    assert_eq!(store.delete(&[keep]).expect("delete"), 1);
    assert_eq!(store.stats().expect("stats").chunks, 0);
    assert_eq!(store.delete(&[]).expect("delete"), 0);
    assert!(matches!(
        store.delete(&vec![1; crate::limits::DELETE_IDS_MAX + 1]),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn team_every_write_grows_the_generation_and_reads_do_not() {
    let Some(db) = database() else { return };
    let mut store = db.open();
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
}

#[test]
fn team_the_meta_keys_the_store_owns_cannot_be_set() {
    let Some(db) = database() else { return };
    let mut store = db.open();

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
}

#[test]
fn team_the_database_itself_refuses_an_embedding_of_the_wrong_size_or_without_a_model() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let id = insert(&mut store, &Seed::new("Guide"));
    let mut admin = db.admin();
    let zeros = format!("[{}]", vec!["0"; 384].join(","));

    let mut insert_chunk = |embedding: Option<&str>, model: Option<&str>| {
        admin.execute(
            "INSERT INTO knowledge_chunks (doc_id, ordinal, title, heading_path, text, chunk_sha, tokens,
                 char_start, char_end, embedding, model_id)
             VALUES ($1, (SELECT coalesce(max(ordinal), 0) + 1 FROM knowledge_chunks), 't', '', 'x', 's', 1, 0, 1,
                     $2::text::vector, $3)",
            &[&id, &embedding, &model],
        )
    };

    assert!(insert_chunk(Some("[0,0,0]"), Some("m")).is_err(), "too short");
    assert!(insert_chunk(Some(&zeros), None).is_err(), "no model");
    assert!(insert_chunk(None, Some("m")).is_err(), "a model without a vector");
    assert!(insert_chunk(Some(&zeros), Some("m")).is_ok());
    assert!(insert_chunk(None, None).is_ok());
}

#[test]
fn team_text_with_a_nul_character_is_stored_without_it() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let id = insert(
        &mut store,
        &Seed::new("Nul guide").chunk("He\0ading", "pay\0load words"),
    );

    let sections = store
        .get(
            id,
            &Detail {
                sections: true,
                chunk: None,
                drafts: false,
            },
        )
        .expect("get")
        .expect("document")
        .sections
        .expect("sections");

    assert_eq!(sections[0].heading_path, "Heading");
    assert_eq!(keyword_ids(&mut store, "payload").len(), 1);
}

#[test]
fn team_a_document_of_many_chunks_goes_in_whole_with_its_vectors() {
    let Some(db) = database() else { return };
    let mut store = db.open();
    let mut seed = Seed::new("Big manual");

    // More than one insert batch, half of the chunks embedded.
    for n in 0..230_u64 {
        let text = format!("section number {n} of the manual");

        seed = if n % 2 == 0 {
            seed.embedded(&format!("Part {n}"), &text, crate::store::testing::fake_vector(n + 1))
        } else {
            seed.chunk(&format!("Part {n}"), &text)
        };
    }

    let id = insert(&mut store, &seed);
    let stats = store.stats().expect("stats");
    assert_eq!((stats.chunks, stats.embedded_chunks), (230, 115));

    let sections = store
        .get(
            id,
            &Detail {
                sections: true,
                chunk: None,
                drafts: false,
            },
        )
        .expect("get")
        .expect("document")
        .sections
        .expect("sections");
    let ordinals: Vec<usize> = sections.iter().map(|section| section.ordinal).collect();
    assert_eq!(ordinals, (0..230).collect::<Vec<_>>(), "in order");

    let hits = store
        .vector_top_k(&crate::store::testing::fake_vector(101), &Scope::everything(), 1)
        .expect("vector");
    assert_eq!(hits[0].heading_path, "Part 100");
}
