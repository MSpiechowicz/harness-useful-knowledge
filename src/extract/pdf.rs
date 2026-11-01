//! PDF: the pages of text the worker process extracted, one section per page headed `Page N`.
//!
//! The PDF reader panics on damaged files and can run for a very long time, so it never runs in the
//! import's own process: [`WorkerProcess`] starts `harness-knowledge extract-worker FILE` (see
//! [`super::worker`]), reads its JSON from stdout up to a cap, and kills it when it outlives its
//! deadline. A crash, a timeout or an unreadable file fails that file only. The worker runs with an
//! empty environment (nothing of the importer's variables, tokens or database URL reaches a process
//! that parses untrusted files), and on Unix with memory and CPU limits (see [`super::worker`]): a
//! worker that hits them ends by a signal and its file fails as too large to read.
//!
//! A PDF made of page images (scanned) has no text layer: it is refused when the pages hold fewer
//! than 100 characters on average, or when half of them hold fewer than 25. OCR is out of scope.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use unicode_normalization::UnicodeNormalization;

use super::{Extracted, Refusal, Result, Section, clean_text};
use crate::limits::{PDF_TIMEOUT_SECS, TEXT_BYTES_MAX};

/// Where the pages of a PDF come from.
pub trait PdfPages: Send + Sync {
    /// The text of each page of the PDF at `path`, or why it cannot be read.
    fn pages(&self, path: &Path) -> std::result::Result<Vec<String>, String>;
}

/// The most JSON the worker's stdout is read for: the text limit with escapes to spare.
const OUTPUT_MAX: u64 = TEXT_BYTES_MAX as u64 * 6 + 65_536;

/// Runs the PDF reader in a child process.
#[derive(Debug, Clone)]
pub struct WorkerProcess {
    program: PathBuf,
    timeout: Duration,
}

impl WorkerProcess {
    /// A worker that runs this very executable (`harness-knowledge`) with the `extract-worker`
    /// subcommand, killed after [`PDF_TIMEOUT_SECS`].
    pub fn current_exe() -> std::result::Result<Self, String> {
        let program = std::env::current_exe().map_err(|error| format!("cannot find the PDF worker: {error}"))?;

        Ok(Self::new(program, Duration::from_secs(PDF_TIMEOUT_SECS)))
    }

    pub fn new(program: PathBuf, timeout: Duration) -> Self {
        Self { program, timeout }
    }

    /// Runs the worker on `path` with the arguments after the program name, returns its stdout.
    fn run(&self, args: &[&str], path: &Path) -> std::result::Result<Vec<u8>, String> {
        let mut command = Command::new(&self.program);

        command
            .args(args)
            .arg(path)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        // Windows cannot start a process without its system folder.
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }

        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot start the PDF worker: {error}"))?;

        let stdout = child.stdout.take().ok_or("the PDF worker has no output")?;
        let reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            let _ = stdout.take(OUTPUT_MAX + 1).read_to_end(&mut output);
            output
        });

        let started = Instant::now();

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() >= self.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();

                    // The reader ends with the pipe: a stray grandchild holding it must not hold us.
                    drop(reader);

                    return Err(format!("reading the PDF took longer than {} s", self.timeout.as_secs()));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => return Err(format!("the PDF worker failed: {error}")),
            }
        };

        let output = reader.join().map_err(|_| "the PDF worker output failed".to_owned())?;

        if output.len() as u64 > OUTPUT_MAX {
            return Err("the PDF has more text than an import takes".to_owned());
        }

        if !status.success() && output.is_empty() {
            return Err(crash_reason(status));
        }

        Ok(output)
    }
}

/// Why a worker that printed nothing is gone: the abort of a refused allocation (a stack overflow
/// aborts alike), the CPU limit or the kernel's out-of-memory kill mean the file asked for more than
/// the worker may take.
fn crash_reason(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use nix::sys::signal::Signal;
        use std::os::unix::process::ExitStatusExt;

        let too_large = [Signal::SIGABRT, Signal::SIGXCPU, Signal::SIGKILL];

        if status
            .signal()
            .is_some_and(|signal| too_large.iter().any(|large| *large as i32 == signal))
        {
            return super::worker::TOO_LARGE.to_owned();
        }
    }

    let _ = status;

    "the PDF reader crashed on this file".to_owned()
}

impl PdfPages for WorkerProcess {
    fn pages(&self, path: &Path) -> std::result::Result<Vec<String>, String> {
        let output = self.run(&["extract-worker"], path)?;

        super::worker::parse_output(&output)
    }
}

/// Reads the PDF at `path` with `source`: NFKC-normalised pages, or a refusal.
pub fn extract(source: &dyn PdfPages, path: &Path) -> Result<Extracted> {
    let pages = source.pages(path).map_err(Refusal::failed)?;

    from_pages(&pages)
}

/// The sections of already extracted pages.
pub fn from_pages(pages: &[String]) -> Result<Extracted> {
    if pages.is_empty() {
        return Err(Refusal::failed("the PDF has no pages"));
    }

    let texts: Vec<String> = pages
        .iter()
        .map(|page| clean_text(&page.nfkc().collect::<String>()))
        .collect();
    let sizes: Vec<usize> = texts
        .iter()
        .map(|text| text.chars().filter(|character| !character.is_whitespace()).count())
        .collect();

    let total: usize = sizes.iter().sum();
    let short = sizes.iter().filter(|size| **size < 25).count();

    if total / pages.len() < 100 || short * 2 >= pages.len() {
        return Err(Refusal::failed("the PDF has no text layer (scanned)"));
    }

    let mut extracted = Extracted::default();

    for (number, text) in texts.into_iter().enumerate() {
        if !text.is_empty() {
            extracted.sections.push(Section {
                heading_path: vec![format!("Page {}", number + 1)],
                level: 1,
                text,
            });
        }
    }

    if short > 0 {
        extracted
            .warnings
            .push(format!("{short} of {} pages have almost no text", pages.len()));
    }

    Ok(extracted)
}
