//! The commands that read and manage the documents, for Harness Desktop and the person at the
//! terminal, never for an agent's MCP calls. Every one of them takes `--json` and prints one JSON
//! object on stdout. Timestamps are RFC 3339, field names camelCase.
//!
//! - `docs list --json [--repo KEY | --global | --all] [--format F] [--query Q] [--sort
//!   title|updated|size|format|chunks|repos] [--desc] [--page N] [--per-page N]` prints `{"items": [...], "total": N}`:
//!   one page of the documents (see [`json::document`] for an item, it has its `links`). `--repo`
//!   keeps the documents linked to that repository key, `--global` those linked to none, `--all`
//!   (the default) every one. `--query` keeps the documents whose title contains it or whose text has
//!   all its words. Sorted by `updated` unless `--sort` says otherwise, ascending unless `--desc`,
//!   ties by id. Pages count from 1, `--per-page` is 1 to 200 (50 when not given). `total` counts
//!   every page.
//! - `docs get --json ID [--sections] [--chunk CID]` prints `{"document": {... "links"}}` and, as
//!   asked, `"sections"` (the outline: every chunk's id, position, heading path and size) and
//!   `"chunk"` (that chunk of the document, with its text).
//! - `docs links set --json ID` reads `[{"repo": "git:host/owner/repo", "globs": ["src/**"]}]` from
//!   stdin (at most 32 links with 32 globs of 256 characters, see [`crate::store::clean`]) and
//!   replaces the document's links; prints `{"id", "links"}`. An empty array makes it global.
//! - `docs rename --json ID --title TITLE` (1 to 300 characters) prints `{"id", "title"}`.
//! - `docs delete --json ID...` deletes the documents, their chunks and index entries, and prints
//!   `{"deleted": N}` (ids that do not exist count nothing).
//! - `docs import --ndjson` reads a manifest from stdin and prints one JSON event per line while it
//!   imports, `docs reindex --json (ID... | --all)` embeds the chunks that have no vector (see
//!   [`import`]).
//! - `repos --json` prints `{"global": N, "repos": [{"key", "docs"}]}`, `stats --json` the counts
//!   of [`overview::stats`], `project-key --json --path PATH` the repository key of a folder, and
//!   `check --json` what the database can do (see [`check`]).
//!
//! A read never creates the local database: without it every list is empty and exit 0. A document
//! that does not exist exits 1 with `harness-knowledge: document N not found` as the last stderr
//! line. Other exit codes and errors as in [`crate::cli`].

mod check;
mod import;
mod json;
mod overview;

use std::io::Read;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{self, Access, OpenError};
use crate::cli::{self, Args};
use crate::config::Backend;
use crate::limits::{DELETE_IDS_MAX, LIST_PAGE_DEFAULT, LIST_PAGE_MAX, STDIN_MAX};
use crate::store::{Detail, KnowledgeStore, Link, ListQuery, RepoFilter, Sort, StoreError, clean};
use crate::support::Env;

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "docs list --json [--repo KEY | --global | --all] [--format F] [--query Q] \
                         [--sort title|updated|size|format|chunks|repos] [--desc] [--page N] [--per-page N] | docs get --json ID \
                         [--sections] [--chunk CID] | docs links set --json ID < links.json | docs rename --json ID \
                         --title TITLE | docs delete --json ID... | docs import --ndjson < manifest.json | docs reindex \
                         --json (ID... | --all) | repos --json | stats --json | project-key --json \
                         --path PATH | check --json";

/// Where a command runs: the environment and the database it reads.
pub struct Context<'a> {
    pub env: &'a Env,
    pub backend: &'a Backend,
}

impl Context<'_> {
    fn open(&self, access: Access) -> Result<Option<Box<dyn KnowledgeStore>>, Failure> {
        Ok(backend::open(self.env, self.backend, access)?)
    }
}

/// What a command printed and how it ended: 0 unless a problem was found (`check`).
#[derive(Debug, Clone, PartialEq)]
pub struct Done {
    pub json: Value,
    pub code: u8,
}

impl Done {
    fn ok(json: Value) -> Self {
        Self { json, code: 0 }
    }
}

/// Why a command printed no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The arguments are wrong: exit 2 with the usage.
    Usage,
    /// The command failed: exit 1.
    Failed(String),
    /// The database lacks something (a `check` problem code): exit 3.
    Missing { code: &'static str, message: String },
}

impl From<StoreError> for Failure {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NoFts5 => Self::Missing {
                code: "no-fts5",
                message: error.to_string(),
            },
            StoreError::Missing { code, message } => Self::Missing { code, message },
            other => Self::Failed(other.to_string()),
        }
    }
}

impl From<OpenError> for Failure {
    fn from(error: OpenError) -> Self {
        match error {
            OpenError::Missing { code, message } => Self::Missing { code, message },
            OpenError::Failed(message) => Self::Failed(message),
        }
    }
}

