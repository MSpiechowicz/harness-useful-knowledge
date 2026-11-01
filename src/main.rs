//! The `harness-knowledge` executable: `harness-knowledge --version` prints the version and
//! `harness-knowledge features` lists what this build can do for Harness Desktop. The commands of
//! the knowledge itself are added as they are built: so far `docs`, `repos`, `stats`, `project-key`
//! and `check` (see [`harness_knowledge::cli_docs`]).

use std::process::ExitCode;

use harness_knowledge::{cli, cli_docs};

/// What this build can do, as `harness-knowledge features` lists it: one token per line, for Harness
/// Desktop to tell what it can offer. A token is added in the same change that makes the feature work.
const FEATURES: [&str; 1] = ["docs"];

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
        _ => cli::usage(&format!("[features | --version | {}]", cli_docs::USAGE)),
    }
}
