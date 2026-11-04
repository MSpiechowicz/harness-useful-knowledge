//! The lookups and counters behind duplicate detection and usefulness: titles in a scope, the
//! documents that share chunks, how often a document was shown or helped, and whether this user
//! may change documents.
//!
//! The fields are schemaless: a counter that was never written reads as 0, and `$inc` creates it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use mongodb::bson::{Bson, Document as Row, doc};
use mongodb::sync::Database;

use super::connection;
use super::ctx::{Ctx, Fail};
use super::documents::{document, seen, seen_chunks};
use super::schema::{CHUNKS, DOCUMENTS};
use super::{MongoStore, int, text};
use crate::limits::SHARED_SHAS_MAX;
use crate::store::quality::{TITLE_SCAN_MAX, matching_titles};
use crate::store::{Document, StoreError, TitleScope, Visibility};

/// How long a counter write may take before the caller moves on.
#[cfg(not(test))]
const COUNT_BUDGET: Duration = Duration::from_millis(750);

/// The team tests run dozens of databases on one server at once, where a write takes longer: the
/// caller then waits up to just past the server's own bound, so a test counts what it wrote.
#[cfg(test)]
const COUNT_BUDGET: Duration = Duration::from_millis(1800);

/// How long the server may work on a counter write before it gives up (`maxTimeMS`), so a write
/// that waits for a lock (a locked server) ends and opens the gate. A server that does not answer
/// at all is cut off by the heartbeat of the counting client (see [`super::open::bound_counting`]).
pub(super) const COUNT_SERVER_BUDGET: Duration = Duration::from_millis(1500);

/// The documents of `scope` that `visibility` lets through and whose title equals `title`, by id.
pub(super) fn by_title(
    ctx: &mut Ctx<'_>,
    title: &str,
    scope: &TitleScope,
    visibility: Visibility,
) -> Result<Vec<Document>, Fail> {
    let mut filter = match scope {
        TitleScope::Global => doc! { "global": true },
        TitleScope::Repo(key) => doc! { "repo_keys": key },
    };
    filter.extend(seen(visibility));
    let limit = i64::try_from(TITLE_SCAN_MAX).unwrap_or(i64::MAX);
    let rows = ctx.find(DOCUMENTS, filter, (doc! { "_id": 1 }, 0, limit), doc! { "title": 1 })?;

    let titles = rows
        .iter()
        .map(|row| (int(row, "_id"), text(row, "title").unwrap_or_default()));
    let ids = matching_titles(titles, title);

    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows = ctx.find(
        DOCUMENTS,
        doc! { "_id": { "$in": ids } },
        (doc! { "_id": 1 }, 0, 0),
        Row::new(),
    )?;

    Ok(rows.iter().map(document).collect::<Result<_, StoreError>>()?)
}

/// Each document `visibility` lets through with a chunk whose hash is in `shas`, with how many
/// distinct ones (the chunks say which review state they are in).
pub(super) fn sharing(ctx: &mut Ctx<'_>, shas: &[String], visibility: Visibility) -> Result<Vec<(i64, usize)>, Fail> {
    if shas.len() > SHARED_SHAS_MAX {
        return Err(StoreError::Invalid(format!("at most {SHARED_SHAS_MAX} chunk hashes at once")).into());
    }

    if shas.is_empty() {
        return Ok(Vec::new());
    }

    let mut wanted = doc! { "chunk_sha": { "$in": shas } };
    wanted.extend(seen_chunks(visibility));

    let rows = ctx.aggregate(
        CHUNKS,
        vec![
            doc! { "$match": wanted },
            doc! { "$group": { "_id": { "doc": "$doc_id", "sha": "$chunk_sha" } } },
            doc! { "$group": { "_id": "$_id.doc", "shared": { "$sum": 1 } } },
            doc! { "$sort": { "_id": 1 } },
        ],
    )?;

    Ok(rows
        .iter()
        .map(|row| (int(row, "_id"), usize::try_from(int(row, "shared")).unwrap_or(0)))
        .collect())
}

/// The gate of the one counter write a process runs at a time: a store that is opened again while
/// a write still stalls meets it, so stalled writes never pile up as threads.
#[cfg(not(test))]
static COUNTING: AtomicBool = AtomicBool::new(false);

/// The gate a new store counts through: the process-wide one (a test gets its own, so tests that
/// count in parallel do not turn each other's writes away).
pub(super) fn counting_gate() -> &'static AtomicBool {
    #[cfg(not(test))]
    {
        &COUNTING
    }

    #[cfg(test)]
    {
        Box::leak(Box::new(AtomicBool::new(false)))
    }
}

