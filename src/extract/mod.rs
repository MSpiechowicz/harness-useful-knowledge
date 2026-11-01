//! Turns a file into the sections of its text.
//!
//! A [`Section`] is the text under one heading, with the headings above it. Every extractor
//! produces them in document order, so the chunker can cut along the author's own structure:
//!
//! - [`markdown`], [`html`] (with the Confluence export), [`asciidoc`], [`rst`] and [`plain`]
//!   read text formats. Code blocks become fenced blocks, lists one line per item and tables one
//!   line per row.
//! - [`subtitles`] reads WebVTT and SubRip transcripts, one section per two to three minutes of talk
//!   headed by its time span.
//! - [`openapi`] reads an OpenAPI or Swagger description (JSON or YAML) into one section per
//!   operation and per schema. Any other JSON or YAML is refused: it is data, not documentation.
//! - [`pdf`] reads the pages a worker process extracted ([`worker`] runs in a child, so a crash or a
//!   hang of the PDF reader costs one file and not the import). A PDF without a text layer is
//!   refused.
//!
//! [`detect`] picks the format from the extension and the first bytes, [`extract`] runs it.
//! Nothing here touches the database.

pub mod asciidoc;
pub mod html;
pub mod markdown;
pub mod openapi;
pub mod outline;
pub mod pdf;
pub mod plain;
pub mod rst;
pub mod subtitles;
pub mod worker;
mod yaml;

use std::path::Path;

pub use outline::Outline;

/// The text under one heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The headings above it and its own, outermost first. Empty for text before the first heading.
    pub heading_path: Vec<String>,
    /// The level of its own heading (1 is the outermost), 0 when it has none.
    pub level: usize,
    /// Its text, blocks separated by a blank line.
    pub text: String,
}

/// What an extractor read from a file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Extracted {
    /// The document's own title, when the file has one.
    pub title: Option<String>,
    pub sections: Vec<Section>,
    /// What the extraction noticed (a page without text).
    pub warnings: Vec<String>,
    /// What identifies the document across imports when it is not the path (a Confluence page id).
    pub source_key: Option<String>,
}

/// The formats an import reads. A document stores [`Format::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Text,
    Html,
    OpenApi,
    AsciiDoc,
    Rst,
    Pdf,
    /// WebVTT or SubRip subtitles.
    Transcript,
}

impl Format {
    /// The name stored and shown.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Text => "text",
            Self::Html => "html",
            Self::OpenApi => "openapi",
            Self::AsciiDoc => "asciidoc",
            Self::Rst => "rst",
            Self::Pdf => "pdf",
            Self::Transcript => "transcript",
        }
    }
}

/// Why a file gave no document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The format is not one an import reads, or the file is not documentation (a JSON file that is
    /// no OpenAPI description). Reported as skipped.
    Unsupported(String),
    /// A page that is not content (a Confluence index). Reported as skipped.
    Skipped(String),
    /// The file is broken or has nothing to read (a malformed or scanned PDF). Reported as failed.
    Failed(String),
}

impl Refusal {
    pub fn failed(reason: impl Into<String>) -> Self {
        Self::Failed(reason.into())
    }

    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported(reason.into())
    }
}

pub type Result<T> = std::result::Result<T, Refusal>;

/// Most characters of a heading kept in a heading path.
pub const HEADING_CHARS_MAX: usize = 200;

/// The format a file name suggests: `None` for an extension an import does not read.
pub fn format_of_name(name: &str) -> Option<Format> {
    let extension = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();

    match extension.as_str() {
        "md" | "markdown" | "mdown" | "mkd" => Some(Format::Markdown),
        "txt" | "text" => Some(Format::Text),
        "html" | "htm" | "xhtml" => Some(Format::Html),
        "json" | "yaml" | "yml" => Some(Format::OpenApi),
        "adoc" | "asciidoc" | "asc" => Some(Format::AsciiDoc),
        "rst" => Some(Format::Rst),
        "pdf" => Some(Format::Pdf),
        "vtt" | "srt" => Some(Format::Transcript),
        _ => None,
    }
}

