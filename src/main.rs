//! The `harness-knowledge` executable: `harness-knowledge --version` prints the version and
//! `harness-knowledge features` lists what this build can do for Harness Desktop. The commands of
//! the knowledge itself are added as they are built: so far `docs`, `repos`, `stats`, `project-key`
//! and `check` (see [`harness_knowledge::cli_docs`]) and `embedder` (see
//! [`harness_knowledge::cli_embedder`]).

use std::process::ExitCode;

use harness_knowledge::{cli, cli_docs, cli_embedder, extract};

/// What this build can do, as `harness-knowledge features` lists it: one token per line, for Harness
/// Desktop to tell what it can offer. A token is added in the same change that makes the feature work.
const FEATURES: [&str; 3] = ["docs", "embedder", "import-ndjson"];

/// mimalloc hands freed memory back to the operating system, which the embedder's idle unload of
/// the model relies on (glibc's allocator keeps it).
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let args: Vec<Option<&str>> = args.iter().map(|arg| arg.to_str()).collect();

    match args.as_slice() {
        [Some("--version")] => {
            println!("harness-knowledge {}", env!("HARNESS_KNOWLEDGE_VERSION"));
            ExitCode::SUCCESS
        }
        [Some("features")] => {
            for feature in FEATURES {
                println!("{feature}");
            }

            ExitCode::SUCCESS
        }
        [Some("docs"), rest @ ..] => cli_docs::docs(rest),
        [Some("repos"), rest @ ..] => cli_docs::repos(rest),
        [Some("stats"), rest @ ..] => cli_docs::stats(rest),
        [Some("project-key"), rest @ ..] => cli_docs::project_key(rest),
        [Some("check"), rest @ ..] => cli_docs::check(rest),
        [Some("embedder"), rest @ ..] => cli_embedder::embedder(rest),
        // Hidden: the child process an import runs to read a PDF (see `extract::worker`).
        [Some("extract-worker"), rest @ ..] => extract::worker::run(rest),
        _ => cli::usage(&format!(
            "[features | --version | {} | {}]",
            cli_docs::USAGE,
            cli_embedder::USAGE
        )),
    }
}
