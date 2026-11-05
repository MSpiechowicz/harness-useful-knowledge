//! The search's tests: the pipeline over a store with fake vectors, the wrapping of untrusted
//! text, and the calibration on the real model.

mod calibration;
mod queries;
mod render;

use super::*;
use crate::project::ProjectKey;
use crate::store::testing::{Seed, angled, axis, insert, store};
use crate::store::{Scope, SqliteStore};

/// An embedder that answers with a fixed vector, or fails.
struct Fake {
    answer: Result<Vec<f32>, String>,
    asked: Vec<String>,
}

impl Fake {
    fn answering(vector: Vec<f32>) -> Self {
        Self {
            answer: Ok(vector),
            asked: Vec::new(),
        }
    }

    fn down() -> Self {
        Self {
            answer: Err("the embedder is not reachable (connection refused)".to_owned()),
            asked: Vec::new(),
        }
    }
}

impl QueryEmbedder for Fake {
    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>, String> {
        self.asked.push(text.to_owned());
        self.answer.clone()
    }
}

fn key(key: &str) -> ProjectKey {
    ProjectKey {
        key: key.to_owned(),
        root: std::path::PathBuf::new(),
        remote: None,
    }
}

fn ask(store: &mut SqliteStore, embedder: Option<&mut Fake>, text: &str, scope: &Scope, paths: &[String]) -> Found {
    let query = Query {
        text,
        scope,
        paths,
        limit: 10,
    };

    run(
        store,
        embedder.map(|embedder| embedder as &mut dyn QueryEmbedder),
        &query,
        Tuning::SEARCH,
    )
    .expect("search")
}

fn titles(found: &Found) -> Vec<&str> {
    found.hits.iter().map(|scored| scored.hit.title.as_str()).collect()
}

#[test]
fn an_empty_database_answers_nothing() {
    let mut store = store();
    let mut fake = Fake::answering(axis(0));

    let found = ask(
        &mut store,
        Some(&mut fake),
        "gateway routing",
        &Scope::everything(),
        &[],
    );

    assert!(found.hits.is_empty());
    assert_eq!(found.vectors, Vectors::Used);
}

#[test]
fn the_query_is_embedded_as_asked_and_a_blank_one_is_not() {
    let mut store = store();
    let mut fake = Fake::answering(axis(0));

    ask(
        &mut store,
        Some(&mut fake),
        "  gateway   routing ",
        &Scope::everything(),
        &[],
    );
    assert_eq!(fake.asked, ["gateway   routing"]);

    let found = ask(&mut store, Some(&mut fake), "   ", &Scope::everything(), &[]);
    assert_eq!(fake.asked.len(), 1, "a blank query asks nobody");
    assert_eq!(found.vectors, Vectors::NotAsked);
}

#[test]
fn without_an_embedder_the_words_alone_answer() {
    let mut store = store();
    insert(
        &mut store,
        &Seed::new("Gateway guide").chunk("Routing", "The gateway routes every request."),
    );
    insert(&mut store, &Seed::new("Cooking").chunk("Soup", "Boil the water."));

    let found = ask(&mut store, None, "gateway routes", &Scope::everything(), &[]);

    assert_eq!(titles(&found), ["Gateway guide"]);
    assert_eq!(found.vectors, Vectors::NotAsked);
}

#[test]
fn an_embedder_that_is_down_costs_the_vector_hits_and_is_reported() {
    let mut store = store();
    insert(
        &mut store,
        &Seed::new("Gateway guide").embedded("Routing", "The gateway routes every request.", axis(0)),
    );
    let mut fake = Fake::down();

    let found = ask(&mut store, Some(&mut fake), "gateway", &Scope::everything(), &[]);

    assert_eq!(titles(&found), ["Gateway guide"], "the keyword hit remains");
    assert!(matches!(&found.vectors, Vectors::Unavailable(reason) if reason.contains("not reachable")));
}

#[test]
fn a_chunk_only_the_vectors_find_is_found() {
    let mut store = store();
    insert(
        &mut store,
        &Seed::new("Login").embedded("Tokens", "Credentials expire after an hour.", angled(10.0)),
    );
    let mut fake = Fake::answering(axis(0));

    let found = ask(&mut store, Some(&mut fake), "token refresh", &Scope::everything(), &[]);

    assert_eq!(titles(&found), ["Login"]);
}

#[test]
fn a_vector_under_the_cosine_floor_is_no_answer() {
    let mut store = store();
    let degrees_of_floor = rank::COSINE_FLOOR.acos().to_degrees() as f32;

    insert(
        &mut store,
        &Seed::new("Near").embedded("A", "Credentials expire.", angled(degrees_of_floor - 3.0)),
    );
    insert(
        &mut store,
        &Seed::new("Far").embedded("B", "Soup recipes.", angled(degrees_of_floor + 3.0)),
    );
    let mut fake = Fake::answering(axis(0));

    let found = ask(&mut store, Some(&mut fake), "token refresh", &Scope::everything(), &[]);

    assert_eq!(titles(&found), ["Near"]);
}

