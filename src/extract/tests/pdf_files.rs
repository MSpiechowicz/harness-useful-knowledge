use pretty_assertions::assert_eq;

use crate::extract::pdf::{PdfPages, from_pages};
use crate::extract::testing::{pdf, prose};
use crate::extract::*;

struct FixedPages(std::result::Result<Vec<String>, String>);

impl PdfPages for FixedPages {
    fn pages(&self, _path: &std::path::Path) -> std::result::Result<Vec<String>, String> {
        self.0.clone()
    }
}

fn page_of(characters: usize) -> String {
    format!("{} end.", prose(characters / 6))
}

#[test]
fn a_pdf_page_is_a_section_normalised_to_nfkc() {
    let extracted = from_pages(&[format!("ﬁne ﬂow {}", page_of(300)), page_of(300)]).expect("text pdf");

    assert_eq!(
        extracted
            .sections
            .iter()
            .map(|s| s.heading_path.join(" > "))
            .collect::<Vec<_>>(),
        ["Page 1", "Page 2"]
    );
    assert!(
        extracted.sections[0].text.starts_with("fine flow word0"),
        "ligatures expanded"
    );
    assert!(extracted.warnings.is_empty());
}

#[test]
fn a_scanned_pdf_has_no_text_layer() {
    let scanned = vec![String::new(), "3".to_owned(), String::new()];
    assert_eq!(
        from_pages(&scanned).expect_err("scanned"),
        Refusal::failed("the PDF has no text layer (scanned)")
    );

    // Pages of a few words average under 100 characters.
    let thin = vec!["a few words only on this page".to_owned(); 4];
    assert!(matches!(from_pages(&thin), Err(Refusal::Failed(reason)) if reason.contains("no text layer")));

    // Half of the pages are blank: refused although the rest is dense.
    let half = vec![page_of(4000), String::new(), page_of(4000), String::new()];
    assert!(matches!(from_pages(&half), Err(Refusal::Failed(reason)) if reason.contains("no text layer")));

    assert!(matches!(from_pages(&[]), Err(Refusal::Failed(_))));
}

#[test]
fn a_pdf_with_a_blank_page_keeps_the_rest_and_says_so() {
    let extracted = from_pages(&[page_of(900), String::new(), page_of(900), page_of(900)]).expect("mostly text");

    assert_eq!(extracted.sections.len(), 3);
    assert_eq!(extracted.warnings, ["1 of 4 pages have almost no text"]);
}

#[test]
fn a_failing_pdf_source_fails_the_file() {
    let source = FixedPages(Err("the PDF cannot be read: broken".to_owned()));
    let refusal = pdf::extract(&source, std::path::Path::new("x.pdf")).expect_err("fails");

    assert_eq!(refusal, Refusal::failed("the PDF cannot be read: broken"));
}

/// The worker's own reader on real PDF bytes.
#[test]
fn the_worker_reads_a_generated_pdf_and_refuses_a_scanned_or_malformed_one() {
    let dir = tempfile::tempdir().expect("temp dir");
    let write = |name: &str, bytes: &[u8]| {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write");
        path.to_string_lossy().into_owned()
    };

    let text = format!("Gateway handbook\n{}\nsecond line", page_of(500));
    let good = write("good.pdf", &pdf(&[&text, &text]));
    let pages = worker::extract_pages(&good).expect("reads");
    assert_eq!(pages.len(), 2);
    assert!(
        pages[0].contains("Gateway handbook") && pages[0].contains("second line"),
        "{:?}",
        pages[0]
    );

    let scanned = write("scanned.pdf", &pdf(&["", "", ""]));
    let pages = worker::extract_pages(&scanned).expect("reads, with no text");
    assert!(matches!(from_pages(&pages), Err(Refusal::Failed(reason)) if reason.contains("scanned")));

    let malformed = write(
        "malformed.pdf",
        b"%PDF-1.4\nthis is not a pdf at all, just words and no objects",
    );
    assert!(worker::extract_pages(&malformed).is_err());

    let truncated_bytes = pdf(&[&text, &text]);
    let truncated = write("truncated.pdf", &truncated_bytes[..truncated_bytes.len() / 2]);
    // A cut-off file either fails or gives what is left, never a panic or a hang.
    let _ = worker::extract_pages(&truncated);

    assert!(worker::extract_pages(&format!("{}/missing.pdf", dir.path().display())).is_err());
}

