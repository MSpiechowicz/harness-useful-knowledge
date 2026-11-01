//! The PDF worker: `harness-knowledge extract-worker FILE`, a hidden subcommand the import runs as
//! a child process (see [`super::pdf`]).
//!
//! It reads one PDF and prints one JSON object on stdout, `{"pages": ["text of page 1", ...]}` or
//! `{"error": "why"}`, never more text than an import takes ([`TEXT_BYTES_MAX`]). A panic of the PDF
//! reader is caught and reported as an error, and a crash that cannot be caught (a stack overflow)
//! ends the process, which the parent reports for that file alone.
//!
//! A hostile PDF can also ask for more memory or time than any real document: a compressed stream
//! that expands a thousand fold, or a page tree that never ends. On Unix the worker so limits its
//! own address space ([`ADDRESS_SPACE_BYTES`]) and CPU time ([`CPU_SECONDS`]) before it opens the
//! file. A refused allocation aborts the process and the CPU limit kills it, and the parent reports
//! both as a PDF that is too large to read (see [`super::pdf`]). The reader's own decompression
//! survives a refused allocation (it logs it and keeps what it inflated so far, which is blank), so
//! the worker listens to that log ([`MemoryWatch`]) and reports such a file as too large to read,
//! not as a scanned one.
//!
//! Windows has no such limit here: `unsafe_code` is forbidden, so there is no job object, and a
//! FlateDecode stream does not declare its decompressed size, so a bomb cannot be told from a large
//! page without inflating it. There the parent's deadline ([`crate::limits::PDF_TIMEOUT_SECS`]) is
//! the only bound, and the worker's memory is not capped (recorded in `docs/plans/knowledge.md`).

use std::io::Write;
use std::panic::catch_unwind;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::limits::TEXT_BYTES_MAX;

/// The most address space the worker may map: about three times what a 64 MiB PDF needs.
#[cfg(unix)]
const ADDRESS_SPACE_BYTES: u64 = 1536 * 1024 * 1024;

/// The CPU time after which the worker is stopped (its soft limit, the hard one is 5 s later).
#[cfg(unix)]
const CPU_SECONDS: u64 = 60;

/// Limits the memory and CPU time of this process. A limit the system refuses is not fatal: the
/// parent's deadline still holds.
#[cfg(unix)]
fn limit_resources() {
    use nix::sys::resource::{Resource, setrlimit};

    let _ = setrlimit(Resource::RLIMIT_AS, ADDRESS_SPACE_BYTES, ADDRESS_SPACE_BYTES);
    let _ = setrlimit(Resource::RLIMIT_CPU, CPU_SECONDS, CPU_SECONDS + 5);
}

#[cfg(not(unix))]
fn limit_resources() {}

/// Set when the PDF reader reported that an allocation was refused.
static OUT_OF_MEMORY: AtomicBool = AtomicBool::new(false);

/// Listens to the reader's log for a refused allocation: its stream decoder logs the failure and
/// goes on with what it inflated so far, which would pass for an empty page.
struct MemoryWatch;

impl log::Log for MemoryWatch {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let message = record.args().to_string().to_ascii_lowercase();

        if message.contains("out of memory") || message.contains("memory allocation") {
            OUT_OF_MEMORY.store(true, Ordering::SeqCst);
        }
    }

    fn flush(&self) {}
}

/// Fails when the reader was refused memory.
fn check_memory() -> Result<(), String> {
    if OUT_OF_MEMORY.load(Ordering::SeqCst) {
        return Err(TOO_LARGE.to_owned());
    }

    Ok(())
}

/// What the parent shows for a PDF that took more than the worker may.
pub const TOO_LARGE: &str = "the PDF is too large to read";

/// `harness-knowledge extract-worker <args>`.
pub fn run(args: &[Option<&str>]) -> ExitCode {
    let [Some(path)] = args else {
        eprintln!("usage: harness-knowledge extract-worker FILE");
        return ExitCode::from(2);
    };

    limit_resources();
    let _ = log::set_logger(&MemoryWatch).map(|()| log::set_max_level(log::LevelFilter::Warn));

    // The reader's panic messages carry file content: the parent only needs to know it failed.
    std::panic::set_hook(Box::new(|_| {}));

    let reply = match extract_pages(path).and_then(|pages| check_memory().map(|()| pages)) {
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
