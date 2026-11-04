//! Drafts and team approval: the states a document is in, the pending version of a live document,
//! the events, comments, original files and snapshots that belong to a review, and the checks every
//! backend runs on them.
//!
//! - A document is `live`, `draft`, `waiting` (asked for approval) or `changesRequested`. Only live
//!   documents are seen by agents (see [`Visibility`] and [`super::Scope`]).
//! - A live document may have one pending version: a separate row with `pending_of` set to the
//!   live id, never live itself. It holds the replacement until it is promoted
//!   ([`super::KnowledgeStore::promote`]) and is not a row of its own in lists or searches.
//! - `review_rev` is raised by every content change and every transition. A transition names the
//!   revision it was decided on (`expect`) and is refused when the document moved on.
//! - The events of a document are its history. The store records them itself, in the transaction of
//!   the change: `drafted` or `updateDrafted` when a non-live row is stored, `edited` when one is
//!   changed, and the transitions of [`ReviewAction`].
//! - An `Asked` transition keeps the section texts as they were asked ([`SectionText`]), so a
//!   reviewer can compare with them later. They are kept for the latest [`SNAPSHOTS_PER_DOC_MAX`]
//!   asks, and not at all for a document over [`SNAPSHOT_BYTES_MAX`].
//! - The original file of an import is kept next to the document for Download ([`StoredFile`]).
//!   Changing the content of a document drops its file, because it no longer is the original.

use super::{Author, StoreError, clean};
use crate::digest::sha256_hex;
use crate::limits::{FILE_BYTES_MAX, PDF_BYTES_MAX};
use crate::support::author;
use serde::{Deserialize, Serialize};

/// Most characters of a comment.
pub const COMMENT_BODY_MAX: usize = 4000;

/// Most characters of the section heading a line comment names.
pub const COMMENT_SECTION_MAX: usize = 300;

/// Most comments (replies included) one document keeps.
pub const COMMENTS_PER_DOC_MAX: usize = 1000;

/// Most comments one review adds at once.
pub const COMMENTS_PER_REVIEW_MAX: usize = 200;

/// Largest line number of a line comment.
pub const COMMENT_LINE_MAX: u32 = 1_000_000;

/// Most characters of the comment of a transition (`request-changes`).
pub const EVENT_COMMENT_MAX: usize = 500;

/// The comment of the `edited` event a rename records.
pub const RENAMED_COMMENT: &str = "renamed";

/// Most events a history lists.
pub const HISTORY_MAX: usize = 200;

/// Most bytes of section text a snapshot keeps; a larger document keeps none.
pub const SNAPSHOT_BYTES_MAX: usize = 2 * 1024 * 1024;

/// Most snapshots a document keeps (the latest asks).
pub const SNAPSHOTS_PER_DOC_MAX: usize = 20;

/// Most characters of the name of a stored file.
pub const FILE_NAME_MAX: usize = 255;

/// The state of a document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Status {
    /// Seen by agents.
    #[default]
    Live,
    /// Kept for the person (or the team) to finish; not seen by agents.
    Draft,
    /// Asked for approval.
    Waiting,
    /// A reviewer asked for changes; the author edits and asks again.
    ChangesRequested,
}

impl Status {
    /// The name stored and shown.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Draft => "draft",
            Self::Waiting => "waiting",
            Self::ChangesRequested => "changesRequested",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "live" => Some(Self::Live),
            "draft" => Some(Self::Draft),
            "waiting" => Some(Self::Waiting),
            "changesRequested" => Some(Self::ChangesRequested),
            _ => None,
        }
    }

    pub fn is_live(self) -> bool {
        self == Self::Live
    }
}

/// Which documents a listing keeps by their state. A live document is kept by the state of its
/// pending version too, because that is the state the person waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    /// Every row that is not a pending version: live documents and drafts.
    All,
    /// Live documents, with or without a pending version. What agents list.
    Live,
    /// Draft rows, and live documents whose pending version is a draft.
    Draft,
    /// Waiting rows, and live documents whose pending version waits.
    Waiting,
    /// `changesRequested` rows, and live documents whose pending version is.
    ChangesRequested,
    /// Live documents that have a pending version.
    Pending,
}

