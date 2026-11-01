//! Cuts the sections of a document into chunks: the passages that are embedded and searched.
//!
//! What the embedder gets for a chunk is its [`passage`]: `<title> › <heading path>\n<text>`. It
//! adds the `passage: ` prefix itself, so the prefix is counted ([`count_passage`]) but never stored
//! or passed on: the passage and the prefix together stay within [`PASSAGE_TOKENS_MAX`] tokens, and
//! the chunk's hash is the hash of the passage, so the same text under the same title and headings
//! reuses an earlier embedding.
//!
//! - A section whose passage fits [`TARGET_TOKENS`] is one chunk. A longer one is cut along its
//!   blocks, lines, sentences and words (see [`atoms`]) and packed to the target, and each chunk
//!   starts with the last atoms of the one before, up to [`OVERLAP_PERCENT`] of the budget. Overlap
//!   exists only inside one section. A fenced code block is kept whole when it fits.
//! - A chunk never crosses a section boundary, except that consecutive sibling sections under
//!   [`TINY_TOKENS`] tokens each (same parent heading, same level) are merged into one chunk
//!   headed by the parent, their own headings written into its text.
//!
//! Offsets are in characters of the document's normalised text ([`Chunked::text`]): the sections
//! with their headings, separated by blank lines. That text is also what the document's content
//! hash is taken from.

pub mod atoms;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use crate::digest::sha256_hex;
use crate::extract::Section;
use crate::tokens::TokenCounter;
use atoms::{Atom, trimmed};

/// The tokens a chunk is packed to, passage included. Set after the embedding-throughput decision:
/// embedding costs about the square of the length, so shorter chunks are cheaper to embed.
pub const TARGET_TOKENS: usize = 350;

/// The most tokens a passage may have (the model reads 512, two of them are its start and end).
pub const PASSAGE_TOKENS_MAX: usize = 510;

/// A section under this many tokens may merge with its tiny siblings.
pub const TINY_TOKENS: usize = 80;

/// How much of a split section's budget the next chunk repeats.
pub const OVERLAP_PERCENT: usize = 15;

/// The least a chunk's text may be given of the budget, however long its title and headings.
const MIN_BODY_TOKENS: usize = 64;

/// Most characters of a heading path.
const HEADING_PATH_CHARS_MAX: usize = 240;

/// One chunk, before it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// The headings above it, `Guide > Install`, without the document's own title.
    pub heading_path: String,
    pub text: String,
    /// Where the text is in [`Chunked::text`], in characters.
    pub char_start: usize,
    pub char_end: usize,
    /// The tokens of its [`passage`] with the prefix ([`count_passage`]).
    pub tokens: usize,
    /// The SHA-256 of its [`passage`].
    pub chunk_sha: String,
}

/// A document's normalised text and its chunks, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunked {
    pub text: String,
    pub pieces: Vec<Piece>,
}

/// What the model reads before every passage; the embedder adds it, the chunker only counts it.
pub const PASSAGE_PREFIX: &str = "passage: ";

/// What the embedder is given for a chunk of the document `title` under `heading_path`.
pub fn passage(title: &str, heading_path: &str, text: &str) -> String {
    if heading_path.is_empty() {
        format!("{title}\n{text}")
    } else {
        format!("{title} › {heading_path}\n{text}")
    }
}

/// The tokens the model reads for `passage`: the prefix is part of them.
pub fn count_passage(counter: &dyn TokenCounter, passage: &str) -> usize {
    counter.count(&format!("{PASSAGE_PREFIX}{passage}"))
}

/// The chunks of a document called `title` with these `sections`, counted with `counter`.
pub fn chunk_document(title: &str, sections: &[Section], counter: &dyn TokenCounter) -> Chunked {
    let layout = Layout::new(sections, counter);
    let cutter = Cutter {
        title,
        counter,
        text: &layout.text,
    };
    let mut raw: Vec<Raw> = Vec::new();
    let mut at = 0;

    while at < layout.kept.len() {
        let end = layout.group_end(at, &cutter);

        if end - at >= 2 {
            let first = layout.kept[at];
            let parent = &first.heading_path[..first.heading_path.len() - 1];
            let range = (layout.spans[at].start, layout.spans[end - 1].end);

            cutter.cut(range, &display_path(parent, title), &mut raw);
        } else {
            let range = (layout.spans[at].body_start, layout.spans[at].end);

            cutter.cut(range, &display_path(&layout.kept[at].heading_path, title), &mut raw);
        }

        at = end;
    }

    let chars = char_offsets(&layout.text, &raw);
    let pieces = raw
        .into_iter()
        .map(|piece| Piece {
            char_start: chars[&piece.start],
            char_end: chars[&piece.end],
            text: layout.text[piece.start..piece.end].to_owned(),
            heading_path: piece.heading_path,
            tokens: piece.tokens,
            chunk_sha: piece.sha,
        })
        .collect();

    Chunked {
        text: layout.text,
        pieces,
    }
}

