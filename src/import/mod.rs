//! Imports files into the knowledge: the manifest, the walk, one document at a time.
//!
//! - [`manifest`]: what `docs import --ndjson` reads from stdin.
//! - [`place`]: where a document is saved.
//! - [`walk`]: the files of the manifest's paths and folders, with the folder walk's rules.
//! - [`prepare`]: one file read the way an import reads it (text, title, chunks, hashes, source key).
//! - [`document`]: one file, from bytes to a stored document ([`Importer`]).
//! - [`similar`]: the stored document a file is probably a version of (`docs similar`).
//! - [`embed`]: the [`Embedder`] trait and the reuse of vectors an import does not recompute.
//! - [`events`]: the NDJSON the import prints.
//! - [`reindex`]: `docs reindex`, which gives chunks that have no vector one.
//!
//! Each document is written in its own transaction, so an import that stops (cancelled between two
//! documents, or an embedding cancelled half way through one) leaves only whole documents. Stdin
//! carries the manifest, so its end is not a cancel: SIGTERM, SIGINT and a closed stdout are.

pub mod document;
pub mod embed;
pub mod events;
pub mod manifest;
pub mod place;
mod prepare;
pub mod reindex;
pub mod similar;
pub mod walk;

#[cfg(test)]
mod tests;

pub use document::{Importer, Outcome};
pub use embed::{EmbedError, Embedder};
pub use manifest::Manifest;

use crate::store::routed::ids;
use events::{Counts, Events};

/// How an import ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub counts: Counts,
    pub cancelled: bool,
    /// Chunks stored without a vector.
    pub pending: usize,
}

/// How far an import is, across the importers (one per place) that take turns.
pub struct Progress {
    report: Report,
    done: usize,
    total: usize,
}

impl Progress {
    /// The start of an import of `total` files; prints the plan.
    pub fn new(total: usize, events: &mut Events) -> Self {
        events.plan(total);

        Self {
            report: Report {
                counts: Counts::default(),
                cancelled: false,
                pending: 0,
            },
            done: 0,
            total,
        }
    }

    /// True once the import was cancelled: nothing more is imported.
    pub fn cancelled(&self) -> bool {
        self.report.cancelled
    }

    /// Imports `candidate` with `importer` over the document `replaces` names (the id the
    /// importer's store knows), printing its events. False once the import is cancelled.
    pub fn file(
        &mut self,
        importer: &mut Importer,
        candidate: &walk::Candidate,
        replaces: Option<i64>,
        events: &mut Events,
    ) -> bool {
        if (importer.cancel)() || events.closed() {
            self.report.cancelled = true;
            return false;
        }

        match importer.import_replacing(candidate, replaces) {
            Outcome::Finished { event, pending } => {
                self.report.counts.count(event.status);
                self.report.pending += pending;
                self.done += 1;
                events.file(&event);
                events.progress(self.done, self.total);

                true
            }
            Outcome::Cancelled => {
                self.report.cancelled = true;
                false
            }
        }
    }

    /// Prints the end of the import; `counter` is what counted the tokens.
    pub fn finish(self, counter: &str, events: &mut Events) -> Report {
        events.done(self.report.counts, self.report.cancelled, counter, self.report.pending);
        self.report
    }
}

/// Imports what `manifest` names with `importer`, printing the events. A file of the manifest's
/// `replacing` replaces its document in the importer's store, whatever place its id carries (the
/// caller has chosen the store, see `docs import` for the one per place). `Err` when the manifest
/// cannot be carried out at all (too many files, a replacement of more than one file).
pub fn run(importer: &mut Importer, manifest: &Manifest, events: &mut Events) -> Result<Report, String> {
    let candidates = walk::plan(manifest)?;

    if importer.replaces.is_some() && candidates.len() != 1 {
        return Err("replaces takes exactly one file".to_owned());
    }

    let replacing = manifest.replacing_by_path();
    let mut progress = Progress::new(candidates.len(), events);

    for candidate in &candidates {
        let replaces = replacing
            .get(&manifest::canonical(&candidate.path))
            .map(|id| ids::split(*id).1)
            .or(importer.replaces);

        if !progress.file(importer, candidate, replaces, events) {
            break;
        }
    }

    Ok(progress.finish(importer.counter.name(), events))
}
