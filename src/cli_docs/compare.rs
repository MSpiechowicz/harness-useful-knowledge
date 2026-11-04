//! `docs compare --json ID [--since REV]`: what a pending version, a draft or a waiting row changes,
//! section by section, as the `FileDiff`s Harness Desktop's Changes view shows.
//!
//! Prints `{"live", "pending", "revision", "sections": [...], "truncated"}`. `revision` is the
//! revision of the row that is reviewed (`pending`), so a review can be sent with it as `--expect`.
//! `ID` is a pending version, or a live
//! row that has one (`live` is the live document, `pending` the version), or a new document that
//! has no live one (`live` is `null`, every line is added). A live row with no pending version is
//! refused as `wrongState`.
//!
//! A section is a `FileDiff`: `{"path": heading, "occurrence": n, "oldPath": null, "status":
//! "added"|"deleted"|"modified", "staged": false, "isBinary": false, "hunks", "additions",
//! "deletions", "truncated"}` (see [`super::diff`] for the hunks). `occurrence` counts from 1 the
//! sections that share the `path` in the version the section is of (the new one, the old one for a
//! deleted section), the ones left out included, so `(path, occurrence)` is what a line comment names
//! as its `section` and `occurrence`. The sections that did not change are left out. They follow the
//! pending order, a removed section at its live position, and a heading that comes back is paired
//! by occurrence. The text before the first heading is the section named like the document. The
//! texts compared are capped at 1 MiB in all: the sections past that come with no hunks and
//! `"truncated": true`, and so does the reply.
//!
//! `--since REV` compares with the version that was asked when the revision became REV, not with
//! the live document (`sinceUnavailable` when that version was not kept: it was too large, or too
//! many asks ago).

use serde_json::{Value, json};

use super::diff::{self, Diff};
use super::review::{failure, missing, open_to_read, refused};
use super::{Context, Done, Failure, Reply, the_id};
use crate::cli::Args;
use crate::review::{self, Code};
use crate::store::routed::ids;
use crate::store::{Detail, KnowledgeStore, SectionText};

/// The bytes of section text a compare compares.
const TEXT_BYTES_MAX: usize = 1024 * 1024;

/// `docs compare`.
pub(super) fn compare(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json"], &["--since"]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let id = the_id(&parsed)?;
    let since = match parsed.value("--since") {
        Some(raw) => Some(
            raw.parse::<i64>()
                .ok()
                .filter(|since| *since >= 0)
                .ok_or(Failure::Usage)?,
        ),
        None => None,
    };
    let mut store = open_to_read(context, id)?;

    compare_on(store.as_mut(), id, since)
}

/// The sides of a compare: the live document (if any), the row that is compared with it, its title and revision.
struct Sides {
    live: Option<i64>,
    subject: i64,
    title: String,
    revision: i64,
}

/// The sides of the compare of row `id`.
fn sides(store: &mut dyn KnowledgeStore, id: i64) -> Result<Sides, Failure> {
    let detail = Detail {
        drafts: true,
        ..Detail::default()
    };
    let found = store.get(id, &detail)?.ok_or_else(|| missing(id))?;

    if let Some(live) = found.doc.pending_of {
        return Ok(Sides {
            live: Some(live),
            subject: id,
            title: found.doc.title,
            revision: found.doc.review_rev,
        });
    }

    if !found.doc.status.is_live() {
        return Ok(Sides {
            live: None,
            subject: id,
            title: found.doc.title,
            revision: found.doc.review_rev,
        });
    }

    let pending = match found.pending {
        Some(pending) => pending.id,
        None => store.pending_of(id)?.map(|pending| pending.id).ok_or_else(|| {
            failure(review::Error::Refused(review::Refusal {
                code: Code::WrongState,
                message: format!(
                    "document {} is live and has no pending version to compare",
                    ids::show(id)
                ),
            }))
        })?,
    };
    let row = store.get(pending, &detail)?.ok_or_else(|| missing(pending))?.doc;

    Ok(Sides {
        live: Some(id),
        subject: pending,
        title: row.title,
        revision: row.review_rev,
    })
}

