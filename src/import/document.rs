//! Imports one file: reads it, extracts and chunks its text, decides whether it is new, changed
//! or already there, embeds what is new and stores the document in one transaction.
//!
//! The decision is by content: the same text hash is `unchanged` (the links of this import are
//! merged in), else the same source key is `updated` in place (its chunks replaced, the vectors of
//! chunks whose hash did not change reused), else the document is `added`.
//!
//! Drafts and team approval (see [`crate::store::review`]):
//! - A new document is a draft when the import asks for drafts ([`Importer::draft`]) or team
//!   approval is on for the place it is written to ([`Importer::approval`]).
//! - A replaced live document is replaced in place, unless a draft is asked for or approval is on:
//!   then its pending version is written (or updated, there is at most one) and the live text stays
//!   until the pending version goes live. The event is `pending`.
//! - A draft, waiting or `changesRequested` row that is replaced stays a draft (the store sends
//!   a waiting row back to draft), whatever the settings.
//! - A file whose text is the live document's, or the pending version's, is `unchanged`.
//! - The file as read is kept as the original for Download ([`KnowledgeStore::put_file`]).
//! - With [`Importer::ask`] the row written is asked for approval in the same step, and so is an
//!   `unchanged` draft (or row with requested changes) that the file matched.

use std::time::{SystemTime, UNIX_EPOCH};

use super::embed::{self, Embedder};
use super::events::{FileEvent, Status as Event};
use super::prepare::{Prepared, Stop, prepare};
use super::walk::Candidate;
use crate::chunk::{Chunked, passage};
use crate::extract::pdf::PdfPages;
use crate::store::{
    Author, Chunk, Detail, Document, KnowledgeStore, Link, Origin, ReviewAction, ReviewChange, ReviewReply, Status,
    Visibility, clean,
};
use crate::tokens::{APPROX_COUNTER, TokenCounter};

/// What importing one file came to.
pub enum Outcome {
    Finished {
        event: FileEvent,
        /// Chunks stored without a vector.
        pending: usize,
    },
    /// The import was cancelled before this file was written: nothing of it is stored.
    Cancelled,
}

/// Imports files into a store.
pub struct Importer<'a> {
    pub store: &'a mut dyn KnowledgeStore,
    pub counter: &'a dyn TokenCounter,
    pub pdf: &'a dyn PdfPages,
    /// `None`: chunks are stored without vectors.
    pub embedder: Option<&'a mut dyn Embedder>,
    /// The repositories every document of this import links to.
    pub links: Vec<Link>,
    /// The document every file of this import replaces, unless [`Importer::import_replacing`] names
    /// one for the file.
    pub replaces: Option<i64>,
    /// Who writes the documents of this import: set as `added_by` of a new document and as
    /// `updated_by` of a changed one, which keeps the `added_by` it had. An unchanged document keeps
    /// both. `None` when no git name is known.
    pub author: Option<Author>,
    /// Who the documents of this import come from: [`Origin::User`], or an agent's proposal the
    /// person approved. A document an import updates keeps its origin, unless it is `replaces`.
    pub origin: Origin,
    /// The title every document of this import gets, instead of the one its file has.
    pub title: Option<String>,
    /// False for an import of untrusted text (an agent's approved proposal): the key its text names
    /// for itself (a Confluence page id) is ignored, and a key that is taken is refused unless the
    /// import `replaces` that document.
    pub trust_extracted_key: bool,
    /// Store the files as drafts, and a replaced live document as a pending version.
    pub draft: bool,
    /// Team approval is on for the place this import writes to (never for this computer's file):
    /// every file is then written as a draft or a pending version. The caller reads it from the
    /// remote database (see [`crate::review::approval_in`]).
    pub approval: bool,
    /// Ask for approval right after writing a row, with [`Importer::note`]. Refuses the import
    /// of every file unless [`Importer::approval`] is on and [`Importer::author`] has a name.
    pub ask: bool,
    /// The note that goes with the ask.
    pub note: Option<String>,
    pub cancel: &'a dyn Fn() -> bool,
    /// Set when the embedder failed: later files do not try it again.
    embedder_failed: bool,
}

/// What an import does with the text of a file.
enum Plan {
    /// The same text is stored already, in the row of this id.
    Unchanged(i64),
    /// Store it as the row `id` (0: a new one) in `status`, a pending version of `pending_of`.
    Store {
        id: i64,
        status: Status,
        pending_of: Option<i64>,
    },
}

