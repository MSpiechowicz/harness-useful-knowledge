//! The review queue commands for Harness Desktop and the person at the terminal: what agents
//! proposed with `knowledge_add` ([`crate::held`]). None is reachable through MCP.
//!
//! - `held list --json` prints `{"held": [...]}`: the waiting proposals (camelCase, as written,
//!   without their text, with a `preview` of its start), newest first.
//! - `held get --json ID` prints `{"held": {...}}`: one proposal with its whole text.
//! - `held approve --json ID [--expect REVISION] [--place remote|local]` imports the proposal as a document (origin
//!   `agentApproved`, embedded when the embedder runs, else its chunks wait for `docs reindex`) and
//!   removes it, printing `{"approved": {"heldId", "docId", "status", "title", "chunks",
//!   "embedding"}}`. `status` is `added`, `updated` or `unchanged` (the same text was imported
//!   already, and its links now include the proposal's). With `--expect` it approves only the
//!   proposal of that `revision` (what the person saw). `--place` says where the document is saved
//!   (the routing's default for its kind otherwise, see [`crate::config::routing`]) and a place the
//!   routing does not keep that kind in is `refused`. `docId` is a number, or `local:12` for a
//!   document on this computer when both places are in use.
//! - `held reject --json ID [--expect REVISION]` removes it, printing `{"rejected": "<heldId>"}`.
//!
//! Approving and rejecting claim the proposal first (see [`crate::held`]), so two decisions about one
//! proposal cannot both be carried out, and an approval that fails puts it back. A refusal prints
//! `{"error": CODE, "message"}` on stdout and the message as the last line of stderr, exit 1:
//! `notFound` (no such proposal, or another decision took it), `changed` (not the expected
//! revision) or `refused` (the import failed, the message says why). Other failures and usage
//! errors as in [`crate::cli`].

use std::path::Path;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{self, Access, OpenError};
use crate::cli::{self, Args};
use crate::config::{Backend, Kind, Layout, Place, Routing};
use crate::extract::pdf::PdfPages;
use crate::held::{Claim, HeldItem, HeldStore};
use crate::import::events::Status;
use crate::import::walk::Candidate;
use crate::import::{Importer, Outcome, embed};
use crate::store::routed::ids;
use crate::store::{Origin, clean};
use crate::support::Env;
use crate::tokens::counter_for;

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "held list --json | held get --json ID | held approve --json ID [--expect REVISION] [--place remote|local] | held \
     reject --json ID [--expect REVISION]";

/// Characters of the start of a proposal's text that `held list` shows.
const PREVIEW_CHARS: usize = 300;

/// Why a command refused, as its `error` code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    NotFound,
    Changed,
    Refused,
}

impl Code {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "notFound",
            Self::Changed => "changed",
            Self::Refused => "refused",
        }
    }
}

/// What a command came to: an object to print, or a refusal.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Done(Value),
    Refused(Code, String),
    Usage,
    /// The command failed (exit 1, see [`cli::failed`]).
    Failed(String),
    /// The database lacks a capability, a `check` problem code (exit 3, see [`cli::missing_capability`]).
    Missing {
        code: &'static str,
        message: String,
    },
}

/// `harness-knowledge held <args>`.
pub fn held(args: &[Option<&str>]) -> ExitCode {
    let Some((env, selection)) = cli::settings() else {
        return ExitCode::FAILURE;
    };

    match reply(&env, &selection.backend, selection.routing, args) {
        Reply::Done(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Reply::Refused(code, message) => {
            let message = selection.backend.secrets().scrub(&message);
            println!("{}", json!({ "error": code.as_str(), "message": message }));
            eprintln!("harness-knowledge: {message}");
            ExitCode::FAILURE
        }
        Reply::Usage => cli::usage(USAGE),
        Reply::Failed(message) => cli::failed(&selection.backend.secrets().scrub(&message)),
        Reply::Missing { code, message } => cli::missing_capability(code, &selection.backend.secrets().scrub(&message)),
    }
}

/// The reply to `held <args>`.
pub fn reply(env: &Env, backend: &Backend, routing: Routing, args: &[Option<&str>]) -> Reply {
    let Some(held) = HeldStore::open(env) else {
        return Reply::Failed("the data folder is unknown".to_owned());
    };

    match args {
        [Some("list"), rest @ ..] => list(&held, rest),
        [Some("get"), rest @ ..] => get(&held, rest),
        [Some("approve"), rest @ ..] => approve(env, backend, routing, &held, rest),
        [Some("reject"), rest @ ..] => reject(&held, rest),
        _ => Reply::Usage,
    }
}

/// What a decision names: the id, the `--expect` revision and, for an approval, the `--place`.
struct Decision<'a> {
    id: &'a str,
    expect: Option<&'a str>,
    place: Option<Place>,
}

