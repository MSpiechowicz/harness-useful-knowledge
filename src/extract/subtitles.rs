//! Subtitle transcripts: WebVTT (`.vtt`) and SubRip (`.srt`), as one section per stretch of talk.
//!
//! Cue numbers, identifiers, timing lines with their cue settings, `NOTE`, `STYLE` and `REGION`
//! blocks, markup (`<i>`, `<c.loud>`, `<00:00:01.000>`, SubRip's `{\an8}`) and a cue that repeats the
//! one before it (rolling captions) are dropped. The cues are merged into sections that end at a
//! pause of more than [`GAP_SECONDS`], at [`SOFT_SECONDS`] when the speaker pauses a moment, and at
//! [`HARD_SECONDS`] whatever happens, so a section is about two to three minutes of talk. Its
//! heading is the time it covers, `00:02:10–00:04:55`. A cue with a speaker (`<v Roger>`) keeps the
//! name in front of the text, `Roger: ...`, and consecutive cues of one speaker are one paragraph.

use super::{Extracted, Outline, Refusal, Result, heading_text};

/// A pause longer than this starts a new section.
const GAP_SECONDS: f64 = 5.0;

/// A section this long ends at the next pause of [`SOFT_GAP_SECONDS`].
const SOFT_SECONDS: f64 = 120.0;

/// The pause that ends a section of [`SOFT_SECONDS`].
const SOFT_GAP_SECONDS: f64 = 1.0;

/// A section never lasts longer than this.
const HARD_SECONDS: f64 = 180.0;

/// One cue: when it shows, who speaks (per voice tag) and what is said.
struct Cue {
    start: f64,
    end: f64,
    /// `(speaker, text)` in order; usually one entry.
    lines: Vec<(Option<String>, String)>,
}

/// True when `head` (lowercase, trimmed) is a WebVTT file or starts with a SubRip cue.
pub fn looks_like(head: &str) -> bool {
    let webvtt = head
        .strip_prefix("webvtt")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t', '\n', '\r']));

    webvtt || starts_like_subrip(head)
}

/// `1\n00:00:01,000 --> 00:00:02,500`.
fn starts_like_subrip(head: &str) -> bool {
    let mut lines = head.lines();
    let number = lines.next().unwrap_or_default().trim();
    let timing = lines.next().unwrap_or_default();

    !number.is_empty()
        && number.chars().all(|character| character.is_ascii_digit())
        && timing
            .split_once("-->")
            .is_some_and(|(start, _)| parse_time(start).is_some())
}

pub fn extract(text: &str) -> Result<Extracted> {
    let title = text
        .strip_prefix("WEBVTT")
        .and_then(|rest| rest.lines().next())
        .map(|line| heading_text(line.trim_start_matches([' ', '\t', '-']).trim()))
        .filter(|title| !title.is_empty());

    let mut cues: Vec<Cue> = Vec::new();

    for block in text.split("\n\n") {
        let Some(cue) = cue(block) else {
            continue;
        };

        // Rolling captions repeat the previous line.
        if cues.last().is_some_and(|last| last.lines == cue.lines) {
            continue;
        }

        cues.push(cue);
    }

    if cues.is_empty() {
        return Err(Refusal::failed("the file has no subtitle cues"));
    }

    let mut outline = Outline::new(Vec::new());

    for section in sections(&cues) {
        let (first, last) = (section[0], section[section.len() - 1]);

        outline.heading(1, &format!("{}–{}", clock(first.start), clock(last.end)));
        outline.block(&render(&section));
    }

    Ok(Extracted {
        title,
        sections: outline.finish(),
        ..Extracted::default()
    })
}

/// The cue of a block of lines; `None` for notes, styles, the header and anything without a timing line.
fn cue(block: &str) -> Option<Cue> {
    let first = block.trim_start().lines().next().unwrap_or_default();

    let word = first.split_whitespace().next().unwrap_or_default();

    if matches!(word, "NOTE" | "STYLE" | "REGION") || first.starts_with("WEBVTT") {
        return None;
    }

    let lines: Vec<&str> = block.lines().collect();
    let at = lines.iter().position(|line| line.contains("-->"))?;
    let (start, rest) = lines[at].split_once("-->")?;
    let end = rest.split_whitespace().next()?;
    let (start, end) = (parse_time(start)?, parse_time(end)?);

    let spoken = speakers(&lines[at + 1..].join(" "));

    (!spoken.is_empty()).then_some(Cue {
        start,
        end: end.max(start),
        lines: spoken,
    })
}

