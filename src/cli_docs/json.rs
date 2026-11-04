//! The JSON shapes of documents, links, sections and chunks.

use serde_json::{Value, json};

use crate::store::routed::ids;
use crate::store::{Author, Chunk, DocUsage, Document, EventKind, History, Link, PendingVersion, ReviewEvent, Section};
use crate::support::time::rfc3339;

/// A document with its links: `id`, `title`, `sourceName`, `sourceKey`, `format`, `contentSha256`,
/// `fileSha256`, `bytes`, `chunkCount`, `createdAt`, `updatedAt` (RFC 3339), `origin` (`user` or
/// `agentApproved`), `warnings`, `addedBy` and `updatedBy` (see [`author`]), `links` and, when both places are in use, `place`
/// (`local` or `remote`). `id` is the number, or the string `local:12` for a document on this computer.
///
/// `shown`, `useful` and `lastUsefulAt` (RFC 3339, `null` when never) are the counters of `usage`,
/// and absent when it is `None` (the database has no counter columns).
///
/// `status` (`live`, `draft`, `waiting` or `changesRequested`) and `revision` (the number a
/// transition gives back as `--expect`) tell where the document is in its review, and `pendingOf`
/// (the live id) is there when the row is the pending version of a live document. The pending
/// version a live document has is added by [`with_pending`], the last review words of a row that is
/// not live by [`with_last_events`].
pub fn document(doc: &Document, links: &[Link], usage: Option<DocUsage>) -> Value {
    let mut shown = json!({
        "id": ids::json(doc.id),
        "title": doc.title,
        "sourceName": doc.source_name,
        "sourceKey": doc.source_key,
        "format": doc.format,
        "contentSha256": doc.content_sha256,
        "fileSha256": doc.file_sha256,
        "bytes": doc.bytes,
        "chunkCount": doc.chunk_count,
        "createdAt": rfc3339(doc.created_at),
        "updatedAt": rfc3339(doc.updated_at),
        "origin": doc.origin.as_str(),
        "status": doc.status.as_str(),
        "revision": doc.review_rev,
        "warnings": doc.warnings,
        "addedBy": author(doc.added_by.as_ref()),
        "updatedBy": author(doc.updated_by.as_ref()),
        "links": links.iter().map(link).collect::<Vec<_>>(),
    });

    if let Some(usage) = usage {
        shown["shown"] = json!(usage.shown);
        shown["useful"] = json!(usage.useful);
        shown["lastUsefulAt"] = usage.last_useful_at.map_or(Value::Null, |at| json!(rfc3339(at)));
    }

    if let Some(place) = ids::place_name(doc.id) {
        shown["place"] = json!(place);
    }

    if let Some(live) = doc.pending_of {
        shown["pendingOf"] = ids::json(live);
    }

    shown
}

/// The pending version of a live document under `pending`: `null`, or
/// `{"id", "status", "revision", "updatedBy", "updatedAt"}`. Only a live document has one, so
/// `shown` of any other row is left as it is.
pub fn with_pending(shown: &mut Value, doc: &Document, pending: Option<&PendingVersion>) {
    if doc.status.is_live() && doc.pending_of.is_none() {
        shown["pending"] = pending.map_or(Value::Null, |pending| {
            json!({
                "id": ids::json(pending.id),
                "status": pending.status.as_str(),
                "revision": pending.review_rev,
                "updatedBy": author(pending.updated_by.as_ref()),
                "updatedAt": rfc3339(pending.updated_at),
            })
        });
    }
}

/// `lastChange` and `lastRequest` of a row that is not live, from its `history`: the latest time a
/// reviewer asked for changes and the latest time the author asked for approval, each `null` or
/// `{"by", "at", "comment"}`.
pub fn with_last_events(shown: &mut Value, doc: &Document, history: Option<&History>) {
    if doc.status.is_live() {
        return;
    }

    let last = |kind: EventKind| {
        history
            .and_then(|history| history.events.iter().find(|event| event.kind == kind))
            .map_or(Value::Null, event)
    };

    shown["lastChange"] = last(EventKind::ChangesRequested);
    shown["lastRequest"] = last(EventKind::Asked);
}

/// One event as `{"by", "at", "comment"}`.
fn event(event: &ReviewEvent) -> Value {
    json!({ "by": event.by, "at": rfc3339(event.at), "comment": event.comment })
}

/// An author: `{"name", "via"}` (`via` is the MCP client that wrote for them, `null` for their own
/// import), or `null` when no git name was known.
pub fn author(author: Option<&Author>) -> Value {
    match author {
        Some(author) => json!({ "name": author.name, "via": author.via }),
        None => Value::Null,
    }
}

/// A link: `repo` and `globs`.
pub fn link(link: &Link) -> Value {
    json!({ "repo": link.repo_key, "globs": link.globs })
}

/// One entry of a document's outline.
pub fn section(section: &Section) -> Value {
    json!({
        "chunkId": ids::json(section.chunk_id),
        "ordinal": section.ordinal,
        "headingPath": section.heading_path,
        "tokens": section.tokens,
        "charStart": section.char_start,
        "charEnd": section.char_end,
    })
}

/// A chunk with its text.
pub fn chunk(chunk: &Chunk) -> Value {
    json!({
        "id": ids::json(chunk.id),
        "docId": ids::json(chunk.doc_id),
        "ordinal": chunk.ordinal,
        "headingPath": chunk.heading_path,
        "text": chunk.text,
        "tokens": chunk.tokens,
        "charStart": chunk.char_start,
        "charEnd": chunk.char_end,
    })
}
