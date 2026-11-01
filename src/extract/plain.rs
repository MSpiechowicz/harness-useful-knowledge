//! Plain text: paragraphs, with no headings.

use super::{Extracted, Outline};

/// Paragraphs are separated by blank lines; a paragraph keeps its own line breaks.
pub fn extract(text: &str) -> Extracted {
    let mut outline = Outline::new(Vec::new());

    for paragraph in text.split("\n\n") {
        outline.block(paragraph);
    }

    Extracted {
        sections: outline.finish(),
        ..Extracted::default()
    }
}
