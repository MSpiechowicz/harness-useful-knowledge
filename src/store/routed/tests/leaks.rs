//! A repository that is a folder of this computer (`path:`) is never sent to the team's database:
//! its half of a path-scoped query is the global documents alone.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::project::ProjectKey;
use crate::search::{self, Query, Tuning};
use crate::store::testing::{Seed, insert, store};
use crate::store::{
    Chunk, ChunkHit, ChunkMatches, Comment, Document, History, Link, NewComment, PendingChunk, PendingVersion,
    RepoCounts, ReviewChange, ReviewReply, Scope, SectionText, Stats, StoreError, StoredFile, TitleScope, Visibility,
};

const FOLDER: &str = "path:/home/secret-owner/projects/ledger";

/// A remote that writes down what it is asked about a repository.
struct Recorder {
    store: SqliteStore,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn note(&self, what: String) {
        self.asked.lock().expect("log").push(what);
    }
}

macro_rules! delegate {
    ($( fn $name:ident(&mut self $(, $arg:ident: $ty:ty)*) -> $ret:ty; )*) => {
        $( fn $name(&mut self $(, $arg: $ty)*) -> $ret { self.store.$name($($arg),*) } )*
    };
}

impl KnowledgeStore for Recorder {
    delegate! {
        fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError>;
        fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError>;
        fn set_metas(&mut self, entries: &[(&str, &str)]) -> Result<(), StoreError>;
        fn generation(&mut self) -> Result<i64, StoreError>;
        fn put_document(&mut self, doc: &Document, chunks: &[Chunk], links: &[Link]) -> Result<i64, StoreError>;
        fn find_by_content_sha(&mut self, sha: &str, visibility: Visibility) -> Result<Option<Document>, StoreError>;
        fn reusable_embeddings(&mut self, model_id: &str, chunk_shas: &[String]) -> Result<HashMap<String, Vec<f32>>, StoreError>;
        fn pending_chunks(&mut self, model_id: &str, after: i64, limit: usize) -> Result<Vec<PendingChunk>, StoreError>;
        fn set_chunk_embeddings(&mut self, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StoreError>;
        fn docs_sharing_chunks(&mut self, chunk_shas: &[String], visibility: Visibility) -> Result<Vec<(i64, usize)>, StoreError>;
        fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError>;
        fn rename(&mut self, id: i64, title: &str, by: Option<&str>, at: i64) -> Result<bool, StoreError>;
        fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError>;
        fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError>;
        fn matching_chunks(&mut self, doc_id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError>;
        fn pending_of(&mut self, id: i64) -> Result<Option<PendingVersion>, StoreError>;
        fn set_review(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError>;
        fn promote(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError>;
        fn section_texts(&mut self, id: i64) -> Result<Option<Vec<SectionText>>, StoreError>;
        fn asked_sections(&mut self, id: i64, revision: i64) -> Result<Option<Vec<SectionText>>, StoreError>;
        fn add_comments(&mut self, id: i64, by: Option<&str>, at: i64, revision: i64, comments: &[NewComment]) -> Result<Vec<i64>, StoreError>;
        fn reply_comment(&mut self, id: i64, parent: i64, by: Option<&str>, at: i64, body: &str) -> Result<Option<i64>, StoreError>;
        fn resolve_comment(&mut self, id: i64, comment: i64, resolved: bool, by: Option<&str>, at: i64) -> Result<bool, StoreError>;
        fn delete_comment(&mut self, id: i64, comment: i64) -> Result<Option<Vec<i64>>, StoreError>;
        fn comments(&mut self, id: i64) -> Result<Vec<Comment>, StoreError>;
        fn put_file(&mut self, id: i64, name: &str, bytes: &[u8]) -> Result<bool, StoreError>;
        fn file(&mut self, id: i64) -> Result<Option<StoredFile>, StoreError>;
        fn repos(&mut self) -> Result<RepoCounts, StoreError>;
        fn stats(&mut self) -> Result<Stats, StoreError>;
    }

    fn history(&mut self, id: i64) -> Result<Option<History>, StoreError> {
        self.note(format!("history {id}"));
        self.store.history(id)
    }

    fn writers_since_live(&mut self, id: i64) -> Result<Vec<String>, StoreError> {
        self.note(format!("writers {id}"));
        self.store.writers_since_live(id)
    }

    fn authorship_of(
        &mut self,
        ids: &[i64],
        per_doc: usize,
    ) -> Result<HashMap<i64, crate::store::Authorship>, StoreError> {
        self.note(format!("authorship {}", ids.len()));
        self.store.authorship_of(ids, per_doc)
    }

    fn find_by_source_key(&mut self, key: &str, visibility: Visibility) -> Result<Option<Document>, StoreError> {
        self.note(format!("source {key}"));
        self.store.find_by_source_key(key, visibility)
    }

    fn find_by_title(
        &mut self,
        title: &str,
        scope: &TitleScope,
        visibility: Visibility,
    ) -> Result<Vec<Document>, StoreError> {
        self.note(format!("title {scope:?}"));
        self.store.find_by_title(title, scope, visibility)
    }

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        self.note(format!("list {:?}", query.filter));
        self.store.list(query)
    }

    fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.note(format!("keyword {:?}", scope.repo_key()));
        self.store.keyword_top_k(query, scope, k)
    }

    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.note(format!("vector {:?}", scope.repo_key()));
        self.store.vector_top_k(vector, scope, k)
    }
}

fn path_scope() -> Scope {
    Scope::from_repo(Some(ProjectKey {
        key: FOLDER.to_owned(),
        root: PathBuf::new(),
        remote: None,
    }))
}

/// A team database with a global and a git-linked document, recorded, over a local file whose
/// documents are linked to the folder.
fn recorded() -> (RoutedStore, Arc<Mutex<Vec<String>>>) {
    let mut team = store();
    insert(
        &mut team,
        &Seed::new("Team global").chunk("Intro", "gateway timeout settings"),
    );
    insert(
        &mut team,
        &Seed::new("Team linked")
            .chunk("Intro", "gateway retry budget")
            .link(REPO, &[]),
    );

    let mut local = store();
    insert(
        &mut local,
        &Seed::new("Folder notes")
            .chunk("Intro", "gateway notes of mine")
            .link(FOLDER, &[]),
    );

    let asked = Arc::new(Mutex::new(Vec::new()));
    let remote = Recorder {
        store: team,
        asked: Arc::clone(&asked),
    };
    let routed = RoutedStore::new(
        Some(Box::new(local)),
        Some(Box::new(remote)),
        Routing::new(Where::RemoteLocal, Where::RemoteLocal),
    );

    (routed, asked)
}

fn asked(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().expect("log").clone()
}

#[test]
fn a_path_scoped_search_reaches_the_team_database_without_the_folder_and_finds_its_global_documents() {
    let (mut routed, log) = recorded();
    let scope = path_scope();
    let query = Query {
        text: "gateway",
        scope: &scope,
        paths: &[],
        limit: 10,
    };

    let found = search::run(&mut routed, None, &query, Tuning::SEARCH).expect("search");
    let mut titles: Vec<String> = found.hits.into_iter().map(|scored| scored.hit.title).collect();
    titles.sort();

    // The global team document and the folder's own document on this computer, not the git-linked one.
    assert_eq!(titles, ["Folder notes", "Team global"]);

    let calls = asked(&log);
    assert!(!calls.is_empty(), "the team database was asked");
    assert!(
        calls
            .iter()
            .all(|call| call.ends_with("None") && !call.contains("secret-owner")),
        "{calls:?}"
    );

    // The routed store's own reads do the same.
    let direct = routed.keyword_top_k("gateway", &scope, 10).expect("keyword");
    let mut titles: Vec<String> = direct.into_iter().map(|hit| hit.title).collect();
    titles.sort();
    assert_eq!(titles, ["Folder notes", "Team global"]);

    let calls = asked(&log);
    assert!(calls.iter().all(|call| !call.contains("secret-owner")), "{calls:?}");
}

#[test]
fn a_list_and_a_title_lookup_of_a_folder_never_reach_the_team_database() {
    let (mut routed, log) = recorded();

    let mut query = query(Sort::Title, false, 0, 50);
    query.filter = RepoFilter::Repo(FOLDER.to_owned());
    let page = routed.list(&query).expect("list");

    assert_eq!(titles(&page), ["Folder notes"], "this computer's own document");
    assert_eq!(asked(&log), Vec::<String>::new(), "the team database was not asked");

    let found = routed
        .find_by_title("Folder notes", &TitleScope::Repo(FOLDER.to_owned()), Visibility::Live)
        .expect("titles");
    assert_eq!(found.len(), 1);
    assert_eq!(asked(&log), Vec::<String>::new());

    // A list of everything still reaches it, and without any repository.
    let all = routed.list(&self::query(Sort::Title, false, 0, 50)).expect("list");
    assert_eq!(all.total, 3);
    assert_eq!(asked(&log), ["list All"]);
}

#[test]
fn a_similar_lookup_for_a_folder_never_asks_the_team_database_for_its_source_key_or_title() {
    use crate::import::similar::{Probe, find_visible};

    let (mut routed, log) = recorded();
    let probe = Probe {
        title: "Folder notes".to_owned(),
        content_sha: "none".to_owned(),
        chunk_shas: Vec::new(),
        source_key: "path-0123456789ab/ledger/notes.md".to_owned(),
        scope: TitleScope::Repo(FOLDER.to_owned()),
    };

    // Found on this computer by its title, and the team database was asked about neither.
    let found = find_visible(&mut routed, &probe, Visibility::WithDrafts)
        .expect("lookup")
        .expect("a match");
    assert_eq!(found.doc.title, "Folder notes");
    assert_eq!(asked(&log), Vec::<String>::new());

    // A source key of the team still is asked about there.
    assert!(
        routed
            .find_by_source_key("global/guide.md", Visibility::Live)
            .expect("lookup")
            .is_none()
    );
    assert_eq!(asked(&log), ["source global/guide.md"]);
}

#[test]
fn a_list_reads_the_authors_of_all_its_waiting_rows_in_one_call() {
    use crate::store::{ReviewAction, ReviewChange};

    let mut team = store();
    let mut waiting = Vec::new();

    for number in 0..8 {
        let id = insert(
            &mut team,
            &Seed::new(&format!("Draft {number}"))
                .source(&format!("/docs/{number}"))
                .status(crate::store::Status::Draft)
                .by("Ada", None)
                .chunk("Intro", "alpha"),
        );
        // Some are renamed by others, so the lists differ.
        if number % 2 == 0 {
            assert!(
                team.rename(id, &format!("Renamed {number}"), Some("Cy"), 20)
                    .expect("rename")
            );
        }

        let expect = team
            .get(
                id,
                &Detail {
                    drafts: true,
                    ..Detail::default()
                },
            )
            .expect("get")
            .expect("row")
            .doc
            .review_rev;
        team.set_review(id, expect, &ReviewChange::new(ReviewAction::Ask, Some("Bob"), 30))
            .expect("ask");
        waiting.push(id);
    }

    let log = Arc::new(Mutex::new(Vec::new()));
    let remote = Recorder {
        store: team,
        asked: Arc::clone(&log),
    };
    let mut routed = RoutedStore::new(None, Some(Box::new(remote)), Routing::new(Where::Remote, Where::Remote));

    let page = crate::cli_docs::list_on(&mut routed, None, &query(Sort::Title, false, 0, 50)).expect("list");
    let items = page["items"].as_array().expect("items");

    assert_eq!(items.len(), 8);
    assert!(items.iter().all(|item| {
        item["authors"]
            .as_array()
            .is_some_and(|names| names.contains(&json!("Bob")))
    }));
    assert_eq!(
        asked(&log),
        ["list All", "authorship 8"],
        "one read for the authors of all, none per row"
    );
}
