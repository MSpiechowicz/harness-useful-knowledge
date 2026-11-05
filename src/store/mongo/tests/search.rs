use std::path::PathBuf;

use crate::project::ProjectKey;
use crate::store::mongo::MongoStore;
use crate::store::testing::{MODEL, Seed, angled, axis, fake_vector};
use crate::store::{ChunkHit, DIMS, KnowledgeStore, Scope, StoreError, fuse, rank};

use super::{TestDb, eventually, insert, store};

const A: &str = "git:github.com/o/a";
const B: &str = "git:github.com/o/b";

fn repo(key: &str) -> ProjectKey {
    ProjectKey {
        key: key.to_owned(),
        root: PathBuf::new(),
        remote: None,
    }
}

fn from(key: &str) -> Scope {
    Scope::from_repo(Some(repo(key)))
}

fn titles(hits: &[ChunkHit]) -> Vec<&str> {
    let mut titles: Vec<&str> = hits.iter().map(|hit| hit.title.as_str()).collect();
    titles.sort_unstable();
    titles
}

fn keyword(store: &mut MongoStore, scope: &Scope) -> Vec<ChunkHit> {
    store.keyword_top_k("common", scope, 50).expect("keyword")
}

fn vector(store: &mut MongoStore, scope: &Scope) -> Vec<ChunkHit> {
    store.vector_top_k(&axis(0), scope, 50).expect("vector")
}

/// Waits until `word` finds `count` chunks.
fn keyword_indexed(store: &mut MongoStore, word: &str, count: usize) {
    eventually(
        &format!("{word:?} is indexed"),
        || store.keyword_top_k(word, &Scope::everything(), 50),
        |hits| hits.len() == count,
    );
}

/// Waits until the vector along axis 0 finds `count` chunks.
fn vectors_indexed(store: &mut MongoStore, count: usize) {
    eventually(
        "the vectors are indexed",
        || store.vector_top_k(&axis(0), &Scope::everything(), 50),
        |hits| hits.len() == count,
    );
}

/// A global document, one for each repository and one for both, all saying the same words, once
/// both indexes have caught up.
fn scoped_store() -> Option<(TestDb, MongoStore)> {
    let (db, mut store) = store()?;

    for (title, links) in [
        ("Global", vec![]),
        ("Only A", vec![(A, vec!["src/a/**"])]),
        ("Only B", vec![(B, vec!["src/b/**"])]),
        ("Both", vec![(A, vec![]), (B, vec![])]),
    ] {
        let mut seed = Seed::new(title).embedded("Intro", "common words everywhere", axis(0));

        for (key, globs) in links {
            seed = seed.link(key, &globs);
        }

        insert(&mut store, &seed);
    }

    keyword_indexed(&mut store, "common", 4);
    vectors_indexed(&mut store, 4);

    Some((db, store))
}

#[test]
fn team_mongo_from_a_repository_a_search_sees_global_and_linked_documents_only() {
    let Some((_db, mut store)) = scoped_store() else { return };

    for search in [keyword, vector] {
        assert_eq!(titles(&search(&mut store, &from(A))), ["Both", "Global", "Only A"]);
        assert_eq!(titles(&search(&mut store, &from(B))), ["Both", "Global", "Only B"]);
    }
}

#[test]
fn team_mongo_outside_a_repository_a_search_sees_the_global_documents() {
    let Some((_db, mut store)) = scoped_store() else { return };

    for search in [keyword, vector] {
        assert_eq!(titles(&search(&mut store, &Scope::from_repo(None))), ["Global"]);
        assert_eq!(
            titles(&search(&mut store, &from("git:github.com/o/unknown"))),
            ["Global"]
        );
    }
}