impl StatusFilter {
    /// Parses `all`, `live`, `draft`, `waiting`, `changesRequested` or `pending`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "all" => Some(Self::All),
            "live" => Some(Self::Live),
            "draft" => Some(Self::Draft),
            "waiting" => Some(Self::Waiting),
            "changesRequested" => Some(Self::ChangesRequested),
            "pending" => Some(Self::Pending),
            _ => None,
        }
    }

    /// The state a row (or its pending version) must be in; `None` for a filter that is not one.
    pub fn state(self) -> Option<Status> {
        match self {
            Self::Draft => Some(Status::Draft),
            Self::Waiting => Some(Status::Waiting),
            Self::ChangesRequested => Some(Status::ChangesRequested),
            Self::All | Self::Live | Self::Pending => None,
        }
    }
}

/// Which documents a lookup (`find_*`) may see. A pending version is never found by a lookup: it
/// is reached from its live document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Live documents only: what agents may see.
    Live,
    /// Live documents and drafts (rows that are not a pending version).
    WithDrafts,
}

/// The pending version of a live document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingVersion {
    pub id: i64,
    pub status: Status,
    pub review_rev: i64,
    pub updated_by: Option<Author>,
    /// Unix seconds of the latest change of the pending content.
    pub updated_at: i64,
}

/// What happened to a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A draft was stored.
    Drafted,
    /// A draft or waiting row was changed.
    Edited,
    /// A pending version of a live document was stored.
    UpdateDrafted,
    Asked,
    /// A reviewer asked for changes.
    ChangesRequested,
    /// Approved by a teammate.
    Approved,
    /// Made live by its author (no approval needed).
    WentLive,
    /// The author took the ask for approval back.
    Withdrawn,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drafted => "drafted",
            Self::Edited => "edited",
            Self::UpdateDrafted => "updateDrafted",
            Self::Asked => "asked",
            Self::ChangesRequested => "changesRequested",
            Self::Approved => "approved",
            Self::WentLive => "wentLive",
            Self::Withdrawn => "withdrawn",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "drafted" => Some(Self::Drafted),
            "edited" => Some(Self::Edited),
            "updateDrafted" => Some(Self::UpdateDrafted),
            "asked" => Some(Self::Asked),
            "changesRequested" => Some(Self::ChangesRequested),
            "approved" => Some(Self::Approved),
            "wentLive" => Some(Self::WentLive),
            "withdrawn" => Some(Self::Withdrawn),
            _ => None,
        }
    }
}

/// One entry of a history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewEvent {
    pub kind: EventKind,
    pub by: Option<String>,
    /// Unix seconds.
    pub at: i64,
    pub comment: Option<String>,
}

/// The newest [`HISTORY_MAX`] events of a document, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    pub events: Vec<ReviewEvent>,
    /// Every event the document has, over the listed ones.
    pub total: usize,
}

/// A transition of the review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewAction {
    /// `draft` or `changesRequested` to `waiting`; keeps the sections as asked.
    Ask,
    /// `waiting` to `changesRequested`.
    RequestChanges,
    /// `waiting` to live (a pending version is promoted).
    Approve,
    /// `draft`, `waiting` or `changesRequested` to live (a pending version is promoted).
    GoLive,
    /// `waiting` back to `draft`: the author takes the ask back.
    TakeBack,
}

impl ReviewAction {
    /// True when a row in state `from` may take this action.
    pub fn allowed_from(self, from: Status) -> bool {
        match self {
            Self::Ask => matches!(from, Status::Draft | Status::ChangesRequested),
            Self::RequestChanges | Self::Approve | Self::TakeBack => from == Status::Waiting,
            Self::GoLive => matches!(from, Status::Draft | Status::Waiting | Status::ChangesRequested),
        }
    }

