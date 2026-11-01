//! AsciiDoc: `=` headings, paragraphs, lists, listing and literal blocks (kept as fenced blocks) and
//! tables. Attribute entries (`:name: value`), block attribute lines (`[source,rust]`), anchors,
//! comments and `include::` lines are dropped; inline markup is left as written.

use super::{Extracted, Outline, heading_text};

pub fn extract(text: &str) -> Extracted {
    let mut outline = Outline::new(Vec::new());
    let mut paragraph: Vec<&str> = Vec::new();
    let mut lines = text.lines();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();

        if let Some((level, title)) = heading(trimmed) {
            flush(&mut outline, &mut paragraph);
            outline.heading(level, title);
        } else if trimmed.is_empty() || dropped(trimmed) {
            flush(&mut outline, &mut paragraph);
        } else if trimmed == "////" {
            flush(&mut outline, &mut paragraph);
            let _ = lines.by_ref().find(|next| next.trim() == "////");
        } else if is_delimiter(trimmed) {
            flush(&mut outline, &mut paragraph);
            let body: Vec<&str> = lines.by_ref().take_while(|next| next.trim() != trimmed).collect();

            outline.block(&block(trimmed, &body));
        } else {
            paragraph.push(trimmed);
        }
    }

    flush(&mut outline, &mut paragraph);

    let title = outline.first_title().map(str::to_owned);

    Extracted {
        title,
        sections: outline.finish(),
        ..Extracted::default()
    }
}

/// `== Title` is level 2, `= Title` (the document title) level 1.
fn heading(line: &str) -> Option<(usize, &str)> {
    let marks = line.chars().take_while(|c| *c == '=').count();
    let rest = line[marks..].strip_prefix(' ')?;

    ((1..=6).contains(&marks) && !rest.trim().is_empty()).then_some((marks, rest.trim()))
}

/// Lines that carry no text: attribute entries, block attributes, anchors, comments, includes.
fn dropped(line: &str) -> bool {
    let attribute_entry = line.starts_with(':') && line[1..].contains(':');
    let block_attribute = line.starts_with('[') && line.ends_with(']');

    attribute_entry || block_attribute || line.starts_with("//") || line.starts_with("include::")
}

/// A delimiter line of a listing (`----`), literal (`....`) or table (`|===`) block.
fn is_delimiter(line: &str) -> bool {
    let repeated = |mark: char| line.len() >= 4 && line.chars().all(|c| c == mark);

    repeated('-') || repeated('.') || line == "|==="
}

/// The block between two `delimiter` lines: code is fenced, a table keeps its rows.
fn block(delimiter: &str, body: &[&str]) -> String {
    let text = body.join("\n");
    let text = text.trim_matches('\n');

    if delimiter == "|===" {
        return text
            .lines()
            .map(|row| {
                heading_text(&row.replace('|', " | "))
                    .trim_matches(|c| c == '|' || c == ' ')
                    .to_owned()
            })
            .filter(|row| !row.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }

    format!("```\n{text}\n```")
}

fn flush(outline: &mut Outline, paragraph: &mut Vec<&str>) {
    if !paragraph.is_empty() {
        outline.block(&paragraph.join("\n"));
        paragraph.clear();
    }
}
