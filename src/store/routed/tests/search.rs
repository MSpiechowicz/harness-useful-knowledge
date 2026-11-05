use pretty_assertions::assert_eq;

use super::*;
use crate::search::{self, Query, QueryEmbedder, Tuning, Vectors};
use crate::store::Scope;
use crate::store::testing::{Seed, angled, axis, insert, store};

/// The chunk ids of a search through the routed store, keyword only.
fn routed_ids(routed: &mut RoutedStore, text: &str) -> Vec<i64> {
    let scope = Scope::everything();
    let query = Query {
        text,
        scope: &scope,
        paths: &[],
        limit: 10,
    };

    let found = search::run(routed, None, &query, Tuning::SEARCH).expect("search");
    found.hits.into_iter().map(|scored| scored.hit.chunk_id).collect()
}

/// A search through the routed store, keyword only.
pub(super) fn search_titles(routed: &mut RoutedStore, text: &str) -> Vec<(String, f64)> {
    let scope = Scope::from_repo(Some(crate::project::ProjectKey {
        key: REPO.to_owned(),
        root: std::path::PathBuf::new(),
        remote: None,
    }));
    let query = Query {
        text,
        scope: &scope,
        paths: &[],
        limit: 10,
    };

    let found = search::run(routed, None, &query, Tuning::SEARCH).expect("search");
    found
        .hits
        .into_iter()
        .map(|scored| (scored.hit.title, scored.score))
        .collect()
}

#[test]
fn a_search_runs_in_each_place_and_fuses_by_rank() {
    let mut routed = fixture(Where::RemoteLocal, Where::RemoteLocal);
    let found = search_titles(&mut routed, "gateway");

    let titles: Vec<&str> = found.iter().map(|(title, _)| title.as_str()).collect();
    assert_eq!(titles.len(), 4, "{titles:?}");

    for title in ["Remote global", "Remote linked", "Local global", "Local linked"] {
        assert!(titles.contains(&title), "{title} in {titles:?}");
    }

    // Chunks of the two places have their own ids even when the databases count to the same number.
    let found_ids = routed_ids(&mut routed, "gateway");
    let mut unique = found_ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 4, "{found_ids:?}");
    assert_eq!(
        found_ids
            .iter()
            .filter(|id| ids::place_of(**id) == Place::Local)
            .count(),
        2
    );

    // Each place's first (the linked one: it is boosted) leads, so the lists are interleaved.
    let first_two: Vec<&str> = titles[..2].to_vec();
    assert!(
        first_two.contains(&"Remote linked") && first_two.contains(&"Local linked"),
        "{titles:?}"
    );

    // Fused scores follow the rank in the place, not the backends' keyword scores.
    assert!(found.windows(2).all(|pair| pair[0].1 >= pair[1].1));
    assert!((found[0].1 - 30_500.0 / 61.0).abs() < 1e-6);
}

#[test]
fn a_search_in_one_place_answers_as_without_the_other() {
    let mut routed = fixture(Where::Remote, Where::Remote);
    let found = search_titles(&mut routed, "gateway");

    let mut alone = store();
    insert(
        &mut alone,
        &Seed::new("Remote global").chunk("Intro", "gateway timeout settings"),
    );
    insert(
        &mut alone,
        &Seed::new("Remote linked")
            .chunk("Intro", "gateway retry budget")
            .link(REPO, &[]),
    );
    let mut single = routed_over_one(alone);
    let expected = search_titles(&mut single, "gateway");

    assert_eq!(found, expected);
    assert_eq!(found.len(), 2);
}

#[test]
fn a_hit_without_a_link_is_a_global_document_in_each_place_and_a_global_scope_sees_nothing_else() {
    let mut routed = fixture(Where::RemoteLocal, Where::RemoteLocal);
    let hits = |routed: &mut RoutedStore, scope: Scope| {
        let query = Query {
            text: "gateway",
            scope: &scope,
            paths: &[],
            limit: 10,
        };

        let found = search::run(routed, None, &query, Tuning::SEARCH).expect("search");
        let mut hits: Vec<(String, bool)> = found
            .hits
            .into_iter()
            .map(|scored| (scored.hit.title, scored.hit.repo_globs.is_none()))
            .collect();
        hits.sort();
        hits
    };

    let repository = Scope::from_repo(Some(crate::project::ProjectKey {
        key: REPO.to_owned(),
        root: std::path::PathBuf::new(),
        remote: None,
    }));

    // From the repository: the linked documents have globs, the global ones have none.
    assert_eq!(
        hits(&mut routed, repository),
        [
            ("Local global".to_owned(), true),
            ("Local linked".to_owned(), false),
            ("Remote global".to_owned(), true),
            ("Remote linked".to_owned(), false),
        ]
    );

    // From no repository: the global documents of both places, and no linked one.
    assert_eq!(
        hits(&mut routed, Scope::from_repo(None)),
        [("Local global".to_owned(), true), ("Remote global".to_owned(), true)]
    );
}

fn routed_over_one(remote: SqliteStore) -> RoutedStore {
    RoutedStore::new(None, Some(Box::new(remote)), Routing::new(Where::Remote, Where::Remote))
}

#[test]
fn a_search_does_not_show_the_kind_a_place_hides_and_keeps_two_chunks_of_a_document() {
    // Global documents only remote, linked ones only here.
    let mut routed = fixture(Where::Local, Where::Remote);
    let titles: Vec<String> = search_titles(&mut routed, "gateway")
        .into_iter()
        .map(|(title, _)| title)
        .collect();

    assert_eq!(titles.len(), 2, "{titles:?}");
    assert!(titles.contains(&"Remote global".to_owned()) && titles.contains(&"Local linked".to_owned()));

    let mut remote = store();
    insert(
        &mut remote,
        &Seed::new("Long guide")
            .chunk("One", "gateway one")
            .chunk("Two", "gateway two")
            .chunk("Three", "gateway three"),
    );
    let mut routed = routed_over(store(), remote, Routing::new(Where::RemoteLocal, Where::RemoteLocal));

    assert_eq!(search_titles(&mut routed, "gateway").len(), 2);
}

/// Embeds every query as axis 0.
struct Axis(usize);

impl QueryEmbedder for Axis {
    fn embed_query(&mut self, _text: &str) -> Result<Vec<f32>, String> {
        self.0 += 1;
        Ok(axis(0))
    }
}

#[test]
fn the_query_is_embedded_once_and_each_place_applies_the_cosine_floor_to_its_own_vectors() {
    let mut remote = store();
    insert(&mut remote, &Seed::new("Near remote").embedded("a", "zzz", angled(5.0)));
    insert(&mut remote, &Seed::new("Far remote").embedded("a", "yyy", angled(80.0)));
    let mut local = store();
    insert(&mut local, &Seed::new("Near local").embedded("a", "xxx", angled(10.0)));

    let mut routed = routed_over(local, remote, Routing::new(Where::RemoteLocal, Where::RemoteLocal));
    let mut embedder = Axis(0);
    let scope = Scope::everything();
    let query = Query {
        text: "something unrelated",
        scope: &scope,
        paths: &[],
        limit: 10,
    };

    let found = search::run(&mut routed, Some(&mut embedder), &query, Tuning::SEARCH).expect("search");
    let titles: Vec<&str> = found.hits.iter().map(|scored| scored.hit.title.as_str()).collect();

    assert_eq!(found.vectors, Vectors::Used);
    assert_eq!(embedder.0, 1);
    assert_eq!(titles, ["Near remote", "Near local"]);
}
