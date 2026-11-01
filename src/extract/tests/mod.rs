use pretty_assertions::assert_eq;

use super::*;

const PAGE_HTML: &str = include_str!("../fixtures/page.html");
const CONFLUENCE_PAGE: &str = include_str!("../fixtures/confluence/Deploy-the-gateway_100003.html");
const CONFLUENCE_INDEX: &str = include_str!("../fixtures/confluence/index.html");

pub(super) fn run(format: Format, name: &str, text: &str) -> Extracted {
    extract(format, name, text.as_bytes()).expect("extracts")
}

/// `(heading path, text)` of every section.
pub(super) fn outline(extracted: &Extracted) -> Vec<(String, &str)> {
    extracted
        .sections
        .iter()
        .map(|section| (section.heading_path.join(" > "), section.text.as_str()))
        .collect()
}

pub(super) fn find<'a>(extracted: &'a Extracted, path: &str) -> &'a Section {
    extracted
        .sections
        .iter()
        .find(|section| section.heading_path.join(" > ") == path)
        .unwrap_or_else(|| panic!("no section {path}: {:?}", outline(extracted)))
}

#[test]
fn the_format_comes_from_the_signature_the_extension_and_then_the_content() {
    assert_eq!(detect("a.md", b"# hi"), Some(Format::Markdown));
    assert_eq!(detect("A.MARKDOWN", b"x"), Some(Format::Markdown));
    assert_eq!(detect("notes.txt", b"x"), Some(Format::Text));
    assert_eq!(detect("p.htm", b"x"), Some(Format::Html));
    assert_eq!(detect("api.yml", b"x"), Some(Format::OpenApi));
    assert_eq!(detect("api.json", b"{}"), Some(Format::OpenApi));
    assert_eq!(detect("g.adoc", b"= T"), Some(Format::AsciiDoc));
    assert_eq!(detect("g.rst", b"T"), Some(Format::Rst));
    assert_eq!(
        detect("scan.txt", b"%PDF-1.7 ..."),
        Some(Format::Pdf),
        "the signature wins"
    );
    assert_eq!(detect("README", b"just words"), Some(Format::Text));
    assert_eq!(detect("page", b"  <!DOCTYPE html><html>"), Some(Format::Html));
    assert_eq!(detect("spec", b"{\"openapi\": \"3.0.0\"}"), Some(Format::OpenApi));
    assert_eq!(detect("spec", b"openapi: 3.0.0\ninfo: {}\n"), Some(Format::OpenApi));
    assert_eq!(detect("blob", &[0x7f, b'E', b'L', b'F', 0, 1, 2]), None, "binary data");
}

