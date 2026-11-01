//! Opens the knowledge store the settings select: the local SQLite file or a team database
//! (PostgreSQL, MongoDB, libSQL).
//!
//! A database that lacks a capability the knowledge needs (pgvector, `mongot`, vectors in libSQL,
//! FTS5) is an [`OpenError::Missing`] with the `check` problem code, which every command answers
//! with exit 3.
//!
//! With both places in use ([`Layout::Both`]) this computer stands in for a remote database that
//! cannot be reached (see [`open`]): it is tried with a short connect timeout, and a failure starts
//! the cooldown of a [`Reach`] shared by every process, during which it is not tried again. A probe
//! that merely ran out of time is recorded with how long it waited, so the 1 s of a hook never
//! makes a longer reader (or a write, which always tries with the driver's own timeout) skip a remote
//! that connects slowly.

use std::time::{Duration, Instant};

use crate::config::{Backend, Layout, Place, Routing};
use crate::location::{DIR_VAR, database_path, override_is_relative};
use crate::store::routed::{Patience, Remote};
use crate::store::{KnowledgeStore, Reach, RoutedStore, SqliteStore, StoreError};
use crate::support::{Env, deadline};

/// How long connecting to the remote database may take before this computer answers instead (when
/// the URL sets no timeout of its own): a dead remote costs this once, then its cooldown applies.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The same for `recall --hook`, which has to finish within [`crate::recall::DEADLINE`].
pub const HOOK_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// How many attempts to open the remote database may still be running after their deadline: a
/// server that takes the connection and never speaks leaves its thread blocked until the server or
/// the network ends it (the drivers' timeouts do not cover that), and a long-lived process must not
/// leave one more behind for every attempt. With all of them in use the remote counts as unreachable.
static SILENT_OPENS: deadline::Slots = deadline::Slots::new(4);

/// What a command needs from the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads only: a database that does not exist yet is not created, [`open`] answers `None`.
    Read,
    /// Reads and changes what is there: a database that does not exist is not created either.
    Existing,
    /// Reads and writes, creating the database when it does not exist (an import).
    Create,
}

/// Why no store was opened. Messages carry no credentials.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OpenError {
    /// The database lacks a capability: `code` is the problem code of `check` (`no-fts5`,
    /// `no-pgvector`, `pgvector-no-privilege`, `pgvector-too-old`, `index-building`, `no-mongot`
    /// or `libsql-no-vector`).
    #[error("{message}")]
    Missing { code: &'static str, message: String },
    /// The database could not be opened.
    #[error("{0}")]
    Failed(String),
    /// The remote database could not be reached (a timeout, a refused connection, a host that
    /// does not resolve).
    #[error("{0}")]
    Unreachable(String),
}

impl OpenError {
    /// A SQLite (the local file or a libSQL server) without FTS5.
    pub fn no_fts5() -> Self {
        Self::Missing {
            code: "no-fts5",
            message: "this SQLite build has no FTS5 full-text search, so the keyword index cannot be kept".to_owned(),
        }
    }
}

/// The store the `routing` selects: the one place in use as it is (plain ids), or both as a
/// [`RoutedStore`] (ids carry their place). `None` when `access` does not create the database
/// and none of the places in use exists (callers answer as for an empty one).
///
/// With both places the remote database opens when first needed: while it cannot be reached (or
/// its cooldown runs) the routed store answers from this computer and says so
/// ([`KnowledgeStore::offline`]), and it tries the remote again once the cooldown ends. With the
/// remote database alone there is nothing to answer from, so an unreachable one fails.
pub fn open(
    env: &Env,
    backend: &Backend,
    routing: Routing,
    access: Access,
) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    open_within(env, backend, routing, access, PROBE_TIMEOUT)
}

/// [`open`] giving up on a remote database after `probe` (see [`PROBE_TIMEOUT`]).
pub fn open_within(
    env: &Env,
    backend: &Backend,
    routing: Routing,
    access: Access,
    probe: Duration,
) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    match routing.layout() {
        Layout::Local => open_place(env, backend, Place::Local, access),
        Layout::Remote => open_place(env, backend, Place::Remote, access),
        Layout::Both => {
            let local = open_place(env, backend, Place::Local, access)?;
            let reach = reach_of(env, backend);
            let secrets = backend.secrets();
            let (env, backend) = (env.clone(), backend.clone());
            let opener = move |patience| {
                let connect = match patience {
                    Patience::Probe => probe,
                    Patience::Full => default_timeout(&backend),
                };

                open_remote_within(&env, &backend, access, connect)
            };
            let remote = Remote::lazy(opener, reach, probe, secrets);
            let mut routed = RoutedStore::new(local, None, routing).with_remote(remote);

            routed.settle()?;

            Ok((!routed.is_empty()).then(|| Box::new(routed) as Box<dyn KnowledgeStore>))
        }
    }
}

/// The store of one place, whatever the routing: the local file or the remote database.
pub fn open_place(
    env: &Env,
    backend: &Backend,
    place: Place,
    access: Access,
) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    open_place_within(env, backend, place, access, default_timeout(backend))
}

/// [`open_place`] giving up on a connection to the remote database after `connect`.
pub fn open_place_within(
    env: &Env,
    backend: &Backend,
    place: Place,
    access: Access,
    connect: Duration,
) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    match (place, backend) {
        (Place::Local, _) => open_local(env, access),
        (Place::Remote, Backend::Sqlite) => Err(OpenError::Failed("no remote database is configured".to_owned())),
        (Place::Remote, Backend::Postgres { url }) => {
            crate::store::postgres::open_boxed(url, access, connect, &backend.secrets())
        }
        (Place::Remote, Backend::Libsql { url, token }) => {
            crate::store::libsql::open_boxed(url, token.as_deref(), access, connect, &backend.secrets())
        }
        (Place::Remote, Backend::Mongodb { url }) => {
            crate::store::mongo::open_boxed(url, access, connect, &backend.secrets())
        }
    }
}