pub type Reply = Result<Done, Failure>;

/// `harness-knowledge docs <args>`.
pub fn docs(args: &[Option<&str>]) -> ExitCode {
    run(args, docs_reply)
}

/// `harness-knowledge repos <args>`.
pub fn repos(args: &[Option<&str>]) -> ExitCode {
    run(args, |context, args, _| overview::repos(context, args))
}

/// `harness-knowledge stats <args>`.
pub fn stats(args: &[Option<&str>]) -> ExitCode {
    run(args, |context, args, _| overview::stats(context, args))
}

/// `harness-knowledge project-key <args>`.
pub fn project_key(args: &[Option<&str>]) -> ExitCode {
    run(args, |context, args, _| overview::project_key(context, args))
}

/// `harness-knowledge check <args>`.
pub fn check(args: &[Option<&str>]) -> ExitCode {
    run(args, |context, args, _| check::check(context, args))
}

type Command = fn(&Context, &[Option<&str>], &mut dyn Read) -> Reply;

/// Reads the settings, runs `command` and prints what it replies.
fn run(args: &[Option<&str>], command: Command) -> ExitCode {
    let Some((env, selection)) = cli::settings() else {
        return ExitCode::FAILURE;
    };

    let context = Context {
        env: &env,
        backend: &selection.backend,
    };

    finish(command(&context, args, &mut std::io::stdin().lock()))
}

/// Prints the reply and gives the exit code.
fn finish(reply: Reply) -> ExitCode {
    match reply {
        Ok(done) => {
            // A command that printed its own output (the import's events) has no object to print.
            if !done.json.is_null() {
                println!("{}", done.json);
            }

            ExitCode::from(done.code)
        }
        Err(Failure::Usage) => cli::usage(USAGE),
        Err(Failure::Failed(message)) => cli::failed(&message),
        Err(Failure::Missing { code, message }) => cli::missing_capability(code, &message),
    }
}

/// The reply of `docs <args>`; `input` is what `docs links set` reads.
pub fn docs_reply(context: &Context, args: &[Option<&str>], input: &mut dyn Read) -> Reply {
    match args {
        [Some("list"), rest @ ..] => list(context, rest),
        [Some("get"), rest @ ..] => get(context, rest),
        [Some("links"), Some("set"), rest @ ..] => links_set(context, rest, input),
        [Some("rename"), rest @ ..] => rename(context, rest),
        [Some("delete"), rest @ ..] => delete(context, rest),
        [Some("import"), rest @ ..] => import::import(context, rest, input),
        [Some("reindex"), rest @ ..] => import::reindex(context, rest),
        _ => Err(Failure::Usage),
    }
}

/// `docs list`.
fn list(context: &Context, args: &[Option<&str>]) -> Reply {
    let query = parse_list(args)?;

    // Without a database every list is empty, and nothing is created.
    let Some(mut store) = context.open(Access::Read)? else {
        return Ok(Done::ok(json!({ "items": [], "total": 0 })));
    };

    let page = store.list(&query)?;
    let items: Vec<Value> = page
        .items
        .iter()
        .map(|row| json::document(&row.doc, &row.links))
        .collect();

    Ok(Done::ok(json!({ "items": items, "total": page.total })))
}

fn parse_list(args: &[Option<&str>]) -> Result<ListQuery, Failure> {
    let valued = ["--repo", "--format", "--query", "--sort", "--page", "--per-page"];
    let parsed = Args::parse(args, &["--json", "--global", "--all", "--desc"], &valued).ok_or(Failure::Usage)?;

    if !parsed.positional.is_empty() || !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let filter = match (parsed.value("--repo"), parsed.flag("--global"), parsed.flag("--all")) {
        (None, false, _) => RepoFilter::All,
        (None, true, false) => RepoFilter::Global,
        (Some(key), false, false) => RepoFilter::Repo(clean::repo_key(key).map_err(|_| Failure::Usage)?.to_owned()),
        _ => return Err(Failure::Usage),
    };

    let format = match parsed.value("--format") {
        Some(format) => Some(parse_format(format)?),
        None => None,
    };

    let text = parsed.value("--query").unwrap_or_default().trim().to_owned();

    if text.chars().count() > QUERY_CHARS_MAX {
        return Err(Failure::Usage);
    }

    let sort = match parsed.value("--sort") {
        None | Some("updated") => Sort::Updated,
        Some("title") => Sort::Title,
        Some("size") => Sort::Size,
        Some("format") => Sort::Format,
        Some("chunks") => Sort::Chunks,
        Some("repos") => Sort::Repos,
        Some(_) => return Err(Failure::Usage),
    };

    let page = match parsed.value("--page") {
        Some(page) => page
            .parse::<usize>()
            .ok()
            .filter(|page| *page >= 1)
            .ok_or(Failure::Usage)?,
        None => 1,
    };

    let per_page = match parsed.value("--per-page") {
        Some(per_page) => per_page
            .parse::<usize>()
            .ok()
            .filter(|per_page| (1..=LIST_PAGE_MAX).contains(per_page))
            .ok_or(Failure::Usage)?,
        None => LIST_PAGE_DEFAULT,
    };

    Ok(ListQuery {
        filter,
        format,
        text,
        sort,
        descending: parsed.flag("--desc"),
        offset: (page - 1).checked_mul(per_page).ok_or(Failure::Usage)?,
        limit: per_page,
    })
}

