//! Test-only text and PDF files.

pub use pdf_file::pdf;

#[path = "../../tests/support/pdf.rs"]
mod pdf_file;

/// A long paragraph of distinct words, `words` of them.
pub fn prose(words: usize) -> String {
    (0..words).map(|n| format!("word{n}")).collect::<Vec<_>>().join(" ")
}
