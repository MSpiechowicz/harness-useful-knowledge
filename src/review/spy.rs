//! A store that counts the reads of the review's listings and lets a test change a document at the
//! moment a review writes its comments (a change that lands meanwhile).

use std::collections::HashMap;

use crate::store::{
    Chunk, ChunkHit, ChunkMatches, Comment, Detail, Document, DocumentDetail, DocumentPage, History, KnowledgeStore,
    Link, ListQuery, NewComment, PendingChunk, PendingVersion, RepoCounts, ReviewChange, ReviewReply, Scope,
    SectionText, SqliteStore, Stats, StoreError, StoredFile, TitleScope, Visibility,
};

/// What a test does to the inner store just before comments are added (once).
pub(super) type Race = Box<dyn FnMut(&mut SqliteStore) + Send>;

pub(super) struct Spy {
    pub(super) store: SqliteStore,
    /// How many times `list`, `get` and `history` ran.
    pub(super) reads: usize,
    pub(super) race: Option<Race>,
}

impl Spy {
    pub(super) fn new(store: SqliteStore) -> Self {
        Self {
            store,
            reads: 0,
            race: None,
        }
    }
}

macro_rules! delegate {
    ($( fn $name:ident(&mut self $(, $arg:ident: $ty:ty)*) -> $ret:ty; )*) => {
        $( fn $name(&mut self $(, $arg: $ty)*) -> $ret { self.store.$name($($arg),*) } )*
    };
}

impl KnowledgeStore for Spy {
    delegate! {
        fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError>;
        fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError>;
        fn set_metas(&mut self, entries: &[(&str, &str)]) -> Result<(), StoreError>;
        fn generation(&mut self) -> Result<i64, StoreError>;
        fn put_document(&mut self, doc: &Document, chunks: &[Chunk], links: &[Link]) -> Result<i64, StoreError>;
        fn find_by_content_sha(&mut self, sha: &str, visibility: Visibility) -> Result<Option<Document>, StoreError>;
        fn find_by_source_key(&mut self, key: &str, visibility: Visibility) -> Result<Option<Document>, StoreError>;
        fn reusable_embeddings(&mut self, model_id: &str, chunk_shas: &[String]) -> Result<HashMap<String, Vec<f32>>, StoreError>;
        fn pending_chunks(&mut self, model_id: &str, after: i64, limit: usize) -> Result<Vec<PendingChunk>, StoreError>;
        fn set_chunk_embeddings(&mut self, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StoreError>;
        fn find_by_title(&mut self, title: &str, scope: &TitleScope, visibility: Visibility) -> Result<Vec<Document>, StoreError>;
        fn docs_sharing_chunks(&mut self, chunk_shas: &[String], visibility: Visibility) -> Result<Vec<(i64, usize)>, StoreError>;
        fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError>;
        fn rename(&mut self, id: i64, title: &str, by: Option<&str>, at: i64) -> Result<bool, StoreError>;
        fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError>;
        fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError>;
        fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError>;
        fn matching_chunks(&mut self, doc_id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError>;
        fn pending_of(&mut self, id: i64) -> Result<Option<PendingVersion>, StoreError>;
        fn set_review(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError>;
        fn promote(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError>;
        fn section_texts(&mut self, id: i64) -> Result<Option<Vec<SectionText>>, StoreError>;
        fn asked_sections(&mut self, id: i64, revision: i64) -> Result<Option<Vec<SectionText>>, StoreError>;
        fn reply_comment(&mut self, id: i64, parent: i64, by: Option<&str>, at: i64, body: &str) -> Result<Option<i64>, StoreError>;
        fn resolve_comment(&mut self, id: i64, comment: i64, resolved: bool, by: Option<&str>, at: i64) -> Result<bool, StoreError>;
        fn delete_comment(&mut self, id: i64, comment: i64) -> Result<Option<Vec<i64>>, StoreError>;
        fn comments(&mut self, id: i64) -> Result<Vec<Comment>, StoreError>;
        fn put_file(&mut self, id: i64, name: &str, bytes: &[u8]) -> Result<bool, StoreError>;
        fn file(&mut self, id: i64) -> Result<Option<StoredFile>, StoreError>;
        fn repos(&mut self) -> Result<RepoCounts, StoreError>;
        fn stats(&mut self) -> Result<Stats, StoreError>;
    }

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        self.reads += 1;
        self.store.list(query)
    }

    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError> {
        self.reads += 1;
        self.store.get(id, detail)
    }

    fn history(&mut self, id: i64) -> Result<Option<History>, StoreError> {
        self.reads += 1;
        self.store.history(id)
    }

    fn writers_since_live(&mut self, id: i64) -> Result<Vec<String>, StoreError> {
        self.reads += 1;
        self.store.writers_since_live(id)
    }

    fn add_comments(
        &mut self,
        id: i64,
        by: Option<&str>,
        at: i64,
        revision: i64,
        comments: &[NewComment],
    ) -> Result<Vec<i64>, StoreError> {
        if let Some(mut race) = self.race.take() {
            race(&mut self.store);
        }

        self.store.add_comments(id, by, at, revision, comments)
    }
}
