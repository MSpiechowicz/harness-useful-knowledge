//! The SQL the local SQLite file and the libSQL server share for the review of drafts (schema
//! version 2, see [`super::sql_shared::review`]): the state of a row, the events, the snapshots of
//! an ask, the comments, the original files and the promote of a pending version.
//!
//! The statements number their parameters (`?1`), so both runners bind them the same way.

use super::sql_quality::document_columns;
use crate::store::{Columns, Piece, Side};

/// The review columns of document `?1`: state, the live document it is pending for, revision, format.
pub(crate) const ROW_SQL: &str = "SELECT status, pending_of, review_rev, format FROM knowledge_documents WHERE id = ?1";

/// Moves document `?1` to state `?2` and the next revision, if it still is at revision `?3`.
pub(crate) const TRANSITION_SQL: &str =
    "UPDATE knowledge_documents SET status = ?2, review_rev = review_rev + 1 WHERE id = ?1 AND review_rev = ?3";

/// The pending versions of the documents in the JSON array `?1`, as document rows.
pub(crate) fn pending_versions_sql(columns: Columns) -> String {
    format!(
        "SELECT {} FROM knowledge_documents d WHERE d.pending_of IN (SELECT value FROM json_each(?1))",
        document_columns(columns)
    )
}

/// Records an event of document `?1`: kind, name, comment, unix seconds.
pub(crate) const EVENT_SQL: &str =
    "INSERT INTO knowledge_doc_events (doc_id, kind, by_name, comment, at) VALUES (?1, ?2, ?3, ?4, ?5)";

/// The newest events of document `?1`, at most `?2`: kind, name, comment, time.
pub(crate) const HISTORY_SQL: &str = "SELECT kind, by_name, comment, at FROM knowledge_doc_events
     WHERE doc_id = ?1 ORDER BY id DESC LIMIT ?2";

/// The names of the writers of document `?1` since it was last live, oldest first: the `drafted`,
/// `edited` and `updateDrafted` events after its newest `approved` or `wentLive` event.
pub(crate) const WRITERS_SQL: &str = "SELECT by_name FROM knowledge_doc_events
     WHERE doc_id = ?1 AND kind IN ('drafted', 'edited', 'updateDrafted') AND by_name IS NOT NULL
       AND id > COALESCE((SELECT max(id) FROM knowledge_doc_events
                          WHERE doc_id = ?1 AND kind IN ('approved', 'wentLive')), 0)
     GROUP BY by_name ORDER BY min(id)";

/// How many events document `?1` has.
pub(crate) const HISTORY_COUNT_SQL: &str = "SELECT count(*) FROM knowledge_doc_events WHERE doc_id = ?1";

/// The chunks of document `?1` in order, to cut it into sections: heading path, text, offsets.
pub(crate) const PIECES_SQL: &str =
    "SELECT heading_path, text, char_start, char_end FROM knowledge_chunks WHERE doc_id = ?1 ORDER BY ordinal";

/// Whether document `?1` exists.
pub(crate) const EXISTS_SQL: &str = "SELECT 1 FROM knowledge_documents WHERE id = ?1";

/// Keeps the sections (JSON) `?3` of document `?1` as of revision `?2`.
pub(crate) const SNAPSHOT_PUT_SQL: &str = "INSERT INTO knowledge_doc_snapshots (doc_id, revision, sections)
     VALUES (?1, ?2, ?3) ON CONFLICT (doc_id, revision) DO UPDATE SET sections = excluded.sections";

/// Drops all but the newest `?2` snapshots of document `?1`.
pub(crate) const SNAPSHOT_PRUNE_SQL: &str = "DELETE FROM knowledge_doc_snapshots WHERE doc_id = ?1
     AND revision NOT IN (SELECT revision FROM knowledge_doc_snapshots WHERE doc_id = ?1 ORDER BY revision DESC LIMIT ?2)";

/// The sections (JSON) document `?1` had as of revision `?2`.
pub(crate) const SNAPSHOT_GET_SQL: &str =
    "SELECT sections FROM knowledge_doc_snapshots WHERE doc_id = ?1 AND revision = ?2";

