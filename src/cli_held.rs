//! The review queue commands for Harness Desktop and the person at the terminal: what agents
//! proposed with `knowledge_add` ([`crate::held`]). None is reachable through MCP.
//!
//! - `held list --json [--sort created|addedBy]` prints `{"held": [...]}`: the waiting proposals
//!   (camelCase, as written, without their text, with a `preview` of its start), newest first
//!   (`created`, the default) or by the person they are written for (`addedBy`: names A to Z with
//!   case ignored, proposals with no name last, each group newest first).
//! - `held get --json ID` prints `{"held": {...}}`: one proposal with its whole text.
//! - `held approve --json ID [--expect REVISION] [--place remote|local]` imports the proposal as a document (origin
//!   `agentApproved`, embedded when the embedder runs, else its chunks wait for `docs reindex`) and
//!   removes it, printing `{"approved": {"heldId", "docId", "status", "title", "chunks",
//!   "embedding"}}`. `status` is `added`, `updated` or `unchanged` (the same text was imported
//!   already, and its links now include the proposal's). With `--expect` it approves only the
//!   proposal of that `revision` (what the person saw). `--place` says where the document is saved
//!   (the routing's default for its kind otherwise, see [`crate::config::routing`]) and a place the
//!   routing does not keep that kind in is `refused`. A proposal that replaces a document is saved
//!   where that document is (`local:12` is on this computer, a plain id in the remote database), and
//!   a `--place` that disagrees is `refused`. `docId` is a number, or `local:12` for a
//!   document on this computer when both places are in use.
//!   A proposal that replaces a document is approved only while that document is the one it was
//!   made about (its text hash and key as held), otherwise it is `refused` and nothing is overwritten.
//!   While team approval is on in the remote database a proposal that goes there is stored as a draft
//!   (`status` `added` with `"draft": true`) or, when it replaces a live document, as that document's
//!   pending version (`status` `pending`): nothing an agent can read changes until a teammate approves
//!   it (see [`crate::review`]). `"draft"` is in every approval of a new, updated or pending document.
//! - `held reject --json ID [--expect REVISION] [--comment TEXT]` removes it, printing
//!   `{"rejected": "<heldId>"}` and, when a reason was given, `"comment"` (one line, at most 500
//!   characters).
//! - `held request-changes --json ID [--expect REVISION] --comment TEXT` marks the proposal
//!   `changesRequested` with the comment (required, one line, at most 500 characters) and prints
//!   `{"held": {...}}` like `held get`. It keeps waiting. Every item of `held list|get` has a
//!   `status` (`waiting` or `changesRequested`) and, after a request, `changesRequested`:
//!   `{"comment", "at", "by"}` (`by` is the reviewer's git `user.name`, or null). A later proposal
//!   of the same session that names it, or has its title, replaces it and clears the request
//!   (see [`crate::held::HeldStore::hold_named`]).
//!
//! Approving and rejecting claim the proposal first (see [`crate::held`]), so two decisions about one
//! proposal cannot both be carried out, and an approval that fails puts it back. A refusal prints
//! `{"error": CODE, "message"}` on stdout and the message as the last line of stderr, exit 1:
//! `notFound` (no such proposal, or another decision took it), `changed` (not the expected
//! revision), `commentRequired` (a request for changes without a comment) or `refused` (the import
//! failed, the message says why). Other failures and usage errors as in [`crate::cli`].

use std::path::Path;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{self, Access, Attempt, OpenError};
use crate::cli::{self, Args};
use crate::config::{Backend, Kind, Layout, Place, Routing};
use crate::extract::pdf::PdfPages;
use crate::held::ChangesRequested;
use crate::held::{Claim, HeldItem, HeldStore};
use crate::import::events::Status;
use crate::import::place::write_place;
use crate::import::walk::Candidate;
use crate::import::{Importer, Outcome, embed};
use crate::review;
use crate::store::routed::{REMOTE_DOWN_WRITE, ids};
use crate::store::{Author, Detail, KnowledgeStore, Origin, clean};
use crate::support::{Env, author, time};
use crate::tokens::counter_for;

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "held list --json [--sort created|addedBy] | held get --json ID | held approve --json ID [--expect REVISION] [--place remote|local] | held \
     reject --json ID [--expect REVISION] [--comment TEXT] | held request-changes --json ID [--expect REVISION] --comment TEXT";