    /// The state after the action.
    pub fn to(self) -> Status {
        match self {
            Self::Ask => Status::Waiting,
            Self::RequestChanges => Status::ChangesRequested,
            Self::TakeBack => Status::Draft,
            Self::Approve | Self::GoLive => Status::Live,
        }
    }

    /// The event the action records.
    pub fn event(self) -> EventKind {
        match self {
            Self::Ask => EventKind::Asked,
            Self::RequestChanges => EventKind::ChangesRequested,
            Self::Approve => EventKind::Approved,
            Self::GoLive => EventKind::WentLive,
            Self::TakeBack => EventKind::Withdrawn,
        }
    }

    /// True when the action makes the document live, so a pending version replaces its live one.
    pub fn goes_live(self) -> bool {
        matches!(self, Self::Approve | Self::GoLive)
    }
}

/// A transition to apply: who made it, when (unix seconds) and the comment that goes with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewChange {
    pub action: ReviewAction,
    pub by: Option<String>,
    pub comment: Option<String>,
    pub at: i64,
}

impl ReviewChange {
    pub fn new(action: ReviewAction, by: Option<&str>, at: i64) -> Self {
        Self {
            action,
            by: by.map(str::to_owned),
            comment: None,
            at,
        }
    }

    pub fn with_comment(mut self, comment: &str) -> Self {
        self.comment = Some(comment.to_owned());
        self
    }
}

/// A transition that was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewed {
    /// The document after the transition: the live id after a promote.
    pub id: i64,
    pub status: Status,
    /// The revision after the transition.
    pub revision: i64,
    /// True when a pending version replaced its live document.
    pub promoted: bool,
}

/// What a transition came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewReply {
    Done(Reviewed),
    /// The document moved on since the revision that was expected.
    Changed {
        revision: i64,
    },
    /// The action does not apply to a document in this state.
    WrongState(Status),
    NotFound,
}

/// What decides a transition before it is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The row is at the expected revision and in a state the action applies to.
    Go,
    Stop(ReviewReply),
}

/// Whether `action` may be applied to a row in `state` (its status and revision; `None` when there
/// is no such row) that the caller saw at revision `expect`: a missing row first, then a row that
/// moved on, then a row in the wrong state.
pub fn gate(state: Option<(Status, i64)>, expect: i64, action: ReviewAction) -> Gate {
    let Some((status, revision)) = state else {
        return Gate::Stop(ReviewReply::NotFound);
    };

    if revision != expect {
        return Gate::Stop(ReviewReply::Changed { revision });
    }

    if !action.allowed_from(status) {
        return Gate::Stop(ReviewReply::WrongState(status));
    }

    Gate::Go
}

/// A chunk's text as it sits in its document, to cut a document into sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub heading_path: String,
    pub text: String,
    pub char_start: usize,
    pub char_end: usize,
}

/// A section of a document: the chunks under one heading path, in order, as one text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionText {
    pub heading: String,
    pub text: String,
}

/// The sections of a document from its chunks (in ordinal order): consecutive chunks with the same
/// heading path are one section. The chunks of a long section overlap, so the characters a chunk
/// repeats from the one before are dropped (the offsets say how many); chunks that do not touch are
/// joined by a line break. A heading path that comes back later is a section of its own.
pub fn group_sections(pieces: &[Piece]) -> Vec<SectionText> {
    let mut sections: Vec<SectionText> = Vec::new();
    let mut end = 0;

    for piece in pieces {
        let same = sections.last().is_some_and(|last| last.heading == piece.heading_path);

        if !same {
            sections.push(SectionText {
                heading: piece.heading_path.clone(),
                text: piece.text.clone(),
            });
            end = piece.char_end;
            continue;
        }

        let overlap = end.saturating_sub(piece.char_start);
        let rest: String = piece.text.chars().skip(overlap).collect();

        if let Some(last) = sections.last_mut() {
            if overlap == 0 && !rest.is_empty() {
                last.text.push('\n');
            }

            last.text.push_str(&rest);
        }

        end = end.max(piece.char_end);
    }

    sections
}