/// Clears the gate of a running counter write when its thread ends, also when it panics.
struct CountGate(&'static AtomicBool);

impl Drop for CountGate {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Runs `job` on a thread of its own when `gate` is open, closing the gate until it ends, and
/// returns where its answer arrives. A closed gate is [`StoreError::Busy`], a thread that cannot
/// start an error.
pub(super) fn run_gated(
    gate: &'static AtomicBool,
    job: impl FnOnce() -> Result<(), StoreError> + Send + 'static,
) -> Result<mpsc::Receiver<Result<(), StoreError>>, StoreError> {
    if gate.swap(true, Ordering::AcqRel) {
        // Not a failure of the database: this count is dropped and the handle is kept.
        return Err(StoreError::Busy);
    }

    let (sender, receiver) = mpsc::channel();

    let spawned = thread::Builder::new().name("mongo-counter".to_owned()).spawn(move || {
        let held = CountGate(gate);
        let written = job();

        drop(held);
        let _ = sender.send(written);
    });

    if let Err(error) = spawned {
        // The closure, and the gate it owned, is gone: open the gate again.
        gate.store(false, Ordering::Release);
        return Err(StoreError::Backend(format!("a counter write could not start: {error}")));
    }

    Ok(receiver)
}

/// Runs the counter write `update` on the documents `filter` selects, for at most
/// [`COUNT_SERVER_BUDGET`] on the server.
fn update_counters(db: &Database, filter: Row, update: Row) -> Result<(), StoreError> {
    let max_time = i64::try_from(COUNT_SERVER_BUDGET.as_millis()).unwrap_or(i64::MAX);
    let mut command = doc! {
        "update": DOCUMENTS,
        "updates": [doc! { "q": filter, "u": update, "multi": true }],
        "maxTimeMS": max_time,
    };

    // A command does not take the write concern of the database by itself: the one of the URL applies as to every other write.
    if let Some(concern) = db
        .write_concern()
        .and_then(|concern| mongodb::bson::to_document(concern).ok())
    {
        command.insert("writeConcern", concern);
    }

    let reply = db
        .run_command(command)
        .run()
        .map_err(|error| connection::backend(&error))?;

    let refused = reply
        .get_array("writeErrors")
        .ok()
        .and_then(|errors| errors.first())
        .and_then(Bson::as_document)
        .or_else(|| reply.get_document("writeConcernError").ok());

    match refused {
        Some(error) => Err(StoreError::Backend(
            error
                .get_str("errmsg")
                .unwrap_or("the counter write was refused")
                .to_owned(),
        )),
        None => Ok(()),
    }
}

/// Selects the live documents `ids` whose counter `field` a `$inc` can still raise: a missing one
/// (created at 1), or a number below the largest integer. A hand-edited value of another type, or
/// one at the end, is skipped instead of failing the whole write. A draft or a pending version is
/// never counted.
fn countable(ids: &[i64], field: &str) -> Row {
    let mut missing = Row::new();
    missing.insert(field, doc! { "$exists": false });

    let mut below = Row::new();
    below.insert(field, doc! { "$type": "number", "$lt": i64::MAX });

    doc! { "_id": { "$in": ids }, "status": "live", "$or": [missing, below] }
}

impl MongoStore {
    /// Runs the counter write `update` on the documents `ids` once, for at most [`COUNT_BUDGET`]
    /// (the server gives up after [`COUNT_SERVER_BUDGET`]):
    /// it writes on a thread of its own, which a late write is left running on (a new counter is
    /// dropped meanwhile, in the whole process), so a stalled database never holds up the caller.
    fn count(&mut self, ids: &[i64], field: &str, update: Row) -> Result<(), StoreError> {
        if self.newer.get() {
            return Err(StoreError::NewerSchema);
        }

        if ids.is_empty() {
            return Ok(());
        }

        let db = self.db.clone();
        let filter = countable(ids, field);

        let receiver = run_gated(self.counting, move || update_counters(&db, filter, update))?;

        receiver
            .recv_timeout(COUNT_BUDGET)
            .unwrap_or_else(|_| Err(StoreError::Backend("the counter write timed out".to_owned())))
    }

    pub(super) fn count_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        self.count(ids, "shown", doc! { "$inc": { "shown": 1_i64 } })
    }

    pub(super) fn count_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.count(
            ids,
            "useful",
            doc! { "$inc": { "useful": 1_i64 }, "$set": { "last_useful_at": at } },
        )
    }

    /// Whether the connected user may update documents, from the privileges the server reports for
    /// it. `None` when it reports none (no authentication, or a server without the command).
    pub(super) fn may_update(&self) -> Option<bool> {
        let status = self
            .db
            .run_command(doc! { "connectionStatus": 1, "showPrivileges": true })
            .run()
            .ok()?;
        let info = status.get_document("authInfo").ok()?;

        if info.get_array("authenticatedUsers").ok()?.is_empty() {
            return None;
        }

        let privileges = info.get_array("authenticatedUserPrivileges").ok()?;
        let name = self.db.name();

        Some(
            privileges
                .iter()
                .filter_map(Bson::as_document)
                .filter(|privilege| allows_update(privilege, name))
                .count()
                > 0,
        )
    }
}

/// True when `privilege` grants `update` on the documents collection of database `name`.
fn allows_update(privilege: &Row, name: &str) -> bool {
    let updates = privilege
        .get_array("actions")
        .is_ok_and(|actions| actions.iter().any(|action| action.as_str() == Some("update")));
    let Ok(resource) = privilege.get_document("resource") else {
        return false;
    };

    let here = resource.get_bool("anyResource").unwrap_or(false)
        || (resource.get_str("db") == Ok(name) && matches!(resource.get_str("collection"), Ok("" | DOCUMENTS)));

    updates && here
}
