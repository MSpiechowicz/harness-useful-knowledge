//! The commands that read and manage the documents, for Harness Desktop and the person at the
//! terminal, never for an agent's MCP calls. Every one of them takes `--json` and prints one JSON
//! object on stdout. Timestamps are RFC 3339, field names camelCase.
//!
//! - `docs list --json [--repo KEY | --global | --all] [--format F] [--query Q] [--sort
//!   title|updated|size|format|chunks|repos] [--desc] [--page N] [--per-page N] [--place remote|local]` prints `{"items": [...], "total": N}`:
//!   one page of the documents (see [`json::document`] for an item, it has its `links`). `--repo`
//!   keeps the documents linked to that repository key, `--global` those linked to none, `--all`
//!   (the default) every one. `--query` keeps the documents whose title contains it or whose text has
//!   all its words. Sorted by `updated` unless `--sort` says otherwise, ascending unless `--desc`,
//!   ties by id. Pages count from 1, `--per-page` is 1 to 200 (50 when not given). `total` counts
//!   every page. `--place remote|local` keeps the documents kept in that place, paged and counted
//!   over that set (only with both places in use does it tell two sets apart: with one place in use
//!   the place that is not in use lists nothing, and a value other than those two is a usage error).
//!   `--place remote` while the remote cannot be reached exits 1 like `docs get` of a remote id.
//!   `offline` is `true` (and absent otherwise) while the remote database cannot be
//!   reached and the page comes from this computer alone (see [`crate::store::routed`]).
//! - `docs get --json ID [--sections] [--chunk CID]` prints `{"document": {... "links"}}` and, as
//!   asked, `"sections"` (the outline: every chunk's id, position, heading path and size) and
//!   `"chunk"` (that chunk of the document, with its text).
//! - `docs matches --json ID --query Q [--limit N]` prints `{"matches": [{"chunkId", "ordinal",
//!   "section", "snippet"}], "total": N}`: the chunks of one document whose heading path or text
//!   hold the words of `Q` next to each other in that order, the last as a prefix (the filter
//!   `docs list --query` applies to the text of documents, see [`crate::store::ListQuery::text`]), by
//!   position, at most `--limit` (1 to 50, 20 when not given). `total` counts every match. A
//!   `snippet` is at most 240 characters of the chunk around the first word found, as it is
//!   written (untrusted text). Read only: it never creates the database.
//! - `docs links set --json ID` reads `[{"repo": "git:host/owner/repo", "globs": ["src/**"]}]` from
//!   stdin (at most 32 links with 32 globs of 256 characters, see [`crate::store::clean`]) and
//!   replaces the document's links; prints `{"id", "links"}`. An empty array makes it global.
//! - `docs rename --json ID --title TITLE` (1 to 300 characters) prints `{"id", "title"}`.
//! - `docs delete --json ID...` deletes the documents, their chunks and index entries, and prints
//!   `{"deleted": N}` (ids that do not exist count nothing).
//! - `docs import --ndjson` reads a manifest from stdin and prints one JSON event per line while it
//!   imports, `docs reindex --json (ID... | --all)` embeds the chunks that have no vector (see
//!   [`import`]).
//! - `repos --json` prints `{"global": N, "repos": [{"key", "docs"}]}` (and `"offline": true`
//!   like `docs list`), `stats --json` the counts of [`overview::stats`], `project-key --json --path PATH` the repository key of a folder, and
//!   `check --json` what the database can do (see [`check`]).
//!
//! A read never creates the local database: without it every list is empty and exit 0. With both
//! places in use, a read of a document of the remote database (`docs get`, `docs matches`, or any
//! change) while it cannot be reached exits 1 with `harness-knowledge: remote knowledge is
//! unreachable right now`. A document that does not exist exits 1 with `harness-knowledge: document N not found` as the last stderr
//! line. Other exit codes and errors as in [`crate::cli`].

mod check;
mod import;
mod json;
mod listing;
mod matches;
mod overview;

use std::io::Read;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{self, Access, Attempt, OpenError};
use crate::cli::{self, Args};
use crate::config::{Backend, Layout, Place, Routing};
use crate::limits::{DELETE_IDS_MAX, STDIN_MAX};
use crate::store::routed::{REMOTE_DOWN_WRITE, ids};
use crate::store::{Detail, KnowledgeStore, Link, StoreError, clean};
use crate::support::Env;

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "docs list --json [--repo KEY | --global | --all] [--format F] [--query Q] \
                         [--sort title|updated|size|format|chunks|repos] [--desc] [--page N] [--per-page N] [--place remote|local] | docs get --json ID \
                         [--sections] [--chunk CID] | docs matches --json ID --query Q [--limit N] | docs links set --json ID < links.json | docs rename --json ID \
                         --title TITLE | docs delete --json ID... | docs import --ndjson < manifest.json | docs reindex \
                         --json (ID... | --all) | repos --json | stats --json | project-key --json \
                         --path PATH | check --json";

