//! A PDF built byte by byte, so no binary fixture is needed. Shared by the unit tests and the
//! tests that run the real binary.

use std::fmt::Write;

/// A PDF with one page per entry of `pages`, each showing its text in Helvetica (lines split at
/// `\n`). An empty page text gives a page with no text at all.
pub fn pdf(pages: &[&str]) -> Vec<u8> {
    let page_ids: Vec<usize> = (0..pages.len()).map(|n| 4 + 2 * n).collect();
    let kids: Vec<String> = page_ids.iter().map(|id| format!("{id} 0 R")).collect();

    let mut objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.join(" "), pages.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_owned(),
    ];

    for (n, text) in pages.iter().enumerate() {
        let content = content_stream(text);

        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> >> >>",
            5 + 2 * n
        ));
        objects.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()));
    }

    assemble(&objects)
}

fn content_stream(text: &str) -> String {
    if text.is_empty() {
        return "q Q".to_owned();
    }

    let mut content = String::from("BT /F1 12 Tf 14 TL 72 720 Td\n");

    for line in text.lines() {
        let escaped = line.replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)");
        let _ = writeln!(content, "({escaped}) Tj T*");
    }

    content.push_str("ET");
    content
}

fn assemble(objects: &[String]) -> Vec<u8> {
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();

    for (n, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        let _ = write!(out, "{} 0 obj\n{body}\nendobj\n", n + 1);
    }

    let xref = out.len();
    let _ = write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);

    for offset in offsets {
        let _ = writeln!(out, "{offset:010} 00000 n ");
    }

    let _ = write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    );

    out.into_bytes()
}
