//! The commands that read and manage the documents, for Harness Desktop and the person at the
//! terminal, never for an agent's MCP calls. Every one of them takes `--json` and prints one JSON
//! object on stdout. Timestamps are RFC 3339, field names camelCase.
//!
//! - `docs list --json [--repo KEY | --global | --all] [--format F] [--query Q] [--useful
//!   any|ever|never] [--status all|live|draft|waiting|changesRequested|pending] [--sort title|updated|size|format|chunks|paths|repos|useful|lastUseful] [--desc] [--page N] [--per-page N] [--place remote|local]`
//!   prints `{"items": [...], "total": N}`:
//!   one page of the documents (see [`json::document`] for an item: its `links`, `addedBy` and
//!   `updatedBy`, and the counters `shown`, `useful` and `lastUsefulAt`, which are absent while the
//!   database has no counter columns). `--useful ever` keeps the documents an agent found useful
//!   at least once, `never` those shown at least 3 times and found useful by none, `any` (the default) all.
//!   On a database without the counter columns `ever` and `never` keep none, `any` all, and the sorts
//!   by `useful` and `lastUseful` (a document never rated counts 0) are the default order. `--repo`
//!   keeps the documents linked to that repository key, `--global` those linked to none, `--all`
//!   (the default) every one. `--query` keeps the documents whose title contains it or whose text has
//!   all its words. Sorted by `updated` unless `--sort` says otherwise, ascending unless `--desc`,
//!   ties by id (a title sorts with ASCII letters folded to lower case and the rest by bytes, the
//!   same in every place, so merged pages equal each place's own order). Pages count from 1, `--per-page` is 1 to 200 (50 when not given). `total` counts
//!   every page. `--place remote|local` keeps the documents kept in that place, paged and counted
//!   over that set (only with both places in use does it tell two sets apart: with one place in use
//!   the place that is not in use lists nothing, and a value other than those two is a usage error).
//!   `--place remote` while the remote cannot be reached exits 1 like `docs get` of a remote id.
//!   `offline` is `true` (and absent otherwise) while the remote database cannot be
//!   reached and the page comes from this computer alone (see [`crate::store::routed`]).
//!   `--status` keeps the documents by their state, `all` (the default) every row that is not a
//!   pending version, drafts included: `live` the live documents, `draft`, `waiting` and
//!   `changesRequested` those rows and the live documents whose pending version is in that state,
//!   `pending` the live documents that have a pending version. An item has its `status` and
//!   `revision`, and a live one `pending`: `null` or `{"id", "status", "revision", "updatedBy",
//!   "updatedAt"}`. A row that is waiting for approval has `authors` (as `docs get` prints it), and
//!   so does the `pending` object of a live row whose pending version is waiting: the people the
//!   `selfApproval` rule counts, left out when the history cannot be read, at most 20 names (for display: the rule itself reads every name). No other row has the key.
//! - `docs get --json ID [--sections] [--chunk CID]` prints `{"document": {... "links"}}` (the item
//!   of `docs list`) and, as
//!   asked, `"sections"` (the outline: every chunk's id, position, heading path and size) and
//!   `"chunk"` (that chunk of the document, with its text). A draft and a pending version can be
//!   opened (agents never see them): a pending version has `pendingOf`, the id of its live
//!   document, and a row that is not live `lastChange` and `lastRequest`: `null` or `{"by", "at",
//!   "comment"}` of the latest time a reviewer asked for changes and the author asked for approval.
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
//! - `docs ask|request-changes|approve|go-live|take-back --json ID --expect REV [--comment TEXT]`,
//!   `docs review --json ID --expect REV --verdict comment|approve|requestChanges [--body TEXT]
//!   [--comments FILE]`, `docs history|comments|compare|download`, `docs comment
//!   reply|resolve|unresolve|delete` and, outside `docs`, `approval get|set` and `upgrade` are the review of
//!   drafts and team approval (see [`review`], [`comments`], [`history`], [`compare`], [`download`],
//!   [`approval`], [`upgrade`]). A refusal is `{"error": CODE, "message"}` with the codes of
//!   [`crate::review::Code`], exit 1.
//! - `docs rename --json ID --title TITLE` (1 to 300 characters) prints `{"id", "title"}`.
//! - `docs delete --json ID...` deletes the documents, their chunks and index entries, and prints
//!   `{"deleted": N}` (ids that do not exist count nothing).
//! - `docs import --ndjson` reads a manifest from stdin and prints one JSON event per line while it
//!   imports, `docs reindex --json (ID... | --all)` embeds the chunks that have no vector (see
//!   [`import`]).
//! - `docs similar --json [--repo KEY | --global] PATH...` prints `{"similar": [{"path", "match"}]}`: for
//!   each file (a folder gives its files) the stored document it is probably a version of, or `null`
//!   (see [`similar`]). Read only.
//! - `repos --json` prints `{"global": N, "repos": [{"key", "docs"}]}` (and `"offline": true`
//!   like `docs list`), `stats --json` the counts of [`overview::stats`] (and `remoteWritable`),
//!   `project-key --json --path PATH` the repository key of a folder, and
//!   `check --json` what the database can do (see [`check`]).
//!
//! A read never creates the local database: without it every list is empty and exit 0. With both
//! places in use, a read of a document of the remote database (`docs get`, `docs matches`, or any
//! change) while it cannot be reached exits 1 with `harness-knowledge: remote knowledge is
//! unreachable right now`, and its stdout is `{"error": "unreachable", "message": ...}` (the
//! code `unreachable` also tells any command that fails because the remote database cannot be
//! reached, an import or a reindex to it included, from `failed`). A document that does not exist exits 1 with `harness-knowledge: document N not found` as the last stderr
//! line. Other exit codes and errors as in [`crate::cli`].