/// Characters of the start of a proposal's text that `held list` shows.
const PREVIEW_CHARS: usize = 300;

/// Why a command refused, as its `error` code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    NotFound,
    Changed,
    CommentRequired,
    Refused,
    /// The proposal is linked to a folder of this computer and the team's database takes none.
    LocalPath,
}

impl Code {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "notFound",
            Self::Changed => "changed",
            Self::CommentRequired => "commentRequired",
            Self::Refused => "refused",
            Self::LocalPath => "localPath",
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
    /// The remote database cannot be reached (exit 1, see [`cli::unreachable`]).
    Unreachable(String),
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
        Reply::Unreachable(message) => cli::unreachable(&selection.backend.secrets().scrub(&message)),
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
        [Some("request-changes"), rest @ ..] => request_changes(env, &held, rest),
        _ => Reply::Usage,
    }
}

/// What a decision names: the id, the `--expect` revision and, for an approval, the `--place`, for
/// a rejection or a request for changes the `--comment`.
struct Decision<'a> {
    id: &'a str,
    expect: Option<&'a str>,
    place: Option<Place>,
    comment: Option<&'a str>,
}

/// Which decision is parsed: it decides the options it takes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Way {
    Approve,
    Reject,
    RequestChanges,
}

/// The decision in `args`; `None` for a usage error. `--place` is only for an approval and is
/// `remote` or `local`, `--comment` only for a rejection or a request for changes.
fn decision<'a>(args: &'a [Option<&'a str>], kind: Way) -> Option<Decision<'a>> {
    let valued: &[&str] = match kind {
        Way::Approve => &["--expect", "--place"],
        Way::Reject | Way::RequestChanges => &["--expect", "--comment"],
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
        comment: parsed.value("--comment"),
    })
}

fn list(held: &HeldStore, args: &[Option<&str>]) -> Reply {
    let Some(parsed) = Args::parse(args, &["--json"], &["--sort"]) else {
        return Reply::Usage;
    };

    if !parsed.flag("--json") || !parsed.positional.is_empty() {
        return Reply::Usage;
    }

    let by_author = match parsed.value("--sort") {
        None | Some("created") => false,
        Some("addedBy") => true,
        Some(_) => return Reply::Usage,
    };

    match held.list() {
        Ok(mut items) => {
            if by_author {
                sort_by_author(&mut items);
            }

            Reply::Done(json!({ "held": items.iter().map(summary).collect::<Vec<_>>() }))
        }
        Err(message) => Reply::Failed(message),
    }
}

/// `items` (newest first) ordered by the name they are written for: trimmed and case-folded, a
/// proposal whose name is missing or not fit to show (see [`public`]) after all the others. The sort
/// is stable, so each group stays newest first.
fn sort_by_author(items: &mut [HeldItem]) {
    items.sort_by_cached_key(|item| {
        let name = item
            .added_by
            .as_deref()
            .and_then(author::clean)
            .map(|name| name.to_lowercase());

        (name.is_none(), name)
    });
}

/// `item` as JSON, its `addedBy` cleaned, its `replaces` written as people and clients read ids (`12` or `local:12`),
/// never the internal tagged number, which a JSON client reading doubles would round.
fn public(item: &HeldItem) -> Value {
    let mut value = serde_json::to_value(item).unwrap_or(Value::Null);

    if let (Value::Object(fields), Some(id)) = (&mut value, item.replaces) {
        fields.insert("replaces".to_owned(), crate::store::routed::ids::json(id));
    }

    if let Value::Object(fields) = &mut value {
        fields.insert("status".to_owned(), json!(item.status()));

        // The comment of the request is read at the top level, beside the `changesRequested` it is in.
        if let Some(requested) = &item.changes_requested {
            fields.insert("comment".to_owned(), json!(requested.comment));
        }
    }

    // The held file is plain text anybody with access to the folder may have edited: the name is
    // cleaned like an approval cleans it, and a name that does not pass is not shown.
    if let (Value::Object(fields), Some(name)) = (&mut value, &item.added_by) {
        match author::clean(name) {
            Some(name) => fields.insert("addedBy".to_owned(), json!(name)),
            None => fields.remove("addedBy"),
        };
    }

    value
}

