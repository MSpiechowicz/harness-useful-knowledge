//! `harness-knowledge search --json --query Q [--project PATH] [--path REL]... [--limit N]
//! [--scope repo|all] [--mode pointers|chunks]`: the same search the MCP tool runs, for Harness
//! Desktop and the person at the terminal.
//!
//! It prints a JSON array, best first, and `[]` (exit 0) when nothing matches or there is no
//! database: a read never creates it. An item is `{"docId", "chunkId", "id", "title", "status",
//! "section", "score", "snippet", "block"}` for `--mode pointers` (the default) and `"text"` instead of
//! `"snippet"` for `--mode chunks`. `id` is what `knowledge_get` takes (`12:340`, `local:12:340` for a document on this computer when both places are in use, where `docId` and `chunkId` are the string `local:12`). The `snippet`
//! and the `text` are the document's own words: whatever shows them to an agent must wrap them as
//! untrusted, and `block` is that wrapping, ready to use. `--project` names the repository the
//! search is for (else the working folder's), `--path` (repeatable) the repository-relative files
//! being worked on, `--limit` is 1 to 20 (8 when not given), `--scope all` sees the documents of
//! every repository (default `repo`: the global ones and this repository's). This is the person's
//! search, so it finds drafts too: `status` says `live`, `draft`, `waiting` or `changesRequested`
//! (agents never see anything but live documents, and a pending version is never a hit of its own).
//!
//! The embedder process answers the query's vector. Without it the model is loaded in this process
//! when it is installed, else the search is keyword-only and says so on stderr.
//!
//! With both places in use, a remote database that cannot be reached (or is cooling down) leaves
//! the answer to this computer's documents. The array keeps its shape, and stderr gets one line
//! before the JSON: `harness-knowledge: Remote knowledge is unreachable. Showing documents on this
//! computer only.` The search itself still exits 0.

use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{self, Access, OpenError};
use crate::cli::{self, Args};
use crate::config::{Backend, Routing};
use crate::search::render::{DOC_TAG, Pointer, block, chunk_ref, words_of};
use crate::search::{self, AutoEmbedder, Query, QueryEmbedder, Tuning, Vectors};
use crate::store::routed::{OFFLINE_NOTICE, ids};
use crate::store::{Scope, StoreError, clean};
use crate::support::Env;

/// The usage of the commands of this module, for the executable's usage line.
pub const USAGE: &str = "search --json --query Q [--project PATH] [--path REL]... [--limit N] [--scope repo|all] \
     [--mode pointers|chunks] | recall --hook [--project PATH]";

/// Rows `--limit` may ask for, and its default.
const LIMIT_MAX: usize = 20;
const LIMIT_DEFAULT: usize = 8;

/// Most characters of the query.
const QUERY_MAX: usize = 1000;

/// Why a search printed no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Usage,
    Failed(String),
    /// The remote database cannot be reached (the error code `unreachable`).
    Unreachable(String),
    Missing {
        code: &'static str,
        message: String,
    },
}

impl From<OpenError> for Failure {
    fn from(error: OpenError) -> Self {
        match error {
            OpenError::Missing { code, message } => Self::Missing { code, message },
            OpenError::Failed(message) => Self::Failed(message),
            OpenError::Unreachable(message) | OpenError::Busy(message) => Self::Unreachable(message),
        }
    }
}

impl From<StoreError> for Failure {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NoFts5 => Self::Missing {
                code: "no-fts5",
                message: error.to_string(),
            },
            StoreError::Missing { code, message } => Self::Missing { code, message },
            StoreError::Unreachable(message) => Self::Unreachable(message),
            other => Self::Failed(other.to_string()),
        }
    }
}

/// What a search printed: the array, and a note for stderr.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub json: Value,
    pub notice: Option<String>,
    /// The remote database could not be reached: the array is from this computer alone, and stderr
    /// says so ([`crate::store::routed::OFFLINE_NOTICE`]).
    pub offline: bool,
}

/// `harness-knowledge search <args>`.
pub fn search(args: &[Option<&str>]) -> ExitCode {
    let Some((env, selection)) = cli::settings() else {
        return ExitCode::FAILURE;
    };

    let Some(root) = crate::embed::data_folder(&env) else {
        return cli::failed("the data folder is unknown");
    };

    let mut embedder = AutoEmbedder::new(&root);

    match answer(&env, &selection.backend, selection.routing, args, &mut embedder) {
        Ok(answer) => {
            if answer.offline {
                eprintln!("harness-knowledge: {OFFLINE_NOTICE}");
            }

            if let Some(notice) = &answer.notice {
                eprintln!("harness-knowledge: {notice}");
            }

            println!("{}", answer.json);
            ExitCode::SUCCESS
        }
        Err(Failure::Usage) => cli::usage(USAGE),
        Err(Failure::Failed(message)) => cli::failed(&selection.backend.secrets().scrub(&message)),
        Err(Failure::Unreachable(message)) => cli::unreachable(&selection.backend.secrets().scrub(&message)),
        Err(Failure::Missing { code, message }) => cli::missing_capability(code, &message),
    }
}