impl<'a> Importer<'a> {
    pub fn new(
        store: &'a mut dyn KnowledgeStore,
        counter: &'a dyn TokenCounter,
        pdf: &'a dyn PdfPages,
        cancel: &'a dyn Fn() -> bool,
    ) -> Self {
        Self {
            store,
            counter,
            pdf,
            embedder: None,
            links: Vec::new(),
            replaces: None,
            author: None,
            origin: Origin::User,
            title: None,
            trust_extracted_key: true,
            draft: false,
            approval: false,
            ask: false,
            note: None,
            cancel,
            embedder_failed: false,
        }
    }

    /// Imports `candidate`, replacing [`Self::replaces`] when that is set.
    pub fn import(&mut self, candidate: &Candidate) -> Outcome {
        self.import_replacing(candidate, self.replaces)
    }

    /// Imports `candidate` over the document `replaces` names (the id the store knows) instead
    /// of [`Self::replaces`], so each file of a batch can replace a document of its own.
    pub fn import_replacing(&mut self, candidate: &Candidate, replaces: Option<i64>) -> Outcome {
        let mut event = FileEvent::new(&candidate.display, Event::Failed);

        match self.import_inner(candidate, replaces, &mut event) {
            Ok(pending) => Outcome::Finished { event, pending },
            Err(Stop::Cancelled) => Outcome::Cancelled,
            Err(Stop::Skipped(reason)) => {
                event.status = Event::Skipped;
                event.reason = Some(reason);
                Outcome::Finished { event, pending: 0 }
            }
            Err(Stop::Failed(reason)) => {
                event.status = Event::Failed;
                event.reason = Some(reason);
                Outcome::Finished { event, pending: 0 }
            }
        }
    }

    fn import_inner(
        &mut self,
        candidate: &Candidate,
        replaces: Option<i64>,
        event: &mut FileEvent,
    ) -> Result<usize, Stop> {
        if self.ask && !(self.approval && self.author.is_some()) {
            return Err(Stop::Failed(
                "asking for approval needs team approval to be on and a git user.name".to_owned(),
            ));
        }

        let Prepared {
            format,
            size,
            file_sha,
            extracted,
            title,
            chunked,
            content_sha,
            source_key,
            bytes,
        } = prepare(
            candidate,
            self.pdf,
            self.counter,
            self.title.as_deref(),
            self.trust_extracted_key,
        )?;

        // The person's import sees drafts too, so a draft is matched and updated, not copied.
        if let Some(existing) = self.store.find_by_content_sha(&content_sha, Visibility::WithDrafts)? {
            if replaces.is_some_and(|id| id != existing.id) {
                return Err(Stop::Failed(format!(
                    "the same content is already document {}",
                    existing.id
                )));
            }

            self.merge_links(existing.id)?;
            event.status = Event::Unchanged;
            event.doc_id = Some(existing.id);
            self.ask_unchanged(existing.id, event);

            return Ok(0);
        }

        let previous = match replaces {
            Some(id) => Some(
                self.store
                    .get(id, &Self::with_drafts())?
                    .ok_or_else(|| Stop::Failed(format!("document {id} not found")))?
                    .doc,
            ),
            None => self.store.find_by_source_key(&source_key, Visibility::WithDrafts)?,
        };

        if !self.trust_extracted_key && replaces.is_none() && previous.is_some() {
            return Err(Stop::Failed("a document with this key exists already".to_owned()));
        }

        let (id, status, pending_of) = match self.plan(previous.as_ref(), &content_sha)? {
            Plan::Unchanged(id) => {
                event.status = Event::Unchanged;
                event.doc_id = Some(id);
                self.ask_unchanged(id, event);

                return Ok(0);
            }
            Plan::Store { id, status, pending_of } => (id, status, pending_of),
        };

        let previous_links = self.links_of(previous.as_ref())?;
        let now = now();
        let mut warnings = extracted.warnings.clone();

        if self.counter.name() == APPROX_COUNTER {
            warnings.push("token counts are estimated: the tokenizer is not installed yet".to_owned());
        }

        let doc = Document {
            id,
            title: title.clone(),
            source_name: candidate.name.clone(),
            source_key,
            format: format.as_str().to_owned(),
            content_sha256: content_sha,
            file_sha256: file_sha,
            bytes: size,
            chunk_count: 0,
            created_at: previous.as_ref().map_or(now, |doc| doc.created_at),
            updated_at: now,
            origin: match &previous {
                Some(doc) if replaces.is_none() && self.origin != Origin::AgentApproved => doc.origin,
                _ => self.origin,
            },
            warnings: warnings.clone(),
            status,
            pending_of,
            review_rev: 0,
            // A stored document keeps who first stored it.
            added_by: match &previous {
                Some(doc) => doc.added_by.clone(),
                None => self.author.clone(),
            },
            updated_by: self.author.clone(),
        };

        let mut chunks = to_chunks(&chunked);
        let pending = self.embed(&title, &chunked, &mut chunks)?;

        // A pending version has no links of its own: the live document's win.
        let links = match pending_of {
            Some(_) => Vec::new(),
            None => clean::links(&[previous_links.clone(), self.links.clone()].concat())?,
        };
        let stored = self.store.put_document(&doc, &chunks, &links)?;

        if let Some(model) = chunks.iter().find_map(|chunk| chunk.model_id.as_deref()) {
            embed::record_model(self.store, model)?;
        }

        if let Some(live) = pending_of {
            self.merge_links(live)?;
        }

        event.status = match (pending_of, previous.is_some()) {
            (Some(_), _) => Event::Pending,
            (None, true) => Event::Updated,
            (None, false) => Event::Added,
        };

        if previous.is_some() && replaces.is_none() && pending_of.is_none() && unrelated(&previous_links, &self.links) {
            event.reason = Some("replaced a document with the same name".to_owned());
        }

        event.doc_id = Some(stored);
        event.chunks = Some(chunks.len());
        event.embedding = Some(if pending == 0 { "done" } else { "pending" });
        event.draft = !status.is_live();
        event.warnings = warnings;

        self.keep_original(stored, candidate, &bytes, event);

        if self.ask {
            self.ask_for_approval(stored, event);
        }

        Ok(pending)
    }

