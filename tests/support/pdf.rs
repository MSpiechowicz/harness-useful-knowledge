//! A PDF built byte by byte, so no binary fixture is needed. Shared by the unit tests and the
//! tests that run the real binary.

use std::fmt::Write;
#[allow(unused_imports)]
use std::io::Write as _;

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

/// A zlib stream of `blocks` MiB of spaces: one block compressed once, then repeated (a sync flush
/// ends each block on a byte boundary, and every back-reference points at spaces), so it takes no
/// time to make and is about a thousandth of its inflated size.
#[allow(dead_code)]
fn zlib_spaces(blocks: usize) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::DeflateEncoder;

    const BLOCK: usize = 1 << 20;
    const ADLER: u128 = 65_521;

    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&vec![b' '; BLOCK]).expect("compress");
    encoder.flush().expect("sync flush");
    let one = encoder.get_ref().clone();

    let mut stream = vec![0x78, 0xDA];

    for _ in 0..blocks {
        stream.extend_from_slice(&one);
    }

    // An empty final block, then the Adler-32 of the inflated spaces.
    stream.extend_from_slice(&[0x03, 0x00]);

    let total = (blocks * BLOCK) as u128;
    let sum = (1 + total * 32) % ADLER;
    let weighted = (total + 32 * (total * (total + 1) / 2)) % ADLER;
    stream.extend_from_slice(&(((weighted << 16) | sum) as u32).to_be_bytes());

    stream
}

/// A PDF whose one page has a FlateDecode content stream of `blocks` MiB of spaces: a few
/// megabytes on disk that inflate to gigabytes (a decompression bomb).
#[allow(dead_code)]
pub fn flate_bomb(blocks: usize) -> Vec<u8> {
    let stream = zlib_spaces(blocks);
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();

    let head = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>".to_owned(),
    ];

    for (n, body) in head.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", n + 1).as_bytes());
    }

    offsets.push(out.len());
    out.extend_from_slice(format!("4 0 obj\n<< /Length {} /Filter /FlateDecode >>\nstream\n", stream.len()).as_bytes());
    out.extend_from_slice(&stream);
    out.extend_from_slice(b"\nendstream\nendobj\n");

    let xref = out.len();
    let mut table = String::from("xref\n0 5\n0000000000 65535 f \n");

    for offset in offsets {
        table.push_str(&format!("{offset:010} 00000 n \n"));
    }

    table.push_str(&format!(
        "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ));
    out.extend_from_slice(table.as_bytes());

    out
}
