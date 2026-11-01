//! `knowledge_get`: a document's outline and text, or one chunk with its neighbors.

use serde::Deserialize;
use serde_json::Value;

use super::reply::{cut, date};
use super::{GET_BYTES_DEFAULT, GET_BYTES_MAX, NEIGHBORS_MAX, Server};
use crate::search::render::{DOC_TAG, FRAME, block, chunk_ref, doc_ref};
use crate::store::{Detail, DocumentDetail, KnowledgeStore, Section};

/// Most lines of the outline of a document.
const OUTLINE_MAX: usize = 40;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GetArgs {
    id: Value,
    neighbors: Option<i64>,
    max_bytes: Option<i64>,
}

/// What an id names.
enum Target {
    Document(i64),
    /// A chunk of a document.
    Chunk(i64, i64),
}

/// `12` (or the number 12) is a document, `12:340` a chunk of it.
fn target(raw: &Value) -> Result<Target, String> {
    let wrong = || "id must be a document id like 12 or a chunk id like 12:340, as a search shows it".to_owned();

    let text = match raw {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.trim().to_owned(),
        _ => return Err(wrong()),
    };

    let number = |part: &str| part.parse::<i64>().ok().filter(|number| *number >= 1);

    match text.split_once(':') {
        None => number(&text).map(Target::Document).ok_or_else(wrong),
        Some((doc, chunk)) => match (number(doc), number(chunk)) {
            (Some(doc), Some(chunk)) => Ok(Target::Chunk(doc, chunk)),
            _ => Err(wrong()),
        },
    }
}

pub(super) fn get(server: &mut Server, args: GetArgs) -> Result<String, String> {
    let target = target(&args.id)?;
    let neighbors = args.neighbors.map_or(0, |count| {
        usize::try_from(count.clamp(0, NEIGHBORS_MAX as i64)).unwrap_or(0)
    });
    let budget = args.max_bytes.map_or(GET_BYTES_DEFAULT, |bytes| {
        usize::try_from(bytes.clamp(1024, GET_BYTES_MAX as i64)).unwrap_or(GET_BYTES_DEFAULT)
    });

    let read = {
        let Some(store) = server.store()? else {
            return Err("there are no documents yet".to_owned());
        };

        match target {
            Target::Document(doc) => document(store, doc, budget),
            Target::Chunk(doc, chunk) => chunk_with_neighbors(store, doc, chunk, neighbors, budget),
        }
    };

    read.map_err(|error| match error {
        Failure::Store(error) => server.failed(&error),
        Failure::Message(message) => message,
    })
}

enum Failure {
    Store(crate::store::StoreError),
    Message(String),
}

impl From<crate::store::StoreError> for Failure {
    fn from(error: crate::store::StoreError) -> Self {
        Self::Store(error)
    }
}

/// The header of the block of a document: its id, title, format, size, date and repositories.
fn attributes(detail: &DocumentDetail, id: String) -> Vec<(&'static str, String)> {
    let doc = &detail.doc;
    let repos: Vec<String> = detail
        .links
        .iter()
        .map(|link| {
            if link.globs.is_empty() {
                link.repo_key.clone()
            } else {
                format!("{} [{}]", link.repo_key, link.globs.join(", "))
            }
        })
        .collect();

    let mut attributes = vec![
        ("id", id),
        ("title", doc.title.clone()),
        ("format", doc.format.clone()),
        ("chunks", doc.chunk_count.to_string()),
        ("updated", date(doc.updated_at)),
    ];

    attributes.push((
        "repos",
        if repos.is_empty() {
            "global".to_owned()
        } else {
            repos.join(", ")
        },
    ));
    attributes
}

/// The text of chunk `chunk` of `doc`, under a marker line with its id and section.
fn chunk_text(store: &mut dyn KnowledgeStore, doc: i64, section: &Section) -> Result<String, Failure> {
    let detail = store
        .get(
            doc,
            &Detail {
                sections: false,
                chunk: Some(section.chunk_id),
            },
        )?
        .and_then(|detail| detail.chunk)
        .ok_or_else(|| Failure::Message(format!("chunk {} of document {doc} is gone", section.chunk_id)))?;

    let marker = if section.heading_path.is_empty() {
        format!("[{}]", chunk_ref(doc, section.chunk_id))
    } else {
        format!("[{}] {}", chunk_ref(doc, section.chunk_id), section.heading_path)
    };

    Ok(format!("{marker}\n{}", detail.text))
}