/// The bytes of the text of `sections`, which a snapshot is limited by.
pub fn sections_bytes(sections: &[SectionText]) -> usize {
    sections
        .iter()
        .map(|section| section.heading.len() + section.text.len())
        .sum()
}

/// The side of a diff a line comment is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The live text.
    Old,
    /// The pending text.
    New,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "old" => Some(Self::Old),
            "new" => Some(Self::New),
            _ => None,
        }
    }
}

/// A comment to add. A line comment names the section (its heading, and which of the sections that
/// share it: `occurrence`, counted from 1), the side and the lines of the section's diff
/// (`from..=to`, counted from 1), and an overall comment of a review names none of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewComment {
    pub section: Option<String>,
    pub occurrence: Option<u32>,
    pub side: Option<Side>,
    pub line_from: Option<u32>,
    pub line_to: Option<u32>,
    pub body: String,
    /// The lines the comment is about, as the reviewer saw them (see [`QuoteLine`]); only a line
    /// comment keeps them, and at most [`QUOTE_LINES_MAX`] lines of [`QUOTE_BYTES_MAX`] bytes.
    pub quote: Option<Vec<QuoteLine>>,
}

impl NewComment {
    /// A comment on the document as a whole.
    pub fn overall(body: &str) -> Self {
        Self {
            section: None,
            occurrence: None,
            side: None,
            line_from: None,
            line_to: None,
            body: body.to_owned(),
            quote: None,
        }
    }

    /// A comment on lines `from..=to` of a section (the first one with that heading).
    pub fn on_lines(section: &str, side: Side, from: u32, to: u32, body: &str) -> Self {
        Self {
            section: Some(section.to_owned()),
            occurrence: Some(1),
            side: Some(side),
            line_from: Some(from),
            line_to: Some(to),
            body: body.to_owned(),
            quote: None,
        }
    }

    /// The same comment with the lines it is about.
    pub fn with_quote(self, quote: Vec<QuoteLine>) -> Self {
        Self {
            quote: Some(quote),
            ..self
        }
    }

    /// The same comment on the `occurrence`-th section (from 1) that has its heading.
    pub fn in_occurrence(self, occurrence: u32) -> Self {
        Self {
            occurrence: Some(occurrence),
            ..self
        }
    }
}

/// A stored comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: i64,
    /// The review (submission) it came with; replies have none.
    pub review_id: Option<i64>,
    pub section: Option<String>,
    /// Which of the sections that share the heading `section` (from 1), none for a comment without one.
    pub occurrence: Option<u32>,
    pub side: Option<Side>,
    pub line_from: Option<u32>,
    pub line_to: Option<u32>,
    pub body: String,
    pub by: Option<String>,
    /// Unix seconds.
    pub at: i64,
    /// The comment this one answers; a reply is never answered itself.
    pub parent_id: Option<i64>,
    pub resolved_at: Option<i64>,
    pub resolved_by: Option<String>,
    /// The revision of the document when the comment was written. A comment of an older revision
    /// than the document's is outdated.
    pub revision: i64,
    /// The lines a line comment was written on; none for an overall comment, a reply and a
    /// comment written before the lines were kept.
    pub quote: Option<Vec<QuoteLine>>,
}

/// Most names a row of `docs list` carries as `authors` (the rule of `selfApproval` and of the
/// take-back always reads every name: this only limits what a list prints).
pub const AUTHORS_LIST_MAX: usize = 20;

/// Most distinct writers per document [`KnowledgeStore::authorship_of`] reads for a list, a little
/// over [`AUTHORS_LIST_MAX`] because names that differ only in case merge afterwards.
pub const AUTHORS_FETCH_MAX: usize = 64;