/// `item` without its text and with a preview of it.
fn summary(item: &HeldItem) -> Value {
    let mut value = public(item);

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
        Ok(Some(item)) => Reply::Done(json!({ "held": public(&item) })),
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
    let Some(Decision {
        id, expect, comment, ..
    }) = decision(args, Way::Reject)
    else {
        return Reply::Usage;
    };

    let comment = match comment.map(|text| crate::store::review::event_comment(Some(text))) {
        Some(Ok(comment)) => comment,
        Some(Err(error)) => return Reply::Refused(Code::Refused, error.to_string()),
        None => None,
    };

    match claim(held, id, expect) {
        Ok(claim) => match claim.finish() {
            Ok(()) => {
                let mut rejected = json!({ "rejected": id });

                if let Some(comment) = comment {
                    rejected["comment"] = json!(comment);
                }

                Reply::Done(rejected)
            }
            Err(message) => Reply::Failed(message),
        },
        Err(reply) => reply,
    }
}

/// `held request-changes`: the proposal keeps waiting, marked with what to change.
fn request_changes(env: &Env, held: &HeldStore, args: &[Option<&str>]) -> Reply {
    let Some(Decision {
        id, expect, comment, ..
    }) = decision(args, Way::RequestChanges)
    else {
        return Reply::Usage;
    };

    let Some(comment) = comment else {
        return Reply::Usage;
    };

    let comment = match crate::store::review::event_comment(Some(comment)) {
        Ok(Some(comment)) => comment,
        Ok(None) => {
            return Reply::Refused(Code::CommentRequired, "requesting changes needs a comment".to_owned());
        }
        Err(error) => return Reply::Refused(Code::Refused, error.to_string()),
    };

    let claim = match claim(held, id, expect) {
        Ok(claim) => claim,
        Err(reply) => return reply,
    };

    let mut item = claim.item.clone();
    item.changes_requested = Some(ChangesRequested {
        comment,
        at: time::rfc3339(crate::held::now()),
        by: std::env::current_dir().ok().and_then(|dir| review::actor_of(env, &dir)),
    });
    item.revision = item.digest();

    match claim.update(&item) {
        Ok(()) => Reply::Done(json!({ "held": public(&item) })),
        Err(message) => Reply::Failed(message),
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
        OpenError::Unreachable(_) | OpenError::Busy(_) => Reply::Unreachable(REMOTE_DOWN_WRITE.to_owned()),
    }
}

/// The store of `place`, which an approval is about to change. With both places in use the remote
/// database is opened like every other write to it (see `Context::open_place_to_change` of `docs`):
/// a server that accepts the connection and never speaks is cut off, the cooldown of an earlier
/// failure is recorded and cleared, and one that cannot be reached is refused, never replaced by this
/// computer.
fn open_to_change(
    env: &Env,
    backend: &Backend,
    routing: Routing,
    place: Place,
) -> Result<Option<Box<dyn KnowledgeStore>>, Reply> {
    if place != Place::Remote || routing.layout() != Layout::Both {
        return backend::open_place(env, backend, place, Access::Create).map_err(open_failure);
    }

    match backend::attempt_remote(env, backend, Access::Create, false).map_err(open_failure)? {
        Attempt::Opened(store) => Ok(store),
        Attempt::Offline(_) => Err(Reply::Unreachable(REMOTE_DOWN_WRITE.to_owned())),
    }
}

