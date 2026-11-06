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
    AuthorFacet, Chunk, ChunkHit, ChunkMatches, Comment, Detail, Document, DocumentDetail, DocumentPage, DocumentRow,
    History, KnowledgeStore, Link, ListQuery, NewComment, PendingChunk, PendingVersion, RepoCounts, ReviewChange,
    ReviewReply, Scope, SectionText, Stats, StoreError, StoredFile, TitleScope, Visibility,
};
use rows::{interleave, tag_detail, tag_reply, tag_row, tagged_hits};

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

    /// The kind of the live document `live` (a pending version takes it).
    fn kind_of(&mut self, live: i64) -> Result<Kind, StoreError> {
        match self.get(live, &Detail::default())? {
            Some(found) => Ok(Kind::of(found.links.len())),
            None => Err(not_found(live)),
        }
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

    /// The remote database's value, never this computer's: no remote has none, and a remote that
    /// cannot be reached is an error, so a caller never takes this computer's answer for the team's.
    fn remote_meta(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        self.ready()?;

        if self.remote.is_down() {
            return Err(StoreError::Unreachable(REMOTE_DOWN.to_owned()));
        }

        let Some(store) = self.remote.store() else {
            return Ok(None);
        };
        let found = store.meta(key);

        self.answer(Place::Remote, found)
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        self.writable(Place::Remote)?;

        match self.primary()? {
            Some(store) => store.set_meta(key, value),
            None => Err(StoreError::Invalid("there is no database to write to".to_owned())),
        }
    }

    fn set_metas(&mut self, entries: &[(&str, &str)]) -> Result<(), StoreError> {
        self.writable(Place::Remote)?;

        match self.primary()? {
            Some(store) => store.set_metas(entries),
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
        // A pending version is kept where its live document is, and is of the kind of that document
        // (it has no links of its own).
        let kind = match doc.pending_of {
            Some(live) => self.kind_of(live)?,
            None => Kind::of(links.len()),
        };
        let (place, inner) = match (doc.id, doc.pending_of) {
            (0, Some(live)) => (ids::place_of(live), 0),
            (0, None) => (self.routing.default_place(kind), 0),
            (id, _) => {
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
            pending_of: doc.pending_of.map(|live| ids::split(live).1),
            ..doc.clone()
        };

        store.put_document(&doc, chunks, links).map(|id| ids::tag(place, id))
    }

    fn find_by_content_sha(&mut self, sha: &str, visibility: Visibility) -> Result<Option<Document>, StoreError> {
        self.first_found(false, |store| store.find_by_content_sha(sha, visibility))
    }

    fn find_by_source_key(&mut self, key: &str, visibility: Visibility) -> Result<Option<Document>, StoreError> {
        self.first_found(crate::store::clean::is_local_source_key(key), |store| {
            store.find_by_source_key(key, visibility)
        })
    }

    /// Counted in the place each id carries, see [`RoutedStore::count`].
    fn record_shown(&mut self, doc_ids: &[i64]) -> Result<(), StoreError> {
        self.count(doc_ids, |store, own| store.record_shown(own))
    }

    fn record_useful(&mut self, doc_ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.count(doc_ids, |store, own| store.record_useful(own, at))
    }

    fn find_by_title(
        &mut self,
        title: &str,
        scope: &TitleScope,
        visibility: Visibility,
    ) -> Result<Vec<Document>, StoreError> {
        self.titled(title, scope, visibility)
    }

    fn docs_sharing_chunks(
        &mut self,
        chunk_shas: &[String],
        visibility: Visibility,
    ) -> Result<Vec<(i64, usize)>, StoreError> {
        self.sharing(chunk_shas, visibility)
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

    fn rename(&mut self, id: i64, title: &str, by: Option<&str>, at: i64) -> Result<bool, StoreError> {
        self.writable(ids::place_of(id))?;

        match self.resolve(id)? {
            Some((place, inner)) => match self.store(place) {
                Some(store) => store.rename(inner, title, by, at),
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
        let mut authors = AuthorFacet::default();

        for place in [Place::Remote, Place::Local] {
            if only.is_some_and(|only| only != place) {
                continue;
            }

            // The team's database holds no link to a folder of this computer and is never asked for one.
            if place == Place::Remote && query.filter.is_local_path() {
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
            authors.merge(page.authors);

            for mut row in page.items {
                tag_row(place, &mut row);
                rows.push(row);
            }
        }

        rows.sort_by(|a, b| merge::compare(a, b, query.sort, query.descending));

        let items = rows
            .into_iter()
            .skip(query.offset)
            .take(query.limit.min(LIST_PAGE_MAX))
            .collect();

        Ok(DocumentPage { items, total, authors })
    }

    fn get(&mut self, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, StoreError> {
        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(None);
        };

        let kinds = self.kinds(place);
        let inner_detail = Detail {
            sections: detail.sections,
            chunk: detail.chunk.map(|chunk| ids::split(chunk).1),
            drafts: detail.drafts,
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
                .keyword_top_k(query, &scope_in(place, scope), k)
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
                .vector_top_k(vector, &scope_in(place, scope), k)
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

    fn pending_of(&mut self, id: i64) -> Result<Option<PendingVersion>, StoreError> {
        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(None);
        };
        let Some(store) = self.store(place) else {
            return Ok(None);
        };
        let found = store.pending_of(inner);
        let mut found = self.answer(place, found)?;

        if let Some(pending) = &mut found {
            pending.id = ids::tag(place, pending.id);
        }

        Ok(found)
    }

    fn set_review(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError> {
        self.writable(ids::place_of(id))?;

        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(ReviewReply::NotFound);
        };
        let Some(store) = self.store(place) else {
            return Ok(ReviewReply::NotFound);
        };

        store
            .set_review(inner, expect, change)
            .map(|reply| tag_reply(place, reply))
    }

    fn promote(&mut self, id: i64, expect: i64, change: &ReviewChange) -> Result<ReviewReply, StoreError> {
        self.writable(ids::place_of(id))?;

        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(ReviewReply::NotFound);
        };
        let Some(store) = self.store(place) else {
            return Ok(ReviewReply::NotFound);
        };

        store
            .promote(inner, expect, change)
            .map(|reply| tag_reply(place, reply))
    }

    fn history(&mut self, id: i64) -> Result<Option<History>, StoreError> {
        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(None);
        };
        let Some(store) = self.store(place) else {
            return Ok(None);
        };
        let found = store.history(inner);

        self.answer(place, found)
    }

    fn authorship_of(
        &mut self,
        ids: &[i64],
        per_doc: usize,
    ) -> Result<HashMap<i64, crate::store::Authorship>, StoreError> {
        let mut found = HashMap::new();

        for place in [Place::Remote, Place::Local] {
            let own: Vec<i64> = ids
                .iter()
                .filter(|id| ids::place_of(**id) == place)
                .map(|id| ids::split(*id).1)
                .collect();

            if own.is_empty() {
                continue;
            }

            let Some(store) = self.store(place) else { continue };
            let asked = store.authorship_of(&own, per_doc);

            if let Some(people) = self.read(place, asked)? {
                found.extend(
                    people
                        .into_iter()
                        .map(|(id, authorship)| (ids::tag(place, id), authorship)),
                );
            }
        }

        Ok(found)
    }

    fn writers_since_live(&mut self, id: i64) -> Result<Vec<String>, StoreError> {
        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(Vec::new());
        };
        let Some(store) = self.store(place) else {
            return Ok(Vec::new());
        };
        let found = store.writers_since_live(inner);

        self.answer(place, found)
    }

    fn section_texts(&mut self, id: i64) -> Result<Option<Vec<SectionText>>, StoreError> {
        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(None);
        };
        let Some(store) = self.store(place) else {
            return Ok(None);
        };
        let found = store.section_texts(inner);

        self.answer(place, found)
    }

    fn asked_sections(&mut self, id: i64, revision: i64) -> Result<Option<Vec<SectionText>>, StoreError> {
        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(None);
        };
        let Some(store) = self.store(place) else {
            return Ok(None);
        };
        let found = store.asked_sections(inner, revision);

        self.answer(place, found)
    }

    fn add_comments(
        &mut self,
        id: i64,
        by: Option<&str>,
        at: i64,
        revision: i64,
        comments: &[NewComment],
    ) -> Result<Vec<i64>, StoreError> {
        self.writable(ids::place_of(id))?;

        let Some((place, inner)) = self.resolve(id)? else {
            return Err(not_found(id));
        };
        let Some(store) = self.store(place) else {
            return Err(not_found(id));
        };

        store
            .add_comments(inner, by, at, revision, comments)
            .map(|added| added.into_iter().map(|comment| ids::tag(place, comment)).collect())
    }

    fn reply_comment(
        &mut self,
        id: i64,
        parent: i64,
        by: Option<&str>,
        at: i64,
        body: &str,
    ) -> Result<Option<i64>, StoreError> {
        self.writable(ids::place_of(id))?;

        let Some((place, inner)) = self.resolve(id)? else {
            return Err(not_found(id));
        };
        let Some(store) = self.store(place) else {
            return Err(not_found(id));
        };

        store
            .reply_comment(inner, ids::split(parent).1, by, at, body)
            .map(|reply| reply.map(|comment| ids::tag(place, comment)))
    }

    fn resolve_comment(
        &mut self,
        id: i64,
        comment: i64,
        resolved: bool,
        by: Option<&str>,
        at: i64,
    ) -> Result<bool, StoreError> {
        self.writable(ids::place_of(id))?;

        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(false);
        };
        let Some(store) = self.store(place) else {
            return Ok(false);
        };

        store.resolve_comment(inner, ids::split(comment).1, resolved, by, at)
    }

    fn delete_comment(&mut self, id: i64, comment: i64) -> Result<Option<Vec<i64>>, StoreError> {
        self.writable(ids::place_of(id))?;

        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(None);
        };
        let Some(store) = self.store(place) else {
            return Ok(None);
        };

        store
            .delete_comment(inner, ids::split(comment).1)
            .map(|deleted| deleted.map(|found| found.into_iter().map(|each| ids::tag(place, each)).collect()))
    }

    fn comments(&mut self, id: i64) -> Result<Vec<Comment>, StoreError> {
        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(Vec::new());
        };
        let Some(store) = self.store(place) else {
            return Ok(Vec::new());
        };
        let found = store.comments(inner);
        let mut found = self.answer(place, found)?;

        for comment in &mut found {
            comment.id = ids::tag(place, comment.id);
            comment.parent_id = comment.parent_id.map(|parent| ids::tag(place, parent));
        }

        Ok(found)
    }

    fn put_file(&mut self, id: i64, name: &str, bytes: &[u8]) -> Result<bool, StoreError> {
        self.writable(ids::place_of(id))?;

        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(false);
        };
        let Some(store) = self.store(place) else {
            return Ok(false);
        };

        store.put_file(inner, name, bytes)
    }

    fn file(&mut self, id: i64) -> Result<Option<StoredFile>, StoreError> {
        let Some((place, inner)) = self.resolve(id)? else {
            return Ok(None);
        };
        let Some(store) = self.store(place) else {
            return Ok(None);
        };
        let found = store.file(inner);

        self.answer(place, found)
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

/// `scope` as the store of `place` is asked it: the team's database never gets a folder of this
/// computer (see [`Scope::for_team`]).
pub(crate) fn scope_in(place: Place, scope: &Scope) -> std::borrow::Cow<'_, Scope> {
    match place {
        Place::Remote => scope.for_team(),
        Place::Local => std::borrow::Cow::Borrowed(scope),
    }
}

fn not_found(id: i64) -> StoreError {
    StoreError::NotFound(format!("document {} not found", ids::show(id)))
}

#[cfg(test)]
mod tests;
