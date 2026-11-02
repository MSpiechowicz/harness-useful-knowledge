//! This computer's SQLite file and a remote database as one [`KnowledgeStore`].
//!
//! The documents of each [kind](crate::config::Kind) follow the [`Routing`]: a place shows a kind
//! when the routing includes the place for it (see [`Kinds`]). Everything this store hands out
//! carries its place in the id (see [`ids`]), and an id given back reaches the place it came from,
//! a plain id being the remote database's.
//!
//! - Reads ask every place that shows something and merge: [`list`](KnowledgeStore::list) merges the
//!   pages by the sort key ([`merge`]), a search is run by [`crate::search::run`] in each place and
//!   fused by rank ([`Kinds`] and [`Part`]).
//! - A write acts on the place of the id it is given. A new document goes where its kind's routing
//!   says by default, and a place the routing does not include for its kind refuses it.
//! - The write counter is the sum of the places', the metadata those of the remote database (this
//!   computer's when there is none).
//!
//! - While the remote database cannot be reached ([`Remote`]) the reads answer from this computer
//!   alone and [`KnowledgeStore::offline`] says so. An id of the remote database is then refused
//!   with [`REMOTE_DOWN`], and so is every write meant for it, never redirected to this computer.
//!
//! A place without a database (the local file that was never created, read-only) is empty.

mod embeddings;
mod guard;
pub mod ids;
mod kinds;
mod merge;
mod overview;
mod quality;
mod remote;
mod rows;

use std::collections::HashMap;

pub use kinds::{Kinds, Part, fuse_places, merge_found};
pub use remote::{Patience, Remote};

use crate::backend::OpenError;
use crate::config::{Kind, Place, Routing};
use crate::limits::LIST_PAGE_MAX;
use crate::store::{
    Chunk, ChunkHit, ChunkMatches, Detail, Document, DocumentDetail, DocumentPage, DocumentRow, KnowledgeStore, Link,
    ListQuery, PendingChunk, RepoCounts, Scope, Stats, StoreError, TitleScope,
};
use rows::{interleave, tag_detail, tag_document, tagged_hits};

/// The line that says a search, a list or a read comes from this computer alone, for the MCP replies
/// and the stderr of `search`.
pub const OFFLINE_NOTICE: &str = "Remote knowledge is unreachable. Showing documents on this computer only.";

/// What a read of a document of the remote database says while it cannot be reached.
pub const REMOTE_DOWN: &str = "remote knowledge is unreachable right now";

/// What a write meant for the remote database says while it cannot be reached.
pub const REMOTE_DOWN_WRITE: &str = "remote knowledge is unreachable right now, so nothing was changed there. Nothing is saved to this computer instead";

/// The stores of the two places, either of which may be absent.
pub struct RoutedStore {
    local: Option<Box<dyn KnowledgeStore>>,
    remote: Remote,
    routing: Routing,
}

impl RoutedStore {
    pub fn new(
        local: Option<Box<dyn KnowledgeStore>>,
        remote: Option<Box<dyn KnowledgeStore>>,
        routing: Routing,
    ) -> Self {
        Self {
            local,
            remote: Remote::fixed(remote),
            routing,
        }
    }

    /// The same store with `remote` as its remote place, opened when first needed.
    pub fn with_remote(mut self, remote: Remote) -> Self {
        self.remote = remote;
        self
    }

    /// Opens the remote database now, or finds it unreachable (see [`Remote::settle`]).
    pub fn settle(&mut self) -> Result<(), OpenError> {
        self.remote.settle()
    }

    /// True when neither place has a database, and the remote is not just out of reach.
    pub fn is_empty(&self) -> bool {
        self.local.is_none() && self.remote.is_none()
    }

    fn store(&mut self, place: Place) -> Option<&mut (dyn KnowledgeStore + 'static)> {
        match place {
            Place::Local => self.local.as_deref_mut(),
            Place::Remote => self.remote.store(),
        }
    }

    /// The kinds `place` shows.
    pub fn kinds(&self, place: Place) -> Kinds {
        Kinds {
            repository: self.routing.reads(Kind::Repository, place),
            global: self.routing.reads(Kind::Global, place),
        }
    }

    /// The stores that exist, remote first.
    fn present(&mut self) -> Result<Vec<Place>, StoreError> {
        self.ready()?;

        Ok([Place::Remote, Place::Local]
            .into_iter()
            .filter(|place| self.store(*place).is_some())
            .collect())
    }

    /// The store the metadata is read from.
    fn primary(&mut self) -> Result<Option<&mut (dyn KnowledgeStore + 'static)>, StoreError> {
        self.ready()?;

        Ok(match self.remote.store() {
            Some(store) => Some(store),
            None => self.local.as_deref_mut(),
        })
    }

    /// The error of a write the routing does not allow.
    fn refuse(&self, kind: Kind, place: Place) -> Result<(), StoreError> {
        self.routing.check_write(kind, place).map_err(StoreError::Invalid)
    }
}