/// The reply to a team approval that could not be read.
fn review_failure(error: review::Error) -> Reply {
    match error {
        review::Error::Refused(refusal) if refusal.code == review::Code::Unreachable => {
            Reply::Unreachable(REMOTE_DOWN_WRITE.to_owned())
        }
        review::Error::Refused(refusal) => Reply::Refused(Code::Refused, refusal.message),
        review::Error::Store(error) => Reply::Failed(error.to_string()),
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
    let Some(Decision { id, expect, place, .. }) = decision(args, Way::Approve) else {
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

    // The proposal may replace a document, which is in the place its id carries.
    let kind = Kind::of(links.len());
    let place = write_place(routing, place, item.replaces, kind).map_err(&refused)?;

    // Not approved, and it stays held: the user can reject it. A team database takes no folder of
    // this computer.
    if place == Place::Remote
        && let Err(error) = clean::refuse_local_paths(&links)
    {
        return Err(Reply::Refused(Code::LocalPath, error.to_string()));
    }

    // The text goes through a file of the format's extension, the way an imported file does.
    let folder =
        tempfile::tempdir().map_err(|error| Reply::Failed(format!("no temporary folder ({})", error.kind())))?;
    let name = item.import_name();
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

    let mut store = match open_to_change(env, backend, routing, place) {
        Ok(Some(store)) => store,
        Ok(None) => return Err(Reply::Failed("the database could not be created".to_owned())),
        Err(reply) => return Err(reply),
    };

    if let Some(id) = item.replaces {
        check_replaced(store.as_mut(), item, ids::split(id).1)?;
    }

    // With team approval on, a proposal for the team's database is a draft or a pending version.
    let approval = review::approval_in(store.as_mut(), place).map_err(review_failure)?;

    let counter = counter_for(env);
    let never = || false;
    let mut embedder = embed::default_embedder(env);

    let mut importer = Importer::new(store.as_mut(), counter.as_ref(), &NoPdf, &never);
    importer.embedder = embedder
        .as_deref_mut()
        .map(|embedder| embedder as &mut dyn embed::Embedder);
    importer.links = links;
    importer.replaces = item.replaces.map(|id| ids::split(id).1);
    importer.author = author_of(env, item);
    importer.origin = Origin::AgentApproved;
    importer.trust_extracted_key = false;
    importer.title = Some(item.title.clone());
    importer.approval = approval;

    let Outcome::Finished { event, .. } = importer.import(&candidate) else {
        return Err(refused("the import was cancelled".to_owned()));
    };

    match event.status {
        Status::Added | Status::Updated | Status::Pending | Status::Unchanged => Ok(json!({
            "heldId": item.held_id,
            "docId": doc_id(event.doc_id, place, routing),
            "status": event.status.as_str(),
            "draft": event.draft,
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

/// Who wrote an approved proposal: the name read when it was held, else (a proposal held by an
/// older version) the git `user.name` of the folder it was made about, read now. Either way through
/// the harness that proposed it. The held file is plain text anybody with access to the folder may
/// have edited, so the name and the harness are cleaned again here (a name that does not pass is no
/// author, see [`crate::support::author::clean`]).
fn author_of(env: &Env, item: &HeldItem) -> Option<Author> {
    let name = match &item.added_by {
        Some(name) => author::clean(name),
        None => HeldItem::author_name(env, item.project_path.as_deref()),
    };

    name.map(|name| Author {
        name,
        via: author::clean_via(&item.client),
    })
}

/// Refuses the approval unless the document `id` of `store` is still the one the proposal was made
/// about (same text hash and key, see [`HeldItem::replaced`]). A document that was deleted, and
/// another that took its number, is not replaced.
fn check_replaced(store: &mut dyn KnowledgeStore, item: &HeldItem, id: i64) -> Result<(), Reply> {
    let changed = || {
        Reply::Refused(
            Code::Refused,
            "the document this proposal replaces changed or was deleted".to_owned(),
        )
    };

    // A proposal names a live document (agents see no other), so only a live one is replaced. With
    // team approval on, the import then writes or updates that document's pending version.
    let found = store
        .get(id, &Detail::default())
        .map_err(|error| Reply::Failed(error.to_string()))?;

    match (found, &item.replaced) {
        (Some(found), Some(expected))
            if found.doc.content_sha256 == expected.content_sha256 && found.doc.source_key == expected.source_key =>
        {
            Ok(())
        }
        _ => Err(changed()),
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

#[cfg(test)]
mod tests;
