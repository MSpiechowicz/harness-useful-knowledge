use pretty_assertions::assert_eq;

mod flaky;
mod offline;
mod paging;
mod quality;
mod search;

use super::*;
use crate::config::Where;
use crate::store::testing::{Seed, axis, insert, store};
use crate::store::{Detail, RepoFilter, Sort, SqliteStore};
use search::search_titles;

const REPO: &str = "git:h/o/r";

/// Remote: a global and a linked document. Local: the same kinds. Titles say where each lives.
fn fixture(repository: Where, global: Where) -> RoutedStore {
    let mut remote = store();
    insert(
        &mut remote,
        &Seed::new("Remote global").chunk("Intro", "gateway timeout settings"),
    );
    insert(
        &mut remote,
        &Seed::new("Remote linked")
            .chunk("Intro", "gateway retry budget")
            .link(REPO, &[]),
    );

    let mut local = store();
    insert(
        &mut local,
        &Seed::new("Local global").chunk("Intro", "gateway notes of mine"),
    );
    insert(
        &mut local,
        &Seed::new("Local linked")
            .chunk("Intro", "gateway scratch")
            .link(REPO, &[]),
    );

    routed_over(local, remote, Routing::new(repository, global))
}

fn routed_over(local: SqliteStore, remote: SqliteStore, routing: Routing) -> RoutedStore {
    RoutedStore::new(Some(Box::new(local)), Some(Box::new(remote)), routing)
}

fn query(sort: Sort, descending: bool, offset: usize, limit: usize) -> ListQuery {
    ListQuery {
        filter: RepoFilter::All,
        format: None,
        text: String::new(),
        useful: None,
        sort,
        descending,
        offset,
        limit,
    }
}

fn titles(page: &DocumentPage) -> Vec<String> {
    page.items.iter().map(|row| row.doc.title.clone()).collect()
}

fn sorted_titles(store: &mut RoutedStore) -> Vec<String> {
    let mut found = titles(&store.list(&query(Sort::Title, false, 0, 50)).expect("list"));
    found.sort();
    found
}

#[test]
fn each_kind_is_shown_in_the_places_its_routing_includes() {
    let all = [Where::Remote, Where::Local, Where::RemoteLocal];

    for repository in all {
        for global in all {
            let mut routed = fixture(repository, global);
            let mut expected: Vec<&str> = Vec::new();

            if global.includes(Place::Remote) {
                expected.push("Remote global");
            }

            if repository.includes(Place::Remote) {
                expected.push("Remote linked");
            }

            if global.includes(Place::Local) {
                expected.push("Local global");
            }

            if repository.includes(Place::Local) {
                expected.push("Local linked");
            }

            expected.sort_unstable();

            let page = routed.list(&query(Sort::Title, false, 0, 50)).expect("list");
            assert_eq!(sorted_titles(&mut routed), expected, "{repository:?} {global:?}");
            assert_eq!(page.total, expected.len(), "{repository:?} {global:?}");

            let counts = routed.repos().expect("repos");
            let linked = expected.iter().filter(|title| title.ends_with("linked")).count();
            assert_eq!(counts.repos.first().map_or(0, |(_, docs)| *docs), linked);
            assert_eq!(counts.global, expected.len() - linked);
        }
    }
}

#[test]
fn a_filter_is_narrowed_to_the_kinds_a_place_shows() {
    // Linked documents only on this computer, global ones only remote.
    let mut routed = fixture(Where::Local, Where::Remote);
    let ask = |routed: &mut RoutedStore, filter: RepoFilter| {
        let mut asked = query(Sort::Title, false, 0, 50);
        asked.filter = filter;
        titles(&routed.list(&asked).expect("list"))
    };

    assert_eq!(ask(&mut routed, RepoFilter::Global), ["Remote global"]);
    assert_eq!(ask(&mut routed, RepoFilter::Linked), ["Local linked"]);
    assert_eq!(ask(&mut routed, RepoFilter::Repo(REPO.to_owned())), ["Local linked"]);
    assert_eq!(ask(&mut routed, RepoFilter::All), ["Local linked", "Remote global"]);
}

#[test]
fn a_document_the_routing_hides_is_not_found_by_id() {
    let mut routed = fixture(Where::Remote, Where::Remote);
    let local = routed.list(&query(Sort::Title, false, 0, 50)).expect("list");

    assert_eq!(titles(&local), ["Remote global", "Remote linked"]);

    let mut both = fixture(Where::RemoteLocal, Where::RemoteLocal);
    let page = both.list(&query(Sort::Title, false, 0, 50)).expect("list");
    let local_global = page
        .items
        .iter()
        .find(|row| row.doc.title == "Local global")
        .expect("local");
    assert_eq!(ids::place_of(local_global.doc.id), Place::Local);

    // The same id in a routing that does not show the local file.
    let mut remote_only = fixture(Where::Remote, Where::Remote);
    assert!(
        remote_only
            .get(local_global.doc.id, &Detail::default())
            .expect("get")
            .is_none()
    );
}

