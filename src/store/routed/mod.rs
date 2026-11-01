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
//! A place without a database (the local file that was never created, read-only) is empty.

pub mod ids;
mod kinds;
mod merge;
mod rows;

use std::collections::HashMap;

pub use kinds::{Kinds, Part, fuse_places, merge_found};

use crate::config::{Kind, Place, Routing};
use crate::limits::LIST_PAGE_MAX;
use crate::store::{
    Chunk, ChunkHit, ChunkMatches, Detail, Document, DocumentDetail, DocumentPage, DocumentRow, KnowledgeStore, Link,
    ListQuery, PendingChunk, RepoCounts, Scope, Stats, StoreError,
};
use rows::{interleave, tag_detail, tag_document, tagged_hits};

/// The stores of the two places, either of which may be absent.
pub struct RoutedStore {
    local: Option<Box<dyn KnowledgeStore>>,
    remote: Option<Box<dyn KnowledgeStore>>,
    routing: Routing,
}

impl RoutedStore {
    pub fn new(
        local: Option<Box<dyn KnowledgeStore>>,
        remote: Option<Box<dyn KnowledgeStore>>,
        routing: Routing,
    ) -> Self {
        Self { local, remote, routing }
    }

    fn store(&mut self, place: Place) -> Option<&mut (dyn KnowledgeStore + 'static)> {
        match place {
            Place::Local => self.local.as_deref_mut(),
            Place::Remote => self.remote.as_deref_mut(),
        }
    }

    /// The kinds `place` shows.
    pub fn kinds(&self, place: Place) -> Kinds {
        Kinds {
            repository: self.routing.reads(Kind::Repository, place),
            global: self.routing.reads(Kind::Global, place),
        }
    }

    /// The place an id is for and the id in that place's store, when the place has a store.
    fn resolve(&mut self, id: i64) -> Option<(Place, i64)> {
        let (place, inner) = ids::split(id);
        let place = place.unwrap_or(Place::Remote);

        self.store(place).map(|_| (place, inner))
    }

    /// The stores that exist, remote first.
    fn present(&mut self) -> Vec<Place> {
        [Place::Remote, Place::Local]
            .into_iter()
            .filter(|place| self.store(*place).is_some())
            .collect()
    }

    /// The store the metadata is read from.
    fn primary(&mut self) -> Option<&mut (dyn KnowledgeStore + 'static)> {
        match self.remote.as_deref_mut() {
            Some(store) => Some(store),
            None => self.local.as_deref_mut(),
        }
    }

    /// The error of a write the routing does not allow.
    fn refuse(&self, kind: Kind, place: Place) -> Result<(), StoreError> {
        self.routing.check_write(kind, place).map_err(StoreError::Invalid)
    }
}

impl KnowledgeStore for RoutedStore {
    fn meta(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        match self.primary() {
            Some(store) => store.meta(key),
            None => Ok(None),
        }
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        match self.primary() {
            Some(store) => store.set_meta(key, value),
            None => Err(StoreError::Invalid("there is no database to write to".to_owned())),
        }
    }