/// Most characters of `--query`.
const QUERY_CHARS_MAX: usize = 200;

/// A format name: lowercase letters, digits, `-` and `_`, 1 to 32 characters.
fn parse_format(raw: &str) -> Result<String, Failure> {
    let fine = (1..=32).contains(&raw.len())
        && raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');

    if fine { Ok(raw.to_owned()) } else { Err(Failure::Usage) }
}

/// A document or chunk id: a positive number.
fn parse_id(raw: &str) -> Result<i64, Failure> {
    raw.parse::<i64>().ok().filter(|id| *id >= 1).ok_or(Failure::Usage)
}

/// The id every document command takes as its one positional argument.
fn the_id(parsed: &Args) -> Result<i64, Failure> {
    match parsed.positional.as_slice() {
        [id] => parse_id(id),
        _ => Err(Failure::Usage),
    }
}

fn not_found(id: i64) -> Failure {
    Failure::Failed(format!("document {id} not found"))
}

/// `docs get`.
fn get(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json", "--sections"], &["--chunk"]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let id = the_id(&parsed)?;
    let chunk = match parsed.value("--chunk") {
        Some(chunk) => Some(parse_id(chunk)?),
        None => None,
    };

    let detail = Detail {
        sections: parsed.flag("--sections"),
        chunk,
    };

    let Some(mut store) = context.open(Access::Read)? else {
        return Err(not_found(id));
    };

    let found = store.get(id, &detail)?.ok_or_else(|| not_found(id))?;
    let mut shown = json!({ "document": json::document(&found.doc, &found.links) });

    if let Some(sections) = &found.sections {
        shown["sections"] = sections.iter().map(json::section).collect();
    }

    if let Some(chunk) = &found.chunk {
        shown["chunk"] = json::chunk(chunk);
    }

    Ok(Done::ok(shown))
}

/// `docs links set`.
fn links_set(context: &Context, args: &[Option<&str>], input: &mut dyn Read) -> Reply {
    let parsed = Args::parse(args, &["--json"], &[]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let id = the_id(&parsed)?;
    let links = read_links(input)?;
    let links = clean::links(&links)?;

    let Some(mut store) = context.open(Access::Existing)? else {
        return Err(not_found(id));
    };

    if !store.set_links(id, &links)? {
        return Err(not_found(id));
    }

    Ok(Done::ok(
        json!({ "id": id, "links": links.iter().map(json::link).collect::<Vec<_>>() }),
    ))
}

/// The links in the JSON read from `input`.
fn read_links(input: &mut dyn Read) -> Result<Vec<Link>, Failure> {
    let mut text = String::new();
    let read = input.take(STDIN_MAX + 1).read_to_string(&mut text);

    if read.is_err() || text.len() as u64 > STDIN_MAX {
        return Err(Failure::Failed(format!(
            "the links must be UTF-8 text of at most {} KiB",
            STDIN_MAX / 1024
        )));
    }

    serde_json::from_str(&text).map_err(|_| {
        Failure::Failed(
            "the links must be a JSON array of {\"repo\": \"git:host/owner/repo\", \"globs\": [...]}".to_owned(),
        )
    })
}

/// `docs rename`.
fn rename(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json"], &["--title"]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let id = the_id(&parsed)?;
    let title = clean::title(parsed.value("--title").ok_or(Failure::Usage)?)?;

    let Some(mut store) = context.open(Access::Existing)? else {
        return Err(not_found(id));
    };

    if !store.rename(id, &title)? {
        return Err(not_found(id));
    }

    Ok(Done::ok(json!({ "id": id, "title": title })))
}

/// `docs delete`.
fn delete(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json"], &[]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") || parsed.positional.is_empty() || parsed.positional.len() > DELETE_IDS_MAX {
        return Err(Failure::Usage);
    }

    let mut ids: Vec<i64> = Vec::new();

    for raw in &parsed.positional {
        let id = parse_id(raw)?;

        if !ids.contains(&id) {
            ids.push(id);
        }
    }

    let Some(mut store) = context.open(Access::Existing)? else {
        return Ok(Done::ok(json!({ "deleted": 0 })));
    };

    Ok(Done::ok(json!({ "deleted": store.delete(&ids)? })))
}

#[cfg(test)]
mod tests;
