//! The JSON shapes of documents, links, sections and chunks.

use serde_json::{Value, json};

use crate::store::{Chunk, Document, Link, Section};
use crate::support::time::rfc3339;

/// A document with its links: `id`, `title`, `sourceName`, `sourceKey`, `format`, `contentSha256`,
/// `fileSha256`, `bytes`, `chunkCount`, `createdAt`, `updatedAt` (RFC 3339), `origin` (`user` or
/// `agentApproved`), `warnings` and `links`.
pub fn document(doc: &Document, links: &[Link]) -> Value {
    json!({
        "id": doc.id,
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
        "links": links.iter().map(link).collect::<Vec<_>>(),
    })
}

/// A link: `repo` and `globs`.
pub fn link(link: &Link) -> Value {
    json!({ "repo": link.repo_key, "globs": link.globs })
}

/// One entry of a document's outline.
pub fn section(section: &Section) -> Value {
    json!({
        "chunkId": section.chunk_id,
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
        "id": chunk.id,
        "docId": chunk.doc_id,
        "ordinal": chunk.ordinal,
        "headingPath": chunk.heading_path,
        "text": chunk.text,
        "tokens": chunk.tokens,
        "charStart": chunk.char_start,
        "charEnd": chunk.char_end,
    })
}
