//! The line diff of one section, as the hunks of Harness Desktop's `FileDiff`: the lines are the
//! text split at its line breaks, and a hunk has three lines of context around its changes.
//!
//! A line is `{"kind": "context"|"add"|"remove", "oldNo", "newNo", "text"}` (`oldNo` is the line's
//! number in the old text for context and removed lines, `newNo` in the new text for context and
//! added ones, both counted from 1) and a hunk `{"header", "oldStart", "oldLines", "newStart",
//! "newLines", "lines"}` with the header `@@ -oldStart,oldLines +newStart,newLines @@`. The
//! comments of a review name their lines by these numbers.

use serde_json::{Value, json};

/// The unchanged lines kept around a change.
const CONTEXT: usize = 3;

/// The most cells the table of the longest common subsequence of the lines that changed may have. A
/// larger one (about two thousand lines on each side) is shown as all removed and all added.
const CELLS_MAX: usize = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Context,
    Add,
    Remove,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

/// One step of the way from the old lines to the new ones: `old` and `new` are the indexes (from 0)
/// of the line in each text, or where an added (removed) line would sit in the old (new) one.
#[derive(Debug, Clone, Copy)]
struct Edit {
    kind: Kind,
    old: usize,
    new: usize,
}

/// A diff: its hunks as JSON and how many lines it adds and removes.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Diff {
    pub hunks: Vec<Value>,
    pub additions: usize,
    pub deletions: usize,
}

/// The lines of `text`: split at the line breaks, none for an empty text, no empty last line.
pub(super) fn lines(text: &str) -> Vec<&str> {
    text.split_terminator('\n').collect()
}

/// The diff from the `old` lines to the `new` ones.
pub(super) fn diff(old: &[&str], new: &[&str]) -> Diff {
    let edits = script(old, new);
    let additions = edits.iter().filter(|edit| edit.kind == Kind::Add).count();
    let deletions = edits.iter().filter(|edit| edit.kind == Kind::Remove).count();

    Diff {
        hunks: hunks(&edits, old, new),
        additions,
        deletions,
    }
}

/// The edits from `old` to `new`: the common start and end are kept, the middle is the longest
/// common subsequence of its lines (removals before additions).
fn script(old: &[&str], new: &[&str]) -> Vec<Edit> {
    let start = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let limit = old.len().min(new.len()) - start;
    let end = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(limit)
        .take_while(|(a, b)| a == b)
        .count();

    let mut edits: Vec<Edit> = (0..start)
        .map(|at| Edit {
            kind: Kind::Context,
            old: at,
            new: at,
        })
        .collect();

    edits.extend(middle(old, new, start, old.len() - end, new.len() - end));

    edits.extend((0..end).map(|at| Edit {
        kind: Kind::Context,
        old: old.len() - end + at,
        new: new.len() - end + at,
    }));

    edits
}

/// The edits of `old[from..old_to]` against `new[from_new..new_to]` (the lines of the middle).
fn middle(old: &[&str], new: &[&str], from: usize, old_to: usize, new_to: usize) -> Vec<Edit> {
    let a = &old[from..old_to];
    let b = &new[from..new_to];
    let (n, m) = (a.len(), b.len());

    let removed = |at: usize, new_at: usize| Edit {
        kind: Kind::Remove,
        old: from + at,
        new: from + new_at,
    };
    let added = |old_at: usize, at: usize| Edit {
        kind: Kind::Add,
        old: from + old_at,
        new: from + at,
    };

    if n == 0 || m == 0 || (n + 1) * (m + 1) > CELLS_MAX {
        let mut edits: Vec<Edit> = (0..n).map(|at| removed(at, 0)).collect();
        edits.extend((0..m).map(|at| added(n, at)));

        return edits;
    }

    // lcs[i][j] is the length of the longest common subsequence of a[i..] and b[j..].
    let width = m + 1;
    let mut lcs = vec![0_u32; (n + 1) * width];

    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * width + j] = if a[i] == b[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }

    let (mut i, mut j) = (0, 0);
    let mut edits = Vec::new();

    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            edits.push(Edit {
                kind: Kind::Context,
                old: from + i,
                new: from + j,
            });
            i += 1;
            j += 1;
        } else if j == m || (i < n && lcs[(i + 1) * width + j] >= lcs[i * width + j + 1]) {
            edits.push(removed(i, j));
            i += 1;
        } else {
            edits.push(added(i, j));
            j += 1;
        }
    }

    edits
}

/// The hunks of `edits`: each change with [`CONTEXT`] lines around it, hunks that touch merged.
fn hunks(edits: &[Edit], old: &[&str], new: &[&str]) -> Vec<Value> {
    let mut spans: Vec<(usize, usize)> = Vec::new();

    for (at, edit) in edits.iter().enumerate() {
        if edit.kind == Kind::Context {
            continue;
        }

        let start = at.saturating_sub(CONTEXT);
        let end = (at + CONTEXT + 1).min(edits.len());

        match spans.last_mut() {
            Some(last) if last.1 >= start => last.1 = end,
            _ => spans.push((start, end)),
        }
    }

    spans
        .into_iter()
        .map(|(start, end)| hunk(&edits[start..end], old, new))
        .collect()
}

