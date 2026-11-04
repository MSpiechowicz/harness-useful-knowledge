//! The count of documents that wait for a person, kept next to the database so the desktop's poll
//! does not scan every waiting row (and read its history) each time.
//!
//! An answer is valid for as long as the place's generation, its number of waiting rows and the
//! person's name stay the same: every change of a document or its review bumps the generation. The
//! file is `waiting-for.json` in the data folder (`0600`, written atomically through
//! [`write_atomic`]). It is a cache only: a file that cannot be read, parsed or written is ignored
//! and the count is worked out again.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{Error, waiting_for};
use crate::config::Place;
use crate::store::KnowledgeStore;
use crate::support::private::{read_private, write_atomic};

/// The cache file in the data folder.
pub const WAITING_CACHE_FILE: &str = "waiting-for.json";

/// Most answers the file keeps (the oldest go first).
const ENTRIES_MAX: usize = 16;

/// Most bytes of the file that are read.
const FILE_BYTES_MAX: usize = 16 * 1024;

/// What a count was worked out for: the place and how it stood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceState {
    pub place: Place,
    pub generation: i64,
    /// How many documents wait in the place (a second check against two databases that happen to
    /// share a generation).
    pub waiting: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    place: String,
    /// The name as [`same_person`](super::same_person) reads it: trimmed and lower-cased.
    name: String,
    generation: i64,
    waiting: usize,
    count: usize,
}

impl Entry {
    fn answers(&self, other: &Self) -> bool {
        self.place == other.place && self.name == other.name
    }

    fn is_current(&self, other: &Self) -> bool {
        self.answers(other) && self.generation == other.generation && self.waiting == other.waiting
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cached {
    #[serde(default)]
    entries: Vec<Entry>,
}

fn load(folder: &Path) -> Cached {
    read_private(&folder.join(WAITING_CACHE_FILE), FILE_BYTES_MAX)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(folder: &Path, cached: &Cached) {
    let Ok(text) = serde_json::to_vec(cached) else {
        return;
    };

    // A cache that cannot be written is only slower.
    let _ = write_atomic(
        folder,
        &folder.join(WAITING_CACHE_FILE),
        &text,
        ".waiting-for-",
        "waiting count",
    );
}

/// [`waiting_for`], answered from the cache in `folder` (the data folder) while `state` and `name`
/// are the ones it was worked out for. With no folder, or one that is not there, it is worked out
/// each time.
pub fn waiting_for_cached(
    store: &mut dyn KnowledgeStore,
    folder: Option<&Path>,
    state: PlaceState,
    name: Option<&str>,
) -> Result<usize, Error> {
    let Some(name) = name.filter(|name| !name.trim().is_empty()) else {
        return Ok(0);
    };

    let Some(folder) = folder.filter(|folder| folder.is_dir()) else {
        return waiting_for(store, Some(name));
    };

    let wanted = Entry {
        place: state.place.as_str().to_owned(),
        name: name.trim().to_lowercase(),
        generation: state.generation,
        waiting: state.waiting,
        count: 0,
    };

    let mut cached = load(folder);

    if let Some(hit) = cached.entries.iter().find(|entry| entry.is_current(&wanted)) {
        return Ok(hit.count);
    }

    let count = waiting_for(store, Some(name))?;

    cached.entries.retain(|entry| !entry.answers(&wanted));
    cached.entries.push(Entry { count, ..wanted });

    if cached.entries.len() > ENTRIES_MAX {
        let extra = cached.entries.len() - ENTRIES_MAX;
        cached.entries.drain(..extra);
    }

    save(folder, &cached);

    Ok(count)
}