mod approval;
mod check;
mod comments;
mod compare;
mod diff;
mod download;
mod history;
mod import;
mod json;
mod listing;
mod matches;
mod overview;
mod quote;
mod replacing;
mod review;
mod similar;
mod upgrade;

use std::io::Read;
use std::process::ExitCode;

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::backend::{self, Access, Attempt, OpenError};
use crate::cli::{self, Args};
use crate::config::{Backend, Layout, Place, Routing};
use crate::import::document::now;
use crate::limits::{DELETE_IDS_MAX, STDIN_MAX};
use crate::store::review::AUTHORS_FETCH_MAX;
use crate::store::routed::{REMOTE_DOWN_WRITE, ids};
use crate::store::{Detail, KnowledgeStore, Link, ListQuery, Status, StoreError, clean};
use crate::support::Env;

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "docs list --json [--repo KEY | --global | --all] [--format F] [--query Q] \
                         [--useful any|ever|never] [--status all|live|draft|waiting|changesRequested|pending] [--sort title|updated|size|format|chunks|paths|repos|useful|lastUseful] [--desc] [--page N] [--per-page N] [--place remote|local] | docs get --json ID \
                         [--sections] [--chunk CID] | docs matches --json ID --query Q [--limit N] | docs links set --json ID < links.json | docs rename --json ID \
                         --title TITLE | docs delete --json ID... | docs import --ndjson < manifest.json | docs reindex \
                         --json (ID... | --all) | docs similar --json [--repo KEY | --global] PATH... | \
                         docs ask|request-changes|approve|go-live|take-back --json ID --expect REV [--comment TEXT] | docs review --json ID \
                         --expect REV --verdict comment|approve|requestChanges [--body TEXT] [--comments FILE] | docs history --json ID | \
                         docs comments --json ID | docs comment reply --json ID COMMENT_ID --body TEXT | docs comment resolve|unresolve|delete \
                         --json ID COMMENT_ID | docs compare --json ID [--since REV] | docs download --json ID --to PATH [--live] | \
                         approval get --json | approval set --json on|off | upgrade --json | \
                         repos --json | stats --json | project-key --json \
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
            Attempt::Offline(_) => Err(Failure::Unreachable(REMOTE_DOWN_WRITE.to_owned())),
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
    /// The remote database cannot be reached: exit 1, with the error code `unreachable`.
    Unreachable(String),
    /// The database lacks something (a `check` problem code): exit 3.
    Missing { code: &'static str, message: String },
    /// A review command was refused: exit 1, printed as `{"error": code, "message"}` (`code` is one
    /// of [`crate::review::Code::as_str`] or `sinceUnavailable`).
    Refused { code: &'static str, message: String },
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
            StoreError::LocalPath(message) => Self::Refused {
                code: crate::review::Code::LocalPath.as_str(),
                message,
            },
            other => Self::Failed(upgrade::worded(other.to_string())),
        }
    }
}

