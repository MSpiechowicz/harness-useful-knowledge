//! The proposal files on disk: `held/<heldId>.json` next to the database (see [`crate::held`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{
    HELD_FILE_MAX, HELD_FOLDER, HELD_MAX_ITEMS, HELD_TOTAL_BYTES, HeldItem, VERSION, is_held_id, now, random_id,
};
use crate::support::Env;
use crate::support::private::{check_folders, private_folder, read_private, write_atomic};

/// How long a proposal is kept.
const KEEP_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long a claim may last: an approval embeds one document (4000 chunks at most, minutes on a
/// slow computer), so a claim older than this was left behind by a process that died, and its
/// proposal waits again. Well above any approval, so a slow one is never put back mid-flight.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// The prefix of the temporary files a proposal is written through or claimed under.
const TEMP_PREFIX: &str = ".held-";

/// Why a proposal was not held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldError {
    /// The queue has [`HELD_MAX_ITEMS`] proposals or [`HELD_TOTAL_BYTES`] bytes waiting, or the
    /// proposal is larger than [`HELD_FILE_MAX`].
    Limit,
    /// The folder or the file could not be used (the reason).
    Failed(String),
}

/// The proposals next to the database.
#[derive(Debug, Clone)]
pub struct HeldStore {
    data: PathBuf,
    folder: PathBuf,
}

/// A waiting proposal and the size of its file.
struct Waiting {
    item: HeldItem,
    bytes: u64,
}

impl HeldStore {
    /// The proposals in the data folder `env` names; `None` when it is unknown.
    pub fn open(env: &Env) -> Option<Self> {
        crate::embed::data_folder(env).map(Self::in_folder)
    }

    /// The proposals in the data folder `data`.
    pub fn in_folder(data: PathBuf) -> Self {
        let folder = data.join(HELD_FOLDER);

        Self { data, folder }
    }

    /// Holds `item` and returns its `heldId`: the id of an identical proposal that waits already
    /// (see the module docs), else a new one. The id, time and revision are set here.
    pub fn hold(&self, mut item: HeldItem) -> Result<String, HoldError> {
        private_folder(&self.folder, "held folder")
            .and_then(|()| self.check())
            .map_err(HoldError::Failed)?;

        self.sweep();

        let waiting = self.waiting().map_err(HoldError::Failed)?;

        if let Some(same) = waiting.iter().find(|waiting| waiting.item.repeats(&item)) {
            return Ok(same.item.held_id.clone());
        }

        let others: u64 = waiting.iter().map(|waiting| waiting.bytes).sum();

        if waiting.len() >= HELD_MAX_ITEMS {
            return Err(HoldError::Limit);
        }

        item.held_id = random_id();
        item.created_at = crate::support::time::rfc3339(now());
        item.revision = item.digest();

        let text = serde_json::to_vec(&item).map_err(|error| HoldError::Failed(error.to_string()))?;
        let total = others.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));

        if text.len() > HELD_FILE_MAX || total > HELD_TOTAL_BYTES {
            return Err(HoldError::Limit);
        }

        write_atomic(
            &self.folder,
            &item_path(&self.folder, &item.held_id),
            &text,
            TEMP_PREFIX,
            "held proposal",
        )
        .map_err(HoldError::Failed)?;

        Ok(item.held_id)
    }

    /// Claims proposal `held_id` for approving or rejecting it; `None` when it is not waiting (a
    /// decision took it, or it never was). Nothing lists, claims or replaces a claimed proposal
    /// until it is [`Claim::restore`]d.
    pub fn claim(&self, held_id: &str) -> Result<Option<Claim>, String> {
        if !is_held_id(held_id) || !self.exists()? {
            return Ok(None);
        }

        let id = random_id();
        // The claim's start is in its name: a rename keeps the file's time, and stamping the file
        // before the move would collide with another claim's rename on Windows.
        let aside = self
            .folder
            .join(format!("{TEMP_PREFIX}{CLAIM_MARK}{}-{id}.tmp", now_millis()));
        let home = item_path(&self.folder, held_id);

        match std::fs::rename(&home, &aside) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("moving the held proposal failed: {error}")),
        }

        match parse(&aside, held_id) {
            Some(item) => Ok(Some(Claim {
                item,
                path: aside,
                home,
            })),
            None => {
                put_back(&aside, &home)?;
                Ok(None)
            }
        }
    }

    /// The proposals waiting, newest first.
    pub fn list(&self) -> Result<Vec<HeldItem>, String> {
        if !self.exists()? {
            return Ok(Vec::new());
        }

        self.restore_stranded();

        let mut items: Vec<HeldItem> = self.waiting()?.into_iter().map(|waiting| waiting.item).collect();
        items.sort_by(|a, b| (&b.created_at, &b.held_id).cmp(&(&a.created_at, &a.held_id)));

        Ok(items)
    }

    /// How many proposals wait: the files named like one, none of them read. A missing folder
    /// holds none, and nothing is created or put back.
    pub fn count(&self) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.folder) else {
            return 0;
        };

        entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_suffix(".json"))
                    .is_some_and(is_held_id)
            })
            .count()
    }

    /// Proposal `held_id`, when it is waiting.
    pub fn get(&self, held_id: &str) -> Result<Option<HeldItem>, String> {
        if !is_held_id(held_id) || !self.exists()? {
            return Ok(None);
        }

        Ok(parse(&item_path(&self.folder, held_id), held_id))
    }

    /// Puts back the proposals whose claim is older than [`CLAIM_TIMEOUT`] (the process that claimed
    /// them died before it finished or restored them), unless a newer proposal has their id.
    fn restore_stranded(&self) {
        let Ok(entries) = std::fs::read_dir(&self.folder) else {
            return;
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();

            if !(name.starts_with(TEMP_PREFIX) && name.ends_with(".tmp"))
                || !claim_older_than(&name, &entry, CLAIM_TIMEOUT)
            {
                continue;
            }

            let path = entry.path();
            let Some(item) = read_item(&path) else {
                continue;
            };

            let _ = put_back(&path, &item_path(&self.folder, &item.held_id));
        }
    }

    /// Removes the proposals and half-written files older than 30 days, after putting back the
    /// proposals of dead claims.
    pub fn sweep(&self) {
        self.restore_stranded();

        let Ok(entries) = std::fs::read_dir(&self.folder) else {
            return;
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let ours = name.ends_with(".json") || (name.starts_with(TEMP_PREFIX) && name.ends_with(".tmp"));

            if ours && older_than(&entry, KEEP_FOR) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// The parsed proposals in the folder, with the sizes of their files.
    fn waiting(&self) -> Result<Vec<Waiting>, String> {
        let entries = match std::fs::read_dir(&self.folder) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("the held folder cannot be read: {error}")),
        };

        let mut waiting = Vec::new();

        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };

            let path = item_path(&self.folder, id);
            let Some(item) = is_held_id(id).then(|| parse(&path, id)).flatten() else {
                continue;
            };

            let bytes = std::fs::symlink_metadata(&path).map_or(0, |metadata| metadata.len());
            waiting.push(Waiting { item, bytes });
        }

        Ok(waiting)
    }

    /// True when the held folder exists; fails when it or the data folder is not private.
    fn exists(&self) -> Result<bool, String> {
        match std::fs::symlink_metadata(&self.folder) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            _ => self.check().map(|()| true),
        }
    }

    /// Fails unless the data folder and the held folder are private folders of this user.
    fn check(&self) -> Result<(), String> {
        check_folders(&[
            (self.data.as_path(), "data folder"),
            (self.folder.as_path(), "held folder"),
        ])
    }

    /// The held folder.
    pub fn folder(&self) -> &Path {
        &self.folder
    }
}

