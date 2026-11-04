//! The review queue: what an agent proposed with `knowledge_add`, waiting for the person to approve
//! or reject it in Harness Desktop (or with `harness-knowledge held`, see [`crate::cli_held`]).
//!
//! Each proposal is one file `held/<heldId>.json` next to the database (`0600` in a `0700` folder of
//! this user, written to a temporary file and renamed, read without following a symbolic link, see
//! [`crate::support::private`]). The queue belongs to the computer, never to a team database, and a
//! proposal is in no store, search or list until it is approved. The text is snapshotted when the
//! agent proposes it: a proposal made from a file holds that file's text of that moment.
//!
//! - At most [`HELD_MAX_ITEMS`] proposals and [`HELD_TOTAL_BYTES`] bytes wait: one more is refused,
//!   none is dropped. A file is at most [`HELD_FILE_MAX`] bytes.
//! - Proposing the same text again, with the same title, links and replacement, finds the proposal
//!   already waiting and returns its id.
//! - A reviewer may ask for changes ([`ChangesRequested`], with a comment): the proposal waits on, marked, and
//!   a later proposal of the same session that names it ([`HeldStore::hold_named`]) or has its title
//!   replaces it in place, keeping its id, so the review goes on where it stopped.
//! - Approving or rejecting claims the proposal first ([`HeldStore::claim`]): its file is renamed to
//!   a claim name in the same folder, so two decisions about one proposal cannot both be carried out.
//! - Proposals and half-written files older than 30 days are swept.
//! - A file that cannot be read, of another version, whose `heldId` is not its name or whose
//!   `revision` does not match its content is skipped.

mod ids;
mod store;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub use ids::{is_held_id, random_id};
pub use store::{Claim, HeldStore, HoldError};

use crate::store::Link;
use crate::support::{Env, author};

/// The folder next to the database that holds the proposals.
pub const HELD_FOLDER: &str = "held";

/// The most proposals that may wait.
pub const HELD_MAX_ITEMS: usize = 100;

/// The largest proposal file, in bytes.
pub const HELD_FILE_MAX: usize = 1024 * 1024;

/// The most bytes all waiting proposals may take together.
pub const HELD_TOTAL_BYTES: u64 = 32 * 1024 * 1024;

/// The most characters of the text of a proposal.
pub const CONTENT_MAX: usize = 256 * 1024;

/// The version of the proposal file format.
const VERSION: u32 = 1;

/// One `held/<heldId>.json` file (camelCase, version 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeldItem {
    pub version: u32,
    pub held_id: String,
    /// RFC 3339.
    pub created_at: String,
    /// The harness: the name the MCP client gave at `initialize`, `unknown` without one.
    pub client: String,
    /// `HARNESS_KNOWLEDGE_SESSION` of the server that took the proposal, when set.
    pub session: Option<String>,
    /// The folder the call was about: its `project_path`, else the server's working folder.
    pub project_path: Option<String>,
    /// The repository key of that folder, when it is in one.
    pub repo_key: Option<String>,
    pub title: String,
    /// `markdown`, `text`, `html`, `asciidoc`, `rst` or `openapi`.
    pub format: String,
    /// The file name the document is imported under (`install-notes.md`).
    pub name: String,
    /// The file the text was read from, relative to the project, when the agent named a path.
    pub source_path: Option<String>,
    /// The text, as it was when the agent proposed it.
    pub content: String,
    pub bytes: usize,
    /// SHA-256 of the text.
    pub content_sha256: String,
    /// The repositories the document links to when it is approved.
    pub links: Vec<Link>,
    /// The document the approval replaces in place.
    pub replaces: Option<i64>,
    /// That document as it was when the proposal was held: an approval replaces it only while it is
    /// still this one (a document that was deleted and a new one that took its number is not).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced: Option<Replaced>,
    /// The git `user.name` of the person the proposal is written for, read when it is held (absent
    /// when none was known, and in proposals held by an older version). The approval writes it as
    /// the document's author, with [`Self::client`] as the harness. It is not part of the
    /// [`Self::digest`], so a file with or without it has the revision it always had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_by: Option<String>,
    /// What a reviewer asked to change, once one did. Part of the [`Self::digest`], so a decision
    /// made on the proposal as it was shown is refused after the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_requested: Option<ChangesRequested>,
    /// The [`Self::digest`] of the content, set when it is held.
    pub revision: String,
}