    /// Where the text of a file goes, given the row `previous` it matched (by the document it
    /// replaces, else by its source key).
    fn plan(&mut self, previous: Option<&Document>, content_sha: &str) -> Result<Plan, Stop> {
        let draft = self.draft || self.approval;

        let Some(previous) = previous else {
            return Ok(Plan::Store {
                id: 0,
                status: if draft { Status::Draft } else { Status::Live },
                pending_of: None,
            });
        };

        // A pending version named directly is changed where it is.
        if let Some(live) = previous.pending_of {
            if previous.content_sha256 == content_sha {
                return Ok(Plan::Unchanged(previous.id));
            }

            return Ok(Plan::Store {
                id: previous.id,
                status: previous.status,
                pending_of: Some(live),
            });
        }

        // A row that is not live stays a draft, whatever the settings.
        if !previous.status.is_live() {
            return Ok(Plan::Store {
                id: previous.id,
                status: previous.status,
                pending_of: None,
            });
        }

        if !draft {
            return Ok(Plan::Store {
                id: previous.id,
                status: Status::Live,
                pending_of: None,
            });
        }

        // A live document that is replaced as a draft: its one pending version is written.
        let Some(version) = self.store.pending_of(previous.id)? else {
            return Ok(Plan::Store {
                id: 0,
                status: Status::Draft,
                pending_of: Some(previous.id),
            });
        };

        let same = self
            .store
            .get(version.id, &Self::with_drafts())?
            .is_some_and(|found| found.doc.content_sha256 == content_sha);

        if same {
            return Ok(Plan::Unchanged(version.id));
        }

        Ok(Plan::Store {
            id: version.id,
            status: version.status,
            pending_of: Some(previous.id),
        })
    }

    fn with_drafts() -> Detail {
        Detail {
            drafts: true,
            ..Detail::default()
        }
    }

    /// Keeps the file as it was read as the original of document `id`. A document is stored
    /// whether or not that works, so a failure is said in the event.
    fn keep_original(&mut self, id: i64, candidate: &Candidate, bytes: &[u8], event: &mut FileEvent) {
        if let Err(error) = self.store.put_file(id, &candidate.name, bytes) {
            event
                .warnings
                .push(format!("the original file could not be kept: {error}"));
        }
    }