#[test]
fn team_mongo_including_other_repositories_or_searching_every_repository_sees_everything() {
    let Some((_db, mut store)) = scoped_store() else { return };
    let others = Scope {
        repo: Some(repo(A)),
        include_other_repos: true,
        drafts: false,
    };
    let every = Scope::from_repo(Some(ProjectKey::every()));
    let all = ["Both", "Global", "Only A", "Only B"];

    for search in [keyword, vector] {
        assert_eq!(titles(&search(&mut store, &others)), all);
        assert_eq!(titles(&search(&mut store, &every)), all);
        assert_eq!(titles(&search(&mut store, &Scope::everything())), all);
    }
}

#[test]
fn team_mongo_a_hit_carries_the_globs_of_its_link_to_the_scope_repository() {
    let Some((_db, mut store)) = scoped_store() else { return };

    for search in [keyword, vector] {
        let hits = search(&mut store, &from(A));
        let globs = |title: &str| {
            hits.iter()
                .find(|hit| hit.title == title)
                .unwrap_or_else(|| panic!("{title} found"))
                .repo_globs
                .clone()
        };

        assert_eq!(globs("Global"), None);
        assert_eq!(globs("Only A"), Some(vec!["src/a/**".to_owned()]));
        assert_eq!(globs("Both"), Some(Vec::new()));

        // Another repository's link is no link to this one, even when documents are shared.
        let seen_from_other = search(
            &mut store,
            &Scope {
                repo: Some(repo(A)),
                include_other_repos: true,
                drafts: false,
            },
        );
        let only_b = seen_from_other.iter().find(|hit| hit.title == "Only B").expect("found");
        assert_eq!(only_b.repo_globs, None);
    }
}

#[test]
fn team_mongo_relinking_a_document_changes_what_each_repository_finds_at_once() {
    let Some((_db, mut store)) = scoped_store() else { return };
    let only_a = store
        .keyword_top_k("common", &from(A), 50)
        .expect("keyword")
        .into_iter()
        .find(|hit| hit.title == "Only A")
        .expect("found")
        .doc_id;

    store
        .set_links(
            only_a,
            &[crate::store::Link {
                repo_key: B.to_owned(),
                globs: Vec::new(),
            }],
        )
        .expect("move");

    // The chunk's scope changed in the collection at once, so a hit the index still lists under
    // the old scope is dropped, and the new scope has it once the index caught up.
    for search in [keyword, vector] {
        assert!(!titles(&search(&mut store, &from(A))).contains(&"Only A"));
    }

    eventually(
        "the move reaches the index",
        || Ok::<_, StoreError>(titles(&keyword(&mut store, &from(B))).contains(&"Only A")),
        |found| *found,
    );
    eventually(
        "the move reaches the vector index",
        || Ok::<_, StoreError>(titles(&vector(&mut store, &from(B))).contains(&"Only A")),
        |found| *found,
    );
}

#[test]
fn team_mongo_a_link_lifts_a_chunk_above_a_global_one_a_rank_ahead_and_a_matching_glob_lifts_it_further() {
    let Some((_db, mut store)) = store() else { return };
    insert(
        &mut store,
        &Seed::new("Global guide").embedded("Intro", "token refresh flow", axis(0)),
    );
    insert(
        &mut store,
        &Seed::new("Linked guide")
            .embedded("Intro", "token refresh flow", axis(0))
            .link(A, &["docs/**"]),
    );
    insert(
        &mut store,
        &Seed::new("Glob guide")
            .embedded("Intro", "token refresh flow", axis(0))
            .link(A, &["src/auth/**"]),
    );
    keyword_indexed(&mut store, "token", 3);
    vectors_indexed(&mut store, 3);

    let scope = from(A);
    let answer = |store: &mut MongoStore, paths: &[&str]| {
        let keyword = store.keyword_top_k("token refresh", &scope, 50).expect("keyword");
        let vector = store.vector_top_k(&axis(0), &scope, 50).expect("vector");
        let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();

        rank::rank(fuse::fuse(keyword, vector, 50), &paths, 10)
            .into_iter()
            .map(|scored| scored.hit.title)
            .collect::<Vec<_>>()
    };

    // The three chunks rank 1, 2 and 3 (the order they were inserted in). The link is a tie-break,
    // so it lifts the chunk one rank behind the global one and not the chunk two ranks behind it.
    assert_eq!(answer(&mut store, &[]), ["Linked guide", "Global guide", "Glob guide"]);
    assert_eq!(
        answer(&mut store, &["src/auth/login.rs"]),
        ["Glob guide", "Linked guide", "Global guide"]
    );
}