/// The next review number of document `?1`.
pub(crate) const REVIEW_ID_SQL: &str =
    "SELECT coalesce(max(review_id), 0) + 1 FROM knowledge_doc_comments WHERE doc_id = ?1";

/// How many comments document `?1` has.
pub(crate) const COMMENT_COUNT_SQL: &str = "SELECT count(*) FROM knowledge_doc_comments WHERE doc_id = ?1";

/// Adds a comment: document, review, section, side, first line, last line, body, name, time,
/// revision, occurrence of the section, quote.
pub(crate) const COMMENT_ADD_SQL: &str = "INSERT INTO knowledge_doc_comments
         (doc_id, review_id, section, side, line_from, line_to, body, by_name, at, revision, occurrence, quote)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)";

/// Adds a reply to comment `?2` of document `?1` (body `?3`, name `?4`, time `?5`, revision `?6`)
/// when that comment is one that is not itself a reply; no row inserted otherwise.
pub(crate) const COMMENT_REPLY_SQL: &str = "INSERT INTO knowledge_doc_comments
         (doc_id, parent_id, body, by_name, at, revision)
     SELECT ?1, c.id, ?3, ?4, ?5, ?6 FROM knowledge_doc_comments c
     WHERE c.id = ?2 AND c.doc_id = ?1 AND c.parent_id IS NULL";

/// Resolves comment `?2` of document `?1` by name `?3` at `?4` (a comment that is not a reply).
pub(crate) const COMMENT_RESOLVE_SQL: &str = "UPDATE knowledge_doc_comments SET resolved_at = ?4, resolved_by = ?3
     WHERE id = ?2 AND doc_id = ?1 AND parent_id IS NULL";

/// Opens comment `?2` of document `?1` again.
pub(crate) const COMMENT_REOPEN_SQL: &str = "UPDATE knowledge_doc_comments SET resolved_at = NULL, resolved_by = NULL
     WHERE id = ?2 AND doc_id = ?1 AND parent_id IS NULL";

/// The ids of comment `?2` of document `?1` and of its replies, the comment first (a reply is
/// written after the comment it answers, so it has the larger id); none when the document has no
/// such comment.
pub(crate) const COMMENT_THREAD_SQL: &str = "SELECT id FROM knowledge_doc_comments
     WHERE doc_id = ?1 AND (id = ?2 OR parent_id = ?2) ORDER BY id";

/// Deletes comment `?2` of document `?1` and its replies.
pub(crate) const COMMENT_DELETE_SQL: &str =
    "DELETE FROM knowledge_doc_comments WHERE doc_id = ?1 AND (id = ?2 OR parent_id = ?2)";

/// Deletes every comment of document `?1` (the replies included): its review is over.
pub(crate) const COMMENTS_DROP_SQL: &str = "DELETE FROM knowledge_doc_comments WHERE doc_id = ?1";

/// The comments of document `?1`, at most `?2`, in the order they were written.
pub(crate) const COMMENTS_SQL: &str = "SELECT id, review_id, section, side, line_from, line_to, body, by_name, at,
         parent_id, resolved_at, resolved_by, revision, occurrence, quote
     FROM knowledge_doc_comments WHERE doc_id = ?1 ORDER BY id LIMIT ?2";

/// The bytes of a file one statement carries (a libSQL server refuses an argument of 8 MiB, and
/// accepts 4).
pub(crate) const FILE_PART_BYTES: usize = 2 * 1024 * 1024;

/// Keeps the file of document `?1`: name, size, hash. The parts follow ([`FILE_PART_PUT_SQL`]).
pub(crate) const FILE_PUT_SQL: &str =
    "INSERT INTO knowledge_doc_files (doc_id, name, size, sha256) VALUES (?1, ?2, ?3, ?4)";

/// Keeps part `?2` of the file of document `?1`.
pub(crate) const FILE_PART_PUT_SQL: &str =
    "INSERT INTO knowledge_doc_file_parts (doc_id, part, bytes) VALUES (?1, ?2, ?3)";

/// The file of document `?1`: name, size, hash.
pub(crate) const FILE_GET_SQL: &str = "SELECT name, size, sha256 FROM knowledge_doc_files WHERE doc_id = ?1";

