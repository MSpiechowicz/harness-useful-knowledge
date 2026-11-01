//! The embedder: one process per computer that holds the multilingual e5 model and a local-socket
//! server every `harness-knowledge` process asks for the vectors of queries and of passages (the
//! chunks an import stores). Knowledge keeps its vectors in its database, so unlike memory's
//! embedder this one has no vector cache and never reads a store.
//!
//! - [`manifest`] and [`model`]: the pinned model files and their download.
//! - [`engine`]: the tokenizer and the graph on tract, one text per inference call, with the
//!   `query: ` and `passage: ` prefixes e5 needs.
//! - [`pool`]: worker threads over one shared model, queries before passages, the model dropped
//!   after ten idle minutes.
//! - [`protocol`], [`server`] and [`client`]: the socket (on Windows a named pipe kept to this
//!   user by `pipe`), and what the other commands call.
//! - [`adapter`]: the importer's [`crate::import::embed::Embedder`] over the socket.
//! - [`service`]: ties them together without a socket of its own, so tests drive it with a fake
//!   engine; `cli_embedder` runs it.

pub mod adapter;
pub mod client;
pub mod engine;
pub mod manifest;
pub mod model;
#[cfg(windows)]
mod pipe;
pub mod pool;
pub mod protocol;
pub mod server;
pub mod service;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

pub use client::EmbedderClient;
pub use engine::{Embed, Kind, Loader, TractLoader};
pub use manifest::{MODEL, Manifest};
pub use service::Service;

use crate::support::Env;

/// The file name of the settings in the data folder.
pub const CONFIG_FILE: &str = "embedder.toml";

/// The file whose lock says an embedder runs.
pub const LOCK_FILE: &str = "embedder.lock";

/// The folder the embedder keeps its files in: next to the local database.
pub fn data_folder(env: &Env) -> Option<PathBuf> {
    crate::database_path(env).and_then(|path| path.parent().map(Path::to_path_buf))
}

/// Something the embedder reports on its standard output, one JSON line each.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Another embedder runs already.
    Busy,
    Download {
        received: u64,
        total: u64,
    },
    Verifying,
    Loading,
    Ready {
        model: String,
        dim: usize,
    },
    Idle,
    Error {
        message: String,
    },
}

impl Event {
    /// The JSON line: `{"event":"ready","model":"...","dim":384}`.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Busy => json!({ "event": "busy" }),
            Self::Download { received, total } => json!({ "event": "download", "received": received, "total": total }),
            Self::Verifying => json!({ "event": "verifying" }),
            Self::Loading => json!({ "event": "loading" }),
            Self::Ready { model, dim } => json!({ "event": "ready", "model": model, "dim": dim }),
            Self::Idle => json!({ "event": "idle" }),
            Self::Error { message } => json!({ "event": "error", "message": message }),
        }
    }
}

/// Where events go.
pub trait Events: Send + Sync {
    fn emit(&self, event: Event);
}

/// Events as JSON lines on standard output.
pub struct JsonLines;

impl Events for JsonLines {
    fn emit(&self, event: Event) {
        use std::io::Write;

        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", event.to_json());
        let _ = out.flush();
    }
}

/// Events thrown away.
pub struct Silent;

impl Events for Silent {
    fn emit(&self, _event: Event) {}
}

/// A flag that wakes whoever waits on it.
#[derive(Default)]
pub struct Stop {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl Stop {
    pub fn stop(&self) {
        *locked(&self.stopped) = true;
        self.wake.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        *locked(&self.stopped)
    }

    /// Waits up to `time`; true when stopped.
    pub fn wait(&self, time: Duration) -> bool {
        let guard = locked(&self.stopped);
        let (guard, _) = self
            .wake
            .wait_timeout_while(guard, time, |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);

        *guard
    }

    /// Waits until stopped.
    pub fn wait_forever(&self) {
        let guard = locked(&self.stopped);
        drop(
            self.wake
                .wait_while(guard, |stopped| !*stopped)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }
}

/// The lock of `mutex`, also when a thread panicked holding it (the embedder keeps serving).
pub(crate) fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How an embedder is set up. [`Options::new`] has the real timings.
#[derive(Clone)]
pub struct Options {
    /// `<data>/harness-knowledge`.
    pub root: PathBuf,
    pub manifest: Manifest,
    /// Replaces the base of the model URLs ([`manifest::BASE_URL_VAR`]).
    pub base_url: Option<String>,
    /// How long a loaded model may sit unused.
    pub idle: Duration,
    /// How many worker threads run the model (see [`pool::worker_count`]).
    pub workers: usize,
    /// The pause before a failed download or load is tried again.
    pub retry: Duration,
}

impl Options {
    pub fn new(root: PathBuf, manifest: Manifest) -> Self {
        Self {
            root,
            manifest,
            base_url: None,
            idle: engine::IDLE,
            workers: pool::workers_here(),
            retry: Duration::from_secs(5 * 60),
        }
    }
}

/// The data folder's `embedder.toml` says embedding is on (a missing file or key means off).
pub fn enabled(root: &Path) -> bool {
    let Ok(Some(bytes)) = crate::support::private::read_private(&root.join(CONFIG_FILE), 4096) else {
        return false;
    };

    let Ok(text) = String::from_utf8(bytes) else {
        return false;
    };

    toml::from_str::<toml::Table>(&text)
        .ok()
        .and_then(|table| table.get("enabled").and_then(toml::Value::as_bool))
        .unwrap_or(false)
}

/// Turns embedding on or off in `embedder.toml`.
pub fn set_enabled(root: &Path, on: bool) -> Result<(), String> {
    crate::support::private::private_folder(root, "data folder")?;

    let text = format!("enabled = {on}\n");
    crate::support::private::write_atomic(
        root,
        &root.join(CONFIG_FILE),
        text.as_bytes(),
        "embedder",
        "embedder settings",
    )
}
