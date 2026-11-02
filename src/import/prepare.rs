//! Reads one file the way an import does: bytes to extracted text, title, chunks, content hash and
//! source key. The import stores the result, `docs similar` only compares it.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use super::walk::{Candidate, Problem};
use crate::chunk::{self, Chunked};
use crate::digest::sha256_hex;
use crate::extract::{self, Extracted, Format, Refusal, pdf::PdfPages};
use crate::limits::{CHUNKS_PER_DOC_MAX, FILE_BYTES_MAX, PDF_BYTES_MAX, TEXT_BYTES_MAX, TITLE_MAX};
use crate::store::{StoreError, clean};
use crate::tokens::TokenCounter;

/// How many bytes of a file decide its format.
pub(super) const HEAD_BYTES: u64 = 8192;

/// Why a file was not stored.
pub(super) enum Stop {
    Skipped(String),
    Failed(String),
    Cancelled,
}

impl Stop {
    /// Why, as text: a cancel has no reason of its own.
    pub(super) fn reason(self) -> String {
        match self {
            Self::Skipped(reason) | Self::Failed(reason) => reason,
            Self::Cancelled => "cancelled".to_owned(),
        }
    }
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

/// A file read and extracted.
struct Loaded {
    format: Format,
    size: u64,
    file_sha: String,
    extracted: Extracted,
}

/// A file ready to be stored or compared.
pub(super) struct Prepared {
    pub(super) format: Format,
    pub(super) size: u64,
    pub(super) file_sha: String,
    pub(super) extracted: Extracted,
    pub(super) title: String,
    pub(super) chunked: Chunked,
    /// The hash of the title and the text: a renamed document is another content.
    pub(super) content_sha: String,
    pub(super) source_key: String,
}

/// Reads `candidate`: `title` replaces the file's own, and the key the text names for itself is
/// used only when `trust_extracted_key`.
pub(super) fn prepare(
    candidate: &Candidate,
    pdf: &dyn PdfPages,
    counter: &dyn TokenCounter,
    title: Option<&str>,
    trust_extracted_key: bool,
) -> Result<Prepared, Stop> {
    match &candidate.problem {
        Some(Problem::Skipped(reason)) => return Err(Stop::Skipped(reason.clone())),
        Some(Problem::Failed(reason)) => return Err(Stop::Failed(reason.clone())),
        None => {}
    }

    let loaded = load(candidate, pdf)?;
    let title = match title {
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

    let chunked = chunk::chunk_document(&title, &loaded.extracted.sections, counter);

    if chunked.pieces.is_empty() {
        return Err(Stop::Failed("the file has no text".to_owned()));
    }

    if chunked.pieces.len() > CHUNKS_PER_DOC_MAX {
        return Err(Stop::Failed(format!(
            "the document would have {} chunks, at most {CHUNKS_PER_DOC_MAX} are kept",
            chunked.pieces.len()
        )));
    }

    let content_sha = content_sha(&title, &chunked.text);
    let extracted_key = loaded.extracted.source_key.clone().filter(|_| trust_extracted_key);
    let source_key = extracted_key.unwrap_or_else(|| candidate.source_key.clone());

    Ok(Prepared {
        format: loaded.format,
        size: loaded.size,
        file_sha: loaded.file_sha,
        extracted: loaded.extracted,
        title,
        chunked,
        content_sha,
        source_key,
    })
}

/// The hash of a document's content. The title is part of it: a renamed document is updated and
/// embedded again.
pub(super) fn content_sha(title: &str, text: &str) -> String {
    sha256_hex(format!("{title}\n{text}").as_bytes())
}

/// Reads the file and extracts its text. The first bytes decide the format, and so which size
/// limit applies, before the rest is read.
fn load(candidate: &Candidate, pdf: &dyn PdfPages) -> Result<Loaded, Stop> {
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
        Format::Pdf => extract::pdf::extract(pdf, path)?,
        other => extract::extract(other, &candidate.name, &bytes)?,
    };

    Ok(Loaded {
        format,
        size: bytes.len() as u64,
        file_sha: sha256_hex(&bytes),
        extracted,
    })
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