impl From<OpenError> for Failure {
    fn from(error: OpenError) -> Self {
        match error {
            OpenError::Missing { code, message } => Self::Missing { code, message },
            OpenError::Failed(message) => Self::Failed(upgrade::worded(message)),
            OpenError::Unreachable(message) | OpenError::Busy(message) => Self::Unreachable(message),
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

/// The reply of `approval <args>`.
pub fn approval_reply(context: &Context, args: &[Option<&str>]) -> Reply {
    approval::approval(context, args)
}

/// The reply of `upgrade <args>`.
pub fn upgrade_reply(context: &Context, args: &[Option<&str>]) -> Reply {
    upgrade::upgrade(context, args)
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

/// `harness-knowledge approval <args>`.
pub fn approval(args: &[Option<&str>]) -> ExitCode {
    run(args, |context, args, _| approval::approval(context, args))
}

/// `harness-knowledge upgrade <args>`.
pub fn upgrade(args: &[Option<&str>]) -> ExitCode {
    run(args, |context, args, _| upgrade::upgrade(context, args))
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
        Err(Failure::Unreachable(message)) => cli::unreachable(&message),
        Err(Failure::Missing { code, message }) => cli::missing_capability(code, &message),
        Err(Failure::Refused { code, message }) => cli::refused(code, &message),
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
        [Some("similar"), rest @ ..] => similar::similar(context, rest),
        [
            Some(verb @ ("ask" | "request-changes" | "approve" | "go-live" | "take-back")),
            rest @ ..,
        ] => review::transition(context, verb, rest),
        [Some("review"), rest @ ..] => review::submit(context, rest),
        [Some("history"), rest @ ..] => history::history(context, rest),
        [Some("comments"), rest @ ..] => comments::comments(context, rest),
        [
            Some("comment"),
            Some(verb @ ("reply" | "resolve" | "unresolve" | "delete")),
            rest @ ..,
        ] => comments::comment(context, verb, rest),
        [Some("compare"), rest @ ..] => compare::compare(context, rest),
        [Some("download"), rest @ ..] => download::download(context, rest),
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

    let mut shown = list_on(store.as_mut(), place, &query)?;

    if store.offline() {
        shown["offline"] = json!(true);
    }

    Ok(Done::ok(shown))
}

/// One page of `docs list` from `store`: `{"items", "total"}`.
pub(crate) fn list_on(
    store: &mut dyn KnowledgeStore,
    place: Option<Place>,
    query: &ListQuery,
) -> Result<Value, Failure> {
    let page = store.list_in(place, query)?;

    // The authors are read only where a reviewer needs them (a row waiting for approval, a pending
    // version that waits), all together: a small constant number of reads however long the page is.
    // A failure drops the authors of the page, never the list.
    let waiting: Vec<i64> = page
        .items
        .iter()
        .flat_map(|row| {
            let own = (row.doc.status == Status::Waiting).then_some(row.doc.id);
            let pending = row
                .pending
                .as_ref()
                .filter(|pending| pending.status == Status::Waiting)
                .map(|pending| pending.id);

            own.into_iter().chain(pending)
        })
        .collect();
    let people = if waiting.is_empty() {
        HashMap::new()
    } else {
        store.authorship_of(&waiting, AUTHORS_FETCH_MAX).unwrap_or_default()
    };
    let listed = |shown: &mut Value, id: i64, updated_by: Option<&crate::store::Author>| {
        if let Some(authorship) = people.get(&id) {
            let updated_by = updated_by.map(|author| author.name.as_str());

            shown["authors"] = json!(crate::review::people_listed(updated_by, authorship));
        }
    };

    let mut items: Vec<Value> = Vec::with_capacity(page.items.len());

    for row in &page.items {
        let mut shown = json::document(&row.doc, &row.links, row.usage);
        json::with_pending(&mut shown, &row.doc, row.pending.as_ref());

        if row.doc.status == Status::Waiting {
            listed(&mut shown, row.doc.id, row.doc.updated_by.as_ref());
        }

        if let Some(pending) = row.pending.as_ref().filter(|pending| pending.status == Status::Waiting)
            && shown["pending"].is_object()
        {
            listed(&mut shown["pending"], pending.id, pending.updated_by.as_ref());
        }

        items.push(shown);
    }

    Ok(json!({ "items": items, "total": page.total }))
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
        // The person's own read: a draft and a pending version are theirs to open.
        drafts: true,
    };

    let Some(mut store) = context.open(Access::Read)? else {
        return Err(not_found(id));
    };

    let found = store.get(id, &detail)?.ok_or_else(|| not_found(id))?;
    let mut document = json::document(&found.doc, &found.links, found.usage);
    json::with_pending(&mut document, &found.doc, found.pending.as_ref());

    if !found.doc.status.is_live() {
        // The words are a courtesy: a history that cannot be read leaves them out.
        let history = store.history(found.doc.id).ok().flatten();
        json::with_last_events(&mut document, &found.doc, history.as_ref());
        with_authors(
            store.as_mut(),
            &mut document,
            found.doc.id,
            found.doc.updated_by.as_ref(),
        );
    }

    if let Some(pending) = found.pending.as_ref().filter(|_| document["pending"].is_object()) {
        with_authors(
            store.as_mut(),
            &mut document["pending"],
            pending.id,
            pending.updated_by.as_ref(),
        );
    }

    let mut shown = json!({ "document": document });

    if let Some(sections) = &found.sections {
        shown["sections"] = sections.iter().map(json::section).collect();
    }

    if let Some(chunk) = &found.chunk {
        shown["chunk"] = json::chunk(chunk);
    }

    Ok(Done::ok(shown))
}

/// `authors` of row `id` (a row that is not live, or the pending version of a live one) under
/// `shown`: the people its approval and take-back rules count (see [`crate::review::people_of`]).
/// A history that cannot be read leaves them out, and the reader then does not know them.
fn with_authors(store: &mut dyn KnowledgeStore, shown: &mut Value, id: i64, updated_by: Option<&crate::store::Author>) {
    let updated_by = updated_by.map(|author| author.name.as_str());

    if let Ok(people) = crate::review::people_of(store, id, updated_by) {
        shown["authors"] = json!(people);
    }
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

    // A team document is never linked to a folder of this computer.
    import::for_place(crate::import::place::place_in_use(context.routing, id), &links)?;

    let Some(mut store) = context.open(Access::Existing)? else {
        return Err(not_found(id));
    };

    // A pending version has no links of its own: the live document's win.
    match crate::review::check_links_target(store.as_mut(), id) {
        Ok(()) => {}
        Err(crate::review::Error::Refused(refusal)) if refusal.code == crate::review::Code::NotFound => {
            return Err(not_found(id));
        }
        Err(error) => return Err(review::failure(error)),
    }

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

    let by = review::actor(context);

    if !store.rename(id, &title, by.as_deref(), now())? {
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
mod drafts_tests;
#[cfg(test)]
mod offline_tests;
#[cfg(test)]
mod quality_tests;
#[cfg(test)]
pub(crate) mod review_tests;
#[cfg(test)]
mod routing_tests;
#[cfg(test)]
mod similar_tests;
#[cfg(test)]
mod tests;