/// The compare of row `id` of `store`.
pub(super) fn compare_on(store: &mut dyn KnowledgeStore, id: i64, since: Option<i64>) -> Reply {
    let Sides {
        live,
        subject,
        title,
        revision,
    } = sides(store, id)?;

    let old = match (since, live) {
        (Some(revision), _) => store.asked_sections(subject, revision)?.ok_or_else(|| {
            refused(
                "sinceUnavailable",
                "the version that was asked at that revision was not kept",
            )
        })?,
        (None, Some(live)) => store.section_texts(live)?.ok_or_else(|| missing(live))?,
        (None, None) => Vec::new(),
    };
    let new = store.section_texts(subject)?.ok_or_else(|| missing(subject))?;
    let (old_nth, new_nth) = (occurrences(&old, &title), occurrences(&new, &title));

    let mut sections: Vec<Value> = Vec::new();
    let mut spent = 0;
    let mut truncated = false;

    for (old_at, new_at) in pairs(&old, &new) {
        let (old, new) = (old_at.map(|at| &old[at]), new_at.map(|at| &new[at]));
        let heading = old.or(new).map_or("", |section| section.heading.as_str());
        let path = if heading.is_empty() { title.as_str() } else { heading };

        let (before, after) = (
            old.map(|section| section.text.as_str()),
            new.map(|section| section.text.as_str()),
        );

        if before == after {
            continue;
        }

        spent += before.map_or(0, str::len) + after.map_or(0, str::len);
        let cut = spent > TEXT_BYTES_MAX;
        truncated |= cut;

        let occurrence = match (new_at, old_at) {
            (Some(at), _) => new_nth[at],
            (None, Some(at)) => old_nth[at],
            (None, None) => 1,
        };

        sections.push(section(path, occurrence, before, after, cut));
    }

    Ok(Done::ok(json!({
        "live": live.map(ids::json),
        "pending": ids::json(subject),
        "revision": revision,
        "sections": sections,
        "truncated": truncated,
    })))
}

/// For each section, which of the sections that share its path (the heading, the `title` for the
/// text before the first heading) it is, counted from 1.
pub(super) fn occurrences(sections: &[SectionText], title: &str) -> Vec<u32> {
    let mut seen: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();

    sections
        .iter()
        .map(|section| {
            let path = if section.heading.is_empty() {
                title
            } else {
                section.heading.as_str()
            };
            let count = seen.entry(path).or_insert(0);

            *count += 1;

            *count
        })
        .collect()
}

/// A section as a `FileDiff`, the `occurrence`-th of its `path`; `cut` leaves its hunks out.
fn section(path: &str, occurrence: u32, old: Option<&str>, new: Option<&str>, cut: bool) -> Value {
    let status = match (old, new) {
        (None, _) => "added",
        (_, None) => "deleted",
        _ => "modified",
    };
    let shown = if cut {
        Diff {
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        }
    } else {
        diff::diff(
            &diff::lines(old.unwrap_or_default()),
            &diff::lines(new.unwrap_or_default()),
        )
    };

    json!({
        "path": path,
        "occurrence": occurrence,
        "oldPath": null,
        "status": status,
        "staged": false,
        "isBinary": false,
        "hunks": shown.hunks,
        "truncated": cut,
        "additions": shown.additions,
        "deletions": shown.deletions,
    })
}

/// The sections of both versions as pairs of indexes, in the order of `new`: sections with the same
/// heading are paired by occurrence, and one that only `old` has comes where it was in `old`, after
/// the pair before it.
fn pairs(old: &[SectionText], new: &[SectionText]) -> Vec<(Option<usize>, Option<usize>)> {
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut paired: Vec<Option<usize>> = Vec::with_capacity(new.len());
    let mut used = vec![false; old.len()];

    for section in new {
        let occurrence = seen.entry(section.heading.as_str()).or_insert(0);
        let found = old
            .iter()
            .enumerate()
            .filter(|(_, other)| other.heading == section.heading)
            .nth(*occurrence)
            .map(|(at, _)| at);

        *occurrence += 1;

        if let Some(at) = found {
            used[at] = true;
        }

        paired.push(found);
    }

    let mut ordered = Vec::new();
    let mut next = 0;

    for (new_at, found) in paired.into_iter().enumerate() {
        let Some(old_at) = found else {
            ordered.push((None, Some(new_at)));
            continue;
        };

        while next < old_at {
            if !used[next] {
                ordered.push((Some(next), None));
            }

            next += 1;
        }

        ordered.push((Some(old_at), Some(new_at)));
        next = next.max(old_at + 1);
    }

    ordered.extend((next..old.len()).filter(|at| !used[*at]).map(|at| (Some(at), None)));

    ordered
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn sections(headings: &[&str]) -> Vec<SectionText> {
        headings
            .iter()
            .map(|heading| SectionText {
                heading: (*heading).to_owned(),
                text: String::new(),
            })
            .collect()
    }

    #[test]
    fn sections_pair_by_heading_and_occurrence() {
        let old = sections(&["A", "B", "C", "B"]);
        let new = sections(&["A", "C", "B", "B", "D"]);

        assert_eq!(
            pairs(&old, &new),
            [
                (Some(0), Some(0)),
                (Some(2), Some(1)),
                (Some(1), Some(2)),
                (Some(3), Some(3)),
                (None, Some(4)),
            ]
        );
    }

    #[test]
    fn a_removed_section_sits_at_its_live_position() {
        let old = sections(&["A", "X", "B", "Y"]);
        let new = sections(&["A", "B"]);

        assert_eq!(
            pairs(&old, &new),
            [(Some(0), Some(0)), (Some(1), None), (Some(2), Some(1)), (Some(3), None)]
        );
    }
}
