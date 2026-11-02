//! A remote database behind a network that can go down, for the tests of the offline fallback.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::backend::OpenError;
use crate::config::Secrets;
use crate::store::testing::{Seed, insert, store};
use crate::store::{
    Chunk, ChunkHit, ChunkMatches, Detail, Document, DocumentDetail, DocumentPage, KnowledgeStore, Link, ListQuery,
    PendingChunk, Reach, RepoCounts, Scope, SqliteStore, Stats, StoreError, TitleScope,
};

/// How long a failed attempt is not repeated in these tests.
pub(super) const COOLDOWN: Duration = Duration::from_millis(250);

/// How long a probe of the remote waits in these tests.
pub(super) const PROBE: Duration = Duration::from_secs(3);

/// The network between this computer and the remote: up or down, and how often it was dialled.
#[derive(Clone)]
pub(super) struct Net {
    up: Arc<AtomicBool>,
    dials: Arc<AtomicUsize>,
    /// Milliseconds a dial takes before it fails (a network that drops the packets).
    lag: Arc<AtomicUsize>,
    /// Another process holds the lock a counter needs.
    busy: Arc<AtomicBool>,
}

impl Net {
    pub(super) fn new(up: bool) -> Self {
        Self {
            up: Arc::new(AtomicBool::new(up)),
            dials: Arc::new(AtomicUsize::new(0)),
            lag: Arc::new(AtomicUsize::new(0)),
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Makes the counters of the remote answer that another process holds the lock.
    pub(super) fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::SeqCst);
    }

    /// Makes every failing dial take `lag` before it gives up.
    pub(super) fn set_lag(&self, lag: Duration) {
        self.lag
            .store(usize::try_from(lag.as_millis()).unwrap_or(usize::MAX), Ordering::SeqCst);
    }

    pub(super) fn set(&self, up: bool) {
        self.up.store(up, Ordering::SeqCst);
    }

    pub(super) fn dials(&self) -> usize {
        self.dials.load(Ordering::SeqCst)
    }

    fn counting(&self) -> Result<(), StoreError> {
        if self.busy.load(Ordering::SeqCst) {
            Err(StoreError::Busy)
        } else {
            Ok(())
        }
    }

    fn check(&self) -> Result<(), StoreError> {
        if self.up.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(StoreError::Unreachable("the connection was lost".to_owned()))
        }
    }
}

/// The remote's file seen through the network: every call fails while it is down.
pub(super) struct Flaky {
    pub(super) inner: SqliteStore,
    pub(super) net: Net,
}