/// A chunk found, as byte offsets into the document text.
struct Raw {
    heading_path: String,
    start: usize,
    end: usize,
    tokens: usize,
    sha: String,
}

/// Where a section is in the document text, in bytes.
struct Span {
    /// The start of its heading line.
    start: usize,
    /// The start of its text, after the heading line.
    body_start: usize,
    end: usize,
    /// The tokens of the section as written in the text (heading line included).
    tokens: usize,
}

/// The sections that have text, laid out as the document's normalised text.
struct Layout<'a> {
    text: String,
    kept: Vec<&'a Section>,
    spans: Vec<Span>,
}

impl<'a> Layout<'a> {
    fn new(sections: &'a [Section], counter: &dyn TokenCounter) -> Self {
        let mut layout = Self {
            text: String::new(),
            kept: Vec::new(),
            spans: Vec::new(),
        };

        for section in sections {
            let body = section.text.trim();

            if body.is_empty() {
                continue;
            }

            if !layout.text.is_empty() {
                layout.text.push_str("\n\n");
            }

            let start = layout.text.len();

            if let (true, Some(heading)) = (section.level > 0, section.heading_path.last()) {
                layout.text.push_str(&"#".repeat(section.level.min(6)));
                layout.text.push(' ');
                layout.text.push_str(heading);
                layout.text.push('\n');
            }

            let body_start = layout.text.len();
            layout.text.push_str(body);

            let end = layout.text.len();
            let tokens = counter.count(&layout.text[start..end]);

            layout.kept.push(section);
            layout.spans.push(Span {
                start,
                body_start,
                end,
                tokens,
            });
        }

        layout
    }

    /// One past the last section of the group that starts at `at`: more than one section only for
    /// tiny siblings that fit one chunk together.
    fn group_end(&self, at: usize, cutter: &Cutter) -> usize {
        let first = self.kept[at];

        if first.level == 0 || first.heading_path.is_empty() || self.spans[at].tokens >= TINY_TOKENS {
            return at + 1;
        }

        let parent = &first.heading_path[..first.heading_path.len() - 1];
        let prefix = count_passage(
            cutter.counter,
            &passage(cutter.title, &display_path(parent, cutter.title), ""),
        );
        let budget = TARGET_TOKENS.saturating_sub(prefix).max(MIN_BODY_TOKENS);

        let mut sum = self.spans[at].tokens;
        let mut end = at + 1;

        while end < self.kept.len() {
            let next = self.kept[end];
            let sibling = next.level == first.level
                && next.heading_path.len() == first.heading_path.len()
                && next.heading_path[..next.heading_path.len() - 1] == *parent;
            let tokens = self.spans[end].tokens;

            if !sibling || tokens >= TINY_TOKENS || sum + tokens + 1 > budget {
                break;
            }

            sum += tokens + 1;
            end += 1;
        }

        end
    }
}

struct Cutter<'a> {
    title: &'a str,
    counter: &'a dyn TokenCounter,
    text: &'a str,
}

