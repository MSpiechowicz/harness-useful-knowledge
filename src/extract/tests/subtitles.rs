use pretty_assertions::assert_eq;

use super::{outline, run};
use crate::extract::*;

const TALK_VTT: &str = include_str!("../fixtures/talk.vtt");
const TALK_SRT: &str = include_str!("../fixtures/talk.srt");

/// `count` cues of `length` seconds, `gap` seconds apart, as WebVTT.
fn cues(count: usize, length: f64, gap: f64) -> String {
    let clock = |seconds: f64| {
        let whole = seconds as u64;

        format!(
            "{:02}:{:02}:{:02}.{:03}",
            whole / 3600,
            whole / 60 % 60,
            whole % 60,
            (seconds.fract() * 1000.0) as u64
        )
    };
    let mut text = String::from("WEBVTT\n\n");

    for n in 0..count {
        let start = n as f64 * (length + gap);
        text.push_str(&format!(
            "{} --> {}\nline number {n}\n\n",
            clock(start),
            clock(start + length)
        ));
    }

    text
}

#[test]
fn webvtt_loses_its_markup_notes_and_settings_and_keeps_the_speakers() {
    let extracted = run(Format::Transcript, "talk.vtt", TALK_VTT);

    assert_eq!(extracted.title.as_deref(), Some("Gateway design review"));
    assert_eq!(
        outline(&extracted),
        [
            (
                "00:00:01–00:00:10".to_owned(),
                "Roger Bingham: Welcome to the gateway design review. We start with routing.\nHildegard: Thanks & hello.\nRouting is by key prefix."
            ),
            ("00:00:32–00:00:35".to_owned(), "The second part starts after a pause."),
        ]
    );
}

#[test]
fn subrip_cues_are_merged_into_sections_split_at_pauses() {
    let extracted = run(Format::Transcript, "talk.srt", TALK_SRT);

    assert_eq!(extracted.title, None);
    assert_eq!(
        outline(&extracted),
        [
            (
                "00:00:01–00:00:06".to_owned(),
                "Hello and welcome. Second line of the same cue."
            ),
            ("00:00:20–00:01:05".to_owned(), "After a pause."),
        ]
    );
}

#[test]
fn a_long_talk_is_cut_into_sections_of_two_to_three_minutes() {
    // No pause longer than half a second: only the length cuts.
    let tight = run(Format::Transcript, "long.vtt", &cues(130, 4.0, 0.5));
    let headings: Vec<String> = outline(&tight).into_iter().map(|(heading, _)| heading).collect();

    assert_eq!(
        headings,
        [
            "00:00:00–00:02:59",
            "00:03:00–00:05:59",
            "00:06:00–00:08:59",
            "00:09:00–00:09:44"
        ]
    );

    // Pauses of 1.5 s let a section end once it has two minutes.
    let paused = run(Format::Transcript, "long.vtt", &cues(130, 4.0, 1.5));
    let first = outline(&paused).remove(0).0;

    assert_eq!(first, "00:00:00–00:02:05");
}

#[test]
fn subtitles_are_recognised_by_extension_and_by_content() {
    assert_eq!(detect("talk.vtt", b"x"), Some(Format::Transcript));
    assert_eq!(detect("TALK.SRT", b"x"), Some(Format::Transcript));
    assert_eq!(format_of_name("a/b.vtt"), Some(Format::Transcript));
    assert_eq!(detect("talk", TALK_VTT.as_bytes()), Some(Format::Transcript));
    assert_eq!(detect("talk", b"\xef\xbb\xbfWEBVTT\n\n"), Some(Format::Transcript));
    assert_eq!(detect("talk", TALK_SRT.as_bytes()), Some(Format::Transcript));
    assert_eq!(detect("talk.txt", TALK_VTT.as_bytes()), Some(Format::Transcript));
    assert_eq!(Format::Transcript.as_str(), "transcript");

    // Words that only look alike are text.
    assert_eq!(detect("notes", b"WEBVTTISH words"), Some(Format::Text));
    assert_eq!(detect("notes.txt", b"1\nplain words\n"), Some(Format::Text));
    assert_eq!(detect("notes", b"7\n--> not a cue\n"), Some(Format::Text));
}

#[test]
fn a_file_without_cues_is_refused() {
    let refusal = extract(Format::Transcript, "empty.vtt", b"WEBVTT\n\nNOTE nothing here\n").expect_err("refused");

    assert!(
        matches!(refusal, Refusal::Failed(ref reason) if reason.contains("no subtitle cues")),
        "{refusal:?}"
    );
}