/// The people of a document that a list needs, read for many documents at once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Authorship {
    /// The distinct names on the `drafted`, `edited` and `updateDrafted` events since the
    /// document was last live, oldest first (see [`KnowledgeStore::writers_since_live`]).
    pub writers: Vec<String>,
    /// Who asked for approval last since then, if anyone did.
    pub asker: Option<String>,
}

/// Who asked for approval last in `history` (newest first), since the version was last made live.
pub fn latest_asker(history: &History) -> Option<String> {
    for event in &history.events {
        match event.kind {
            EventKind::WentLive | EventKind::Approved => return None,
            EventKind::Asked => return event.by.clone(),
            _ => {}
        }
    }

    None
}

/// Most lines of a quote.
pub const QUOTE_LINES_MAX: usize = 20;

/// Most bytes of text of a quote, all its lines together.
pub const QUOTE_BYTES_MAX: usize = 4000;

/// What a quoted line is in the diff the reviewer saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuoteKind {
    Added,
    Removed,
    Same,
}

impl QuoteKind {
    /// The name shown and stored.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Same => "same",
        }
    }
}

/// A line a comment was written on: its number on the side of the comment, what it is in the diff
/// and its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteLine {
    pub line: u32,
    pub kind: QuoteKind,
    pub text: String,
    /// The line was longer than what was kept: `text` is its start. Left out of the stored JSON
    /// when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cut: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// `quote` cut to [`QUOTE_LINES_MAX`] lines and [`QUOTE_BYTES_MAX`] bytes (a line that does not
/// fit is cut at a character and marked `cut`, and the lines after it are dropped); none when
/// nothing is left.
pub fn capped_quote(quote: &[QuoteLine]) -> Option<Vec<QuoteLine>> {
    let mut kept: Vec<QuoteLine> = Vec::new();
    let mut spent = 0;

    for line in quote.iter().take(QUOTE_LINES_MAX) {
        let room = QUOTE_BYTES_MAX - spent;

        if line.text.len() > room {
            let text: String = line
                .text
                .char_indices()
                .take_while(|(at, ch)| at + ch.len_utf8() <= room)
                .map(|(_, ch)| ch)
                .collect();

            // Nothing of the line fits: it is left out, not kept as an empty cut one.
            if text.is_empty() {
                break;
            }

            kept.push(QuoteLine {
                text,
                cut: true,
                ..line.clone()
            });

            break;
        }

        spent += line.text.len();
        kept.push(line.clone());
    }

    (!kept.is_empty()).then_some(kept)
}

/// The text a quote is stored as (JSON), none for no quote.
pub fn quote_text(quote: Option<&[QuoteLine]>) -> Option<String> {
    let kept = capped_quote(quote?)?;

    serde_json::to_string(&kept).ok()
}

/// The quote of a stored text. Anybody with access to the database may have filled the column with
/// something else, so what is not a quote reads as none.
pub fn read_quote(stored: Option<&str>) -> Option<Vec<QuoteLine>> {
    let lines: Vec<QuoteLine> = serde_json::from_str(stored?).ok()?;

    capped_quote(&lines)
}

/// The original file of an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

/// What a store that cannot keep drafts yet answers to the calls only a draft needs.
pub fn not_supported() -> StoreError {
    StoreError::Backend("this database does not keep drafts yet".to_owned())
}

/// A comment body: line breaks normalised, control and invisible characters other than line breaks
/// and tabs dropped, trimmed, 1 to [`COMMENT_BODY_MAX`] characters.
pub fn comment_body(raw: &str, max: usize) -> Result<String, StoreError> {
    let text = raw.replace("\r\n", "\n").replace('\r', "\n");
    let kept: String = text
        .chars()
        .filter(|character| {
            matches!(character, '\n' | '\t')
                || (!character.is_control() && !crate::support::text::is_format_character(*character))
        })
        .collect();
    let body = kept.trim().to_owned();
    let length = body.chars().count();

    if length == 0 || length > max {
        return Err(StoreError::Invalid(format!(
            "a comment must be 1-{max} characters (got {length})"
        )));
    }

    Ok(body)
}