    /// Asks for the approval of row `id`, which was just written, as its author, with the note of
    /// the import. The row is stored whether or not that works, so a failure is said in the event.
    fn ask_for_approval(&mut self, id: i64, event: &mut FileEvent) {
        let name = self.author.as_ref().map(|author| author.name.clone());
        let mut change = ReviewChange::new(ReviewAction::Ask, name.as_deref(), now());

        if let Some(note) = &self.note {
            change = change.with_comment(note);
        }

        let revision = match self.store.get(id, &Self::with_drafts()) {
            Ok(Some(found)) => found.doc.review_rev,
            Ok(None) => return,
            Err(error) => return Self::not_asked(event, &error.to_string()),
        };

        match self.store.set_review(id, revision, &change) {
            Ok(ReviewReply::Done(_)) => event.asked = true,
            Ok(other) => Self::not_asked(event, &format!("{other:?}")),
            Err(error) => Self::not_asked(event, &error.to_string()),
        }
    }

    /// With [`Importer::ask`], asks for the approval of row `id`, whose text this file has already:
    /// asking is what the person wants even when there is nothing new to write. Only a draft or a row
    /// with requested changes is asked: a waiting row is asked already and a live document has
    /// nothing to approve.
    fn ask_unchanged(&mut self, id: i64, event: &mut FileEvent) {
        if !self.ask {
            return;
        }

        match self.store.get(id, &Self::with_drafts()) {
            Ok(Some(found)) if matches!(found.doc.status, Status::Draft | Status::ChangesRequested) => {
                event.draft = true;
                self.ask_for_approval(id, event);
            }
            Ok(_) => {}
            Err(error) => Self::not_asked(event, &error.to_string()),
        }
    }

    fn not_asked(event: &mut FileEvent, why: &str) {
        let reason = format!("saved as a draft, but asking for approval failed: {why}");

        event.reason = Some(match event.reason.take() {
            Some(earlier) => format!("{earlier}. {reason}"),
            None => reason,
        });
    }

    /// The links the document has now: none for a new one.
    fn links_of(&mut self, previous: Option<&Document>) -> Result<Vec<Link>, Stop> {
        let Some(doc) = previous else {
            return Ok(Vec::new());
        };

        Ok(self
            .store
            .get(doc.id, &Self::with_drafts())?
            .map(|found| found.links)
            .unwrap_or_default())
    }

    /// An unchanged document gets the links of this import besides its own.
    fn merge_links(&mut self, id: i64) -> Result<(), Stop> {
        if self.links.is_empty() {
            return Ok(());
        }

        let Some(found) = self.store.get(id, &Self::with_drafts())? else {
            return Ok(());
        };

        let mut all = found.links.clone();
        all.extend(self.links.iter().cloned());
        let merged = clean::links(&all)?;

        if merged != found.links {
            self.store.set_links(id, &merged)?;
        }

        Ok(())
    }

    /// Gives the chunks their vectors when there is an embedder; returns how many have none.
    fn embed(&mut self, title: &str, chunked: &Chunked, chunks: &mut [Chunk]) -> Result<usize, Stop> {
        if self.embedder_failed {
            return Ok(chunks.len());
        }

        let Some(embedder) = self.embedder.as_deref_mut() else {
            return Ok(chunks.len());
        };

        let passages: Vec<String> = chunked
            .pieces
            .iter()
            .map(|piece| passage(title, &piece.heading_path, &piece.text))
            .collect();
        let filled = embed::fill(self.store, embedder, chunks, &passages, self.cancel)?;

        if filled.cancelled {
            return Err(Stop::Cancelled);
        }

        if filled.failed.is_some() {
            self.embedder_failed = true;
        }

        Ok(chunks.iter().filter(|chunk| chunk.embedding.is_none()).count())
    }
}

/// True when a document's own links and those of the import share no repository, and at least one
/// side is linked: a document of the same name from somewhere else, not a newer version.
fn unrelated(current: &[Link], incoming: &[Link]) -> bool {
    if current.is_empty() && incoming.is_empty() {
        return false;
    }

    !current
        .iter()
        .any(|link| incoming.iter().any(|other| other.repo_key == link.repo_key))
}

fn to_chunks(chunked: &Chunked) -> Vec<Chunk> {
    chunked
        .pieces
        .iter()
        .enumerate()
        .map(|(ordinal, piece)| Chunk {
            id: 0,
            doc_id: 0,
            ordinal,
            heading_path: piece.heading_path.clone(),
            text: piece.text.clone(),
            chunk_sha: piece.chunk_sha.clone(),
            tokens: piece.tokens,
            char_start: piece.char_start,
            char_end: piece.char_end,
            embedding: None,
            model_id: None,
        })
        .collect()
}

/// Unix seconds now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}