/// The format of a file named `name` that starts with `head`. The PDF signature wins over the
/// extension, then the extension decides (a `.txt` that holds subtitles is a transcript), and a file
/// without a known extension is sniffed (HTML, an OpenAPI description, subtitles, else text unless it
/// holds binary data).
pub fn detect(name: &str, head: &[u8]) -> Option<Format> {
    if head.starts_with(b"%PDF-") {
        return Some(Format::Pdf);
    }

    match format_of_name(name) {
        // A transcript saved as `.txt` is still one.
        Some(Format::Text) if sniff(head) == Some(Format::Transcript) => Some(Format::Transcript),
        Some(format) => Some(format),
        None => sniff(head),
    }
}

fn sniff(head: &[u8]) -> Option<Format> {
    if head.contains(&0) && !starts_with_utf16_mark(head) {
        return None;
    }

    let text = String::from_utf8_lossy(head);
    let start = text.trim_start_matches('\u{feff}').trim_start().to_ascii_lowercase();

    if start.starts_with("<!doctype html") || start.starts_with("<html") {
        return Some(Format::Html);
    }

    if openapi::looks_like_description(&start) {
        return Some(Format::OpenApi);
    }

    if subtitles::looks_like(&start) {
        return Some(Format::Transcript);
    }

    Some(Format::Text)
}

fn starts_with_utf16_mark(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF])
}

/// Extracts the text formats from the bytes of a file called `name`.
/// [`Format::Pdf`] goes through [`pdf::extract`] (it needs the file's path).
pub fn extract(format: Format, name: &str, bytes: &[u8]) -> Result<Extracted> {
    if format == Format::Pdf {
        return Err(Refusal::failed("a PDF is read from its path"));
    }

    let (text, warning) = decode(bytes)?;
    let mut extracted = match format {
        Format::Markdown => markdown::extract(&text),
        Format::Text => plain::extract(&text),
        Format::Html => html::extract(&text, name)?,
        Format::OpenApi => openapi::extract(&text)?,
        Format::AsciiDoc => asciidoc::extract(&text),
        Format::Rst => rst::extract(&text),
        // A `.txt` that only looked like a transcript (it starts with "WebVTT …") is read as text.
        Format::Transcript => match subtitles::extract(&text) {
            Err(_) if format_of_name(name) == Some(Format::Text) => plain::extract(&text),
            other => other?,
        },
        Format::Pdf => unreachable!("handled above"),
    };

    extracted.warnings.extend(warning);

    Ok(extracted)
}

/// The text of `bytes`: UTF-8 (a byte-order mark dropped, invalid bytes replaced, which is said in
/// the warning) or UTF-16 with a byte-order mark; line endings made `\n` and control characters
/// dropped.
pub fn decode(bytes: &[u8]) -> Result<(String, Option<String>)> {
    let mut warning = None;
    let raw = if starts_with_utf16_mark(bytes) {
        decode_utf16(bytes)?
    } else {
        match std::str::from_utf8(bytes) {
            Ok(text) => text.to_owned(),
            Err(_) => {
                warning = Some("the file is not valid UTF-8, some characters were replaced".to_owned());
                String::from_utf8_lossy(bytes).into_owned()
            }
        }
    };

    Ok((clean_text(&raw), warning))
}

fn decode_utf16(bytes: &[u8]) -> Result<String> {
    let little = bytes.starts_with(&[0xFF, 0xFE]);
    let (pairs, _) = bytes[2..].as_chunks::<2>();
    let units: Vec<u16> = pairs
        .iter()
        .map(|pair| {
            if little {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect();

    String::from_utf16(&units).map_err(|_| Refusal::failed("the file is not valid UTF-16"))
}

/// `text` with `\n` line endings, no byte-order mark, no control characters except tab and newline,
/// no trailing spaces and at most one blank line in a row.
pub fn clean_text(text: &str) -> String {
    let unified = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(unified.len());
    let mut blank_run = 0;

    for line in unified.trim_start_matches('\u{feff}').split('\n') {
        let kept: String = line
            .chars()
            .filter(|character| *character == '\t' || !character.is_control())
            .collect();
        let kept = kept.trim_end();

        if kept.is_empty() {
            blank_run += 1;

            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }

        out.push_str(kept);
        out.push('\n');
    }

    out.trim().to_owned()
}

/// A heading as a path element: one line, at most [`HEADING_CHARS_MAX`] characters.
pub fn heading_text(raw: &str) -> String {
    let line: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");

    line.chars().take(HEADING_CHARS_MAX).collect()
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