/// A reviewer asked for changes to a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangesRequested {
    /// What to change, one line of at most 500 characters.
    pub comment: String,
    /// RFC 3339.
    pub at: String,
    /// The git `user.name` of the reviewer, when one was known.
    pub by: Option<String>,
}

/// What identifies the document a proposal replaces: its text and the key it was imported under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Replaced {
    pub content_sha256: String,
    pub source_key: String,
}

/// Who proposed, and from where.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Source {
    pub client: String,
    pub session: Option<String>,
    pub project_path: Option<String>,
    pub repo_key: Option<String>,
}

/// What is proposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub title: String,
    pub format: String,
    pub name: String,
    pub source_path: Option<String>,
    pub content: String,
    pub links: Vec<Link>,
    pub replaces: Option<i64>,
    /// The document `replaces` names, as it is now.
    pub replaced: Option<Replaced>,
}

impl HeldItem {
    /// A proposal ready to be held (its id, time and revision are set when it is).
    pub fn new(source: Source, proposal: Proposal) -> Self {
        Self {
            version: VERSION,
            held_id: String::new(),
            created_at: String::new(),
            client: source.client,
            session: source.session,
            project_path: source.project_path,
            repo_key: source.repo_key,
            title: proposal.title,
            format: proposal.format,
            name: proposal.name,
            source_path: proposal.source_path,
            bytes: proposal.content.len(),
            content_sha256: crate::digest::sha256_hex(proposal.content.as_bytes()),
            content: proposal.content,
            links: proposal.links,
            replaces: proposal.replaces,
            replaced: proposal.replaced,
            added_by: None,
            changes_requested: None,
            revision: String::new(),
        }
    }

    /// The state the proposal is in, as `held list` shows it: `waiting` for the person's decision,
    /// or `changesRequested` while the agent is asked to rework it.
    pub fn status(&self) -> &'static str {
        if self.changes_requested.is_some() {
            "changesRequested"
        } else {
            "waiting"
        }
    }

    /// The lowercase hex SHA-256 of the item's canonical JSON (keys sorted, no blanks) without its
    /// `revision` and `addedBy`.
    pub fn digest(&self) -> String {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);

        if let Value::Object(fields) = &mut value {
            fields.remove("revision");
            fields.remove("addedBy");
        }

        let text = serde_json::to_vec(&sorted(value)).unwrap_or_default();

        hex(&Sha256::digest(&text))
    }

    /// The name the text is imported under: the proposal's name with the extension of its format.
    pub fn import_name(&self) -> String {
        let extension = match self.format.as_str() {
            "text" => "txt",
            "html" => "html",
            "asciidoc" => "adoc",
            "rst" => "rst",
            "openapi" => "yaml",
            _ => "md",
        };

        let stem = Path::new(&self.name)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "document".to_owned());

        format!("{stem}.{extension}")
    }

    /// The git `user.name` for a proposal about `project_path`: read in that folder when it is one,
    /// else in the current folder.
    pub fn author_name(env: &Env, project_path: Option<&str>) -> Option<String> {
        let dir = project_path
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
            .or_else(|| std::env::current_dir().ok())?;

        author::resolve(env, &dir)
    }

    /// True when this proposal, which has requested changes, is the one `new` answers: `named` is
    /// its id, or (no name) the title is the same. Both need a session and the same one: two
    /// proposals that name none are not known to come from one conversation.
    fn is_answered_by(&self, new: &Self, named: Option<&str>) -> bool {
        if self.changes_requested.is_none() {
            return false;
        }

        match named {
            Some(id) => self.held_id == id && self.session.is_some() && self.session == new.session,
            None => {
                self.session.is_some()
                    && self.session == new.session
                    && self.title.trim().to_lowercase() == new.title.trim().to_lowercase()
            }
        }
    }

    /// The same proposal as `other` (a retry): same text, title, format, links and replacement (the
    /// same document, as it was).
    fn repeats(&self, other: &Self) -> bool {
        self.content_sha256 == other.content_sha256
            && self.title == other.title
            && self.format == other.format
            && self.links == other.links
            && self.replaces == other.replaces
            && self.replaced == other.replaced
    }
}

/// `bytes` as lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `value` with the keys of every object sorted.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut fields: Vec<(String, Value)> = fields.into_iter().collect();
            fields.sort_by(|a, b| a.0.cmp(&b.0));

            Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| (key, sorted(value)))
                    .collect::<Map<_, _>>(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// Unix seconds now.
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}
