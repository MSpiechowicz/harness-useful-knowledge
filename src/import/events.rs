//! The events an import prints, one JSON object per line (NDJSON), so Harness Desktop can show
//! each file as it is done and a cancel leaves a readable record.
//!
//! - `{"type":"plan","total":N}` first: how many files the import will look at.
//! - `{"type":"file","path","status","reason"?,"docId"?,"chunks"?,"embedding"?,"warnings"?,"draft"?,
//!   "asked"?}` after each file. `status` is `added`, `updated`, `pending`, `unchanged`, `skipped` or
//!   `failed`, and `reason` says why for the last two (and for an `updated` that replaced a document
//!   of the same name from another repository). `pending` is a pending version stored for a live
//!   document (the live text is unchanged until it goes live, `docId` is the pending row).
//!   `draft` (on `added`, `updated` and `pending`) is true when the row is not live, `asked` that it
//!   was asked for approval in the same step. `embedding` is `done` or `pending` (no embedder was
//!   available, so the chunks wait for `docs reindex`).
//!   `docId` is a number, or the string `local:12` for a document on this computer when both places
//!   are in use.
//! - `{"type":"progress","done":N,"total":N}` after each file.
//! - `{"type":"done","counts":{...},"cancelled":bool,"counter":"tokenizer"|"approximate",
//!   "embeddingPending":N}` last: `counter` is what counted the tokens, `embeddingPending` how many
//!   chunks have no vector yet. `counts` has `added`, `updated`, `unchanged`, `skipped` and `failed`,
//!   and `pending` when a pending version was stored.

use std::io::Write;

use serde_json::{Value, json};

use crate::config::Place;
use crate::store::routed::ids;

/// What became of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Updated,
    /// A pending version of a live document was stored.
    Pending,
    Unchanged,
    Skipped,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Updated => "updated",
            Self::Pending => "pending",
            Self::Unchanged => "unchanged",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

/// How many files ended each way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub added: usize,
    pub updated: usize,
    pub pending: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub failed: usize,
}

impl Counts {
    pub fn count(&mut self, status: Status) {
        match status {
            Status::Added => self.added += 1,
            Status::Updated => self.updated += 1,
            Status::Pending => self.pending += 1,
            Status::Unchanged => self.unchanged += 1,
            Status::Skipped => self.skipped += 1,
            Status::Failed => self.failed += 1,
        }
    }

    /// The counts as the `done` event carries them: `pending` only when some file was.
    fn json(self) -> Value {
        let mut counts = json!({
            "added": self.added,
            "updated": self.updated,
            "unchanged": self.unchanged,
            "skipped": self.skipped,
            "failed": self.failed,
        });

        if self.pending > 0 {
            counts["pending"] = json!(self.pending);
        }

        counts
    }
}

/// The `file` event of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEvent {
    pub path: String,
    pub status: Status,
    pub reason: Option<String>,
    pub doc_id: Option<i64>,
    pub chunks: Option<usize>,
    pub embedding: Option<&'static str>,
    pub warnings: Vec<String>,
    /// True when the row written is not live (a draft or a pending version).
    pub draft: bool,
    /// True when the row was asked for approval in the same step.
    pub asked: bool,
}

impl FileEvent {
    pub fn new(path: &str, status: Status) -> Self {
        Self {
            path: path.to_owned(),
            status,
            reason: None,
            doc_id: None,
            chunks: None,
            embedding: None,
            warnings: Vec::new(),
            draft: false,
            asked: false,
        }
    }
}

/// Writes events to `out`, remembering that the reader went away.
pub struct Events<'a> {
    out: &'a mut dyn Write,
    closed: bool,
    /// The place of the documents when two are in use: their ids are written with it (`local:12`).
    place: Option<Place>,
}

impl<'a> Events<'a> {
    pub fn new(out: &'a mut dyn Write) -> Self {
        Self {
            out,
            closed: false,
            place: None,
        }
    }

    /// Writes document ids tagged with `place` (when both places are in use).
    pub fn in_place(mut self, place: Option<Place>) -> Self {
        self.place = place;
        self
    }

    /// As [`Events::in_place`], for an import that moves on to another place.
    pub fn set_place(&mut self, place: Option<Place>) {
        self.place = place;
    }

    /// True once a write failed (the reader closed the pipe): nobody is watching, so the import
    /// stops like a cancel.
    pub fn closed(&self) -> bool {
        self.closed
    }

    pub fn plan(&mut self, total: usize) {
        self.emit(json!({ "type": "plan", "total": total }));
    }

    pub fn file(&mut self, event: &FileEvent) {
        let mut line = json!({ "type": "file", "path": event.path, "status": event.status.as_str() });

        if let Some(reason) = &event.reason {
            line["reason"] = json!(reason);
        }

        if let Some(id) = event.doc_id {
            line["docId"] = match self.place {
                Some(place) => ids::json(ids::tag(place, id)),
                None => json!(id),
            };
        }

        if let Some(chunks) = event.chunks {
            line["chunks"] = json!(chunks);
        }

        if let Some(embedding) = event.embedding {
            line["embedding"] = json!(embedding);
        }

        if !event.warnings.is_empty() {
            line["warnings"] = json!(event.warnings);
        }

        if matches!(event.status, Status::Added | Status::Updated | Status::Pending) {
            line["draft"] = json!(event.draft);
        }

        if event.asked {
            line["asked"] = json!(true);
        }

        self.emit(line);
    }

    pub fn progress(&mut self, done: usize, total: usize) {
        self.emit(json!({ "type": "progress", "done": done, "total": total }));
    }

    pub fn done(&mut self, counts: Counts, cancelled: bool, counter: &str, embedding_pending: usize) {
        self.emit(json!({
            "type": "done",
            "counts": counts.json(),
            "cancelled": cancelled,
            "counter": counter,
            "embeddingPending": embedding_pending,
        }));
    }

    fn emit(&mut self, line: Value) {
        if writeln!(self.out, "{line}").and_then(|()| self.out.flush()).is_err() {
            self.closed = true;
        }
    }
}
