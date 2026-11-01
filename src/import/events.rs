//! The events an import prints, one JSON object per line (NDJSON), so Harness Desktop can show
//! each file as it is done and a cancel leaves a readable record.
//!
//! - `{"type":"plan","total":N}` first: how many files the import will look at.
//! - `{"type":"file","path","status","reason"?,"docId"?,"chunks"?,"embedding"?,"warnings"?}` after each
//!   file. `status` is `added`, `updated`, `unchanged`, `skipped` or `failed`, and `reason` says why
//!   for the last two (and for an `updated` that replaced a document of the same name from another
//!   repository). `embedding` is `done` or `pending` (no embedder was available, so the chunks
//!   wait for `docs reindex`).
//! - `{"type":"progress","done":N,"total":N}` after each file.
//! - `{"type":"done","counts":{...},"cancelled":bool,"counter":"tokenizer"|"approximate",
//!   "embeddingPending":N}` last: `counter` is what counted the tokens, `embeddingPending` how many
//!   chunks have no vector yet.

use std::io::Write;

use serde_json::{Value, json};

/// What became of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Updated,
    Unchanged,
    Skipped,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Updated => "updated",
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
    pub unchanged: usize,
    pub skipped: usize,
    pub failed: usize,
}

impl Counts {
    pub fn count(&mut self, status: Status) {
        match status {
            Status::Added => self.added += 1,
            Status::Updated => self.updated += 1,
            Status::Unchanged => self.unchanged += 1,
            Status::Skipped => self.skipped += 1,
            Status::Failed => self.failed += 1,
        }
    }

    fn json(self) -> Value {
        json!({
            "added": self.added,
            "updated": self.updated,
            "unchanged": self.unchanged,
            "skipped": self.skipped,
            "failed": self.failed,
        })
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
        }
    }
}

/// Writes events to `out`, remembering that the reader went away.
pub struct Events<'a> {
    out: &'a mut dyn Write,
    closed: bool,
}

impl<'a> Events<'a> {
    pub fn new(out: &'a mut dyn Write) -> Self {
        Self { out, closed: false }
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
            line["docId"] = json!(id);
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
