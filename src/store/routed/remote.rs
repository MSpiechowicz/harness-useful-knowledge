//! The remote place of a [`super::RoutedStore`]: opened when first needed, and left out while it
//! cannot be reached.
//!
//! A [`Remote`] made with [`Remote::lazy`] opens the database through its opener the first time a
//! call needs it. When the opener says the remote is unreachable, or a call to the open store does
//! ([`Remote::lost`]), the failure goes to the shared [`Reach`] and the remote is left out: the
//! routed store answers from this computer, and the remote is not tried again until the cooldown
//! of the reach (which every process shares) has ended. The next call after that opens it again,
//! and a success clears the cooldown for everyone.

use crate::backend::OpenError;
use crate::store::{KnowledgeStore, Reach, StoreError};

/// What it takes to open the remote database.
type Opener = Box<dyn FnMut() -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> + Send>;

/// How the remote is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Not tried yet.
    Unopened,
    /// Open.
    Open,
    /// There is nothing to open: no database, or no knowledge tables yet.
    Absent,
    /// It could not be reached.
    Down,
}

/// How the remote is opened and where its failures are recorded.
struct Link {
    open: Opener,
    reach: Reach,
}

/// The remote store of a routed one, if there is one.
pub struct Remote {
    store: Option<Box<dyn KnowledgeStore>>,
    state: State,
    link: Option<Link>,
}

impl Remote {
    /// An already opened remote store, or none: it is never opened again, and a failure of it
    /// is an error like any other.
    pub fn fixed(store: Option<Box<dyn KnowledgeStore>>) -> Self {
        let state = if store.is_some() { State::Open } else { State::Absent };

        Self {
            store,
            state,
            link: None,
        }
    }

    /// A remote that `open` opens when first needed, recording its failures in `reach`.
    pub fn lazy(
        open: impl FnMut() -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> + Send + 'static,
        reach: Reach,
    ) -> Self {
        Self {
            store: None,
            state: State::Unopened,
            link: Some(Link {
                open: Box::new(open),
                reach,
            }),
        }
    }

    /// The open store.
    pub(super) fn store(&mut self) -> Option<&mut (dyn KnowledgeStore + 'static)> {
        self.store.as_deref_mut()
    }

    /// True while the remote cannot be reached and this computer answers instead.
    pub fn is_down(&self) -> bool {
        self.state == State::Down
    }

    /// True when there is no remote store, and none to wait for.
    pub fn is_none(&self) -> bool {
        self.state == State::Absent
    }

    /// Decides before a call whether the remote is there: opens it the first time, and again once
    /// the cooldown of an unreachable one has ended. Only a failure that is not about reaching the
    /// remote (a database without pgvector, say) is an error.
    pub fn settle(&mut self) -> Result<(), OpenError> {
        if matches!(self.state, State::Open | State::Absent) {
            return Ok(());
        }

        let Some(link) = self.link.as_mut() else {
            return Ok(());
        };

        if link.reach.cooling().is_some() {
            self.state = State::Down;
            return Ok(());
        }

        match (link.open)() {
            Ok(store) => {
                link.reach.reached();
                self.state = if store.is_some() { State::Open } else { State::Absent };
                self.store = store;

                Ok(())
            }
            Err(OpenError::Unreachable(message)) => {
                link.reach.failed(&message);
                self.state = State::Down;

                Ok(())
            }
            Err(other) => Err(other),
        }
    }

    /// Leaves the remote out from now on when `error` says it cannot be reached and a call can
    /// be answered without it. False for any other error, and for a remote that was given open.
    pub fn lost(&mut self, error: &StoreError) -> bool {
        let (StoreError::Unreachable(message), Some(link)) = (error, self.link.as_ref()) else {
            return false;
        };

        link.reach.failed(message);
        self.store = None;
        self.state = State::Down;

        true
    }
}
