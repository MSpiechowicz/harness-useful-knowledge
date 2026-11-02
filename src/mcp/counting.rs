//! The usefulness counters of the documents a session showed or found helpful.
//!
//! They are the only thing the MCP server writes to the database. A reply only queues them; they are
//! written once the reply is out ([`Counting::flush`]), so counting never delays an answer, and
//! each document counts once per session and kind. The write goes through a handle of its own
//! ([`Counters`], opened with [`Access::Counters`]) that offers nothing else, opened on first use.
//!
//! Counting never adds anything to the database and never waits: the handle does not create the
//! database, migrate or upgrade it, add columns or run any DDL (the columns come from the paths that
//! may wait: an import, an approval, the CLI writes, the desktop), so a database that needs one of
//! those, or lacks the columns, is not counted in. It does not wait for a lock either, and a flush as a
//! whole is cut off after [`FLUSH_BUDGET`]. Whatever fails, nothing is reported and the count is
//! dropped. A handle that failed to open, or to write, or ran out of time is not tried again for
//! [`OPEN_RETRY`]; a lock another process held only drops that count. With both places in use, a remote
//! that cannot be reached is left out of the handle until its shared cooldown ends.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::backend::{self, Access, HOOK_PROBE_TIMEOUT};
use crate::config::{Backend, Routing};
use crate::store::{KnowledgeStore, StoreError};
use crate::support::{Env, deadline};

/// How long after a failed open or write the handle is not tried again, so a database that is down
/// costs one slow attempt, not one after every reply.
const OPEN_RETRY: Duration = Duration::from_secs(60);

/// The most one flush may take, opening the handle included: the next request waits for it, so it
/// ends there however the database behaves. A remote database gets [`HOOK_PROBE_TIMEOUT`] to
/// connect, the rest is for the statements.
const FLUSH_BUDGET: Duration = Duration::from_secs(1);

