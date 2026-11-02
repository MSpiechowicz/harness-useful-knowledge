//! `knowledge_feedback`: the documents of a search that helped with the next step.
//!
//! Only a counter moves (see [`super::counting`]): usefulness never changes what a search returns.

use serde::Deserialize;
use serde_json::Value;

use super::get::{Target, Visible, target};
use super::scope::{repository, scope_arg};
use super::{FEEDBACK_REASON_MAX, SEARCH_LIMIT_MAX, Server};
use crate::held::now;
use crate::search::render::doc_ref;
use crate::store::{Detail, StoreError};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FeedbackArgs {
    ids: Vec<Value>,
    reason: Option<String>,
    project_path: Option<String>,
    scope: Option<String>,
}

/// What became of the documents asked about.
#[derive(Default)]
struct Checked {
    /// Visible to the caller: counted.
    found: Vec<i64>,
    /// Not there, or linked to other repositories.
    missing: Vec<i64>,
    /// The database could not say.
    unchecked: Vec<i64>,
}

/// The body of `knowledge_feedback`: counts the documents among `args.ids` this caller can see as
/// useful. A search shows chunk ids (`12:340`) and a chunk counts for its document. The reason is
/// for the agent to word its answer: nothing keeps it.
pub(super) fn feedback(server: &mut Server, args: FeedbackArgs) -> Result<String, String> {
    if args.ids.len() > SEARCH_LIMIT_MAX {
        return Err(format!("ids must list at most {SEARCH_LIMIT_MAX} document ids"));
    }

    if args
        .reason
        .as_deref()
        .is_some_and(|reason| reason.chars().count() > FEEDBACK_REASON_MAX)
    {
        return Err(format!("reason must be at most {FEEDBACK_REASON_MAX} characters"));
    }

    let asked = documents(&args.ids)?;
    let everything = scope_arg(args.scope.as_deref())?;
    let repo = repository(server, args.project_path.as_deref())?;
    let visible = Visible {
        repo: repo.filter(|repo| !repo.is_every()).map(|repo| repo.key),
        everything,
    };

    let mut checked = Checked::default();
    let mut failure: Option<StoreError> = None;

    if !asked.is_empty() {
        match server.store()? {
            None => checked.missing = asked.clone(),
            Some(store) => {
                for doc in &asked {
                    match store.get(*doc, &Detail::default()) {
                        Ok(Some(detail)) if visible.allows(&detail) => checked.found.push(*doc),
                        Ok(_) | Err(StoreError::NotFound(_)) => checked.missing.push(*doc),
                        Err(error) => {
                            checked.unchecked.push(*doc);
                            failure = Some(error);
                        }
                    }
                }
            }
        }
    }

    // A database failure drops the connection, so the next call opens it again.
    if let Some(error) = failure {
        server.failed(&error);
    }

    // Written after the reply, once per document and session; a counter never fails the call.
    if !checked.found.is_empty() {
        server.counting.queue_useful(&checked.found, now());
    }

    Ok(text(&checked))
}

/// The documents `ids` name, each once and in the order asked: a chunk id stands for its document.
fn documents(ids: &[Value]) -> Result<Vec<i64>, String> {
    let mut docs: Vec<i64> = Vec::with_capacity(ids.len());

    for raw in ids {
        let doc = match target(raw)? {
            Target::Document(doc) | Target::Chunk(doc, _) => doc,
        };

        if !docs.contains(&doc) {
            docs.push(doc);
        }
    }

    Ok(docs)
}

fn text(checked: &Checked) -> String {
    let listed = |docs: &[i64]| docs.iter().map(|doc| doc_ref(*doc)).collect::<Vec<_>>().join(", ");

    let mut text = match checked.found.len() {
        0 => "marked no documents useful".to_owned(),
        1 => format!("marked 1 document useful: {}", listed(&checked.found)),
        count => format!("marked {count} documents useful: {}", listed(&checked.found)),
    };

    if !checked.missing.is_empty() {
        text.push_str(&format!("\nnot found: {}", listed(&checked.missing)));
    }

    if !checked.unchecked.is_empty() {
        text.push_str(&format!("\nnot checked: {}", listed(&checked.unchecked)));
    }

    text
}