impl KnowledgeStore for Flaky {
    fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        self.net.check()?;
        self.inner.meta(key)
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        self.net.check()?;
        self.inner.set_meta(key, value)
    }

    fn generation(&mut self) -> Result<i64, StoreError> {
        self.net.check()?;
        self.inner.generation()
    }

    fn put_document(&mut self, doc: &Document, chunks: &[Chunk], links: &[Link]) -> Result<i64, StoreError> {
        self.net.check()?;
        self.inner.put_document(doc, chunks, links)
    }

    fn find_by_content_sha(&mut self, sha: &str) -> Result<Option<Document>, StoreError> {
        self.net.check()?;
        self.inner.find_by_content_sha(sha)
    }

    fn find_by_source_key(&mut self, key: &str) -> Result<Option<Document>, StoreError> {
        self.net.check()?;
        self.inner.find_by_source_key(key)
    }

    fn reusable_embeddings(
        &mut self,
        model_id: &str,
        shas: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, StoreError> {
        self.net.check()?;
        self.inner.reusable_embeddings(model_id, shas)
    }

    fn pending_chunks(&mut self, model_id: &str, after: i64, limit: usize) -> Result<Vec<PendingChunk>, StoreError> {
        self.net.check()?;
        self.inner.pending_chunks(model_id, after, limit)
    }

    fn set_chunk_embeddings(&mut self, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StoreError> {
        self.net.check()?;
        self.inner.set_chunk_embeddings(model_id, embeddings)
    }

    fn record_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        self.net.check()?;
        self.net.counting()?;
        self.inner.record_shown(ids)
    }

    fn record_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.net.check()?;
        self.net.counting()?;
        self.inner.record_useful(ids, at)
    }

    fn find_by_title(&mut self, title: &str, scope: &TitleScope) -> Result<Vec<Document>, StoreError> {
        self.net.check()?;
        self.inner.find_by_title(title, scope)
    }

    fn docs_sharing_chunks(&mut self, chunk_shas: &[String]) -> Result<Vec<(i64, usize)>, StoreError> {
        self.net.check()?;
        self.inner.docs_sharing_chunks(chunk_shas)
    }

    fn remote_writable(&mut self) -> Option<bool> {
        self.net.check().ok()?;
        self.inner.remote_writable()
    }

    fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError> {
        self.net.check()?;
        self.inner.set_links(id, links)
    }

    fn rename(&mut self, id: i64, title: &str) -> Result<bool, StoreError> {
        self.net.check()?;
        self.inner.rename(id, title)
    }

    fn delete(&mut self, ids: &[i64]) -> Result<usize, StoreError> {
        self.net.check()?;
        self.inner.delete(ids)
    }

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        self.net.check()?;
        self.inner.list(query)
    }

    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError> {
        self.net.check()?;
        self.inner.get(id, detail)
    }

    fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.net.check()?;
        self.inner.keyword_top_k(query, scope, k)
    }

    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.net.check()?;
        self.inner.vector_top_k(vector, scope, k)
    }

    fn matching_chunks(&mut self, doc_id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError> {
        self.net.check()?;
        self.inner.matching_chunks(doc_id, text, limit)
    }

    fn repos(&mut self) -> Result<RepoCounts, StoreError> {
        self.net.check()?;
        self.inner.repos()
    }

    fn stats(&mut self) -> Result<Stats, StoreError> {
        self.net.check()?;
        self.inner.stats()
    }
}

/// A computer with a remote database behind `net` (a file with a global and a linked document) and
/// this computer's own (the same kinds), both shown.
pub(super) struct Machine {
    folder: tempfile::TempDir,
    pub(super) net: Net,
}

impl Machine {
    pub(super) fn new(up: bool) -> Self {
        let folder = tempfile::tempdir().expect("folder");
        let mut remote = SqliteStore::open(&folder.path().join("remote.sqlite3")).expect("remote file");

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

        Self {
            folder,
            net: Net::new(up),
        }
    }

    /// The database file the processes of this computer share their cooldown beside.
    pub(super) fn database(&self) -> PathBuf {
        self.folder.path().join("knowledge.sqlite3")
    }

    /// One process: its own routed store (and so its own connection) over the shared remote.
    pub(super) fn process(&self) -> RoutedStore {
        self.process_probing(PROBE)
    }

    /// One process that waits `probe` for the remote before it answers from this computer.
    pub(super) fn process_probing(&self, probe: Duration) -> RoutedStore {
        let net = self.net.clone();
        let file = self.folder.path().join("remote.sqlite3");
        let reach = Reach::beside_with(&self.database(), "remote:test", COOLDOWN);

        let remote = Remote::lazy(
            move |_patience| {
                net.dials.fetch_add(1, Ordering::SeqCst);

                if !net.up.load(Ordering::SeqCst) {
                    let lag = net.lag.load(Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(u64::try_from(lag).unwrap_or(0)));

                    return Err(OpenError::Unreachable("cannot connect".to_owned()));
                }

                let inner = SqliteStore::open(&file).map_err(|error| OpenError::Failed(error.to_string()))?;

                Ok(Some(Box::new(Flaky {
                    inner,
                    net: net.clone(),
                }) as Box<dyn KnowledgeStore>))
            },
            reach,
            probe,
            Secrets::default(),
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

        let routing = Routing::new(Where::RemoteLocal, Where::RemoteLocal);
        let mut routed = RoutedStore::new(Some(Box::new(local)), None, routing).with_remote(remote);
        routed.settle().expect("settles");

        routed
    }
}
