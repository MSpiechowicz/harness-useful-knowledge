//! The PDF worker: `harness-knowledge extract-worker FILE`, a hidden subcommand the import runs as
//! a child process (see [`super::pdf`]).
//!
//! It reads one PDF and prints one JSON object on stdout, `{"pages": ["text of page 1", ...]}` or
//! `{"error": "why"}`, never more text than an import takes ([`TEXT_BYTES_MAX`]). A panic of the PDF
//! reader is caught and reported as an error, and a crash that cannot be caught (a stack overflow)
//! ends the process, which the parent reports for that file alone.

use std::io::Write;
use std::panic::catch_unwind;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::limits::TEXT_BYTES_MAX;

/// `harness-knowledge extract-worker <args>`.
pub fn run(args: &[Option<&str>]) -> ExitCode {
    let [Some(path)] = args else {
        eprintln!("usage: harness-knowledge extract-worker FILE");
        return ExitCode::from(2);
    };

    // The reader's panic messages carry file content: the parent only needs to know it failed.
    std::panic::set_hook(Box::new(|_| {}));

    let reply = match extract_pages(path) {
        Ok(pages) => json!({ "pages": pages }),
        Err(message) => json!({ "error": message }),
    };

    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{reply}");
    let _ = stdout.flush();

    ExitCode::SUCCESS
}

/// The text of each page of the PDF at `path`.
pub fn extract_pages(path: &str) -> Result<Vec<String>, String> {
    let read = catch_unwind(|| pdf_extract::extract_text_by_pages(path))
        .map_err(|_| "the PDF reader crashed on this file".to_owned())?;
    let pages = read.map_err(|error| format!("the PDF cannot be read: {}", short(&error.to_string())))?;

    let bytes: usize = pages.iter().map(String::len).sum();

    if bytes > TEXT_BYTES_MAX {
        return Err("the PDF has more text than an import takes".to_owned());
    }

    Ok(pages)
}

/// The pages in the worker's output, or the reason it gave.
pub fn parse_output(output: &[u8]) -> Result<Vec<String>, String> {
    let reply: Value = serde_json::from_slice(output).map_err(|_| "the PDF worker gave no result".to_owned())?;

    if let Some(message) = reply.get("error").and_then(Value::as_str) {
        return Err(message.to_owned());
    }

    reply
        .get("pages")
        .and_then(Value::as_array)
        .map(|pages| {
            pages
                .iter()
                .map(|page| page.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .ok_or_else(|| "the PDF worker gave no pages".to_owned())
}

/// The first line of an error message, at most 200 characters.
fn short(message: &str) -> String {
    message.lines().next().unwrap_or("").chars().take(200).collect()
}
