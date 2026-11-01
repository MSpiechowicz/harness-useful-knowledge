//! What a routed store does about a remote that cannot be reached: settling it before a call,
//! leaving it out of a read, refusing an id or a write that is meant for it.

use super::{REMOTE_DOWN, REMOTE_DOWN_WRITE, RoutedStore, ids};
use crate::backend::OpenError;
use crate::config::Place;
use crate::store::StoreError;

impl RoutedStore {
    /// Settles the remote before a call, see [`RoutedStore::settle`].
    pub(super) fn ready(&mut self) -> Result<(), StoreError> {
        self.remote.settle().map_err(|error| match error {
            OpenError::Missing { code, message } => StoreError::Missing { code, message },
            OpenError::Failed(message) | OpenError::Unreachable(message) => StoreError::Backend(message),
        })
    }

    /// The result of a call to the store of `place`. A remote that turns out unreachable is left
    /// out from now on (see [`Remote::lost`]).
    pub(super) fn answer<T>(&mut self, place: Place, result: Result<T, StoreError>) -> Result<T, StoreError> {
        if let (Err(error), Place::Remote) = (&result, place) {
            self.remote.lost(error);
        }

        result
    }

    /// [`RoutedStore::answer`] for a read that the other place can do without: `None` when the
    /// remote was unreachable.
    pub(super) fn read<T>(&mut self, place: Place, result: Result<T, StoreError>) -> Result<Option<T>, StoreError> {
        match self.answer(place, result) {
            Err(StoreError::Unreachable(_)) if place == Place::Remote && self.remote.is_down() => Ok(None),
            other => other.map(Some),
        }
    }

    /// Refuses a write meant for `place` while the remote is unreachable.
    pub(super) fn writable(&mut self, place: Place) -> Result<(), StoreError> {
        if place != Place::Remote {
            return Ok(());
        }

        self.ready()?;

        if self.remote.is_down() {
            return Err(StoreError::Unreachable(REMOTE_DOWN_WRITE.to_owned()));
        }

        Ok(())
    }

    /// The place an id is for and the id in that place's store, when the place has a store. An id
    /// of the remote database is refused while it is unreachable.
    pub(super) fn resolve(&mut self, id: i64) -> Result<Option<(Place, i64)>, StoreError> {
        let (place, inner) = ids::split(id);
        let place = place.unwrap_or(Place::Remote);

        if place == Place::Remote {
            self.ready()?;

            if self.remote.is_down() {
                return Err(StoreError::Unreachable(REMOTE_DOWN.to_owned()));
            }
        }

        Ok(self.store(place).map(|_| (place, inner)))
    }
}