/// The same chunk (text and vector) in a global document, one linked to `a` and one linked to `b`.
fn three_repositories(store: &mut SqliteStore) {
    let seed = |title: &str| Seed::new(title).embedded("Routing", "The gateway routes every request.", axis(0));

    insert(store, &seed("Global"));
    insert(store, &seed("Repo A").link("git:h/o/a", &[]));
    insert(store, &seed("Repo B").link("git:h/o/b", &[]));
}

#[test]
fn a_search_sees_the_global_documents_and_its_own_repositorys() {
    let mut store = store();
    three_repositories(&mut store);

    let from = |store: &mut SqliteStore, scope: &Scope| {
        let mut fake = Fake::answering(axis(0));
        let found = ask(store, Some(&mut fake), "gateway routes", scope, &[]);
        let mut titles: Vec<String> = titles(&found).into_iter().map(str::to_owned).collect();
        titles.sort();
        titles
    };

    assert_eq!(
        from(&mut store, &Scope::from_repo(Some(key("git:h/o/a")))),
        ["Global", "Repo A"]
    );
    assert_eq!(
        from(&mut store, &Scope::from_repo(Some(key("git:h/o/b")))),
        ["Global", "Repo B"]
    );
    assert_eq!(
        from(&mut store, &Scope::from_repo(None)),
        ["Global"],
        "outside a repository only the global ones"
    );
    assert_eq!(from(&mut store, &Scope::everything()), ["Global", "Repo A", "Repo B"]);
    assert_eq!(
        from(
            &mut store,
            &Scope {
                repo: Some(key("git:h/o/a")),
                include_other_repos: true,
                drafts: false,
            }
        ),
        ["Global", "Repo A", "Repo B"],
        "scope all sees every repository"
    );
}

#[test]
fn the_repository_of_the_search_breaks_a_tie_and_a_matching_glob_ranks_before_it() {
    let mut store = store();
    let seed = |title: &str| Seed::new(title).embedded("Routing", "The gateway routes every request.", axis(0));

    insert(&mut store, &seed("Global"));
    insert(&mut store, &seed("Linked").link("git:h/o/a", &[]));
    insert(&mut store, &seed("Linked to api").link("git:h/o/a", &["src/api/**"]));
    insert(&mut store, &seed("Other repo").link("git:h/o/b", &[]));

    let scope = Scope {
        repo: Some(key("git:h/o/a")),
        include_other_repos: true,
        drafts: false,
    };
    let mut fake = Fake::answering(axis(0));

    // The four chunks are equally relevant and rank 1 to 4 in the order they were inserted in. The
    // link is a tie-break: it lifts the chunk one rank behind the global one, not the chunk two
    // ranks behind it.
    let without = ask(&mut store, Some(&mut fake), "gateway routes", &scope, &[]);
    assert_eq!(titles(&without)[..2], ["Linked", "Global"], "{:?}", titles(&without));

    let paths = vec!["src/api/server.rs".to_owned()];
    let with = ask(&mut store, Some(&mut fake), "gateway routes", &scope, &paths);
    assert_eq!(titles(&with)[0], "Linked to api");
    assert_eq!(titles(&with).len(), 4);
}

#[test]
fn a_document_has_at_most_two_chunks_in_an_answer() {
    let mut store = store();
    insert(
        &mut store,
        &Seed::new("Long guide")
            .chunk("One", "The gateway routes requests.")
            .chunk("Two", "The gateway retries requests.")
            .chunk("Three", "The gateway logs requests."),
    );

    let found = ask(&mut store, None, "gateway requests", &Scope::everything(), &[]);

    assert_eq!(found.hits.len(), 2);
}

#[test]
fn a_weak_chunk_does_not_follow_a_strong_one() {
    let mut store = store();
    insert(
        &mut store,
        &Seed::new("Strong").embedded("A", "The gateway routes every request.", axis(0)),
    );

    // Far down the vector list and in no keyword list: a fraction of the best chunk's score.
    for number in 0..30 {
        insert(
            &mut store,
            &Seed::new(&format!("Weak {number}")).embedded(
                "B",
                &format!("Unrelated text {number}."),
                angled(30.0 + number as f32),
            ),
        );
    }

    let mut fake = Fake::answering(axis(0));
    let found = ask(&mut store, Some(&mut fake), "gateway routes", &Scope::everything(), &[]);

    assert_eq!(titles(&found)[0], "Strong");
    assert!(found.hits.len() < 31, "the tail was cut: {}", found.hits.len());
    assert!(found.hits.iter().all(|scored| scored.score >= rank::FLOOR));
}
