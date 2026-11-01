//! Whether the remote database could be reached when it was last tried.
//!
//! A [`Reach`] is shared by the [`crate::store::RoutedStore`] that opens the remote database and
//! the commands that probe it. A failed attempt (the remote is unreachable, see
//! [`crate::store::StoreError::Unreachable`]) starts a cooldown: until it ends, the remote is not
//! tried again and the routed store answers from this computer at once, so a remote that is down
//! costs its connect timeout once and not on every call.
//!
//! The cooldown is also kept in `remote-unreachable.json` next to the local database
//! ([`Reach::beside`]), because every `harness-knowledge docs list` Harness Desktop runs is a new
//! process: without the file each of them would wait out the timeout itself. The file names the
//! remote database it is about and the (already scrubbed) reason, it is private to the owner, and
//! one that cannot be read or does not match counts as no failure.
//!
//! A failure that is only a wait that ran out (the remote did not answer within a probe the caller
//! chose, see [`Reach::failed_in`]) carries how long it waited, and is honoured by a reader only when
//! that reader would not wait longer ([`Reach::cooling_for`]). So the 1 s a hook gives the remote
//! never makes a 3 s read, or a write that waits as long as its driver does, skip a remote that
//! merely connects slowly. A refused connection, a host that does not resolve and the like are
//! facts for every reader.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::support::private::{private_folder, read_private, write_atomic};

/// The file name of the shared cooldown, next to the local database.
const FILE: &str = "remote-unreachable.json";

/// The most bytes read from the file.
const MAX_BYTES: usize = 16 * 1024;

/// How long an unreachable remote is not tried again.
pub const COOLDOWN: Duration = Duration::from_secs(60);

/// The last failed attempt.
struct Failure {
    at: Instant,
    message: String,
    /// How long the attempt waited for an answer, for a failure that is a wait that ran out.
    waited: Option<Duration>,
}

/// The failure as other processes see it.
#[derive(Serialize, Deserialize)]
struct Recorded {
    remote: String,
    /// Milliseconds since the Unix epoch.
    at: u64,
    message: String,
    /// Milliseconds the attempt waited for an answer, absent for a failure that is not a wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    waited: Option<u64>,
}

/// Where the cooldown is shared between processes, for the remote database `remote`.
#[derive(Clone)]
struct Shared {
    folder: PathBuf,
    path: PathBuf,
    remote: String,
}

/// Shared state of the remote database's reachability, clones share it.
#[derive(Clone)]
pub struct Reach {
    cooldown: Duration,
    last: Arc<Mutex<Option<Failure>>>,
    shared: Option<Shared>,
}

impl Default for Reach {
    fn default() -> Self {
        Self::with_cooldown(COOLDOWN)
    }
}

impl std::fmt::Debug for Reach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reach")
            .field("cooling", &self.cooling().is_some())
            .finish_non_exhaustive()
    }
}