/// The seconds of `HH:MM:SS,mmm`, `HH:MM:SS.mmm` or `MM:SS.mmm`.
fn parse_time(raw: &str) -> Option<f64> {
    let parts: Vec<&str> = raw.trim().split(':').collect();

    if !(2..=3).contains(&parts.len()) {
        return None;
    }

    let (seconds, whole) = parts.split_last()?;
    let seconds: f64 = seconds.replace(',', ".").parse().ok()?;
    let mut total = seconds;

    for (position, part) in whole.iter().rev().enumerate() {
        let part: f64 = part.parse().ok()?;
        total += part * if position == 0 { 60.0 } else { 3600.0 };
    }

    (total.is_finite() && total >= 0.0).then_some(total)
}

/// The text of a cue without markup, split where a `<v Name>` tag names a speaker.
fn speakers(raw: &str) -> Vec<(Option<String>, String)> {
    let mut out: Vec<(Option<String>, String)> = Vec::new();
    let mut speaker: Option<String> = None;
    let mut current = String::new();
    let mut rest = raw;

    let mut push = |speaker: &Option<String>, text: &mut String| {
        let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
        text.clear();

        if !words.is_empty() {
            out.push((speaker.clone(), entities(&words)));
        }
    };

    while let Some(open) = rest.find('<') {
        current.push_str(&rest[..open]);

        // A cue tag starts with a letter, `/` or a timestamp digit; `x < 5 and y > 3` is text.
        let starts_tag = rest[open + 1..]
            .chars()
            .next()
            .is_some_and(|next| next.is_ascii_alphanumeric() || next == '/');

        if !starts_tag {
            current.push('<');
            rest = &rest[open + 1..];
            continue;
        }

        let Some(close) = rest[open..].find('>') else {
            // A lone `<` is text.
            current.push_str(&rest[open..]);
            rest = "";
            break;
        };

        let tag = &rest[open + 1..open + close];
        let name = tag.split_whitespace().next().unwrap_or_default();

        if name == "v" || name.starts_with("v.") {
            push(&speaker, &mut current);
            speaker = Some(heading_text(tag[name.len()..].trim())).filter(|speaker| !speaker.is_empty());
        }

        rest = &rest[open + close + 1..];
    }

    current.push_str(rest);
    push(&speaker, &mut current);

    out
}

/// `text` without SubRip's `{\an8}` overrides and with the entities a cue may hold decoded.
fn entities(text: &str) -> String {
    let mut plain = String::new();
    let mut rest = text;

    while let Some(open) = rest.find("{\\") {
        plain.push_str(&rest[..open]);
        rest = rest[open..].find('}').map_or("", |close| &rest[open + close + 1..]);
    }

    plain.push_str(rest);

    plain
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&lrm;", "")
        .replace("&rlm;", "")
        .replace("&amp;", "&")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The cues grouped into sections.
fn sections(cues: &[Cue]) -> Vec<Vec<&Cue>> {
    let mut sections: Vec<Vec<&Cue>> = Vec::new();

    for cue in cues {
        let split = sections.last().is_some_and(|section| {
            let from = section[0].start;
            let to = section[section.len() - 1].end;
            let gap = cue.start - to;

            gap > GAP_SECONDS || cue.end - from > HARD_SECONDS || (to - from >= SOFT_SECONDS && gap >= SOFT_GAP_SECONDS)
        });

        match sections.last_mut() {
            Some(section) if !split => section.push(cue),
            _ => sections.push(vec![cue]),
        }
    }

    sections
}

/// The text of a section: a line for each run of one speaker, `Name: text` when there is one.
fn render(section: &[&Cue]) -> String {
    let mut runs: Vec<(Option<&str>, String)> = Vec::new();

    for (speaker, text) in section.iter().flat_map(|cue| &cue.lines) {
        match runs.last_mut() {
            Some((last, run)) if *last == speaker.as_deref() => {
                run.push(' ');
                run.push_str(text);
            }
            _ => runs.push((speaker.as_deref(), text.clone())),
        }
    }

    runs.iter()
        .map(|(speaker, text)| match speaker {
            Some(name) => format!("{name}: {text}"),
            None => text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `HH:MM:SS`.
fn clock(seconds: f64) -> String {
    let whole = seconds as u64;

    format!("{:02}:{:02}:{:02}", whole / 3600, whole / 60 % 60, whole % 60)
}