/// The decision in `args`; `None` for a usage error. `--place` is only for an approval and is
/// `remote` or `local`.
fn decision<'a>(args: &'a [Option<&'a str>], approval: bool) -> Option<Decision<'a>> {
    let valued: &[&str] = if approval {
        &["--expect", "--place"]
    } else {
        &["--expect"]
    };
    let parsed = Args::parse(args, &["--json"], valued)?;

    if !parsed.flag("--json") || parsed.positional.len() != 1 {
        return None;
    }

    let place = match parsed.value("--place") {
        Some(raw) => Some(Place::parse(raw)?),
        None => None,
    };

    Some(Decision {
        id: parsed.positional[0],
        expect: parsed.value("--expect"),
        place,
    })
}

fn list(held: &HeldStore, args: &[Option<&str>]) -> Reply {
    let Some(parsed) = Args::parse(args, &["--json"], &[]) else {
        return Reply::Usage;
    };

    if !parsed.flag("--json") || !parsed.positional.is_empty() {
        return Reply::Usage;
    }

    match held.list() {
        Ok(items) => Reply::Done(json!({ "held": items.iter().map(summary).collect::<Vec<_>>() })),
        Err(message) => Reply::Failed(message),
    }
}

/// `item` without its text and with a preview of it.
fn summary(item: &HeldItem) -> Value {
    let mut value = serde_json::to_value(item).unwrap_or(Value::Null);

    if let Value::Object(fields) = &mut value {
        fields.remove("content");
        fields.insert(
            "preview".to_owned(),
            json!(item.content.chars().take(PREVIEW_CHARS).collect::<String>()),
        );
    }

    value
}

fn get(held: &HeldStore, args: &[Option<&str>]) -> Reply {
    let Some(parsed) = Args::parse(args, &["--json"], &[]) else {
        return Reply::Usage;
    };

    if !parsed.flag("--json") || parsed.positional.len() != 1 {
        return Reply::Usage;
    }

    match held.get(parsed.positional[0]) {
        Ok(Some(item)) => Reply::Done(json!({ "held": item })),
        Ok(None) => not_found(),
        Err(message) => Reply::Failed(message),
    }
}

fn not_found() -> Reply {
    Reply::Refused(
        Code::NotFound,
        "no such held proposal, or another decision took it".to_owned(),
    )
}

fn reject(held: &HeldStore, args: &[Option<&str>]) -> Reply {
    let Some(Decision { id, expect, .. }) = decision(args, false) else {
        return Reply::Usage;
    };

    match claim(held, id, expect) {
        Ok(claim) => match claim.finish() {
            Ok(()) => Reply::Done(json!({ "rejected": id })),
            Err(message) => Reply::Failed(message),
        },
        Err(reply) => reply,
    }
}

/// The claim of proposal `id`, put back and refused when it is not the revision `expect` names.
fn claim(held: &HeldStore, id: &str, expect: Option<&str>) -> Result<Claim, Reply> {
    let claim = match held.claim(id) {
        Ok(Some(claim)) => claim,
        Ok(None) => return Err(not_found()),
        Err(message) => return Err(Reply::Failed(message)),
    };

    if expect.is_some_and(|revision| revision != claim.item.revision) {
        let _ = claim.restore();

        return Err(Reply::Refused(
            Code::Changed,
            "the held proposal changed since it was shown".to_owned(),
        ));
    }

    Ok(claim)
}

/// The reply to a database that did not open: a missing capability is exit 3, like `docs` and `search`.
fn open_failure(error: OpenError) -> Reply {
    match error {
        OpenError::Missing { code, message } => Reply::Missing { code, message },
        OpenError::Failed(message) => Reply::Failed(message),
    }
}

/// A PDF reader that is never used: a proposal is text.
struct NoPdf;

impl PdfPages for NoPdf {
    fn pages(&self, _path: &Path) -> Result<Vec<String>, String> {
        Err("a proposal is never a PDF".to_owned())
    }
}

fn approve(env: &Env, backend: &Backend, routing: Routing, held: &HeldStore, args: &[Option<&str>]) -> Reply {
    let Some(Decision { id, expect, place }) = decision(args, true) else {
        return Reply::Usage;
    };

    let claim = match claim(held, id, expect) {
        Ok(claim) => claim,
        Err(reply) => return reply,
    };

    match import(env, backend, routing, place, &claim.item) {
        Ok(approved) => match claim.finish() {
            Ok(()) => Reply::Done(json!({ "approved": approved })),
            Err(message) => Reply::Failed(message),
        },
        Err(reply) => {
            // Not approved: it waits for another try.
            let _ = claim.restore();
            reply
        }
    }
}

/// Imports `item` as a document and describes the result.
fn import(
    env: &Env,
    backend: &Backend,
    routing: Routing,
    place: Option<Place>,
    item: &HeldItem,
) -> Result<Value, Reply> {
    let refused = |message: String| Reply::Refused(Code::Refused, message);

    let links = clean::links(&item.links).map_err(|error| refused(error.to_string()))?;

    let kind = Kind::of(links.len());
    let place = place.unwrap_or_else(|| routing.default_place(kind));
    routing.check_write(kind, place).map_err(&refused)?;

    // The text goes through a file of the format's extension, the way an imported file does.
    let folder =
        tempfile::tempdir().map_err(|error| Reply::Failed(format!("no temporary folder ({})", error.kind())))?;
    let name = file_name(item);
    let path = folder.path().join(&name);
    std::fs::write(&path, item.content.as_bytes())
        .map_err(|error| Reply::Failed(format!("writing the text failed ({})", error.kind())))?;

    let candidate = Candidate {
        path,
        display: item.name.clone(),
        name: name.clone(),
        source_key: format!("agent/{}/{name}", item.held_id),
        problem: None,
    };

    let mut store = match backend::open_place(env, backend, place, Access::Create) {
        Ok(Some(store)) => store,
        Ok(None) => return Err(Reply::Failed("the database could not be created".to_owned())),
        Err(error) => return Err(open_failure(error)),
    };

    let counter = counter_for(env);
    let never = || false;
    let mut embedder = embed::default_embedder(env);

    let mut importer = Importer::new(store.as_mut(), counter.as_ref(), &NoPdf, &never);
    importer.embedder = embedder
        .as_deref_mut()
        .map(|embedder| embedder as &mut dyn embed::Embedder);
    importer.links = links;
    importer.replaces = item.replaces;
    importer.origin = Origin::AgentApproved;
    importer.trust_extracted_key = false;
    importer.title = Some(item.title.clone());

    let Outcome::Finished { event, .. } = importer.import(&candidate) else {
        return Err(refused("the import was cancelled".to_owned()));
    };

    match event.status {
        Status::Added | Status::Updated | Status::Unchanged => Ok(json!({
            "heldId": item.held_id,
            "docId": doc_id(event.doc_id, place, routing),
            "status": event.status.as_str(),
            "title": item.title,
            "chunks": event.chunks,
            "embedding": event.embedding,
        })),
        Status::Skipped | Status::Failed => Err(refused(
            event
                .reason
                .unwrap_or_else(|| "the document could not be imported".to_owned()),
        )),
    }
}

/// The id of the imported document as written: tagged with its place when both are in use.
fn doc_id(id: Option<i64>, place: Place, routing: Routing) -> Value {
    match id {
        Some(id) if routing.layout() == Layout::Both => ids::json(ids::tag(place, id)),
        Some(id) => json!(id),
        None => Value::Null,
    }
}

/// The name the text is imported under: the proposal's name with the extension of its format.
fn file_name(item: &HeldItem) -> String {
    let extension = match item.format.as_str() {
        "text" => "txt",
        "html" => "html",
        "asciidoc" => "adoc",
        "rst" => "rst",
        "openapi" => "yaml",
        _ => "md",
    };

    let stem = Path::new(&item.name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "document".to_owned());

    format!("{stem}.{extension}")
}

#[cfg(test)]
mod tests;