/// Opens the remote database, which is unreachable when it has not answered within twice the
/// `connect` timeout. The drivers' timeouts cover the connection only, so a server that accepts it
/// and then never speaks is cut off here.
fn open_remote_within(
    env: &Env,
    backend: &Backend,
    access: Access,
    connect: Duration,
) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    let (env, backend) = (env.clone(), backend.clone());
    let silent = || OpenError::Unreachable("the remote database did not answer in time".to_owned());

    deadline::within_slots(&SILENT_OPENS, "remote-open", connect * 2, move || {
        open_place_within(&env, &backend, Place::Remote, access, connect)
    })
    .unwrap_or_else(|| Err(silent()))
}

/// How long connecting to the remote database takes before it counts as unreachable, unless the
/// URL says otherwise.
fn default_timeout(backend: &Backend) -> Duration {
    match backend {
        Backend::Sqlite => Duration::ZERO,
        Backend::Postgres { .. } => crate::store::postgres::CONNECT_TIMEOUT,
        Backend::Libsql { .. } => crate::store::libsql::CONNECT_TIMEOUT,
        Backend::Mongodb { .. } => crate::store::mongo::SERVER_SELECTION_TIMEOUT,
    }
}

/// What the remote database is known to be doing, shared by every process of this computer: the
/// cooldown file sits next to the local database (kept in memory alone when its folder is unknown).
pub fn reach_of(env: &Env, backend: &Backend) -> Reach {
    match database_path(env) {
        Some(path) => Reach::beside(&path, &backend.remote_key()),
        None => Reach::default(),
    }
}

/// How an attempt to open the remote database for a command ended.
pub enum Attempt {
    /// It opened (`None`: it has no knowledge tables and `access` does not create them).
    Opened(Option<Box<dyn KnowledgeStore>>),
    /// It cannot be reached: the reason, safe to show.
    Offline(String),
}

/// Opens the remote database for a command that has this computer to fall back on (`stats`, or
/// an import): a failure to reach it is recorded in the shared cooldown and a success
/// clears it. With `cooldown` (a read) an attempt is not even made while the cooldown of an earlier
/// one runs, and the remote gets [`PROBE_TIMEOUT`] to connect. Without it (a write) the remote is
/// always tried, with the full connect timeout of its driver, so the person is not refused for a
/// minute after the remote came back, nor because it connects slowly.
pub fn attempt_remote(env: &Env, backend: &Backend, access: Access, cooldown: bool) -> Result<Attempt, OpenError> {
    let reach = reach_of(env, backend);

    if let Some(message) = cooldown.then(|| reach.cooling_for(PROBE_TIMEOUT)).flatten() {
        return Ok(Attempt::Offline(message));
    }

    let connect = if cooldown {
        PROBE_TIMEOUT
    } else {
        default_timeout(backend)
    };
    let started = Instant::now();

    match open_remote_within(env, backend, access, connect) {
        Ok(store) => {
            reach.reached();
            Ok(Attempt::Opened(store))
        }
        Err(OpenError::Unreachable(message)) => {
            reach.failed_in(&message, started.elapsed(), PROBE_TIMEOUT);
            Ok(Attempt::Offline(message))
        }
        Err(other) => Err(other),
    }
}

/// The name of the backend, as `check` reports it.
pub fn name(backend: &Backend) -> &'static str {
    match backend {
        Backend::Sqlite => "sqlite",
        Backend::Postgres { .. } => "postgres",
        Backend::Mongodb { .. } => "mongodb",
        Backend::Libsql { .. } => "libsql",
    }
}

fn open_local(env: &Env, access: Access) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    let Some(path) = database_path(env) else {
        return Err(OpenError::Failed("the data folder is unknown".to_owned()));
    };

    if override_is_relative(env) {
        return Err(OpenError::Failed(format!("{DIR_VAR} must be an absolute folder")));
    }

    let opened = match access {
        Access::Read => SqliteStore::open_read_only(&path),
        Access::Existing => SqliteStore::open_existing(&path),
        Access::Create => SqliteStore::open(&path).map(Some),
    };

    match opened {
        Ok(store) => Ok(store.map(|store| Box::new(store) as Box<dyn KnowledgeStore>)),
        Err(StoreError::NoFts5) => Err(OpenError::no_fts5()),
        Err(error) => Err(OpenError::Failed(format!("cannot open the local database: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::FILE_NAME;

    fn env_in(dir: &std::path::Path) -> Env {
        Env::for_tests("/home/u").with_var(DIR_VAR, dir.to_string_lossy())
    }

    #[test]
    fn reading_and_changing_never_create_the_local_database_and_creating_does() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("knowledge");
        let env = env_in(&folder);

        assert!(
            open(&env, &Backend::Sqlite, Routing::local(), Access::Read)
                .expect("read")
                .is_none()
        );
        assert!(
            open(&env, &Backend::Sqlite, Routing::local(), Access::Existing)
                .expect("existing")
                .is_none()
        );
        assert!(!folder.exists());

        assert!(
            open(&env, &Backend::Sqlite, Routing::local(), Access::Create)
                .expect("create")
                .is_some()
        );
        assert!(folder.join(FILE_NAME).is_file());

        assert!(
            open(&env, &Backend::Sqlite, Routing::local(), Access::Read)
                .expect("read")
                .is_some()
        );
    }

    #[test]
    fn a_relative_folder_override_is_refused() {
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, "relative");

        assert!(matches!(
            open(&env, &Backend::Sqlite, Routing::local(), Access::Read),
            Err(OpenError::Failed(_))
        ));
    }
}