#[test]
fn writes_go_where_the_routing_says_and_a_place_it_does_not_allow_is_refused() {
    // Linked documents remote or here, global ones here.
    let mut routed = fixture(Where::RemoteLocal, Where::Local);
    let (doc, chunks, _) = Seed::new("Fresh").chunk("a", "fresh text").parts();

    let id = routed.put_document(&doc, &chunks, &[]).expect("a global document");
    assert_eq!(ids::place_of(id), Place::Local);
    assert!(ids::split(id).0.is_some());

    let link = Link {
        repo_key: REPO.to_owned(),
        globs: Vec::new(),
    };
    let (doc, chunks, _) = Seed::new("Fresh linked").chunk("a", "text").parts();
    let linked = routed
        .put_document(&doc, &chunks, std::slice::from_ref(&link))
        .expect("linked");
    assert_eq!(
        ids::place_of(linked),
        Place::Remote,
        "a linked document goes remote by default"
    );

    // A global document may not move to a place that keeps no global documents.
    assert!(routed.set_links(id, &[]).expect("no change of kind"));

    let mut strict = fixture(Where::Remote, Where::Local);
    let page = strict.list(&query(Sort::Title, false, 0, 10)).expect("list");
    let local_global = page
        .items
        .iter()
        .find(|row| row.doc.title == "Local global")
        .expect("doc")
        .doc
        .id;

    let error = strict.set_links(local_global, &[link]).expect_err("refused");
    assert!(
        matches!(&error, StoreError::Invalid(message) if message.contains("repository_documents")),
        "{error}"
    );
    assert!(strict.get(local_global, &Detail::default()).expect("get").is_some());
}

#[test]
fn rename_links_and_delete_act_on_the_documents_own_place() {
    let mut routed = fixture(Where::RemoteLocal, Where::RemoteLocal);
    let page = routed.list(&query(Sort::Title, false, 0, 10)).expect("list");
    let id_of = |title: &str| {
        page.items
            .iter()
            .find(|row| row.doc.title == title)
            .expect(title)
            .doc
            .id
    };
    let (local, remote) = (id_of("Local global"), id_of("Remote global"));

    assert!(routed.rename(local, "Mine").expect("rename"));
    assert!(routed.rename(remote, "Theirs").expect("rename"));
    assert!(!routed.rename(ids::tag(Place::Local, 9999), "x").expect("missing"));

    let found = routed.get(local, &Detail::default()).expect("get").expect("found");
    assert_eq!((found.doc.title.as_str(), found.doc.id), ("Mine", local));
    assert_eq!(
        routed
            .get(remote, &Detail::default())
            .expect("get")
            .expect("found")
            .doc
            .title,
        "Theirs"
    );

    // A plain id is the remote database's.
    let plain = ids::split(remote).1;
    assert_eq!(
        routed
            .get(plain, &Detail::default())
            .expect("get")
            .expect("found")
            .doc
            .title,
        "Theirs"
    );

    assert_eq!(routed.delete(&[local, remote, 424_242]).expect("delete"), 2);
    assert_eq!(
        titles(&routed.list(&query(Sort::Title, false, 0, 10)).expect("list")),
        ["Local linked", "Remote linked"]
    );
}

#[test]
fn a_place_without_a_database_is_empty() {
    let mut remote = store();
    insert(&mut remote, &Seed::new("Only remote").chunk("a", "gateway"));

    let mut routed = RoutedStore::new(
        None,
        Some(Box::new(remote)),
        Routing::new(Where::RemoteLocal, Where::RemoteLocal),
    );

    let page = routed.list(&query(Sort::Title, false, 0, 10)).expect("list");
    assert_eq!((page.total, titles(&page)), (1, vec!["Only remote".to_owned()]));
    assert!(
        routed
            .get(ids::tag(Place::Local, 1), &Detail::default())
            .expect("get")
            .is_none()
    );
    assert_eq!(routed.delete(&[ids::tag(Place::Local, 1)]).expect("delete"), 0);
    assert_eq!(search_titles(&mut routed, "gateway").len(), 1);
}

#[test]
fn the_chunks_of_a_document_that_match_come_from_its_own_place_with_its_ids() {
    let mut local = store();
    insert(
        &mut local,
        &Seed::new("Versions")
            .chunk("Intro", "nothing here")
            .chunk("Release", "version 1.2 is out")
            .chunk("Later", "version two, then 10 more"),
    );
    let mut routed = routed_over(local, store(), Routing::new(Where::RemoteLocal, Where::RemoteLocal));
    let page = routed.list(&query(Sort::Title, false, 0, 10)).expect("list");
    let id = page.items[0].doc.id;

    let found = routed.matching_chunks(id, "version 1", 10).expect("matches");
    assert_eq!(found.total, 1);
    assert_eq!(found.chunks[0].ordinal, 1);
    assert_eq!(ids::place_of(found.chunks[0].chunk_id), Place::Local);

    // The chunk id reads back through `get`.
    let chunk = routed
        .get(
            id,
            &Detail {
                sections: false,
                chunk: Some(found.chunks[0].chunk_id),
            },
        )
        .expect("get")
        .expect("found")
        .chunk
        .expect("chunk");
    assert!(chunk.text.contains("version 1.2"));
}

#[test]
fn pending_chunks_go_through_both_places_in_one_cursor_and_embeddings_return_to_their_place() {
    let mut routed = fixture(Where::RemoteLocal, Where::RemoteLocal);

    let first = routed.pending_chunks("m", 0, 3).expect("page");
    assert_eq!(first.len(), 3);
    let rest = routed
        .pending_chunks("m", first.last().expect("last").chunk_id, 10)
        .expect("page");
    assert_eq!(first.len() + rest.len(), 4);

    let places: Vec<Place> = first
        .iter()
        .chain(&rest)
        .map(|chunk| ids::place_of(chunk.chunk_id))
        .collect();
    assert_eq!(places, [Place::Remote, Place::Remote, Place::Local, Place::Local]);

    let all: Vec<(i64, Vec<f32>)> = first
        .iter()
        .chain(&rest)
        .map(|chunk| (chunk.chunk_id, axis(2)))
        .collect();
    assert_eq!(routed.set_chunk_embeddings("m", &all).expect("set"), 4);
    assert!(routed.pending_chunks("m", 0, 10).expect("page").is_empty());
}
