//! Imports one file: reads it, extracts and chunks its text, decides whether it is new, changed
//! or already there, embeds what is new and stores the document in one transaction.
//!
//! The decision is by content: the same text hash is `unchanged` (the links of this import are
//! merged in), else the same source key is `updated` in place (its chunks replaced, the vectors of
//! chunks whose hash did not change reused), else the document is `added`.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::embed::{self, Embedder};
use super::events::{FileEvent, Status};
use super::walk::{Candidate, Problem};
use crate::chunk::{self, Chunked, passage};
use crate::digest::sha256_hex;
use crate::extract::{self, Extracted, Format, Refusal, pdf::PdfPages};
use crate::limits::{CHUNKS_PER_DOC_MAX, FILE_BYTES_MAX, PDF_BYTES_MAX, TEXT_BYTES_MAX, TITLE_MAX};
use crate::store::{Chunk, Detail, Document, KnowledgeStore, Link, Origin, StoreError, clean};
use crate::tokens::{APPROX_COUNTER, TokenCounter};

/// How many bytes of a file decide its format.
const HEAD_BYTES: u64 = 8192;

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

/// Why a file was not stored.
enum Stop {
    Skipped(String),
    Failed(String),
    Cancelled,
}

impl From<Refusal> for Stop {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Unsupported(reason) | Refusal::Skipped(reason) => Self::Skipped(reason),
            Refusal::Failed(reason) => Self::Failed(reason),
        }
    }
}

impl From<StoreError> for Stop {
    fn from(error: StoreError) -> Self {
        Self::Failed(error.to_string())
    }
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
    /// The document the one file of this import replaces.
    pub replaces: Option<i64>,
    /// Who the documents of this import come from: [`Origin::User`], or an agent's proposal the
    /// person approved. A document an import updates keeps its origin, unless it is `replaces`.
    pub origin: Origin,
    /// The title every document of this import gets, instead of the one its file has.
    pub title: Option<String>,
    /// False for an import of untrusted text (an agent's approved proposal): the key its text names
    /// for itself (a Confluence page id) is ignored, and a key that is taken is refused unless the
    /// import `replaces` that document.
    pub trust_extracted_key: bool,
    pub cancel: &'a dyn Fn() -> bool,
    /// Set when the embedder failed: later files do not try it again.
    embedder_failed: bool,
}