impl Reach {
    /// A reach whose failed attempts are not retried for `cooldown`.
    pub fn with_cooldown(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last: Arc::new(Mutex::new(None)),
            shared: None,
        }
    }

    /// A reach whose cooldown is also kept next to the local `database`, for the remote database
    /// `remote` ([`crate::config::Backend::remote_key`]), so other processes skip the wait too.
    pub fn beside(database: &Path, remote: &str) -> Self {
        Self::beside_with(database, remote, COOLDOWN)
    }

    /// [`Reach::beside`] with another cooldown.
    pub fn beside_with(database: &Path, remote: &str, cooldown: Duration) -> Self {
        let path = database.with_file_name(FILE);
        let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();

        Self {
            shared: Some(Shared {
                folder,
                path,
                remote: remote.to_owned(),
            }),
            ..Self::with_cooldown(cooldown)
        }
    }

    /// Records an unreachable remote that told so itself (`message` must be safe to show): a refused
    /// connection, a host that does not resolve, a lost connection. Every reader honours it.
    pub fn failed(&self, message: &str) {
        self.record(message, None);
    }

    /// Records a remote that did not answer within `waited`, a wait the caller chose: only a reader
    /// that waits no longer than that honours it ([`Reach::cooling_for`]).
    pub fn failed_after(&self, message: &str, waited: Duration) {
        self.record(message, Some(waited));
    }

    /// Records a failed attempt that took `elapsed` when the caller was willing to wait `probe`: a
    /// failure that came quicker was the remote's own answer, one that used up the probe was only
    /// the wait running out.
    pub fn failed_in(&self, message: &str, elapsed: Duration, probe: Duration) {
        if elapsed >= probe {
            self.failed_after(message, elapsed);
        } else {
            self.failed(message);
        }
    }

    fn record(&self, message: &str, waited: Option<Duration>) {
        let failure = Failure {
            at: Instant::now(),
            message: message.to_owned(),
            waited,
        };

        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(failure);

        let Some(shared) = &self.shared else { return };
        let recorded = Recorded {
            remote: shared.remote.clone(),
            at: now(),
            message: message.to_owned(),
            waited: waited.map(millis),
        };

        // A file that cannot be written only costs the other processes their own wait.
        if let Ok(bytes) = serde_json::to_vec(&recorded)
            && (shared.folder.is_dir() || private_folder(&shared.folder, "knowledge folder").is_ok())
        {
            let _written = write_atomic(
                &shared.folder,
                &shared.path,
                &bytes,
                "remote-unreachable-",
                "record of the unreachable remote database",
            );
        }
    }

    /// Records that the remote answered. Only the record about this remote database is removed: the
    /// file may hold another one's failure, which this answer says nothing about.
    pub fn reached(&self) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = None;

        if let Some(shared) = &self.shared
            && self.recorded_of(shared).is_some()
        {
            let _removed = std::fs::remove_file(&shared.path);
        }
    }

    /// The message of the last failed attempt while its cooldown runs, whoever's wait it was, `None`
    /// once it has ended or when the last attempt worked.
    pub fn cooling(&self) -> Option<String> {
        self.cooling_with(None)
    }

    /// As [`Reach::cooling`], for a reader that waits `budget` for the remote: a failure that was
    /// only a shorter wait does not count (this reader may well get its answer).
    pub fn cooling_for(&self, budget: Duration) -> Option<String> {
        self.cooling_with(Some(budget))
    }

    fn cooling_with(&self, budget: Option<Duration>) -> Option<String> {
        let last = self.last.lock().unwrap_or_else(PoisonError::into_inner);

        if let Some(failure) = last.as_ref() {
            let honoured = failure.at.elapsed() < self.cooldown && honours(failure.waited, budget);

            return honoured.then(|| failure.message.clone());
        }

        drop(last);

        self.recorded(budget)
    }

    /// The failure another process recorded for this remote database, while its cooldown runs.
    fn recorded(&self, budget: Option<Duration>) -> Option<String> {
        let shared = self.shared.as_ref()?;
        let recorded = self.recorded_of(shared)?;

        // A time in the future (a clock set back) is not trusted: the next call tries the remote.
        let age = now().checked_sub(recorded.at)?;
        let waited = recorded.waited.map(Duration::from_millis);

        (u128::from(age) < self.cooldown.as_millis() && honours(waited, budget)).then_some(recorded.message)
    }

    /// The record in the file when it is about this remote database.
    fn recorded_of(&self, shared: &Shared) -> Option<Recorded> {
        let bytes = read_private(&shared.path, MAX_BYTES).ok().flatten()?;
        let recorded: Recorded = serde_json::from_slice(&bytes).ok()?;

        (recorded.remote == shared.remote).then_some(recorded)
    }
}