/// The options of a search.
struct Options {
    query: String,
    project: Option<String>,
    paths: Vec<String>,
    limit: usize,
    everything: bool,
    chunks: bool,
}

/// The arguments with every `--path REL` and `--path=REL` taken out, and the paths.
fn take_paths<'a>(args: &[Option<&'a str>]) -> Option<(Vec<Option<&'a str>>, Vec<String>)> {
    let mut rest = Vec::new();
    let mut paths = Vec::new();
    let mut remaining = args.iter().copied();

    while let Some(arg) = remaining.next() {
        match arg? {
            "--path" => paths.push(remaining.next().flatten()?.to_owned()),
            other => match other.strip_prefix("--path=") {
                Some(path) => paths.push(path.to_owned()),
                None => rest.push(arg),
            },
        }
    }

    Some((rest, paths))
}

fn parse(args: &[Option<&str>]) -> Result<Options, Failure> {
    let (rest, paths) = take_paths(args).ok_or(Failure::Usage)?;
    let parsed = Args::parse(
        &rest,
        &["--json"],
        &["--query", "--project", "--limit", "--scope", "--mode"],
    )
    .ok_or(Failure::Usage)?;

    if !parsed.flag("--json") || !parsed.positional.is_empty() {
        return Err(Failure::Usage);
    }

    let query = clean::line(parsed.value("--query").ok_or(Failure::Usage)?);

    if query.is_empty() || query.chars().count() > QUERY_MAX {
        return Err(Failure::Usage);
    }

    let limit = match parsed.value("--limit") {
        Some(limit) => limit
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=LIMIT_MAX).contains(limit))
            .ok_or(Failure::Usage)?,
        None => LIMIT_DEFAULT,
    };

    let everything = match parsed.value("--scope") {
        None | Some("repo") => false,
        Some("all") => true,
        Some(_) => return Err(Failure::Usage),
    };

    let chunks = match parsed.value("--mode") {
        None | Some("pointers") => false,
        Some("chunks") => true,
        Some(_) => return Err(Failure::Usage),
    };

    if paths.len() > 20 || paths.iter().any(|path| path.chars().count() > 512) {
        return Err(Failure::Usage);
    }

    Ok(Options {
        query,
        project: parsed.project().map_err(|_| Failure::Usage)?,
        paths: paths
            .iter()
            .map(|path| clean::line(path).trim_start_matches("./").replace('\\', "/"))
            .filter(|path| !path.is_empty())
            .collect(),
        limit,
        everything,
        chunks,
    })
}

/// The answer to `args`, with `embedder` for the query's vector.
pub fn answer(
    env: &Env,
    backend: &Backend,
    routing: Routing,
    args: &[Option<&str>],
    embedder: &mut dyn QueryEmbedder,
) -> Result<Answer, Failure> {
    let options = parse(args)?;

    let Some(repo) = cli::repository(env, options.project.as_deref()) else {
        return Err(Failure::Failed(
            "--project must be an existing folder that is not a filesystem root".to_owned(),
        ));
    };

    let Some(mut store) = backend::open(env, backend, routing, Access::Read)? else {
        return Ok(Answer {
            json: json!([]),
            notice: None,
            offline: false,
        });
    };

    // The person's own search: drafts are found too, each hit says its status. (Agents never see
    // them, see `crate::mcp`.)
    let mut scope = Scope::from_repo(repo).with_drafts();
    scope.include_other_repos = options.everything;
    let query = Query {
        text: &options.query,
        scope: &scope,
        paths: &options.paths,
        limit: options.limit,
    };

    let found = search::run(store.as_mut(), Some(embedder), &query, Tuning::SEARCH)?;
    let words = words_of(&options.query);

    let items: Vec<Value> = found
        .hits
        .iter()
        .map(|scored| {
            let pointer = Pointer::of(scored, &words);
            let wrapped = if options.chunks {
                block(DOC_TAG, &pointer.attributes(), &scored.hit.text)
            } else {
                pointer.block()
            };

            let mut item = json!({
                "docId": ids::json(pointer.doc_id),
                "chunkId": ids::json(pointer.chunk_id),
                "id": chunk_ref(pointer.doc_id, pointer.chunk_id),
                "title": pointer.title,
                "status": scored.hit.status.as_str(),
                "section": pointer.section,
                "score": pointer.score.round() as i64,
            });

            if options.chunks {
                item["text"] = json!(scored.hit.text);
            } else {
                item["snippet"] = json!(pointer.snippet);
            }

            item["block"] = json!(wrapped);
            item
        })
        .collect();

    let notice = match found.vectors {
        Vectors::Unavailable(reason) => Some(format!("vector search unavailable ({reason}), keyword results only")),
        _ => None,
    };

    Ok(Answer {
        json: Value::Array(items),
        notice,
        offline: store.offline(),
    })
}

#[cfg(test)]
mod tests;
