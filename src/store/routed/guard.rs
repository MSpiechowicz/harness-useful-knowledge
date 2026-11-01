//! What a routed store does about a remote that cannot be reached: settling it before a call,
//! leaving it out of a read, refusing an id or a write that is meant for it.
//!
//! A read honours the cooldown and waits the short probe. A write does not: it always tries the
//! remote, with the driver's full timeout, so it is refused only when the remote cannot be reached
//! now (see [`Remote::settle_for_write`](super::Remote::settle_for_write)).

use super::{REMOTE_DOWN, REMOTE_DOWN_WRITE, RoutedStore, ids};
use crate::backend::OpenError;
use crate::config::Place;
use crate::store::StoreError;

/// What a remote that could not be opened is, for a store call.
fn open_failure(error: OpenError) -> StoreError {
    match error {
        OpenError::Missing { code, message } => StoreError::Missing { code, message },
        OpenError::Failed(message) | OpenError::Unreachable(message) => StoreError::Backend(message),
    }
}

impl RoutedStore {
    /// Settles the remote before a call, see [`RoutedStore::settle`].
    pub(super) fn ready(&mut self) -> Result<(), StoreError> {
        self.remote.settle().map_err(open_failure)
    }

    /// Settles the remote for a write: the cooldown is not honoured, see [`Remote::settle_for_write`].
    pub(super) fn ready_to_write(&mut self) -> Result<(), StoreError> {
        self.remote.settle_for_write().map_err(open_failure)
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

    /// Refuses a write meant for `place` while the remote cannot be reached, after trying it.
    pub(super) fn writable(&mut self, place: Place) -> Result<(), StoreError> {
        if place != Place::Remote {
            return Ok(());
        }

        self.ready_to_write()?;

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