#[test]
fn team_mongo_a_title_word_outweighs_a_text_word_and_diacritics_and_case_do_not_matter() {
    let Some((_db, mut store)) = store() else { return };
    insert(&mut store, &Seed::new("Deployment").chunk("Intro", "unrelated body"));
    insert(
        &mut store,
        &Seed::new("Unrelated").chunk("Intro", "a note about deployment steps"),
    );
    insert(&mut store, &Seed::new("Politique").chunk("Sécurité", "Café protégé"));
    keyword_indexed(&mut store, "deployment", 2);

    let hits = store
        .keyword_top_k("deployment", &Scope::everything(), 10)
        .expect("keyword");
    let order: Vec<&str> = hits.iter().map(|hit| hit.title.as_str()).collect();
    assert_eq!(order, ["Deployment", "Unrelated"]);
    assert!(hits[0].score > hits[1].score && hits[1].score > 0.0, "{hits:?}");

    keyword_indexed(&mut store, "politique", 1);
    let folded = store
        .keyword_top_k("CAFE PROTEGE securite", &Scope::everything(), 10)
        .expect("keyword");
    assert_eq!(titles(&folded), ["Politique"]);
}

#[test]
fn team_mongo_a_heading_word_outweighs_a_text_word() {
    let Some((_db, mut store)) = store() else { return };
    insert(
        &mut store,
        &Seed::new("One").chunk("Rollback", "steps of the procedure"),
    );
    insert(
        &mut store,
        &Seed::new("Two").chunk("Procedure", "how to rollback a release"),
    );
    keyword_indexed(&mut store, "rollback", 2);

    let hits = store
        .keyword_top_k("rollback", &Scope::everything(), 10)
        .expect("keyword");
    assert_eq!(hits[0].title, "One", "{hits:?}");
}

#[test]
fn team_mongo_queries_full_of_search_syntax_find_their_words_and_never_fail() {
    let Some((_db, mut store)) = store() else { return };
    insert(
        &mut store,
        &Seed::new("Operators").chunk("Intro", "the NEAR operator and the word secret"),
    );
    insert(&mut store, &Seed::new("Other").chunk("Intro", "nothing here"));
    keyword_indexed(&mut store, "secret", 1);

    for query in [
        "\"",
        "\"\"\"",
        "(",
        "NEAR(",
        "a AND",
        "OR OR OR",
        "* *",
        "text:secret",
        "title: ^ -",
        "\" OR \"",
        "'; DROP TABLE knowledge_chunks; --",
        "{\"$where\": \"1\"}",
        "   ",
        "",
        "\u{0}",
        "近い",
    ] {
        store
            .keyword_top_k(query, &Scope::everything(), 10)
            .unwrap_or_else(|error| panic!("{query:?} failed: {error}"));
    }

    let hits = store
        .keyword_top_k("\"secret\" NEAR( -", &Scope::everything(), 10)
        .expect("keyword");
    assert_eq!(titles(&hits), ["Operators"], "the plain words still match");

    assert!(
        store
            .keyword_top_k("secret", &Scope::everything(), 0)
            .expect("keyword")
            .is_empty()
    );
    assert_eq!(store.stats().expect("stats").chunks, 2, "nothing was dropped");
}

