//! Imports files into the knowledge: the manifest, the walk, one document at a time.
//!
//! - [`manifest`]: what `docs import --ndjson` reads from stdin.
//! - [`walk`]: the files of the manifest's paths and folders, with the folder walk's rules.
//! - [`document`]: one file, from bytes to a stored document ([`Importer`]).
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
pub mod reindex;
pub mod walk;

#[cfg(test)]
mod tests;

pub use document::{Importer, Outcome};
pub use embed::{EmbedError, Embedder};
pub use manifest::Manifest;

use events::{Counts, Events};

/// How an import ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub counts: Counts,
    pub cancelled: bool,
    /// Chunks stored without a vector.
    pub pending: usize,
}

/// Imports what `manifest` names with `importer`, printing the events. `Err` when the manifest
/// cannot be carried out at all (too many files, a replacement of more than one file).
pub fn run(importer: &mut Importer, manifest: &Manifest, events: &mut Events) -> Result<Report, String> {
    let candidates = walk::plan(manifest)?;

    if importer.replaces.is_some() && candidates.len() != 1 {
        return Err("replaces takes exactly one file".to_owned());
    }

    let total = candidates.len();
    let mut report = Report {
        counts: Counts::default(),
        cancelled: false,
        pending: 0,
    };

    events.plan(total);

    for (done, candidate) in candidates.iter().enumerate() {
        if (importer.cancel)() || events.closed() {
            report.cancelled = true;
            break;
        }

        match importer.import(candidate) {
            Outcome::Finished { event, pending } => {
                report.counts.count(event.status);
                report.pending += pending;
                events.file(&event);
                events.progress(done + 1, total);
            }
            Outcome::Cancelled => {
                report.cancelled = true;
                break;
            }
        }
    }

    events.done(report.counts, report.cancelled, importer.counter.name(), report.pending);

    Ok(report)
}