fn hunk(edits: &[Edit], old: &[&str], new: &[&str]) -> Value {
    let old_lines = edits.iter().filter(|edit| edit.kind != Kind::Add).count();
    let new_lines = edits.iter().filter(|edit| edit.kind != Kind::Remove).count();

    // A side with no line in the hunk starts at the line before it, as a diff of git writes it.
    let old_start = edits
        .iter()
        .find(|edit| edit.kind != Kind::Add)
        .map_or(edits[0].old, |edit| edit.old + 1);
    let new_start = edits
        .iter()
        .find(|edit| edit.kind != Kind::Remove)
        .map_or(edits[0].new, |edit| edit.new + 1);

    let lines: Vec<Value> = edits
        .iter()
        .map(|edit| match edit.kind {
            Kind::Context => line(edit.kind, Some(edit.old + 1), Some(edit.new + 1), old[edit.old]),
            Kind::Remove => line(edit.kind, Some(edit.old + 1), None, old[edit.old]),
            Kind::Add => line(edit.kind, None, Some(edit.new + 1), new[edit.new]),
        })
        .collect();

    json!({
        "header": format!("@@ -{old_start},{old_lines} +{new_start},{new_lines} @@"),
        "oldStart": old_start,
        "oldLines": old_lines,
        "newStart": new_start,
        "newLines": new_lines,
        "lines": lines,
    })
}

fn line(kind: Kind, old_no: Option<usize>, new_no: Option<usize>, text: &str) -> Value {
    json!({ "kind": kind.as_str(), "oldNo": old_no, "newNo": new_no, "text": text })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn shown(old: &str, new: &str) -> Vec<String> {
        let diff = diff(&lines(old), &lines(new));

        diff.hunks
            .iter()
            .flat_map(|hunk| {
                let header = hunk["header"].as_str().expect("header").to_owned();
                let body = hunk["lines"].as_array().expect("lines").iter().map(|line| {
                    let mark = match line["kind"].as_str().expect("kind") {
                        "add" => '+',
                        "remove" => '-',
                        _ => ' ',
                    };

                    format!(
                        "{mark}{} {}:{}",
                        line["text"].as_str().expect("text"),
                        line["oldNo"],
                        line["newNo"]
                    )
                });

                std::iter::once(header).chain(body)
            })
            .collect()
    }

    #[test]
    fn lines_are_split_at_line_breaks() {
        assert_eq!(lines(""), Vec::<&str>::new());
        assert_eq!(lines("a"), ["a"]);
        assert_eq!(lines("a\nb\n"), ["a", "b"]);
        assert_eq!(lines("a\n\nb"), ["a", "", "b"]);
    }

    #[test]
    fn nothing_changed_is_no_hunk() {
        let diff = diff(&lines("a\nb"), &lines("a\nb"));

        assert_eq!((diff.hunks.len(), diff.additions, diff.deletions), (0, 0, 0));
    }

    #[test]
    fn a_new_text_is_all_added_from_line_one() {
        assert_eq!(
            shown("", "a\nb"),
            ["@@ -0,0 +1,2 @@", "+a null:1", "+b null:2"],
            "a pure addition starts at the line before"
        );
    }

    #[test]
    fn a_removed_text_is_all_removed() {
        assert_eq!(shown("a\nb", ""), ["@@ -1,2 +0,0 @@", "-a 1:null", "-b 2:null"]);
    }

    #[test]
    fn a_changed_line_is_removed_then_added_between_its_context() {
        assert_eq!(
            shown("one\ntwo\nthree\nfour", "one\n2\nthree\nfour"),
            [
                "@@ -1,4 +1,4 @@",
                " one 1:1",
                "-two 2:null",
                "+2 null:2",
                " three 3:3",
                " four 4:4",
            ]
        );
    }

    #[test]
    fn context_is_three_lines_and_far_changes_are_hunks_of_their_own() {
        let old: String = (1..=20).map(|n| format!("l{n}\n")).collect();
        let new = old.replace("l2\n", "L2\n").replace("l19\n", "L19\n");
        let diff = diff(&lines(&old), &lines(&new));

        assert_eq!(diff.hunks.len(), 2);
        assert_eq!(diff.hunks[0]["header"], "@@ -1,5 +1,5 @@");
        assert_eq!(diff.hunks[1]["header"], "@@ -16,5 +16,5 @@");
        assert_eq!((diff.additions, diff.deletions), (2, 2));
    }

    #[test]
    fn numbers_count_the_lines_of_each_side() {
        assert_eq!(
            shown("a\nb\nc", "a\nx\ny\nc"),
            [
                "@@ -1,3 +1,4 @@",
                " a 1:1",
                "-b 2:null",
                "+x null:2",
                "+y null:3",
                " c 3:4",
            ]
        );
    }

    #[test]
    fn a_text_too_large_to_align_is_all_removed_and_all_added() {
        let old: Vec<String> = (0..2100).map(|n| format!("old {n}")).collect();
        let new: Vec<String> = (0..2100).map(|n| format!("new {n}")).collect();
        let (old, new): (Vec<&str>, Vec<&str>) = (
            old.iter().map(String::as_str).collect(),
            new.iter().map(String::as_str).collect(),
        );
        let diff = diff(&old, &new);

        assert_eq!((diff.additions, diff.deletions), (2100, 2100));
    }
}