/// Whether a failure that waited `waited` counts for a reader that waits `budget`: one that was not
/// a wait counts for everybody, a wait counts for a reader that would not wait longer, and every
/// failure counts when the reader has no budget (it is only asking).
fn honours(waited: Option<Duration>, budget: Option<Duration>) -> bool {
    match (waited, budget) {
        (Some(waited), Some(budget)) => waited >= budget,
        _ => true,
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Milliseconds since the Unix epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_is_repeated_until_its_cooldown_ends_or_the_remote_answers() {
        let reach = Reach::with_cooldown(Duration::from_millis(40));
        assert_eq!(reach.cooling(), None, "nothing failed yet");

        reach.failed("down");
        assert_eq!(reach.cooling().as_deref(), Some("down"));
        assert_eq!(reach.clone().cooling().as_deref(), Some("down"), "clones share it");

        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(reach.cooling(), None, "the next call tries the remote");

        reach.failed("down again");
        reach.reached();
        assert_eq!(reach.cooling(), None);
    }

    #[test]
    fn another_process_sees_the_failure_of_its_own_remote_until_it_ends_or_one_succeeds() {
        let dir = tempfile::tempdir().expect("dir");
        let database = dir.path().join("knowledge").join("knowledge.sqlite3");

        Reach::beside(&database, "remote:a").failed("down");

        let other_process = |remote: &str| Reach::beside(&database, remote).cooling();
        assert_eq!(other_process("remote:a").as_deref(), Some("down"));
        assert_eq!(other_process("remote:b"), None, "another remote database is tried");

        let short = Reach::beside_with(&database, "remote:a", Duration::from_secs(0));
        assert_eq!(short.cooling(), None, "an ended cooldown is not repeated");

        Reach::beside(&database, "remote:a").reached();
        assert_eq!(other_process("remote:a"), None, "an answer clears it for everyone");
    }

    #[test]
    fn a_file_that_is_not_a_record_or_is_from_the_future_counts_as_no_failure() {
        let dir = tempfile::tempdir().expect("dir");
        let database = dir.path().join("knowledge.sqlite3");
        let file = dir.path().join(FILE);

        std::fs::write(&file, "not json").expect("write");
        assert_eq!(Reach::beside(&database, "remote:a").cooling(), None);

        let future = format!(r#"{{"remote":"remote:a","at":{},"message":"down"}}"#, now() + 3_600_000);
        std::fs::write(&file, future).expect("write");
        assert_eq!(Reach::beside(&database, "remote:a").cooling(), None);
    }

    #[test]
    fn a_wait_that_ran_out_counts_only_for_a_reader_that_waits_no_longer() {
        let dir = tempfile::tempdir().expect("dir");
        let database = dir.path().join("knowledge.sqlite3");
        let hook = Duration::from_secs(1);
        let read = Duration::from_secs(3);

        // The hook gave up after its second: that says nothing about a reader that waits three.
        Reach::beside(&database, "remote:a").failed_in("slow", hook, hook);

        let cooling = |budget: Duration| Reach::beside(&database, "remote:a").cooling_for(budget);
        assert_eq!(cooling(hook).as_deref(), Some("slow"));
        assert_eq!(cooling(read), None);

        // A read that waited its three seconds is a reason for the hook as well.
        Reach::beside(&database, "remote:a").failed_in("slower", read, read);
        assert_eq!(cooling(hook).as_deref(), Some("slower"));
        assert_eq!(cooling(read).as_deref(), Some("slower"));

        // An answer that came at once (refused) is a fact for every reader.
        Reach::beside(&database, "remote:a").failed_in("refused", Duration::from_millis(5), read);
        assert_eq!(cooling(read).as_deref(), Some("refused"));
        assert_eq!(cooling(Duration::from_secs(60)).as_deref(), Some("refused"));

        // The same process sees it the same way.
        let process = Reach::with_cooldown(Duration::from_secs(60));
        process.failed_in("slow", hook, hook);
        assert_eq!(process.cooling_for(read), None);
        assert_eq!(process.cooling_for(hook).as_deref(), Some("slow"));
    }

    #[test]
    fn an_answer_clears_the_failure_of_its_own_remote_and_leaves_another_ones() {
        let dir = tempfile::tempdir().expect("dir");
        let database = dir.path().join("knowledge.sqlite3");

        Reach::beside(&database, "remote:a").failed("down");

        Reach::beside(&database, "remote:b").reached();
        assert_eq!(
            Reach::beside(&database, "remote:a").cooling().as_deref(),
            Some("down"),
            "remote b answering says nothing about remote a"
        );

        Reach::beside(&database, "remote:a").reached();
        assert_eq!(Reach::beside(&database, "remote:a").cooling(), None);
        assert!(!dir.path().join(FILE).exists());
    }
}