impl Cutter<'_> {
    fn passage_tokens(&self, path: &str, start: usize, end: usize) -> (usize, String) {
        let passage = passage(self.title, path, &self.text[start..end]);

        (count_passage(self.counter, &passage), passage)
    }

    /// Cuts the text between `range` (byte offsets) into chunks under `path`.
    fn cut(&self, range: (usize, usize), path: &str, out: &mut Vec<Raw>) {
        let (from, to) = trimmed(self.text, range.0, range.1);

        if from >= to {
            return;
        }

        let prefix = count_passage(self.counter, &passage(self.title, path, ""));
        let budget = TARGET_TOKENS.saturating_sub(prefix).max(MIN_BODY_TOKENS);
        let (whole, _) = self.passage_tokens(path, from, to);

        // The target, unless a long title and heading path leave the text less than a minimum.
        let limit = TARGET_TOKENS.max(prefix + MIN_BODY_TOKENS).min(PASSAGE_TOKENS_MAX);

        if whole <= limit {
            out.push(self.raw(path, from, to));
            return;
        }

        let atoms = atoms::split(&self.text[from..to], budget, self.counter);
        let overlap = budget * OVERLAP_PERCENT / 100;
        let mut first = 0;

        while first < atoms.len() {
            let mut last = self.fill(&atoms, first, budget);
            let piece = self.raw_of_atoms(path, from, &atoms, first, &mut last, limit);

            out.push(piece);

            if last >= atoms.len() {
                break;
            }

            first = next_start(&atoms, first, last, overlap, budget);
        }
    }

    /// One past the last atom that fits the budget when the chunk starts at atom `first`.
    fn fill(&self, atoms: &[Atom], first: usize, budget: usize) -> usize {
        let mut last = first + 1;
        let mut sum = atoms[first].tokens;

        while last < atoms.len() && sum + atoms[last].tokens <= budget {
            sum += atoms[last].tokens;
            last += 1;
        }

        last
    }

    /// The chunk of `atoms[first..*last]`, with `last` lowered when the passage is over `limit`
    /// (the sum of the atoms is only an estimate: it leaves out the space between them).
    fn raw_of_atoms(
        &self,
        path: &str,
        base: usize,
        atoms: &[Atom],
        first: usize,
        last: &mut usize,
        limit: usize,
    ) -> Raw {
        let start = base + atoms[first].start;

        loop {
            let end = base + atoms[*last - 1].end;
            let (tokens, _) = self.passage_tokens(path, start, end);

            if tokens <= limit || *last == first + 1 {
                return self.raw(path, start, end);
            }

            *last -= 1;
        }
    }

    /// A chunk of the text `start..end`, shortened by whole characters until it is within the limit.
    fn raw(&self, path: &str, start: usize, mut end: usize) -> Raw {
        loop {
            let (tokens, passage) = self.passage_tokens(path, start, end);

            if tokens <= PASSAGE_TOKENS_MAX || end <= start + 1 {
                return Raw {
                    heading_path: path.to_owned(),
                    start,
                    end,
                    tokens,
                    sha: sha256_hex(passage.as_bytes()),
                };
            }

            let length = self.text[start..end].chars().count();
            let keep = (length * 9 / 10).min(length - 1);

            end = self.text[start..end]
                .char_indices()
                .nth(keep)
                .map_or(end, |(offset, _)| start + offset);
        }
    }
}

/// The atom the next chunk starts at: the last atoms of this one that fit `overlap` tokens, unless
/// the next chunk could not then reach past this one.
fn next_start(atoms: &[Atom], first: usize, last: usize, overlap: usize, budget: usize) -> usize {
    let mut start = last;
    let mut repeated = 0;

    while start > first + 1 && repeated + atoms[start - 1].tokens <= overlap {
        repeated += atoms[start - 1].tokens;
        start -= 1;
    }

    if repeated + atoms[last].tokens > budget {
        return last;
    }

    start
}

/// The heading path as shown and embedded: `A > B`, without a first heading that repeats the
/// document's title, and at most [`HEADING_PATH_CHARS_MAX`] characters (the deepest headings kept).
pub fn display_path(path: &[String], title: &str) -> String {
    let skip = usize::from(
        path.first()
            .is_some_and(|first| first.trim().eq_ignore_ascii_case(title.trim())),
    );
    let joined = path[skip..].join(" > ");
    let length = joined.chars().count();

    if length <= HEADING_PATH_CHARS_MAX {
        return joined;
    }

    let tail: String = joined.chars().skip(length - HEADING_PATH_CHARS_MAX + 1).collect();

    format!("…{tail}")
}

/// The character offset of each byte offset a chunk starts or ends at.
fn char_offsets(text: &str, raw: &[Raw]) -> HashMap<usize, usize> {
    let mut wanted: Vec<usize> = raw.iter().flat_map(|piece| [piece.start, piece.end]).collect();
    wanted.sort_unstable();
    wanted.dedup();

    let mut found = HashMap::new();
    let mut next = 0;
    let mut total = 0;

    for (index, (byte, _)) in text.char_indices().enumerate() {
        while next < wanted.len() && wanted[next] == byte {
            found.insert(byte, index);
            next += 1;
        }

        total = index + 1;
    }

    // Offsets at the very end of the text.
    for byte in &wanted[next..] {
        found.insert(*byte, total);
    }

    found
}