#[test]
fn text_is_decoded_and_cleaned() {
    let (text, warning) = decode(b"\xef\xbb\xbfone\r\ntwo \x07\r\n\r\n\r\n\r\nthree\r").expect("decodes");
    assert_eq!(text, "one\ntwo\n\nthree");
    assert_eq!(warning, None);

    let (lossy, warning) = decode(b"caf\xe9 au lait").expect("decodes");
    assert!(lossy.starts_with("caf") && lossy.contains('\u{fffd}'));
    assert!(warning.is_some_and(|warning| warning.contains("not valid UTF-8")));

    let utf16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain("zażółć".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    assert_eq!(decode(&utf16).expect("utf-16").0, "zażółć");
}

#[test]
fn plain_text_is_paragraphs_without_headings() {
    let extracted = run(
        Format::Text,
        "notes.txt",
        "first line\nsecond line\n\nanother paragraph",
    );

    assert_eq!(
        outline(&extracted),
        [("".to_owned(), "first line\nsecond line\n\nanother paragraph")]
    );
    assert_eq!(extracted.title, None);
}

#[test]
fn markdown_keeps_the_heading_path_code_lists_and_tables() {
    let source = "---\ntitle: \"Front matter title\"\n---\n\n# Guide\n\nIntro with `code` and a [link](http://x).\n\n\
                  ## Install\n\n- one\n- two\n  - nested\n\n1. first\n2. second\n\n```bash\napt install x\n\n# comment\n```\n\n\
                  ### Linux\n\n| Flag | Meaning |\n|---|---|\n| -v | verbose |\n\n<div>raw html</div>\n\n## Use\n\nRun it.\n";
    let extracted = run(Format::Markdown, "guide.md", source);

    assert_eq!(extracted.title.as_deref(), Some("Front matter title"));
    assert_eq!(
        extracted
            .sections
            .iter()
            .map(|s| s.heading_path.join(" > "))
            .collect::<Vec<_>>(),
        ["Guide", "Guide > Install", "Guide > Install > Linux", "Guide > Use"]
    );
    assert_eq!(find(&extracted, "Guide").text, "Intro with `code` and a link.");
    assert_eq!(
        find(&extracted, "Guide > Install").text,
        "- one\n- two\n  - nested\n\n1. first\n2. second\n\n```bash\napt install x\n\n# comment\n```"
    );
    assert_eq!(
        find(&extracted, "Guide > Install > Linux").text,
        "Flag | Meaning\n-v | verbose"
    );
    assert_eq!(find(&extracted, "Guide > Use").text, "Run it.");
    assert_eq!(find(&extracted, "Guide").level, 1);
    assert_eq!(find(&extracted, "Guide > Install > Linux").level, 3);
}

#[test]
fn markdown_without_a_front_matter_title_takes_the_first_heading() {
    let extracted = run(Format::Markdown, "x.md", "preamble\n\n# Real title\n\ntext");

    assert_eq!(extracted.title.as_deref(), Some("Real title"));
    assert_eq!(extracted.sections[0].level, 0);
    assert_eq!(extracted.sections[0].heading_path, Vec::<String>::new());
}

#[test]
fn html_drops_scripts_styles_and_navigation() {
    let extracted = run(Format::Html, "page.html", PAGE_HTML);

    assert_eq!(extracted.title.as_deref(), Some("Gateway guide & limits"));
    assert_eq!(extracted.source_key, None);

    let all: String = extracted.sections.iter().map(|section| section.text.as_str()).collect();
    for dropped in ["Home", "Copyright", "injected", "color: red", "<h2>"] {
        assert!(!all.contains(dropped), "{dropped} leaked: {all}");
    }

    assert_eq!(
        find(&extracted, "Gateway guide").text,
        "The gateway routes requests to services."
    );
    assert_eq!(
        find(&extracted, "Gateway guide > Limits").text,
        "Each key may send 100 requests per minute.\n\n- First step\n- Second step\n- nested\n\n\
         ```\ncurl -H \"X-Key: abc\" https://gw/api\ncurl https://gw/health\n```\n\nPlan | Rate\nfree | 10"
    );
}

#[test]
fn a_confluence_page_leads_with_its_breadcrumbs_and_is_keyed_by_its_page_id() {
    let extracted = run(Format::Html, "Deploy-the-gateway_100003.html", CONFLUENCE_PAGE);

    assert_eq!(extracted.title.as_deref(), Some("Deploy the gateway"));
    assert_eq!(extracted.source_key.as_deref(), Some("confluence:100003"));
    assert_eq!(
        extracted
            .sections
            .iter()
            .map(|s| s.heading_path.join(" > "))
            .collect::<Vec<_>>(),
        [
            "Platform > Runbooks > Gateway",
            "Platform > Runbooks > Gateway > Prerequisites",
            "Platform > Runbooks > Gateway > Steps",
            "Platform > Runbooks > Gateway > Rollback",
        ]
    );
    assert_eq!(
        find(&extracted, "Platform > Runbooks > Gateway").text,
        "Use this runbook to deploy the gateway to production."
    );
    assert_eq!(
        find(&extracted, "Platform > Runbooks > Gateway > Prerequisites").text,
        "- Access to the cluster\n- The gwctl tool"
    );
    assert_eq!(
        find(&extracted, "Platform > Runbooks > Gateway > Steps").text,
        "```\ngwctl deploy --env prod\ngwctl status\n```\n\nEnv | URL\nprod | https://gw.example.com"
    );
    assert_eq!(
        find(&extracted, "Platform > Runbooks > Gateway > Rollback").text,
        "Run gwctl rollback & check the status."
    );

    let all: String = extracted.sections.iter().map(|section| section.text.as_str()).collect();
    for dropped in ["Created by", "Document generated", "Not content"] {
        assert!(!all.contains(dropped), "{dropped} leaked");
    }
}

#[test]
fn the_confluence_index_page_is_skipped() {
    let refusal = extract(Format::Html, "index.html", CONFLUENCE_INDEX.as_bytes()).expect_err("refused");

    assert!(matches!(refusal, Refusal::Skipped(reason) if reason.contains("index")));
    assert!(extract(Format::Html, "other.html", CONFLUENCE_INDEX.as_bytes()).is_ok());
}

mod formats;
mod pdf_files;
