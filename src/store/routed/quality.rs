//! The lookups and counters of a routed store: titles and shared chunks are asked of every place
//! and tagged, a counter goes to the place its id carries.

use super::rows::tag_document;
use super::{RoutedStore, ids};
use crate::config::{Kind, Place};
use crate::store::{Document, KnowledgeStore, StoreError, TitleScope};

impl RoutedStore {
    /// The first document the places find with `find`, remote first, tagged with its place.
    pub(super) fn first_found(
        &mut self,
        find: impl Fn(&mut dyn KnowledgeStore) -> Result<Option<Document>, StoreError>,
    ) -> Result<Option<Document>, StoreError> {
        for place in self.present()? {
            let Some(store) = self.store(place) else { continue };

            if let Some(mut doc) = find(store)? {
                tag_document(place, &mut doc);
                return Ok(Some(doc));
            }
        }

        Ok(None)
    }

    /// Runs `count` on each place with the ids that carry it, in that place's own numbers. A
    /// remote that is down is skipped (a counter is best effort, and waiting for it is not worth
    /// it). Every place is tried; the first failure is the one reported, and a place that was only
    /// busy ([`StoreError::Busy`]) counts as one only when no other place failed.
    pub(super) fn count(
        &mut self,
        doc_ids: &[i64],
        count: impl Fn(&mut dyn KnowledgeStore, &[i64]) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut first = None;
        let mut busy = None;

        for place in [Place::Remote, Place::Local] {
            let own: Vec<i64> = doc_ids
                .iter()
                .filter(|id| ids::place_of(**id) == place)
                .map(|id| ids::split(*id).1)
                .collect();

            if own.is_empty() {
                continue;
            }

            if place == Place::Remote {
                self.ready()?;

                if self.remote.is_down() {
                    continue;
                }
            }

            let Some(store) = self.store(place) else { continue };
            let counted = count(store, &own);

            match self.answer(place, counted) {
                Ok(()) => {}
                Err(StoreError::Busy) => busy = Some(StoreError::Busy),
                Err(error) => {
                    first.get_or_insert(error);
                }
            }
        }

        first.or(busy).map_or(Ok(()), Err)
    }

    /// The documents of `scope` titled `title` in every place that shows documents of that kind.
    pub(super) fn titled(&mut self, title: &str, scope: &TitleScope) -> Result<Vec<Document>, StoreError> {
        let kind = match scope {
            TitleScope::Repo(_) => Kind::Repository,
            TitleScope::Global => Kind::Global,
        };
        let mut all = Vec::new();

        for place in self.present()? {
            if !self.kinds(place).allows(kind) {
                continue;
            }

            let Some(store) = self.store(place) else { continue };
            let found = store.find_by_title(title, scope);

            if let Some(docs) = self.read(place, found)? {
                for mut doc in docs {
                    tag_document(place, &mut doc);
                    all.push(doc);
                }
            }
        }

        Ok(all)
    }

    /// The documents of every place with chunks among `shas`, ids tagged with their place.
    pub(super) fn sharing(&mut self, shas: &[String]) -> Result<Vec<(i64, usize)>, StoreError> {
        let mut all = Vec::new();

        for place in self.present()? {
            let Some(store) = self.store(place) else { continue };
            let found = store.docs_sharing_chunks(shas);

            if let Some(shared) = self.read(place, found)? {
                all.extend(shared.into_iter().map(|(id, count)| (ids::tag(place, id), count)));
            }
        }

        Ok(all)
    }

    /// What the remote database says about changing documents: `None` without one, while it
    /// cannot be reached, or when it does not say.
    pub(super) fn remote_may_write(&mut self) -> Option<bool> {
        self.ready().ok()?;

        if self.remote.is_down() {
            return None;
        }

        self.remote.store()?.remote_writable()
    }
}