/// Part `?2` of the file of document `?1`.
pub(crate) const FILE_PART_GET_SQL: &str = "SELECT bytes FROM knowledge_doc_file_parts WHERE doc_id = ?1 AND part = ?2";

/// Drops the file of document `?1` and its parts: its content changed, so it no longer is the original.
pub(crate) const FILE_DROP_SQL: &str = "DELETE FROM knowledge_doc_files WHERE doc_id = ?1";

/// Which ids a statement of [`promote_statements`] binds: the live document, the pending version, or
/// both (`?1` the live one, `?2` the pending one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bind {
    Live,
    Pending,
    Both,
}

/// The statements that promote a pending version over its live document, in the order to run them:
/// the live chunks go, the live row takes the pending row's content, the pending chunks move over
/// (after the title, so the retitle trigger finds no chunk to rewrite), the events, comments and file
/// follow, and the pending row is deleted last. A statement that binds one id names it `?1`.
pub(crate) fn promote_statements(columns: Columns) -> Vec<(String, Bind)> {
    let authors = if columns.authors {
        ", updated_by_name = p.updated_by_name, updated_by_via = p.updated_by_via"
    } else {
        ""
    };

    vec![
        ("DELETE FROM knowledge_chunks WHERE doc_id = ?1".to_owned(), Bind::Live),
        (
            format!(
                "UPDATE knowledge_documents SET source_name = p.source_name, format = p.format,
                     content_sha256 = p.content_sha256, file_sha256 = p.file_sha256, bytes = p.bytes,
                     chunk_count = p.chunk_count, origin = p.origin, warnings = p.warnings,
                     updated_at = p.updated_at{authors},
                     review_rev = max(knowledge_documents.review_rev, p.review_rev) + 1
                 FROM knowledge_documents p WHERE p.id = ?2 AND knowledge_documents.id = ?1"
            ),
            Bind::Both,
        ),
        (
            "UPDATE knowledge_documents SET title = p.title FROM knowledge_documents p
                 WHERE p.id = ?2 AND knowledge_documents.id = ?1 AND knowledge_documents.title <> p.title"
                .to_owned(),
            Bind::Both,
        ),
        (
            "UPDATE knowledge_chunks SET doc_id = ?1 WHERE doc_id = ?2".to_owned(),
            Bind::Both,
        ),
        (
            "UPDATE knowledge_doc_events SET doc_id = ?1 WHERE doc_id = ?2".to_owned(),
            Bind::Both,
        ),
        (
            // The review is over: the comments of both versions go, the history keeps its record.
            "DELETE FROM knowledge_doc_comments WHERE doc_id IN (?1, ?2)".to_owned(),
            Bind::Both,
        ),
        (
            "DELETE FROM knowledge_doc_files WHERE doc_id = ?1".to_owned(),
            Bind::Live,
        ),
        (
            "UPDATE knowledge_doc_files SET doc_id = ?1 WHERE doc_id = ?2".to_owned(),
            Bind::Both,
        ),
        (
            "DELETE FROM knowledge_documents WHERE id = ?1".to_owned(),
            Bind::Pending,
        ),
    ]
}

/// The pieces of a chunk row as [`Piece`]s need them, from the values of [`PIECES_SQL`].
pub(crate) fn piece(heading_path: String, text: String, char_start: usize, char_end: usize) -> Piece {
    Piece {
        heading_path,
        text,
        char_start,
        char_end,
    }
}

/// The sections of a snapshot as JSON.
pub(crate) fn snapshot_json(sections: &[crate::store::SectionText]) -> String {
    let list: Vec<serde_json::Value> = sections
        .iter()
        .map(|section| serde_json::json!({ "heading": section.heading, "text": section.text }))
        .collect();

    serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_owned())
}

/// The sections in a snapshot's JSON; none when it does not read.
pub(crate) fn snapshot_sections(json: &str) -> Option<Vec<crate::store::SectionText>> {
    let list: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;

    list.into_iter()
        .map(|entry| {
            Some(crate::store::SectionText {
                heading: entry.get("heading")?.as_str()?.to_owned(),
                text: entry.get("text")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

/// A stored side, none when it is not one.
pub(crate) fn side(text: Option<&str>) -> Option<Side> {
    text.and_then(Side::parse)
}