/// Where a command runs: the environment and the database it reads.
pub struct Context<'a> {
    pub env: &'a Env,
    pub backend: &'a Backend,
    pub routing: Routing,
}

impl<'a> Context<'a> {
    pub fn new(env: &'a Env, backend: &'a Backend, routing: Routing) -> Self {
        Self { env, backend, routing }
    }

    /// The store of the routing: one place as it is, or both routed.
    fn open(&self, access: Access) -> Result<Option<Box<dyn KnowledgeStore>>, Failure> {
        Ok(backend::open(self.env, self.backend, self.routing, access)?)
    }

    /// The store of one place alone, for what is done to a place (an import, a reindex, a check).
    fn open_place(&self, place: Place, access: Access) -> Result<Option<Box<dyn KnowledgeStore>>, Failure> {
        Ok(backend::open_place(self.env, self.backend, place, access)?)
    }

    /// The store of one place that is about to change. With both places in use, a remote database
    /// that cannot be reached is refused ([`REMOTE_DOWN_WRITE`]) and never replaced by this
    /// computer. It is always tried, cooldown or not, so a person is not refused for a minute after
    /// the remote came back.
    fn open_place_to_change(&self, place: Place, access: Access) -> Result<Option<Box<dyn KnowledgeStore>>, Failure> {
        if place != Place::Remote || self.routing.layout() != Layout::Both {
            return self.open_place(place, access);
        }

        match backend::attempt_remote(self.env, self.backend, access, false)? {
            Attempt::Opened(store) => Ok(store),
            Attempt::Offline(_) => Err(Failure::Failed(REMOTE_DOWN_WRITE.to_owned())),
        }
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
            OpenError::Failed(message) | OpenError::Unreachable(message) => Self::Failed(message),
        }
    }
}

pub type Reply = Result<Done, Failure>;

/// `harness-knowledge docs <args>`.
pub fn docs(args: &[Option<&str>]) -> ExitCode {
    run(args, docs_reply)
}

/// The reply of `stats <args>`.
pub fn stats_reply(context: &Context, args: &[Option<&str>]) -> Reply {
    overview::stats(context, args)
}

/// The reply of `check <args>`.
pub fn check_reply(context: &Context, args: &[Option<&str>]) -> Reply {
    check::check(context, args)
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

    let context = Context::new(&env, &selection.backend, selection.routing);

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
        [Some("matches"), rest @ ..] => matches::matches(context, rest),
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
    let (query, place) = listing::parse_list(args)?;

    // Without a database every list is empty, and nothing is created. So is the list of a place
    // that is not in use.
    let wanted = place.is_none_or(|place| context.routing.uses(place));
    let store = if wanted { context.open(Access::Read)? } else { None };

    let Some(mut store) = store else {
        return Ok(Done::ok(json!({ "items": [], "total": 0 })));
    };

    let page = store.list_in(place, &query)?;
    let items: Vec<Value> = page
        .items
        .iter()
        .map(|row| json::document(&row.doc, &row.links))
        .collect();
    let mut shown = json!({ "items": items, "total": page.total });

    if store.offline() {
        shown["offline"] = json!(true);
    }

    Ok(Done::ok(shown))
}

/// A document or chunk id as written: `12`, or `local:12` for one on this computer (see
/// [`crate::store::routed::ids`]).
fn parse_id(raw: &str) -> Result<i64, Failure> {
    ids::parse(raw).ok_or(Failure::Usage)
}

/// The id every document command takes as its one positional argument.
fn the_id(parsed: &Args) -> Result<i64, Failure> {
    match parsed.positional.as_slice() {
        [id] => parse_id(id),
        _ => Err(Failure::Usage),
    }
}

fn not_found(id: i64) -> Failure {
    Failure::Failed(format!("document {} not found", ids::show(id)))
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
        json!({ "id": ids::json(id), "links": links.iter().map(json::link).collect::<Vec<_>>() }),
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

    Ok(Done::ok(json!({ "id": ids::json(id), "title": title })))
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
mod offline_tests;
#[cfg(test)]
mod routing_tests;
#[cfg(test)]
mod tests;