/// The comments of one review checked and cleaned: all or none of section, side and lines are
/// given, the lines are `1 <= from <= to`.
pub fn check_comments(comments: &[NewComment]) -> Result<Vec<NewComment>, StoreError> {
    if comments.is_empty() {
        return Err(StoreError::Invalid("a review needs at least one comment".to_owned()));
    }

    if comments.len() > COMMENTS_PER_REVIEW_MAX {
        return Err(StoreError::Invalid(format!(
            "at most {COMMENTS_PER_REVIEW_MAX} comments at once"
        )));
    }

    comments.iter().map(check_comment).collect()
}

fn check_comment(comment: &NewComment) -> Result<NewComment, StoreError> {
    let body = comment_body(&comment.body, COMMENT_BODY_MAX)?;

    match (&comment.section, comment.side, comment.line_from, comment.line_to) {
        (None, None, None, None) if comment.occurrence.is_none() => Ok(NewComment {
            body,
            quote: None,
            ..comment.clone()
        }),
        (Some(section), Some(_), Some(from), Some(to)) => {
            let section = clean::line(section);
            let length = section.chars().count();

            if length > COMMENT_SECTION_MAX {
                return Err(StoreError::Invalid(format!(
                    "a section heading is at most {COMMENT_SECTION_MAX} characters"
                )));
            }

            if from == 0 || from > to || to > COMMENT_LINE_MAX {
                return Err(StoreError::Invalid(format!(
                    "the lines of a comment are 1 <= from <= to <= {COMMENT_LINE_MAX}"
                )));
            }

            let occurrence = comment.occurrence.unwrap_or(1);

            if occurrence == 0 || occurrence > COMMENT_LINE_MAX {
                return Err(StoreError::Invalid(format!(
                    "the occurrence of a section is 1 <= n <= {COMMENT_LINE_MAX}"
                )));
            }

            Ok(NewComment {
                section: Some(section),
                occurrence: Some(occurrence),
                body,
                quote: comment.quote.as_deref().and_then(capped_quote),
                ..comment.clone()
            })
        }
        _ => Err(StoreError::Invalid(
            "a line comment names its section, side and lines together, an overall one none of them".to_owned(),
        )),
    }
}

/// A name that is stored as the author of an event or comment: cleaned as author names are, and
/// none when it is not fit to store.
pub fn actor(name: Option<&str>) -> Option<String> {
    author::clean(name?)
}

/// The comment of a transition: one line of at most [`EVENT_COMMENT_MAX`] characters, none when blank.
pub fn event_comment(raw: Option<&str>) -> Result<Option<String>, StoreError> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    let comment = clean::line(raw);
    let length = comment.chars().count();

    if length > EVENT_COMMENT_MAX {
        return Err(StoreError::Invalid(format!(
            "a comment is at most {EVENT_COMMENT_MAX} characters (got {length})"
        )));
    }

    Ok((!comment.is_empty()).then_some(comment))
}

/// A transition checked: the cleaned actor and comment.
pub fn check_change(change: &ReviewChange) -> Result<(Option<String>, Option<String>), StoreError> {
    Ok((actor(change.by.as_deref()), event_comment(change.comment.as_deref())?))
}

/// The name of a stored file checked and cleaned.
pub fn file_name(raw: &str) -> Result<String, StoreError> {
    let name = clean::line(raw);
    let length = name.chars().count();

    if length == 0 || length > FILE_NAME_MAX {
        return Err(StoreError::Invalid(format!(
            "a file name is 1-{FILE_NAME_MAX} characters (got {length})"
        )));
    }

    Ok(name)
}

/// The size a stored file may have before a store looks at the document: the largest an import
/// reads.
pub fn check_file_size(bytes: &[u8]) -> Result<(), StoreError> {
    let limit = usize::try_from(PDF_BYTES_MAX).unwrap_or(usize::MAX);

    if bytes.len() > limit {
        return Err(StoreError::Invalid(format!(
            "a stored file is at most {PDF_BYTES_MAX} bytes (got {})",
            bytes.len()
        )));
    }

    Ok(())
}

