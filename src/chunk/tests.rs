use pretty_assertions::assert_eq;

use super::*;
use crate::digest::sha256_hex;
use crate::extract::testing::prose;
use crate::tokens::{ApproxCounter, test_tokenizer};

fn section(path: &[&str], text: &str) -> Section {
    Section {
        heading_path: path.iter().map(|part| (*part).to_owned()).collect(),
        level: path.len(),
        text: text.to_owned(),
    }
}

/// A paragraph of about `tokens` approximate tokens, made of sentences so it splits cleanly.
fn paragraph(tag: &str, tokens: usize) -> String {
    // A sentence is about 60 characters, 17 approximate tokens.
    let sentences = tokens / 17 + 1;

    (0..sentences)
        .map(|n| format!("Sentence {tag}{n} explains one more detail of the {tag} part."))
        .collect::<Vec<_>>()
        .join(" ")
}

fn chunk(title: &str, sections: &[Section]) -> Chunked {
    chunk_document(title, sections, &ApproxCounter)
}

/// The characters `start..end` of `text`.
fn chars(text: &str, start: usize, end: usize) -> String {
    text.chars().skip(start).take(end - start).collect()
}

#[test]
fn a_short_section_is_one_chunk_with_its_passage_hashed() {
    let chunked = chunk(
        "Gateway",
        &[section(&["Gateway", "Limits"], "Each key may send 100 requests.")],
    );

    assert_eq!(chunked.pieces.len(), 1);

    let piece = &chunked.pieces[0];
    let embedded = "Gateway › Limits\nEach key may send 100 requests.";

    assert_eq!(piece.heading_path, "Limits", "the title is not repeated");
    assert_eq!(piece.text, "Each key may send 100 requests.");
    assert_eq!(piece.chunk_sha, sha256_hex(embedded.as_bytes()));
    assert_eq!(piece.tokens, count_passage(&ApproxCounter, embedded));
    assert_eq!(chunked.text, "## Limits\nEach key may send 100 requests.");
}

#[test]
fn the_passage_has_the_prefix_title_and_path() {
    assert_eq!(passage("Guide", "A > B", "text"), "Guide › A > B\ntext");
    assert_eq!(passage("Guide", "", "text"), "Guide\ntext");
}

#[test]
fn the_title_is_dropped_from_the_path_and_deep_paths_are_cut() {
    let path = |parts: &[&str]| parts.iter().map(|part| (*part).to_owned()).collect::<Vec<_>>();

    assert_eq!(display_path(&path(&["Guide", "Install"]), "guide"), "Install");
    assert_eq!(display_path(&path(&["Guide"]), "Guide"), "");
    assert_eq!(display_path(&path(&["Other", "Install"]), "Guide"), "Other > Install");

    let deep = path(&[&"a".repeat(150), &"b".repeat(150), "last"]);
    let shown = display_path(&deep, "Guide");
    assert_eq!(shown.chars().count(), HEADING_PATH_CHARS_MAX);
    assert!(shown.starts_with('…') && shown.ends_with("> last"));
}

#[test]
fn a_long_section_splits_under_the_budget_with_overlap_and_offsets() {
    let body = paragraph("alpha", 1500);
    let chunked = chunk("Doc", &[section(&["Doc", "Long"], &body)]);
    let pieces = &chunked.pieces;

    assert!(pieces.len() >= 4, "{} chunks", pieces.len());

    for piece in pieces {
        let embedded = passage("Doc", &piece.heading_path, &piece.text);

        assert!(
            count_passage(&ApproxCounter, &embedded) <= TARGET_TOKENS,
            "{} tokens",
            piece.tokens
        );
        assert_eq!(piece.tokens, count_passage(&ApproxCounter, &embedded));
        assert_eq!(chars(&chunked.text, piece.char_start, piece.char_end), piece.text);
        assert_eq!(piece.chunk_sha, sha256_hex(embedded.as_bytes()));
        assert_eq!(piece.heading_path, "Long");
    }

    for pair in pieces.windows(2) {
        assert!(pair[1].char_start < pair[0].char_end, "consecutive chunks overlap");
        assert!(pair[1].char_start > pair[0].char_start, "and still progress");
        assert!(pair[1].char_end > pair[0].char_end);
    }

    let overlap = pieces[0].char_end - pieces[1].char_start;
    let size = pieces[0].char_end - pieces[0].char_start;
    assert!(overlap > 0);
    assert!(
        overlap * 100 / size <= 20,
        "about 15 percent, got {}",
        overlap * 100 / size
    );
}