    fn generation(&mut self) -> Result<i64, StoreError> {
        let mut sum = 0;

        for place in self.present() {
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
        for place in self.present() {
            if let Some(mut found) = self
                .store(place)
                .and_then(|store| store.find_by_content_sha(sha).transpose())
            {
                if let Ok(doc) = &mut found {
                    tag_document(place, doc);
                }

                return found.map(Some);
            }
        }

        Ok(None)
    }

    fn find_by_source_key(&mut self, key: &str) -> Result<Option<Document>, StoreError> {
        for place in self.present() {
            if let Some(mut found) = self
                .store(place)
                .and_then(|store| store.find_by_source_key(key).transpose())
            {
                if let Ok(doc) = &mut found {
                    tag_document(place, doc);
                }

                return found.map(Some);
            }
        }

        Ok(None)
    }

    fn reusable_embeddings(
        &mut self,
        model_id: &str,
        chunk_shas: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, StoreError> {
        let mut all = HashMap::new();

        for place in self.present() {
            if let Some(store) = self.store(place) {
                all.extend(store.reusable_embeddings(model_id, chunk_shas)?);
            }
        }

        Ok(all)
    }

    /// The remote chunks first, then the local ones: the order of the tagged ids.
    fn pending_chunks(&mut self, model_id: &str, after: i64, limit: usize) -> Result<Vec<PendingChunk>, StoreError> {
        let (after_place, inner) = ids::split(after);
        let mut page: Vec<PendingChunk> = Vec::new();

        for place in [Place::Remote, Place::Local] {
            let start = match (after_place, place) {
                (Some(Place::Local), Place::Remote) => continue,
                (Some(given), place) if given == place => inner,
                (None, Place::Remote) => inner,
                _ => 0,
            };
            let room = limit.saturating_sub(page.len());

            if room == 0 {
                break;
            }

            let Some(store) = self.store(place) else { continue };

            for mut chunk in store.pending_chunks(model_id, start, room)? {
                chunk.chunk_id = ids::tag(place, chunk.chunk_id);
                chunk.doc_id = ids::tag(place, chunk.doc_id);
                page.push(chunk);
            }
        }

        Ok(page)
    }

    fn set_chunk_embeddings(&mut self, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, StoreError> {
        let mut done = 0;

        for place in [Place::Remote, Place::Local] {
            let own: Vec<(i64, Vec<f32>)> = embeddings
                .iter()
                .filter(|(id, _)| ids::place_of(*id) == place)
                .map(|(id, vector)| (ids::split(*id).1, vector.clone()))
                .collect();

            if own.is_empty() {
                continue;
            }

            if let Some(store) = self.store(place) {
                done += store.set_chunk_embeddings(model_id, &own)?;
            }
        }

        Ok(done)
    }

    fn set_links(&mut self, id: i64, links: &[Link]) -> Result<bool, StoreError> {
        let Some((place, inner)) = self.resolve(id) else {
            return Ok(false);
        };

        self.refuse(Kind::of(links.len()), place)?;

        match self.store(place) {
            Some(store) => store.set_links(inner, links),
            None => Ok(false),
        }
    }

    fn rename(&mut self, id: i64, title: &str) -> Result<bool, StoreError> {
        match self.resolve(id) {
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

            if let Some(store) = self.store(place) {
                deleted += store.delete(&own)?;
            }
        }

        Ok(deleted)
    }

    fn list(&mut self, query: &ListQuery) -> Result<DocumentPage, StoreError> {
        let want = query.offset.saturating_add(query.limit);
        let mut rows: Vec<DocumentRow> = Vec::new();
        let mut total = 0;

        for place in [Place::Remote, Place::Local] {
            let Some(filter) = merge::filter_for(&query.filter, self.kinds(place)) else {
                continue;
            };
            let Some(store) = self.store(place) else { continue };

            let page = merge::fetch(store, query, filter, want)?;
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
        let Some((place, inner)) = self.resolve(id) else {
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
        let Some(mut found) = store.get(inner, &inner_detail)? else {
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
        let mut lists: Vec<Vec<ChunkHit>> = Vec::new();

        for place in [Place::Remote, Place::Local] {
            let kinds = self.kinds(place);

            if let Some(store) = self.store(place) {
                let found = store.keyword_top_k(query, scope, k)?;
                lists.push(tagged_hits(place, kinds.keep(store, found)?));
            }
        }

        Ok(interleave(lists, k))
    }

    /// As [`RoutedStore::keyword_top_k`], for the vectors.
    fn vector_top_k(&mut self, vector: &[f32], scope: &Scope, k: usize) -> Result<Vec<ChunkHit>, StoreError> {
        let mut lists: Vec<Vec<ChunkHit>> = Vec::new();

        for place in [Place::Remote, Place::Local] {
            let kinds = self.kinds(place);

            if let Some(store) = self.store(place) {
                let found = store.vector_top_k(vector, scope, k)?;
                lists.push(tagged_hits(place, kinds.keep(store, found)?));
            }
        }

        Ok(interleave(lists, k))
    }

    fn matching_chunks(&mut self, doc_id: i64, text: &str, limit: usize) -> Result<ChunkMatches, StoreError> {
        let Some((place, inner)) = self.resolve(doc_id) else {
            return Ok(ChunkMatches::default());
        };

        let Some(store) = self.store(place) else {
            return Ok(ChunkMatches::default());
        };
        let mut found = store.matching_chunks(inner, text, limit)?;

        for chunk in &mut found.chunks {
            chunk.chunk_id = ids::tag(place, chunk.chunk_id);
        }

        Ok(found)
    }

    fn routed_parts(&mut self) -> Option<Vec<Part<'_>>> {
        let local = Kinds {
            repository: self.routing.reads(Kind::Repository, Place::Local),
            global: self.routing.reads(Kind::Global, Place::Local),
        };
        let remote = Kinds {
            repository: self.routing.reads(Kind::Repository, Place::Remote),
            global: self.routing.reads(Kind::Global, Place::Remote),
        };

        let mut parts = Vec::new();

        if let Some(store) = self.remote.as_deref_mut() {
            parts.push(Part {
                place: Place::Remote,
                store,
                kinds: remote,
            });
        }

        if let Some(store) = self.local.as_deref_mut() {
            parts.push(Part {
                place: Place::Local,
                store,
                kinds: local,
            });
        }

        Some(parts)
    }

    fn repos(&mut self) -> Result<RepoCounts, StoreError> {
        let mut merged = RepoCounts::default();
        let mut by_key: HashMap<String, usize> = HashMap::new();

        for place in [Place::Remote, Place::Local] {
            let kinds = self.kinds(place);

            if let Some(store) = self.store(place) {
                let counts = store.repos()?;

                if kinds.global {
                    merged.global += counts.global;
                }

                if kinds.repository {
                    for (key, docs) in counts.repos {
                        *by_key.entry(key).or_default() += docs;
                    }
                }
            }
        }

        let mut repos: Vec<(String, usize)> = by_key.into_iter().collect();
        repos.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        merged.repos = repos;

        Ok(merged)
    }

    /// The sums of the places' counts, over everything each holds whatever its routing shows. The
    /// model and the schema are the remote database's (this computer's when there is none).
    fn stats(&mut self) -> Result<Stats, StoreError> {
        let mut sum: Option<Stats> = None;

        for place in [Place::Remote, Place::Local] {
            let Some(store) = self.store(place) else { continue };
            let stats = store.stats()?;

            sum = Some(match sum {
                None => stats,
                Some(first) => Stats {
                    documents: first.documents + stats.documents,
                    chunks: first.chunks + stats.chunks,
                    embedded_chunks: first.embedded_chunks + stats.embedded_chunks,
                    bytes: first.bytes + stats.bytes,
                    global_documents: first.global_documents + stats.global_documents,
                    // A repository can have documents in both places: the larger count is a lower bound.
                    repositories: first.repositories.max(stats.repositories),
                    generation: first.generation + stats.generation,
                    schema_version: first.schema_version,
                    model_id: first.model_id.or(stats.model_id),
                },
            });
        }

        Ok(sum.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests;
