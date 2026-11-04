use pretty_assertions::assert_eq;

use super::support::{insert, server};
use crate::store::testing::{MODEL, Seed, axis, fake_vector};
use crate::store::{Detail, KnowledgeStore, Link, Scope, StoreError};

fn link(repo: &str, globs: &[&str]) -> Link {
    Link {
        repo_key: repo.to_owned(),
        globs: globs.iter().map(|glob| (*glob).to_owned()).collect(),
    }
}

fn keyword_ids(store: &mut dyn KnowledgeStore, query: &str) -> Vec<i64> {
    let hits = store.keyword_top_k(query, &Scope::everything(), 50).expect("keyword");
    hits.iter().map(|hit| hit.chunk_id).collect()
}

#[test]
fn team_a_document_comes_back_with_its_chunks_links_and_counts() {
    let Some(server) = server() else { return };
    let mut store = server.store();
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
    let Some(server) = server() else { return };
    let mut store = server.store();
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
    assert!(store.get(999, &Detail::default()).expect("get").is_none());
}

#[test]
fn team_putting_a_stored_document_replaces_its_chunks_and_links_and_keeps_its_creation_time() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    let id = insert(
        &mut store,
        &Seed::new("Guide")
            .updated(100)
            .embedded("Old", "stale words", axis(5))
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
    assert!(
        store
            .vector_top_k(&axis(5), &Scope::everything(), 10)
            .expect("vector")
            .is_empty(),
        "the old vector left the vector index"
    );
    assert_eq!(store.stats().expect("stats").chunks, 1);

    doc.id = 4242;
    assert!(matches!(
        store.put_document(&doc, &chunks, &links),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn team_a_rejected_put_stores_nothing() {
    let Some(server) = server() else { return };
    let mut store = server.store();
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
fn team_a_failure_in_the_middle_of_a_write_rolls_all_of_it_back() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    insert(&mut store, &Seed::new("Existing").chunk("A", "kept"));

    // The first statement lands, the second is refused by the server (a vector of the wrong
    // size): the transaction must leave neither the document nor its first chunk.
    let outcome = store.write(|db| {
        db.execute(
            "INSERT INTO knowledge_documents (title, source_name, source_key, format, content_sha256, file_sha256,
                 bytes, chunk_count, origin, created_at, updated_at)
             VALUES ('Half', 's', 'k', 'text', 'c', 'f', 1, 1, 'user', 1, 1)",
            &[],
        )?;
        db.execute(
            "INSERT INTO knowledge_chunks (doc_id, ordinal, title, heading_path, text, chunk_sha, tokens,
                 char_start, char_end, embedding, model_id)
             VALUES (last_insert_rowid(), 0, 'Half', '', 'x', 's', 1, 0, 1, vector32('[1,2]'), 'm')",
            &[],
        )?;
        Ok(())
    });

    assert!(outcome.is_err());

    let stats = store.stats().expect("stats");
    assert_eq!((stats.documents, stats.chunks, stats.generation), (1, 1, 1));
    assert_eq!(
        server.integer("SELECT count(*) FROM knowledge_documents WHERE title = 'Half'"),
        0
    );
}

#[test]
fn team_documents_are_found_by_content_hash_and_by_source() {
    let Some(server) = server() else { return };
    let mut store = server.store();
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
fn team_stored_embeddings_are_reused_by_chunk_hash_and_model_and_come_back_exact() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    let precise = fake_vector(11);
    insert(
        &mut store,
        &Seed::new("Guide")
            .embedded("A", "first text", axis(2))
            .embedded("B", "second text", precise.clone())
            .chunk("C", "third text"),
    );

    let shas: Vec<String> = ["sha-first text", "sha-second text", "sha-third text", "sha-unknown"]
        .iter()
        .map(|sha| (*sha).to_owned())
        .collect();

    let found = store.reusable_embeddings(MODEL, &shas).expect("reuse");
    assert_eq!(
        found.len(),
        2,
        "an unembedded and an unknown chunk have nothing to reuse"
    );
    assert_eq!(found["sha-first text"], axis(2));
    assert_eq!(found["sha-second text"], precise, "every float survives the round trip");

    assert!(
        store
            .reusable_embeddings("another-model", &shas)
            .expect("reuse")
            .is_empty()
    );
}

#[test]
fn team_setting_links_replaces_them_and_merges_repeats() {
    let Some(server) = server() else { return };
    let mut store = server.store();
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
fn team_renaming_changes_what_the_keyword_index_finds_and_keeps_who_wrote_it() {
    let Some(server) = server() else { return };
    let mut store = server.store();
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

    assert!(!store.rename(999, "Anything", None, 0).expect("missing"));
    assert!(matches!(store.rename(id, " ", None, 0), Err(StoreError::Invalid(_))));
}

#[test]
fn team_deleting_cascades_to_chunks_links_the_keyword_index_and_the_vector_index() {
    let Some(server) = server() else { return };
    let mut store = server.store();
    let keep = insert(&mut store, &Seed::new("Keeper").chunk("A", "shared word keeper"));
    let gone = insert(
        &mut store,
        &Seed::new("Goner")
            .chunk("A", "shared word goner")
            .embedded("B", "other goner", axis(4))
            .link("git:h/o/r", &["x/**"]),
    );

    assert_eq!(store.delete(&[gone, gone, 999]).expect("delete"), 1);

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

    // The index holds exactly what the chunks table holds: deleting through the cascade left
    // nothing behind.
    server
        .execute("INSERT INTO knowledge_chunks_fts (knowledge_chunks_fts) VALUES ('integrity-check')")
        .expect("the keyword index matches the chunks");

    assert_eq!(store.delete(&[keep]).expect("delete"), 1);
    assert_eq!(store.stats().expect("stats").chunks, 0);
    assert!(matches!(store.delete(&vec![1; 1001]), Err(StoreError::Invalid(_))));
}

#[test]
fn team_every_write_grows_the_generation_and_reads_do_not() {
    let Some(server) = server() else { return };
    let mut store = server.store();
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
    let Some(server) = server() else { return };
    let mut store = server.store();

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
    let Some(server) = server() else { return };
    let mut store = server.store();
    let id = insert(&mut store, &Seed::new("Host").chunk("A", "x"));

    let zeros = vec!["0"; 384].join(",");
    let insert_chunk = |embedding: &str, model: &str| {
        server.execute(&format!(
            "INSERT INTO knowledge_chunks (doc_id, ordinal, title, heading_path, text, chunk_sha, tokens,
                 char_start, char_end, embedding, model_id)
             VALUES ({id}, abs(random()), 't', '', 'x', 's', 1, 0, 1, {embedding}, {model})"
        ))
    };

    assert!(insert_chunk("vector32('[0,0,0]')", "'m'").is_err(), "too short");
    assert!(
        insert_chunk(&format!("vector32('[{zeros}]')"), "NULL").is_err(),
        "no model"
    );
    assert!(insert_chunk("NULL", "'m'").is_err(), "a model without a vector");
    assert!(insert_chunk(&format!("vector32('[{zeros}]')"), "'m'").is_ok());
    assert!(insert_chunk("NULL", "NULL").is_ok());
}