fn document(store: &mut dyn KnowledgeStore, doc: i64, budget: usize) -> Result<String, Failure> {
    let detail = store
        .get(
            doc,
            &Detail {
                sections: true,
                chunk: None,
            },
        )?
        .ok_or_else(|| Failure::Message(format!("no document {doc}")))?;
    let sections = detail.sections.clone().unwrap_or_default();

    let mut body = String::new();

    if sections.len() > 1 {
        body.push_str("outline:\n");

        for section in sections.iter().take(OUTLINE_MAX) {
            let heading = if section.heading_path.is_empty() {
                "(start)"
            } else {
                &section.heading_path
            };

            body.push_str(&format!(
                "  {} {}\n",
                chunk_ref(doc, section.chunk_id),
                cut(heading, 100)
            ));
        }

        if sections.len() > OUTLINE_MAX {
            body.push_str(&format!("  … {} more\n", sections.len() - OUTLINE_MAX));
        }

        body.push('\n');
    }

    let (text, next) = read_chunks(store, doc, &sections, budget.saturating_sub(body.len() + 400))?;
    body.push_str(&text);

    Ok(reply(&detail, doc_ref(doc), &body, next.map(|section| (doc, section))))
}

fn chunk_with_neighbors(
    store: &mut dyn KnowledgeStore,
    doc: i64,
    chunk: i64,
    neighbors: usize,
    budget: usize,
) -> Result<String, Failure> {
    let detail = store
        .get(
            doc,
            &Detail {
                sections: true,
                chunk: Some(chunk),
            },
        )?
        .ok_or_else(|| Failure::Message(format!("no document {doc}")))?;
    let sections = detail.sections.clone().unwrap_or_default();

    let Some(at) = sections.iter().position(|section| section.chunk_id == chunk) else {
        return Err(Failure::Message(format!("document {doc} has no chunk {chunk}")));
    };

    let first = at.saturating_sub(neighbors);
    let window = &sections[first..(at + neighbors + 1).min(sections.len())];
    let (text, next) = read_chunks(store, doc, window, budget.saturating_sub(400))?;
    let after = next.or_else(|| sections.get(at + neighbors + 1).cloned());

    Ok(reply(
        &detail,
        chunk_ref(doc, chunk),
        &text,
        after.map(|section| (doc, section)),
    ))
}

/// The text of `sections` in order, at most `budget` bytes: whole chunks while they fit, the first
/// one cut when it alone does not. The first section left out is the second part.
fn read_chunks(
    store: &mut dyn KnowledgeStore,
    doc: i64,
    sections: &[Section],
    budget: usize,
) -> Result<(String, Option<Section>), Failure> {
    let mut text = String::new();

    for section in sections {
        let chunk = chunk_text(store, doc, section)?;
        let separator = if text.is_empty() { 0 } else { 2 };

        if text.len() + separator + chunk.len() > budget {
            if text.is_empty() {
                text = cut(&chunk, budget);
                // The cut chunk is the one to go on from.
                return Ok((text, Some(section.clone())));
            }

            return Ok((text, Some(section.clone())));
        }

        if !text.is_empty() {
            text.push_str("\n\n");
        }

        text.push_str(&chunk);
    }

    Ok((text, None))
}

/// The reply: the frame, the block of the document and, when the text goes on, where to read next.
fn reply(detail: &DocumentDetail, id: String, body: &str, next: Option<(i64, Section)>) -> String {
    let mut text = format!("{FRAME}\n{}", block(DOC_TAG, &attributes(detail, id), body.trim_end()));

    if let Some((doc, section)) = next {
        text.push_str(&format!(
            "\nThe text goes on: continue with knowledge_get id \"{}\".",
            chunk_ref(doc, section.chunk_id)
        ));
    }

    text
}