/// A proposal claimed for approving or rejecting ([`HeldStore::claim`]): its file under a claim name.
#[derive(Debug)]
pub struct Claim {
    pub item: HeldItem,
    path: PathBuf,
    /// Where the file was waiting.
    home: PathBuf,
}

impl Claim {
    /// Removes the claimed proposal: it was approved or rejected.
    pub fn finish(self) -> Result<(), String> {
        std::fs::remove_file(&self.path).map_err(|error| format!("removing the held proposal failed: {error}"))
    }

    /// Puts the proposal back where it waited: it was not approved.
    pub fn restore(self) -> Result<(), String> {
        put_back(&self.path, &self.home)
    }
}

/// Moves the file at `aside` back to `home`, unless another file took that name meanwhile.
fn put_back(aside: &Path, home: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(home).is_ok() {
        return Err("the held proposal could not be put back: a newer one has its id".to_owned());
    }

    std::fs::rename(aside, home).map_err(|error| format!("putting the held proposal back failed: {error}"))
}

/// True when the file of `entry` was last written more than `age` ago.
/// The marker of a claimed proposal's name, followed by the claim's start in milliseconds.
const CLAIM_MARK: &str = "c";

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis())
}

/// Whether the claim a temporary file stands for began more than `age` ago: by the time in its
/// name, or by the file's own time for a name without one (a half-written proposal).
fn claim_older_than(name: &str, entry: &std::fs::DirEntry, age: Duration) -> bool {
    let started = name
        .strip_prefix(TEMP_PREFIX)
        .and_then(|rest| rest.strip_prefix(CLAIM_MARK))
        .and_then(|rest| rest.split('-').next())
        .and_then(|millis| millis.parse::<u64>().ok());

    match started {
        Some(millis) => now_millis().saturating_sub(u128::from(millis)) > age.as_millis(),
        None => older_than(entry, age),
    }
}

fn older_than(entry: &std::fs::DirEntry, age: Duration) -> bool {
    entry
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|elapsed| elapsed > age)
}

/// The proposal in the file at `path`, whatever its name: its id is the one it holds.
fn read_item(path: &Path) -> Option<HeldItem> {
    let bytes = read_private(path, HELD_FILE_MAX).ok()??;
    let item: HeldItem = serde_json::from_slice(&bytes).ok()?;

    (item.version == VERSION && is_held_id(&item.held_id) && item.revision == item.digest()).then_some(item)
}

/// The proposal `id` read from `path`, when the file is readable, of this version, names that id
/// and has the revision of its content.
fn parse(path: &Path, id: &str) -> Option<HeldItem> {
    read_item(path).filter(|item| item.held_id == id)
}

fn item_path(folder: &Path, id: &str) -> PathBuf {
    folder.join(format!("{id}.json"))
}

#[cfg(test)]
pub(super) fn path_of(store: &HeldStore, id: &str) -> PathBuf {
    item_path(&store.folder, id)
}