/// A file read and extracted.
struct Loaded {
    format: Format,
    size: u64,
    file_sha: String,
    extracted: Extracted,
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
            origin: Origin::User,
            title: None,
            trust_extracted_key: true,
            cancel,
            embedder_failed: false,
        }
    }

    /// Imports `candidate`.
    pub fn import(&mut self, candidate: &Candidate) -> Outcome {
        let mut event = FileEvent::new(&candidate.display, Status::Failed);

        match self.import_inner(candidate, &mut event) {
            Ok(pending) => Outcome::Finished { event, pending },
            Err(Stop::Cancelled) => Outcome::Cancelled,
            Err(Stop::Skipped(reason)) => {
                event.status = Status::Skipped;
                event.reason = Some(reason);
                Outcome::Finished { event, pending: 0 }
            }
            Err(Stop::Failed(reason)) => {
                event.status = Status::Failed;
                event.reason = Some(reason);
                Outcome::Finished { event, pending: 0 }
            }
        }
    }

    fn import_inner(&mut self, candidate: &Candidate, event: &mut FileEvent) -> Result<usize, Stop> {
        match &candidate.problem {
            Some(Problem::Skipped(reason)) => return Err(Stop::Skipped(reason.clone())),
            Some(Problem::Failed(reason)) => return Err(Stop::Failed(reason.clone())),
            None => {}
        }

        let loaded = self.load(candidate)?;
        let title = match &self.title {
            Some(title) => clean::title(title)?,
            None => title_of(&loaded.extracted, &candidate.name),
        };
        let text_bytes: usize = loaded.extracted.sections.iter().map(|section| section.text.len()).sum();

        if text_bytes > TEXT_BYTES_MAX {
            return Err(Stop::Failed(format!(
                "the text is over {} MiB",
                TEXT_BYTES_MAX / (1024 * 1024)
            )));
        }

        let chunked = chunk::chunk_document(&title, &loaded.extracted.sections, self.counter);

        if chunked.pieces.is_empty() {
            return Err(Stop::Failed("the file has no text".to_owned()));
        }

        if chunked.pieces.len() > CHUNKS_PER_DOC_MAX {
            return Err(Stop::Failed(format!(
                "the document would have {} chunks, at most {CHUNKS_PER_DOC_MAX} are kept",
                chunked.pieces.len()
            )));
        }

        // The title is part of the content: a renamed document is updated and embedded again.
        let content_sha = sha256_hex(format!("{title}\n{}", chunked.text).as_bytes());
        let extracted_key = loaded.extracted.source_key.clone().filter(|_| self.trust_extracted_key);
        let source_key = extracted_key.unwrap_or_else(|| candidate.source_key.clone());

        if let Some(existing) = self.store.find_by_content_sha(&content_sha)? {
            if self.replaces.is_some_and(|id| id != existing.id) {
                return Err(Stop::Failed(format!(
                    "the same content is already document {}",
                    existing.id
                )));
            }

            self.merge_links(existing.id)?;
            event.status = Status::Unchanged;
            event.doc_id = Some(existing.id);

            return Ok(0);
        }

        let previous = match self.replaces {
            Some(id) => Some(
                self.store
                    .get(id, &Detail::default())?
                    .ok_or_else(|| Stop::Failed(format!("document {id} not found")))?
                    .doc,
            ),
            None => self.store.find_by_source_key(&source_key)?,
        };

        if !self.trust_extracted_key && self.replaces.is_none() && previous.is_some() {
            return Err(Stop::Failed("a document with this key exists already".to_owned()));
        }

        let previous_links = self.links_of(previous.as_ref())?;
        let now = now();
        let mut warnings = loaded.extracted.warnings.clone();

        if self.counter.name() == APPROX_COUNTER {
            warnings.push("token counts are estimated: the tokenizer is not installed yet".to_owned());
        }

        let doc = Document {
            id: previous.as_ref().map_or(0, |doc| doc.id),
            title: title.clone(),
            source_name: candidate.name.clone(),
            source_key,
            format: loaded.format.as_str().to_owned(),
            content_sha256: content_sha,
            file_sha256: loaded.file_sha,
            bytes: loaded.size,
            chunk_count: 0,
            created_at: previous.as_ref().map_or(now, |doc| doc.created_at),
            updated_at: now,
            origin: match &previous {
                Some(doc) if self.replaces.is_none() && self.origin != Origin::AgentApproved => doc.origin,
                _ => self.origin,
            },
            warnings: warnings.clone(),
        };

        let mut chunks = to_chunks(&chunked);
        let pending = self.embed(&title, &chunked, &mut chunks)?;
        let links = clean::links(&[previous_links.clone(), self.links.clone()].concat())?;
        let id = self.store.put_document(&doc, &chunks, &links)?;

        if let Some(model) = chunks.iter().find_map(|chunk| chunk.model_id.as_deref()) {
            embed::record_model(self.store, model)?;
        }

        event.status = if previous.is_some() {
            Status::Updated
        } else {
            Status::Added
        };

        if previous.is_some() && self.replaces.is_none() && unrelated(&previous_links, &self.links) {
            event.reason = Some("replaced a document with the same name".to_owned());
        }

        event.doc_id = Some(id);
        event.chunks = Some(chunks.len());
        event.embedding = Some(if pending == 0 { "done" } else { "pending" });
        event.warnings = warnings;

        Ok(pending)
    }

    /// Reads the file and extracts its text. The first bytes decide the format, and so which size
    /// limit applies, before the rest is read.
    fn load(&mut self, candidate: &Candidate) -> Result<Loaded, Stop> {
        let path = &candidate.path;
        let unreadable = |_| Stop::Failed("the file cannot be read".to_owned());
        let mut file = File::open(path).map_err(unreadable)?;
        let size = file.metadata().map_err(unreadable)?.len();

        let mut bytes = Vec::new();
        (&mut file)
            .take(HEAD_BYTES)
            .read_to_end(&mut bytes)
            .map_err(unreadable)?;

        let format = extract::detect(&candidate.name, &bytes)
            .ok_or_else(|| Stop::Skipped("unsupported format: the file holds binary data".to_owned()))?;
        let limit = if format == Format::Pdf {
            PDF_BYTES_MAX
        } else {
            FILE_BYTES_MAX
        };

        if size > limit {
            return Err(Stop::Skipped(format!("larger than {} MiB", limit / (1024 * 1024))));
        }

        // The limit holds even when the file grows while it is read.
        file.take(limit + 1 - bytes.len() as u64)
            .read_to_end(&mut bytes)
            .map_err(unreadable)?;

        if bytes.len() as u64 > limit {
            return Err(Stop::Skipped(format!("larger than {} MiB", limit / (1024 * 1024))));
        }

        let extracted = match format {
            Format::Pdf => extract::pdf::extract(self.pdf, path)?,
            other => extract::extract(other, &candidate.name, &bytes)?,
        };

        Ok(Loaded {
            format,
            size: bytes.len() as u64,
            file_sha: sha256_hex(&bytes),
            extracted,
        })
    }

    /// The links the document has now: none for a new one.
    fn links_of(&mut self, previous: Option<&Document>) -> Result<Vec<Link>, Stop> {
        let Some(doc) = previous else {
            return Ok(Vec::new());
        };

        Ok(self
            .store
            .get(doc.id, &Detail::default())?
            .map(|found| found.links)
            .unwrap_or_default())
    }

    /// An unchanged document gets the links of this import besides its own.
    fn merge_links(&mut self, id: i64) -> Result<(), Stop> {
        if self.links.is_empty() {
            return Ok(());
        }

        let Some(found) = self.store.get(id, &Detail::default())? else {
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

/// The document's title: its own, else the file name without its extension.
fn title_of(extracted: &Extracted, name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned());

    [extracted.title.clone(), stem]
        .into_iter()
        .flatten()
        .map(|title| clean::line(&title).chars().take(TITLE_MAX).collect::<String>())
        .map(|title| title.trim().to_owned())
        .find(|title| !title.is_empty())
        .unwrap_or_else(|| "Untitled".to_owned())
}

/// Unix seconds now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}
