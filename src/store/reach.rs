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
}

/// The failure as other processes see it.
#[derive(Serialize, Deserialize)]
struct Recorded {
    remote: String,
    /// Milliseconds since the Unix epoch.
    at: u64,
    message: String,
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

    /// Records an unreachable remote (`message` must be safe to show).
    pub fn failed(&self, message: &str) {
        let failure = Failure {
            at: Instant::now(),
            message: message.to_owned(),
        };

        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(failure);

        let Some(shared) = &self.shared else { return };
        let recorded = Recorded {
            remote: shared.remote.clone(),
            at: now(),
            message: message.to_owned(),
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

    /// Records that the remote answered.
    pub fn reached(&self) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = None;

        if let Some(shared) = &self.shared {
            let _removed = std::fs::remove_file(&shared.path);
        }
    }

    /// The message of the last failed attempt while its cooldown runs, `None` once it has ended
    /// (the next call tries the remote) or when the last attempt worked.
    pub fn cooling(&self) -> Option<String> {
        let last = self.last.lock().unwrap_or_else(PoisonError::into_inner);

        if let Some(failure) = last.as_ref() {
            return (failure.at.elapsed() < self.cooldown).then(|| failure.message.clone());
        }

        drop(last);

        self.recorded()
    }

    /// The failure another process recorded for this remote database, while its cooldown runs.
    fn recorded(&self) -> Option<String> {
        let shared = self.shared.as_ref()?;
        let bytes = read_private(&shared.path, MAX_BYTES).ok().flatten()?;
        let recorded: Recorded = serde_json::from_slice(&bytes).ok()?;

        // A time in the future (a clock set back) is not trusted: the next call tries the remote.
        let age = now().checked_sub(recorded.at)?;

        (recorded.remote == shared.remote && u128::from(age) < self.cooldown.as_millis()).then_some(recorded.message)
    }
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
}