impl KnowledgeStore for RoutedStore {
    fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        match self.primary()? {
            Some(store) => store.meta(key),
            None => Ok(None),
        }
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        self.writable(Place::Remote)?;

        match self.primary()? {
            Some(store) => store.set_meta(key, value),
            None => Err(StoreError::Invalid("there is no database to write to".to_owned())),
        }
    }

    fn generation(&mut self) -> Result<i64, StoreError> {
        let mut sum = 0;

        for place in self.present()? {
            if let Some(store) = self.store(place) {
                sum += store.generation()?;
            }
        }

        Ok(sum)
    }

    fn put_document(&mut self, doc: &Document, chunks: &[Chunk], links: &[Link]) -> Result<i64, StoreError> {
        let kind = Kind::of(links.len());
        let (place, inner) = match doc.id {
            0 => (self.routing.default_place(kind), 0),
            id => {
                let (place, inner) = ids::split(id);
                (place.unwrap_or(Place::Remote), inner)
            }
        };

        self.refuse(kind, place)?;
        self.writable(place)?;

        let store = self
            .store(place)
            .ok_or_else(|| StoreError::NotFound(format!("document {} not found", ids::show(doc.id))))?;
        let doc = Document {
            id: inner,
            ..doc.clone()
        };

        store.put_document(&doc, chunks, links).map(|id| ids::tag(place, id))
    }

    fn find_by_content_sha(&mut self, sha: &str) -> Result<Option<Document>, StoreError> {
        self.first_found(|store| store.find_by_content_sha(sha))
    }

    fn find_by_source_key(&mut self, key: &str) -> Result<Option<Document>, StoreError> {
        self.first_found(|store| store.find_by_source_key(key))
    }

    /// Counted in the place each id carries, see [`RoutedStore::count`].
    fn record_shown(&mut self, doc_ids: &[i64]) -> Result<(), StoreError> {
        self.count(doc_ids, |store, own| store.record_shown(own))
    }

    fn record_useful(&mut self, doc_ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.count(doc_ids, |store, own| store.record_useful(own, at))
    }

    fn find_by_title(&mut self, title: &str, scope: &TitleScope) -> Result<Vec<Document>, StoreError> {
        self.titled(title, scope)
    }

    fn docs_sharing_chunks(&mut self, chunk_shas: &[String]) -> Result<Vec<(i64, usize)>, StoreError> {
        self.sharing(chunk_shas)
    }

    fn remote_writable(&mut self) -> Option<bool> {
        self.remote_may_write()
    }

    fn reusable_embeddings(
        &mut self,
        model_id: &str,
        chunk_shas: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, StoreError> {
        let mut all = HashMap::new();

        for place in self.present()? {
            if let Some(store) = self.store(place) {
                all.extend(store.reusable_embeddings(model_id, chunk_shas)?);
            }
        }

        Ok(all)
    }

    /// The remote chunks first, then the local ones: the order of the tagged ids.
    fn pending_chunks(&mut self, model_id: &str, after: i64, limit: usize) -> Result<Vec<PendingChunk>, StoreError> {
        embeddings::pending(self, model_id, after, limit)
    }

    fn set_chunk_embeddings(&mut self, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StoreError> {
        embeddings::set(self, model_id, embeddings)
    }

    fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError> {
        let (place, inner) = ids::split(id);
        let place = place.unwrap_or(Place::Remote);

        self.writable(place)?;

        let Some((place, inner)) = self.resolve(ids::tag(place, inner))? else {
            return Ok(false);
        };

        self.refuse(Kind::of(links.len()), place)?;

        match self.store(place) {
            Some(store) => store.set_links(inner, links),
            None => Ok(false),
        }
    }

    fn rename(&mut self, id: i64, title: &str) -> Result<bool, StoreError> {
        self.writable(ids::place_of(id))?;

        match self.resolve(id)? {
            Some((place, inner)) => match self.store(place) {
                Some(store) => store.rename(inner, title),
                None => Ok(false),
            },
            None => Ok(false),
        }
    }

    fn delete(&mut self, doc_ids: &[i64]) -> Result<usize, StoreError> {
        let mut deleted = 0;

        for place in [Place::Remote, Place::Local] {
            let own: Vec<i64> = doc_ids
                .iter()
                .filter(|id| ids::place_of(**id) == place)
                .map(|id| ids::split(*id).1)
                .collect();

            if own.is_empty() {
                continue;
            }

            self.writable(place)?;

            if let Some(store) = self.store(place) {
                deleted += store.delete(&own)?;
            }
        }

        Ok(deleted)
    }

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        self.list_in(None, query)
    }

    /// The merged list of the places, or of `only` alone: the page and the total are of that set.
    /// The remote database that cannot be reached is an error when it is the one asked for.
    fn list_in(&mut self, only: Option<Place>, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        self.ready()?;

        if only == Some(Place::Remote) && self.remote.is_down() {
            return Err(StoreError::Unreachable(REMOTE_DOWN.to_owned()));
        }

        let want = query.offset.saturating_add(query.limit);
        let mut rows: Vec<DocumentRow> = Vec::new();
        let mut total = 0;

        for place in [Place::Remote, Place::Local] {
            if only.is_some_and(|only| only != place) {
                continue;
            }

            let Some(filter) = merge::filter_for(&query.filter, self.kinds(place)) else {
                continue;
            };
            let Some(store) = self.store(place) else { continue };

            let page = merge::fetch(store, query, filter, want);
            let Some(page) = self.read(place, page)? else {
                if only.is_some() {
                    return Err(StoreError::Unreachable(REMOTE_DOWN.to_owned()));
                }

                continue;
            };
            total += page.total;

            for mut row in page.items {
                tag_document(place, &mut row.doc);
                rows.push(row);
            }
        }

        rows.sort_by(|a, b| merge::compare(a, b, query.sort, query.descending));

        let items = rows
            .into_iter()
            .skip(query.offset)
            .take(query.limit.min(LIST_PAGE_MAX))
            .collect();

        Ok(DocumentPage { items, total })
    }

    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError> {
        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(None);
        };

        let kinds = self.kinds(place);
        let inner_detail = Detail {
            sections: detail.sections,
            chunk: detail.chunk.map(|chunk| ids::split(chunk).1),
        };

        let Some(store) = self.store(place) else {
            return Ok(None);
        };
        let found = store.get(inner, &inner_detail);
        let Some(mut found) = self.answer(place, found)? else {
            return Ok(None);
        };

        if !kinds.allows(Kind::of(found.links.len())) {
            return Ok(None);
        }

        tag_detail(place, &mut found);
        Ok(Some(found))
    }

    /// The best `k` chunks of each place, by rank in its place: the first of one, the first of the
    /// other, and so on. Scores of different databases are not comparable, so none is compared
    /// ([`crate::search::run`] does the fusion that matters).
    fn keyword_top_k(&mut self, query: &str, scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.ready()?;

        let mut lists: Vec<Vec<ChunkHit>> = Vec::new();

        for place in [Place::Remote, Place::Local] {
            let kinds = self.kinds(place);
            let Some(store) = self.store(place) else { continue };

            let found = store
                .keyword_top_k(query, scope, k)
                .and_then(|found| kinds.keep(store, found));

            if let Some(hits) = self.read(place, found)? {
                lists.push(tagged_hits(place, hits));
            }
        }

        Ok(interleave(lists, k))
    }

    /// As [`RoutedStore::keyword_top_k`], for the vectors.
    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        self.ready()?;

        let mut lists: Vec<Vec<ChunkHit>> = Vec::new();

        for place in [Place::Remote, Place::Local] {
            let kinds = self.kinds(place);
            let Some(store) = self.store(place) else { continue };

            let found = store
                .vector_top_k(vector, scope, k)
                .and_then(|found| kinds.keep(store, found));

            if let Some(hits) = self.read(place, found)? {
                lists.push(tagged_hits(place, hits));
            }
        }

        Ok(interleave(lists, k))
    }

    fn matching_chunks(&mut self, doc_id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError> {
        let Some((place, inner)) = self.resolve(doc_id)? else {
            return Ok(ChunkMatches::default());
        };

        let Some(store) = self.store(place) else {
            return Ok(ChunkMatches::default());
        };
        let found = store.matching_chunks(inner, text, limit);
        let mut found = self.answer(place, found)?;

        for chunk in &mut found.chunks {
            chunk.chunk_id = ids::tag(place, chunk.chunk_id);
        }

        Ok(found)
    }

    fn routed_parts(&mut self) -> Option<Vec<Part<'_>>> {
        // A failure to settle comes back from the call that reads the places one by one.
        self.ready().ok()?;

        let remote_kinds = self.kinds(Place::Remote);
        let local_kinds = self.kinds(Place::Local);
        let mut parts = Vec::new();

        if let Some(store) = self.remote.store() {
            parts.push(Part {
                place: Place::Remote,
                store,
                kinds: remote_kinds,
            });
        }

        if let Some(store) = self.local.as_deref_mut() {
            parts.push(Part {
                place: Place::Local,
                store,
                kinds: local_kinds,
            });
        }

        Some(parts)
    }

    fn repos(&mut self) -> Result<RepoCounts, StoreError> {
        overview::repos(self)
    }

    fn stats(&mut self) -> Result<Stats, StoreError> {
        overview::stats(self)
    }

    fn offline(&self) -> bool {
        self.remote.is_down()
    }

    fn remote_lost(&mut self, error: &StoreError) -> bool {
        self.remote.lost(error)
    }
}

#[cfg(test)]
mod tests;
