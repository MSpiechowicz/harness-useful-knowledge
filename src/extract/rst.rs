//! reStructuredText: titles (an underline, or an overline and an underline), paragraphs, literal
//! blocks and code directives (kept as fenced blocks). The level of a title is the order its
//! adornment style first appeared in. Comments, `toctree` and images are dropped, other
//! directives keep their content.

use super::{Extracted, Outline};

/// What an adornment line is made of.
const ADORNMENTS: &str = "=-`:.'\"~^_*+#<>";

pub fn extract(text: &str) -> Extracted {
    let lines: Vec<&str> = text.lines().collect();
    let mut outline = Outline::new(Vec::new());
    let mut styles: Vec<(char, bool)> = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut at = 0;

    while at < lines.len() {
        let line = lines[at];

        if let Some((title, style, used)) = title_at(&lines, at) {
            flush(&mut outline, &mut paragraph);

            let level = match styles.iter().position(|seen| *seen == style) {
                Some(position) => position + 1,
                None => {
                    styles.push(style);
                    styles.len()
                }
            };

            outline.heading(level, title);
            at += used;
        } else if line.trim().is_empty() {
            flush(&mut outline, &mut paragraph);
            at += 1;
        } else if let Some(directive) = line.trim().strip_prefix(".. ") {
            flush(&mut outline, &mut paragraph);
            at = directive_block(&lines, at, directive, &mut outline);
        } else if line.trim_end().ends_with("::") && !line.trim_start().starts_with(".. ") {
            let lead = line.trim_end().trim_end_matches(':').trim_end();
            let (body, next) = indented(&lines, at + 1);

            if !lead.is_empty() {
                paragraph.push(lead);
            }

            flush(&mut outline, &mut paragraph);
            outline.block(&fenced(&body));
            at = next;
        } else {
            paragraph.push(line.trim());
            at += 1;
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

/// The title that starts at `at`: its text, its style (the character and whether it has an
/// overline) and how many lines it takes.
fn title_at<'a>(lines: &[&'a str], at: usize) -> Option<(&'a str, (char, bool), usize)> {
    let first = lines[at].trim_end();
    let second = lines.get(at + 1).map(|line| line.trim_end());

    if let (Some(mark), Some(text), Some(under)) = (adornment(first), second, lines.get(at + 2)) {
        let text = text.trim();

        if !text.is_empty() && adornment(under.trim_end()) == Some(mark) && first.len() >= text.chars().count() {
            return Some((text, (mark, true), 3));
        }
    }

    let (text, under) = (first, second?);
    let mark = adornment(under)?;

    let fits = !text.trim().is_empty()
        && adornment(text).is_none()
        && !text.starts_with(' ')
        && under.chars().count() >= text.trim().chars().count()
        && under.chars().count() >= 2;

    fits.then_some((text.trim(), (mark, false), 2))
}

/// The character a line is made of, when the whole line is one adornment character.
fn adornment(line: &str) -> Option<char> {
    let mark = line.chars().next()?;

    (ADORNMENTS.contains(mark) && line.len() >= 2 && line.chars().all(|c| c == mark)).then_some(mark)
}

/// The directive block that starts at `at` (its first line is `directive` without the `.. `): code
/// is fenced, content directives keep their body, the rest is dropped. Returns the next line.
fn directive_block(lines: &[&str], at: usize, directive: &str, outline: &mut Outline) -> usize {
    let (body, next) = indented(lines, at + 1);

    let Some((name, argument)) = directive.split_once("::") else {
        // A comment.
        return next;
    };

    match name.trim() {
        "code-block" | "code" | "sourcecode" => {
            let body: Vec<&str> = body
                .iter()
                .copied()
                .skip_while(|line| line.trim_start().starts_with(':'))
                .collect();
            outline.block(&fenced(&body));
        }
        "toctree" | "image" | "figure" | "include" | "literalinclude" | "raw" | "only" | "index" | "contents" => {}
        _ => {
            outline.block(argument.trim());
            outline.block(&body.iter().map(|line| line.trim()).collect::<Vec<_>>().join("\n"));
        }
    }

    next
}

/// The indented lines that follow (blank lines inside kept), dedented, and the line after them.
fn indented<'a>(lines: &[&'a str], from: usize) -> (Vec<&'a str>, usize) {
    let mut end = from;
    let mut last_content = from;

    while end < lines.len() && (lines[end].trim().is_empty() || lines[end].starts_with([' ', '\t'])) {
        if !lines[end].trim().is_empty() {
            last_content = end + 1;
        }

        end += 1;
    }

    let body = &lines[from..last_content];
    let indent = body
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    let dedented = body
        .iter()
        .map(|line| if line.len() >= indent { &line[indent..] } else { "" })
        .collect();

    (dedented, last_content)
}

fn fenced(body: &[&str]) -> String {
    let text = body.join("\n");
    let text = text.trim_matches('\n');

    if text.trim().is_empty() {
        String::new()
    } else {
        format!("```\n{text}\n```")
    }
}

fn flush(outline: &mut Outline, paragraph: &mut Vec<&str>) {
    if !paragraph.is_empty() {
        outline.block(&paragraph.join("\n"));
        paragraph.clear();
    }
}