#[test]
fn chunks_never_cross_a_section_and_only_a_split_section_overlaps() {
    let sections = [
        section(&["Doc", "First"], &paragraph("first", 900)),
        section(&["Doc", "Second"], &paragraph("second", 900)),
        section(&["Doc", "Third"], &paragraph("third", 300)),
    ];
    let chunked = chunk("Doc", &sections);
    let names = ["First", "Second", "Third"];

    // Where each section's heading line starts, in characters.
    let starts: Vec<usize> = names
        .iter()
        .map(|name| {
            let byte = chunked.text.find(&format!("## {name}\n")).expect("heading in the text");
            chunked.text[..byte].chars().count()
        })
        .collect();
    let end = chunked.text.chars().count();
    let section_end = |at: usize| starts.get(at + 1).copied().unwrap_or(end);

    for piece in &chunked.pieces {
        let at = names
            .iter()
            .position(|name| *name == piece.heading_path)
            .expect("a known section");

        assert!(
            piece.char_start >= starts[at] && piece.char_end <= section_end(at),
            "a chunk of {} leaves its section",
            piece.heading_path
        );
    }

    for (at, name) in names.iter().enumerate() {
        let own: Vec<&Piece> = chunked
            .pieces
            .iter()
            .filter(|piece| piece.heading_path == *name)
            .collect();

        assert!(!own.is_empty());
        assert!(own.last().expect("a chunk").char_end <= section_end(at));
    }

    let third: Vec<&Piece> = chunked
        .pieces
        .iter()
        .filter(|piece| piece.heading_path == "Third")
        .collect();
    assert_eq!(third.len(), 1, "a section that fits is not split, so it has no overlap");

    let first: Vec<&Piece> = chunked
        .pieces
        .iter()
        .filter(|piece| piece.heading_path == "First")
        .collect();
    assert!(
        first.len() > 1 && first[1].char_start < first[0].char_end,
        "a split section overlaps"
    );
}

#[test]
fn tiny_sibling_sections_merge_under_their_parent() {
    let sections = [
        section(&["Doc", "Ops", "GET /a"], "Returns a."),
        section(&["Doc", "Ops", "GET /b"], "Returns b."),
        section(&["Doc", "Ops", "GET /c"], "Returns c."),
        section(&["Doc", "Other"], "A different parent entirely."),
    ];
    let chunked = chunk("Doc", &sections);

    assert_eq!(chunked.pieces.len(), 2);
    assert_eq!(chunked.pieces[0].heading_path, "Ops");
    assert_eq!(
        chunked.pieces[0].text,
        "### GET /a\nReturns a.\n\n### GET /b\nReturns b.\n\n### GET /c\nReturns c."
    );
    assert_eq!(chunked.pieces[1].heading_path, "Other");
}

#[test]
fn a_section_that_is_not_tiny_or_not_a_sibling_stands_alone() {
    let sections = [
        section(&["Doc", "A"], "tiny"),
        section(&["Doc", "B"], &paragraph("big", 100)),
        section(&["Doc", "C"], "tiny"),
        section(&["Doc", "C", "Deeper"], "tiny"),
        section(&["Doc", "D"], "tiny"),
    ];
    let chunked = chunk("Doc", &sections);

    let paths: Vec<&str> = chunked.pieces.iter().map(|piece| piece.heading_path.as_str()).collect();
    assert_eq!(paths, ["A", "B", "C", "C > Deeper", "D"]);
}

