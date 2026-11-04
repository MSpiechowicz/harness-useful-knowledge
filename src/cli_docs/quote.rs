//! The lines a line comment was written on, and whether they are still there.
//!
//! - A review keeps, with each line comment, the lines `from..=to` of the side the comment names,
//!   as the compare of the reviewed row showed them (see [`super::compare`]): `{"line", "kind":
//!   "added"|"removed"|"same", "text"}` for each (at most [`crate::store::review::QUOTE_LINES_MAX`]
//!   lines, see [`crate::store::review::capped_quote`]). A comment whose lines the compare does not show
//!   (or a compare that cannot be made) keeps none, and the review goes on.
//! - A line comment is outdated when the row no longer has those lines as they were: its section is
//!   gone (the one of its heading and occurrence), or the lines of its side differ from the quote.
//!   A comment with no quote (written before quotes were kept) is outdated when its section is gone,
//!   or the content of the row changed since it was written. Overall comments and replies never are.
//! - Only the lines the quote kept are compared (the first 20 of a longer range, see
//!   [`crate::store::review::QUOTE_LINES_MAX`]), and a line cut at the byte cap (`cut`) by its start.

use serde_json::Value;

use super::compare::{compare_on, occurrences};
use super::review::missing;
use super::{Failure, diff};
use crate::store::review::{QUOTE_LINES_MAX, capped_quote};
use crate::store::{Comment, Detail, KnowledgeStore, NewComment, QuoteKind, QuoteLine, SectionText, Side, clean};

/// `comments` with the lines of each line comment, as the compare of row `row` shows them now.
pub(super) fn with_quotes(store: &mut dyn KnowledgeStore, row: i64, comments: &[NewComment]) -> Vec<NewComment> {
    if comments.iter().all(|comment| comment.section.is_none()) {
        return comments.to_vec();
    }

    let Ok(done) = compare_on(store, row, None) else {
        return comments.to_vec();
    };
    let sections = done.json["sections"].as_array().map_or(&[][..], Vec::as_slice);

    comments
        .iter()
        .map(|comment| match quoted(sections, comment) {
            Some(quote) => comment.clone().with_quote(quote),
            None => comment.clone(),
        })
        .collect()
}

/// The lines of `comment` in the `sections` of a compare, none when any of them is not shown.
fn quoted(sections: &[Value], comment: &NewComment) -> Option<Vec<QuoteLine>> {
    let (section, side) = (comment.section.as_deref()?, comment.side?);
    let (from, to) = (comment.line_from?, comment.line_to?);

    if from == 0 || to < from {
        return None;
    }

    let wanted = clean::line(section);
    let occurrence = u64::from(comment.occurrence.unwrap_or(1));

    let found = sections.iter().find(|shown| {
        shown["path"].as_str().map(clean::line).as_deref() == Some(wanted.as_str())
            && shown["occurrence"].as_u64() == Some(occurrence)
    })?;

    let last = to.min(from.saturating_add(u32::try_from(QUOTE_LINES_MAX).unwrap_or(u32::MAX) - 1));
    let mut shown: Vec<QuoteLine> = Vec::new();

    for hunk in found["hunks"].as_array()? {
        for line in hunk["lines"].as_array()? {
            let (kind, number) = match (line["kind"].as_str()?, side) {
                ("context", Side::Old) => (QuoteKind::Same, &line["oldNo"]),
                ("context", Side::New) => (QuoteKind::Same, &line["newNo"]),
                ("remove", Side::Old) => (QuoteKind::Removed, &line["oldNo"]),
                ("add", Side::New) => (QuoteKind::Added, &line["newNo"]),
                _ => continue,
            };
            let number = u32::try_from(number.as_u64()?).ok()?;

            if (from..=last).contains(&number) {
                shown.push(QuoteLine {
                    line: number,
                    kind,
                    text: line["text"].as_str()?.to_owned(),
                    cut: false,
                });
            }
        }
    }

    shown.sort_by_key(|line| line.line);

    // Every line of the range must have been shown, else it is not what the reviewer saw.
    let complete = shown.len() == usize::try_from(last - from + 1).ok()?
        && shown.iter().zip(from..).all(|(line, number)| line.line == number);

    if complete { capped_quote(&shown) } else { None }
}

/// Whether the line `now` is the quoted `line`: the same text, or for a cut line one that starts
/// with what was kept.
fn matches_quoted(now: &str, line: &QuoteLine) -> bool {
    if line.cut {
        now.starts_with(&line.text)
    } else {
        now == line.text
    }
}

/// What a row holds now: the sections its comments name.
pub(super) struct Current {
    title: String,
    /// The sections of the row itself, the ones of the `new` side.
    new: Vec<SectionText>,
    /// The sections of the live document the row is the pending version of, the `old` side.
    old: Option<Vec<SectionText>>,
}

/// Whether a comment is outdated by what it says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Never outdated: an overall comment or a reply.
    Never,
    /// Outdated or not, from its section and quote.
    Known(bool),
    /// Its section is there but it has no quote: the revision it was written at tells.
    ByRevision,
}

impl Current {
    /// The sections of row `row` and of the live document it replaces.
    pub(super) fn load(store: &mut dyn KnowledgeStore, row: i64) -> Result<Self, Failure> {
        let detail = Detail {
            drafts: true,
            ..Detail::default()
        };
        let found = store.get(row, &detail)?.ok_or_else(|| missing(row))?.doc;
        let new = store.section_texts(row)?.ok_or_else(|| missing(row))?;
        let old = match found.pending_of {
            Some(live) => store.section_texts(live)?,
            None => None,
        };

        Ok(Self {
            title: found.title,
            new,
            old,
        })
    }

    /// Whether `comment` is outdated, as far as the sections and its quote say.
    pub(super) fn verdict(&self, comment: &Comment) -> Verdict {
        let (Some(section), Some(side), Some(from)) = (&comment.section, comment.side, comment.line_from) else {
            return Verdict::Never;
        };
        let sections = match side {
            Side::New => Some(&self.new),
            Side::Old => self.old.as_ref(),
        };
        let wanted = clean::line(section);
        let occurrence = comment.occurrence.unwrap_or(1);

        let found = sections.and_then(|sections| {
            let nth = occurrences(sections, &self.title);

            sections.iter().zip(nth).find(|(shown, nth)| {
                let path = if shown.heading.is_empty() {
                    self.title.as_str()
                } else {
                    shown.heading.as_str()
                };

                clean::line(path) == wanted && *nth == occurrence
            })
        });

        let Some((section, _)) = found else {
            return Verdict::Known(true);
        };
        let Some(quote) = &comment.quote else {
            return Verdict::ByRevision;
        };

        let lines = diff::lines(&section.text);
        let first = usize::try_from(from).unwrap_or(usize::MAX).saturating_sub(1);
        let same = quote
            .iter()
            .enumerate()
            .all(|(at, line)| lines.get(first + at).is_some_and(|now| matches_quoted(now, line)));

        Verdict::Known(!same)
    }
}
