use std::path::PathBuf;

use crate::project::ProjectKey;
use crate::store::testing::{MODEL, Seed, angled, axis, fake_vector, insert, store};
use crate::store::{ChunkHit, DIMS, KnowledgeStore, Scope, SqliteStore, StoreError, fuse, rank};

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

/// A global document, one for each repository and one for both, all saying the same words.
fn scoped_store() -> SqliteStore {
    let mut store = store();

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

    store
}

fn keyword(store: &mut SqliteStore, scope: &Scope) -> Vec<ChunkHit> {
    store.keyword_top_k("common", scope, 50).expect("keyword")
}

fn vector(store: &mut SqliteStore, scope: &Scope) -> Vec<ChunkHit> {
    store.vector_top_k(&axis(0), scope, 50).expect("vector")
}

#[test]
fn from_a_repository_a_search_sees_global_and_linked_documents_only() {
    let mut store = scoped_store();

    for search in [keyword, vector] {
        assert_eq!(titles(&search(&mut store, &from(A))), ["Both", "Global", "Only A"]);
        assert_eq!(titles(&search(&mut store, &from(B))), ["Both", "Global", "Only B"]);
    }
}

#[test]
fn outside_a_repository_a_search_sees_the_global_documents() {
    let mut store = scoped_store();

    for search in [keyword, vector] {
        assert_eq!(titles(&search(&mut store, &Scope::from_repo(None))), ["Global"]);
        assert_eq!(
            titles(&search(&mut store, &from("git:github.com/o/unknown"))),
            ["Global"]
        );
    }
}

#[test]
fn including_other_repositories_or_searching_every_repository_sees_everything() {
    let mut store = scoped_store();
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
fn a_hit_carries_the_globs_of_its_link_to_the_scope_repository() {
    let mut store = scoped_store();

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
fn a_link_lifts_a_chunk_above_a_global_one_a_rank_ahead_and_a_matching_glob_lifts_it_further() {
    let mut store = store();
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

    let scope = from(A);
    let answer = |store: &mut SqliteStore, paths: &[&str]| {
        let keyword = store.keyword_top_k("token refresh", &scope, 50).expect("keyword");
        let vector = store.vector_top_k(&axis(0), &scope, 50).expect("vector");
        let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();

        rank::rank(fuse::fuse(keyword, vector, 50), &paths, 10)
            .into_iter()
            .map(|scored| scored.hit.title)
            .collect::<Vec<_>>()
    };

    // The three chunks are equally relevant and rank 1, 2 and 3 (the order they were inserted in).
    // The link is a tie-break, so it lifts the chunk one rank behind the global one and not the
    // chunk two ranks behind it.
    assert_eq!(answer(&mut store, &[]), ["Linked guide", "Global guide", "Glob guide"]);
    assert_eq!(
        answer(&mut store, &["src/auth/login.rs"]),
        ["Glob guide", "Linked guide", "Global guide"]
    );
}

#[test]
fn a_title_word_outweighs_a_text_word_and_diacritics_and_case_do_not_matter() {
    let mut store = store();
    insert(&mut store, &Seed::new("Deployment").chunk("Intro", "unrelated body"));
    insert(
        &mut store,
        &Seed::new("Unrelated").chunk("Intro", "a note about deployment steps"),
    );
    insert(&mut store, &Seed::new("Politique").chunk("Sécurité", "Café protégé"));

    let hits = store
        .keyword_top_k("deployment", &Scope::everything(), 10)
        .expect("keyword");
    let order: Vec<&str> = hits.iter().map(|hit| hit.title.as_str()).collect();
    assert_eq!(order, ["Deployment", "Unrelated"]);
    assert!(hits[0].score > hits[1].score && hits[1].score > 0.0, "{hits:?}");

    let folded = store
        .keyword_top_k("CAFE PROTEGE securite", &Scope::everything(), 10)
        .expect("keyword");
    assert_eq!(titles(&folded), ["Politique"]);
}

#[test]
fn queries_full_of_search_syntax_find_their_words_and_never_fail() {
    let mut store = store();
    insert(
        &mut store,
        &Seed::new("Operators").chunk("Intro", "the NEAR operator and the word secret"),
    );
    insert(&mut store, &Seed::new("Other").chunk("Intro", "nothing here"));

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
fn the_closest_vector_comes_first_and_k_limits_the_list() {
    let mut store = store();

    for (title, degrees) in [("far", 80.0), ("near", 10.0), ("middle", 45.0), ("opposite", 180.0)] {
        insert(&mut store, &Seed::new(title).embedded("A", title, angled(degrees)));
    }

    let hits = store.vector_top_k(&axis(0), &Scope::everything(), 10).expect("vector");
    let order: Vec<&str> = hits.iter().map(|hit| hit.title.as_str()).collect();
    assert_eq!(order, ["near", "middle", "far", "opposite"]);

    assert!((hits[0].score - 10.0_f64.to_radians().cos()).abs() < 1e-6);
    assert!((hits[3].score + 1.0).abs() < 1e-6);

    let two = store.vector_top_k(&axis(0), &Scope::everything(), 2).expect("vector");
    assert_eq!(two.len(), 2);
    assert_eq!(two[0].title, "near");
}

#[test]
fn only_embedded_chunks_of_the_current_model_are_compared() {
    let mut store = store();
    insert(&mut store, &Seed::new("Embedded").embedded("A", "x", axis(0)));
    insert(&mut store, &Seed::new("Plain").chunk("A", "x"));

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
fn a_query_vector_of_the_wrong_size_is_refused_and_pseudo_random_vectors_rank_themselves_first() {
    let mut store = store();

    for seed in 1..=20_u64 {
        insert(
            &mut store,
            &Seed::new(&format!("doc {seed}")).embedded("A", "x", fake_vector(seed)),
        );
    }

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
    assert!((hits[0].score - 1.0).abs() < 1e-5);
}