/// Marks a flush as running until the thread that runs it ends (also when it panics), so a database
/// that never answers leaves one blocked thread behind and no more are started meanwhile.
struct Running(Arc<AtomicBool>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A counter to write after the reply that caused it went out.
#[derive(Clone)]
enum Counted {
    Shown(Vec<i64>),
    Useful(Vec<i64>, i64),
}

/// A store handle that can count and nothing else: it is opened with [`Access::Counters`], which
/// neither migrates, upgrades nor adds columns and never waits, and it is never used to read or
/// change a document.
struct Counters(Box<dyn KnowledgeStore>);

impl Counters {
    fn write(&mut self, counted: &Counted) -> Result<(), StoreError> {
        match counted {
            Counted::Shown(ids) => self.0.record_shown(ids),
            Counted::Useful(ids, at) => self.0.record_useful(ids, *at),
        }
    }
}

/// What a flush did, as the thread that ran it reports.
struct Report {
    /// The handle, when it can be used for the next flush.
    handle: Option<Counters>,
    /// Whether the database was there: its documents count as counted in this session.
    reached: bool,
    /// Whether the handle could not be opened or a write failed: it is not tried again for a while.
    failed: bool,
}

impl Report {
    fn nothing(failed: bool) -> Self {
        Self {
            handle: None,
            reached: false,
            failed,
        }
    }
}

/// What a session has queued and counted so far.
#[derive(Default)]
pub(super) struct Counting {
    /// The writes of the reply being answered.
    queue: Vec<Counted>,
    /// The documents counted as shown / as useful in this session already: each counts once.
    shown: HashSet<i64>,
    useful: HashSet<i64>,
    handle: Option<Counters>,
    /// When the handle last failed to open or to write.
    failed_at: Option<Instant>,
    /// Whether a flush that went past its budget is still running.
    running: Arc<AtomicBool>,
}

impl Counting {
    /// Queues the documents `ids` not counted as shown in this session yet.
    pub fn queue_shown(&mut self, ids: &[i64]) {
        let new = uncounted(ids, &self.shown);

        if !new.is_empty() {
            self.queue.push(Counted::Shown(new));
        }
    }

    /// Queues the documents `ids` not counted as useful in this session yet, at `at`.
    pub fn queue_useful(&mut self, ids: &[i64], at: i64) {
        let new = uncounted(ids, &self.useful);

        if !new.is_empty() {
            self.queue.push(Counted::Useful(new, at));
        }
    }

    /// Writes what the last reply queued, in at most [`FLUSH_BUDGET`]. A document counts as counted
    /// in this session once its write is attempted, so a database that does not exist yet leaves it to
    /// a later reply. A handle that failed, or took too long, is not opened again for [`OPEN_RETRY`].
    pub fn flush(&mut self, env: &Env, backend: &Backend, routing: Routing) {
        let queue = std::mem::take(&mut self.queue);

        if queue.is_empty() {
            return;
        }

        if self.handle.is_none() && self.failed_at.is_some_and(|at| at.elapsed() < OPEN_RETRY) {
            return;
        }

        if self.running.swap(true, Ordering::AcqRel) {
            return;
        }

        let attempted = queue.clone();
        let (handle, env, backend) = (self.handle.take(), env.clone(), backend.clone());
        let running = Running(Arc::clone(&self.running));
        let work = move || {
            let _running = running;

            write(handle, &env, &backend, routing, &queue)
        };

        match deadline::within("knowledge-count", FLUSH_BUDGET, work) {
            Some(report) => {
                self.handle = report.handle;
                self.failed_at = report.failed.then(Instant::now);

                if report.reached {
                    self.note(&attempted);
                }
            }
            // The handle went with the thread that is still using it, and what that writes is
            // attempted: the documents do not count again.
            None => {
                self.failed_at = Some(Instant::now());
                self.note(&attempted);
            }
        }
    }

    /// Notes the documents of the writes `attempted` as counted in this session.
    fn note(&mut self, attempted: &[Counted]) {
        for counted in attempted {
            match counted {
                Counted::Shown(ids) => self.shown.extend(ids),
                Counted::Useful(ids, _) => self.useful.extend(ids),
            }
        }
    }
}

/// Writes `queue` through `handle`, opened first when there is none.
fn write(handle: Option<Counters>, env: &Env, backend: &Backend, routing: Routing, queue: &[Counted]) -> Report {
    let mut counters = match handle {
        Some(counters) => counters,
        None => match backend::open_within(env, backend, routing, Access::Counters, HOOK_PROBE_TIMEOUT) {
            Ok(Some(store)) => Counters(store),
            // No database yet: a later reply tries again.
            Ok(None) => return Report::nothing(false),
            Err(_) => return Report::nothing(true),
        },
    };

    let mut failed = false;

    for counted in queue {
        match counters.write(counted) {
            Ok(()) => {}
            // Another process held the lock: this count is dropped, the handle is fine.
            Err(StoreError::Busy) => {}
            Err(_) => failed = true,
        }
    }

    Report {
        handle: (!failed).then_some(counters),
        reached: true,
        failed,
    }
}

/// Counts the documents `ids` as shown through a handle opened for this one attempt (the recall hook
/// of a prompt): nothing happens, and nothing is reported, when it cannot be opened or written.
pub(crate) fn record_shown_once(env: &Env, backend: &Backend, routing: Routing, ids: &[i64]) {
    let Ok(Some(store)) = backend::open_within(env, backend, routing, Access::Counters, HOOK_PROBE_TIMEOUT) else {
        return;
    };

    let _ = Counters(store).write(&Counted::Shown(ids.to_vec()));
}

/// The ids among `ids` not in `counted`, each once.
fn uncounted(ids: &[i64], counted: &HashSet<i64>) -> Vec<i64> {
    let mut new: Vec<i64> = Vec::new();

    for id in ids {
        if !counted.contains(id) && !new.contains(id) {
            new.push(*id);
        }
    }

    new
}