/// The size a stored file of a document in `format` may have: a PDF [`PDF_BYTES_MAX`], any other
/// file [`FILE_BYTES_MAX`].
pub fn check_file_for(format: &str, bytes: &[u8]) -> Result<(), StoreError> {
    let limit = if format == "pdf" { PDF_BYTES_MAX } else { FILE_BYTES_MAX };

    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(StoreError::Invalid(format!(
            "a stored {format} file is at most {limit} bytes (got {})",
            bytes.len()
        )));
    }

    Ok(())
}

/// The hash a stored file is kept with.
pub fn file_sha(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piece(heading: &str, text: &str, start: usize, end: usize) -> Piece {
        Piece {
            heading_path: heading.to_owned(),
            text: text.to_owned(),
            char_start: start,
            char_end: end,
        }
    }

    #[test]
    fn overlapping_chunks_of_a_section_join_without_repeating_the_overlap() {
        let sections = group_sections(&[
            piece("Guide", "one two three four", 10, 28),
            piece("Guide", "three four five", 18, 33),
            piece("Other", "alone", 40, 45),
            piece("Guide", "again", 50, 55),
        ]);

        let texts: Vec<(&str, &str)> = sections
            .iter()
            .map(|section| (section.heading.as_str(), section.text.as_str()))
            .collect();

        assert_eq!(
            texts,
            [
                ("Guide", "one two three four five"),
                ("Other", "alone"),
                ("Guide", "again")
            ]
        );
    }

    #[test]
    fn chunks_that_do_not_touch_are_joined_by_a_line_break() {
        let sections = group_sections(&[piece("A", "first", 0, 5), piece("A", "second", 7, 13)]);

        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].text, "first\nsecond");
    }

    #[test]
    fn a_quote_line_with_no_room_left_is_dropped_not_kept_as_an_empty_cut_one() {
        let line = |number: u32, text: String| QuoteLine {
            line: number,
            kind: QuoteKind::Same,
            text,
            cut: false,
        };

        // The first line fills the bytes exactly, so nothing of the second fits.
        let kept = capped_quote(&[line(1, "a".repeat(QUOTE_BYTES_MAX)), line(2, "b".to_owned())]).expect("a quote");
        assert_eq!(kept.len(), 1);
        assert!(!kept[0].cut);

        // A line that crosses the cap is still kept up to it, marked.
        let kept = capped_quote(&[line(1, "a".repeat(10)), line(2, "b".repeat(QUOTE_BYTES_MAX))]).expect("a quote");
        assert_eq!(kept.len(), 2);
        assert!(kept[1].cut && kept[1].text.len() == QUOTE_BYTES_MAX - 10);

        // A first line of a multi-byte character that cannot fit leaves nothing.
        assert!(
            capped_quote(&[line(1, "a".repeat(QUOTE_BYTES_MAX)), line(2, "ł".to_owned())])
                .is_some_and(|q| q.len() == 1)
        );
    }

    #[test]
    fn a_line_comment_names_everything_or_nothing() {
        assert!(check_comments(&[NewComment::overall(" fine \r\n")]).is_ok());
        assert!(check_comments(&[NewComment::on_lines("Setup", Side::New, 2, 3, "x")]).is_ok());

        let partial = NewComment {
            side: Some(Side::Old),
            ..NewComment::overall("x")
        };
        assert!(check_comments(&[partial]).is_err());
        assert!(check_comments(&[NewComment::on_lines("Setup", Side::New, 3, 2, "x")]).is_err());
        assert!(check_comments(&[NewComment::on_lines("Setup", Side::New, 0, 2, "x")]).is_err());
        assert!(check_comments(&[NewComment::overall("   ")]).is_err());
        assert!(check_comments(&[]).is_err());
    }
}