#[test]
fn merged_siblings_stay_within_the_target() {
    let many: Vec<Section> = (0..40)
        .map(|n| {
            section(
                &["Doc", "Ops", &format!("op{n}")],
                &format!("Does thing number {n} with a few words."),
            )
        })
        .collect();
    let chunked = chunk("Doc", &many);

    assert!(chunked.pieces.len() > 1, "split into several merged chunks");

    for piece in &chunked.pieces {
        assert!(piece.tokens <= TARGET_TOKENS + 5, "{}", piece.tokens);
        assert_eq!(piece.heading_path, "Ops");
    }

    let joined: String = chunked
        .pieces
        .iter()
        .map(|piece| piece.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    for n in 0..40 {
        assert!(joined.contains(&format!("### op{n}\n")), "op{n} kept");
    }
}

#[test]
fn a_code_block_that_fits_is_kept_whole_and_one_that_does_not_splits_on_lines() {
    let code = (0..12)
        .map(|n| format!("let value_{n} = compute({n});"))
        .collect::<Vec<_>>()
        .join("\n");
    let body = format!(
        "{}\n\n```rust\n{code}\n```\n\n{}",
        paragraph("before", 200),
        paragraph("after", 200)
    );
    let chunked = chunk("Doc", &[section(&["Doc", "Code"], &body)]);

    let holder = chunked
        .pieces
        .iter()
        .find(|piece| piece.text.contains("```rust"))
        .expect("a chunk has the block");
    assert!(
        holder.text.contains(&format!("{code}\n```")),
        "whole block in one chunk:\n{}",
        holder.text
    );

    let long: String = (0..400)
        .map(|n| format!("let value_{n} = compute({n});"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunked = chunk("Doc", &[section(&["Doc", "Code"], &format!("```\n{long}\n```"))]);

    assert!(chunked.pieces.len() > 3);

    for piece in &chunked.pieces {
        assert!(piece.tokens <= TARGET_TOKENS, "{}", piece.tokens);

        // Cut between lines: every code line of a chunk is a whole line of the source.
        for line in piece.text.lines().filter(|line| line.starts_with("let ")) {
            assert!(line.ends_with(");"), "{line}");
        }
    }
}

#[test]
fn nothing_exceeds_the_hard_limit_even_for_a_long_title_and_a_word_without_spaces() {
    let title = "Ü".repeat(300);
    let blob = "x".repeat(20_000);
    let chunked = chunk(&title, &[section(&["Deep", &"h".repeat(200), &"g".repeat(200)], &blob)]);

    assert!(chunked.pieces.len() > 10);

    for piece in &chunked.pieces {
        let embedded = passage(&title, &piece.heading_path, &piece.text);

        assert!(
            count_passage(&ApproxCounter, &embedded) <= PASSAGE_TOKENS_MAX,
            "{}",
            piece.tokens
        );
    }

    let covered: String = chunked.pieces.iter().map(|piece| piece.text.as_str()).collect();
    assert!(covered.len() >= blob.len(), "no text lost");
}

#[test]
fn empty_sections_give_no_chunks_and_the_text_joins_the_rest() {
    let chunked = chunk("Doc", &[section(&["Doc"], "  \n "), section(&["Doc", "A"], "body")]);

    assert_eq!(chunked.pieces.len(), 1);
    assert_eq!(chunked.text, "## A\nbody");
    assert!(chunk("Doc", &[]).pieces.is_empty());
}

#[test]
fn offsets_count_characters_not_bytes() {
    let chunked = chunk("Doc", &[section(&["Doc", "Zażółć"], &"Gęślą jaźń. ".repeat(300))]);

    assert!(chunked.pieces.len() > 1);

    for piece in &chunked.pieces {
        assert_eq!(chars(&chunked.text, piece.char_start, piece.char_end), piece.text);
    }
}

#[test]
fn chunking_is_deterministic() {
    let sections = [
        section(&["Doc", "A"], &paragraph("a", 800)),
        section(&["Doc", "B"], "small"),
    ];

    assert_eq!(chunk("Doc", &sections), chunk("Doc", &sections));
}

#[test]
fn a_page_of_a_pdf_without_structure_still_splits() {
    let chunked = chunk("Manual", &[section(&["Page 1"], &prose(1200))]);

    assert!(chunked.pieces.len() > 3);
    assert!(chunked.pieces.iter().all(|piece| piece.heading_path == "Page 1"));
    assert!(chunked.pieces.iter().all(|piece| piece.tokens <= TARGET_TOKENS));
}

/// The same properties with the model's own tokenizer, when the run has one.
#[test]
fn chunks_fit_the_real_tokenizer_limit() {
    let Some(counter) = test_tokenizer() else {
        return;
    };

    let body = format!(
        "{}\n\n{}",
        paragraph("alpha", 1400),
        "żółć gęś 日本語のテキスト ".repeat(400)
    );
    let sections = [
        section(&["Doc", "Long"], &body),
        section(&["Doc", "Tiny one"], "Short."),
        section(&["Doc", "Tiny two"], "Also short."),
    ];
    let chunked = chunk_document("Doc", &sections, &counter);

    assert!(chunked.pieces.len() > 5);

    for piece in &chunked.pieces {
        let embedded = passage("Doc", &piece.heading_path, &piece.text);
        let tokens = count_passage(&counter, &embedded);

        assert!(tokens <= PASSAGE_TOKENS_MAX, "{tokens} tokens");
        assert_eq!(piece.tokens, tokens);
    }
}
