//! The JSON shapes of documents, links, sections and chunks.

use serde_json::{Value, json};

use crate::store::routed::ids;
use crate::store::{Author, Chunk, DocUsage, Document, Link, Section};
use crate::support::time::rfc3339;

/// A document with its links: `id`, `title`, `sourceName`, `sourceKey`, `format`, `contentSha256`,
/// `fileSha256`, `bytes`, `chunkCount`, `createdAt`, `updatedAt` (RFC 3339), `origin` (`user` or
/// `agentApproved`), `warnings`, `addedBy` and `updatedBy` (see [`author`]), `links` and, when both places are in use, `place`
/// (`local` or `remote`). `id` is the number, or the string `local:12` for a document on this computer.
///
/// `shown`, `useful` and `lastUsefulAt` (RFC 3339, `null` when never) are the counters of `usage`,
/// and absent when it is `None` (the database has no counter columns).
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

    shown
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
