//! The remote place of a [`super::RoutedStore`]: opened when first needed, and left out while it
//! cannot be reached.
//!
//! A [`Remote`] made with [`Remote::lazy`] opens the database through its opener the first time a
//! call needs it. When the opener says the remote is unreachable, or a call to the open store does
//! ([`Remote::lost`]), the failure goes to the shared [`Reach`] and the remote is left out: the
//! routed store answers from this computer, and the remote is not tried again until the cooldown
//! of the reach (which every process shares) has ended. The next call after that opens it again,
//! and a success clears the cooldown for everyone.
//!
//! A read opens the remote with a short probe ([`Patience::Probe`]) and honours the cooldown. A write
//! has nothing to fall back on, so it always tries, cooldown or not, with the driver's own timeout
//! ([`Patience::Full`], see [`Remote::settle_for_write`]): a person is not refused for a minute after
//! the remote came back, nor because it connects slowly. A probe that merely ran out of time is
//! recorded as such (see [`Reach::failed_in`]) and so never turns away a reader that waits longer.

use std::time::{Duration, Instant};

use crate::backend::OpenError;
use crate::config::Secrets;
use crate::store::{KnowledgeStore, Reach, StoreError};

/// How long an attempt to open the remote database may wait for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Patience {
    /// The short probe of a read, which this computer can answer instead.
    Probe,
    /// The driver's own timeout, for a write that nothing else can answer.
    Full,
}

/// What it takes to open the remote database.
type Opener = Box<dyn FnMut(Patience) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> + Send>;

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
    /// How long a [`Patience::Probe`] waits.
    probe: Duration,
    /// What the messages of a lost connection are scrubbed of before the reach keeps them.
    secrets: Secrets,
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

    /// A remote that `open` opens when first needed, recording its failures in `reach`. A probe
    /// waits `probe` for it. The reason a connection was lost later is scrubbed of `secrets` first
    /// (the opener's own errors already are).
    pub fn lazy(
        open: impl FnMut(Patience) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> + Send + 'static,
        reach: Reach,
        probe: Duration,
        secrets: Secrets,
    ) -> Self {
        Self {
            store: None,
            state: State::Unopened,
            link: Some(Link {
                open: Box::new(open),
                reach,
                probe,
                secrets,
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

    /// Decides before a read whether the remote is there: opens it the first time, and again once
    /// the cooldown of an unreachable one has ended. Only a failure that is not about reaching the
    /// remote (a database without pgvector, say) is an error.
    pub fn settle(&mut self) -> Result<(), OpenError> {
        if matches!(self.state, State::Open | State::Absent) {
            return Ok(());
        }

        let Some(link) = self.link.as_ref() else {
            return Ok(());
        };

        if link.reach.cooling_for(link.probe).is_some() {
            self.state = State::Down;
            return Ok(());
        }

        self.open(Patience::Probe)
    }

    /// Decides before a write whether the remote is there. The cooldown is not honoured and the
    /// driver's full timeout applies: a write that would be refused because of an earlier failure
    /// (or a probe that was too short) is tried against the remote instead, and the remote that
    /// answers is used from then on.
    pub fn settle_for_write(&mut self) -> Result<(), OpenError> {
        if matches!(self.state, State::Open | State::Absent) || self.link.is_none() {
            return Ok(());
        }

        self.open(Patience::Full)
    }

    /// Tries to open the remote, and notes the outcome in the reach.
    fn open(&mut self, patience: Patience) -> Result<(), OpenError> {
        let Some(link) = self.link.as_mut() else {
            return Ok(());
        };

        let started = Instant::now();

        match (link.open)(patience) {
            Ok(store) => {
                link.reach.reached();
                self.state = if store.is_some() { State::Open } else { State::Absent };
                self.store = store;

                Ok(())
            }
            Err(OpenError::Unreachable(message)) => {
                link.reach.failed_in(&message, started.elapsed(), link.probe);
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

        link.reach.failed(&link.secrets.scrub(message));
        self.store = None;
        self.state = State::Down;

        true
    }
}