#[test]
fn the_worker_output_is_pages_or_a_reason() {
    assert_eq!(
        worker::parse_output(br#"{"pages":["a","b"]}"#),
        Ok(vec!["a".to_owned(), "b".to_owned()])
    );
    assert_eq!(
        worker::parse_output(br#"{"error":"crashed"}"#),
        Err("crashed".to_owned())
    );
    assert!(worker::parse_output(b"not json").is_err());
    assert!(worker::parse_output(b"{}").is_err());
}

#[cfg(unix)]
mod process {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::super::pdf::{PdfPages, WorkerProcess};

    /// Scripts are written and run one test at a time: a script that a parallel test's fork still
    /// holds open for writing cannot be executed ("Text file busy").
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A shell script standing in for the worker: `sh script.sh extract-worker FILE`.
    fn script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("worker.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[test]
    fn a_worker_that_outlives_its_deadline_is_killed() {
        let _serial = serial();
        let dir = tempfile::tempdir().expect("temp dir");
        let worker = WorkerProcess::new(script(dir.path(), "sleep 30"), Duration::from_millis(300));
        let started = Instant::now();

        let error = worker.pages(Path::new("/x.pdf")).expect_err("times out");

        assert!(error.contains("took longer"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10), "killed, not waited for");
    }

    #[test]
    fn a_worker_that_crashes_fails_the_file() {
        let _serial = serial();
        let dir = tempfile::tempdir().expect("temp dir");
        let worker = WorkerProcess::new(script(dir.path(), "exit 101"), Duration::from_secs(10));

        assert_eq!(
            worker.pages(Path::new("/x.pdf")).expect_err("crashes"),
            "the PDF reader crashed on this file"
        );
    }

    #[test]
    fn a_worker_stopped_by_its_memory_or_cpu_limit_fails_the_file_as_too_large() {
        let _serial = serial();
        let dir = tempfile::tempdir().expect("temp dir");

        for signal in ["ABRT", "XCPU", "KILL"] {
            let worker = WorkerProcess::new(
                script(dir.path(), &format!("kill -{signal} $$")),
                Duration::from_secs(10),
            );

            assert_eq!(
                worker.pages(Path::new("/x.pdf")).expect_err("stopped"),
                "the PDF is too large to read",
                "{signal}"
            );
        }
    }

    #[test]
    fn the_worker_starts_with_an_empty_environment() {
        let _serial = serial();
        let dir = tempfile::tempdir().expect("temp dir");
        let worker = WorkerProcess::new(
            script(
                dir.path(),
                "echo \"{\\\"error\\\":\\\"home=[$HOME] db=[$HARNESS_KNOWLEDGE_DATABASE_URL]\\\"}\"",
            ),
            Duration::from_secs(10),
        );

        // The test process has a HOME: the worker has none.
        assert!(std::env::var_os("HOME").is_some());
        assert_eq!(worker.pages(Path::new("/x.pdf")), Err("home=[] db=[]".to_owned()));
    }

    #[test]
    fn a_worker_reply_is_read_from_stdout() {
        let _serial = serial();
        let dir = tempfile::tempdir().expect("temp dir");
        let worker = WorkerProcess::new(
            script(dir.path(), "echo '{\"pages\":[\"one\",\"two\"]}'"),
            Duration::from_secs(10),
        );
        assert_eq!(
            worker.pages(Path::new("/x.pdf")),
            Ok(vec!["one".to_owned(), "two".to_owned()])
        );

        let failing = WorkerProcess::new(
            script(dir.path(), "echo '{\"error\":\"nope\"}'"),
            Duration::from_secs(10),
        );
        assert_eq!(failing.pages(Path::new("/x.pdf")), Err("nope".to_owned()));

        let missing = WorkerProcess::new(dir.path().join("none"), Duration::from_secs(1));
        assert!(
            missing
                .pages(Path::new("/x.pdf"))
                .expect_err("no program")
                .contains("cannot start")
        );
    }
}