#[test]
fn team_mongo_k_limits_the_keyword_list_and_the_cap_is_applied() {
    let Some((_db, mut store)) = store() else { return };

    for n in 0..5 {
        insert(&mut store, &Seed::new(&format!("Doc {n}")).chunk("A", "limit words"));
    }
    keyword_indexed(&mut store, "limit", 5);

    assert_eq!(
        store
            .keyword_top_k("limit", &Scope::everything(), 2)
            .expect("keyword")
            .len(),
        2
    );
    assert_eq!(
        store
            .keyword_top_k("limit", &Scope::everything(), 100_000)
            .expect("keyword")
            .len(),
        5
    );
}

#[test]
fn team_mongo_the_closest_vector_comes_first_and_k_limits_the_list() {
    let Some((_db, mut store)) = store() else { return };

    for (title, degrees) in [("far", 80.0), ("near", 10.0), ("middle", 45.0), ("opposite", 180.0)] {
        insert(&mut store, &Seed::new(title).embedded("A", title, angled(degrees)));
    }
    vectors_indexed(&mut store, 4);

    let hits = store.vector_top_k(&axis(0), &Scope::everything(), 10).expect("vector");
    let order: Vec<&str> = hits.iter().map(|hit| hit.title.as_str()).collect();
    assert_eq!(order, ["near", "middle", "far", "opposite"]);

    assert!(
        (hits[0].score - 10.0_f64.to_radians().cos()).abs() < 1e-4,
        "{}",
        hits[0].score
    );
    assert!((hits[3].score + 1.0).abs() < 1e-4, "{}", hits[3].score);

    let two = store.vector_top_k(&axis(0), &Scope::everything(), 2).expect("vector");
    assert_eq!(two.len(), 2);
    assert_eq!(two[0].title, "near");
}

#[test]
fn team_mongo_only_embedded_chunks_of_the_current_model_are_compared() {
    let Some((_db, mut store)) = store() else { return };
    insert(&mut store, &Seed::new("Embedded").embedded("A", "x", axis(0)));
    insert(&mut store, &Seed::new("Plain").chunk("A", "x"));
    vectors_indexed(&mut store, 1);

    let hits = store.vector_top_k(&axis(0), &Scope::everything(), 10).expect("vector");
    assert_eq!(titles(&hits), ["Embedded"]);

    // Once a model is recorded, vectors another model made no longer count.
    store.set_meta("model_id", "another-model").expect("meta");
    assert!(
        store
            .vector_top_k(&axis(0), &Scope::everything(), 10)
            .expect("vector")
            .is_empty()
    );

    store.set_meta("model_id", MODEL).expect("meta");
    assert_eq!(
        store
            .vector_top_k(&axis(0), &Scope::everything(), 10)
            .expect("vector")
            .len(),
        1
    );
}

#[test]
fn team_mongo_a_query_vector_of_the_wrong_size_is_refused_and_pseudo_random_vectors_rank_themselves_first() {
    let Some((_db, mut store)) = store() else { return };

    for seed in 1..=20_u64 {
        insert(
            &mut store,
            &Seed::new(&format!("doc {seed}")).embedded("A", "x", fake_vector(seed)),
        );
    }
    vectors_indexed_by(&mut store, 20);

    for bad in [vec![0.1; DIMS - 1], vec![f32::NAN; DIMS], Vec::new()] {
        assert!(matches!(
            store.vector_top_k(&bad, &Scope::everything(), 5),
            Err(StoreError::Invalid(_))
        ));
    }

    let hits = store
        .vector_top_k(&fake_vector(7), &Scope::everything(), 3)
        .expect("vector");
    assert_eq!(hits[0].title, "doc 7");
    assert!((hits[0].score - 1.0).abs() < 1e-4);
}

/// Waits until a query for the vector of `doc 7` finds `count` chunks among the first 20.
fn vectors_indexed_by(store: &mut MongoStore, count: usize) {
    eventually(
        "the vectors are indexed",
        || store.vector_top_k(&fake_vector(7), &Scope::everything(), 50),
        |hits| hits.len() == count,
    );
}
