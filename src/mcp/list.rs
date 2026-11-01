//! `knowledge_list`: the documents of the repository and the global ones, a page at a time.

use serde::Deserialize;

use super::Server;
use super::reply::{cut, date};
use super::scope::{NO_PROJECT, repository};
use crate::search::render::{FRAME, LIST_TAG, block, doc_ref};
use crate::store::{DocumentRow, KnowledgeStore, ListQuery, RepoFilter, Sort, StoreError, clean};

/// Documents per page.
const PAGE: usize = 20;

/// The last page that can be asked for: a page number an agent makes up (it can be any integer)
/// must not overflow the offset.
const PAGE_MAX: usize = 1_000_000;

/// Most characters of a list query.
const QUERY_CHARS_MAX: usize = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListArgs {
    project_path: Option<String>,
    query: Option<String>,
    page: Option<i64>,
}

pub(super) fn list(server: &mut Server, args: ListArgs) -> Result<String, String> {
    let text = clean::line(args.query.as_deref().unwrap_or_default());

    if text.chars().count() > QUERY_CHARS_MAX {
        return Err(format!("query must be at most {QUERY_CHARS_MAX} characters"));
    }

    let page = usize::try_from(args.page.unwrap_or(1).max(1)).map_or(PAGE_MAX, |page| page.min(PAGE_MAX));
    let repo = repository(server, args.project_path.as_deref())?;
    let key = repo
        .as_ref()
        .filter(|repo| !repo.is_every())
        .map(|repo| repo.key.clone());

    let listed = {
        let Some(store) = server.store()? else {
            return Ok("no documents yet".to_owned());
        };

        collect(store, key.as_deref(), &text, (page - 1).saturating_mul(PAGE))
    };

    let (rows, total) = listed.map_err(|error| server.failed(&error))?;

    if rows.is_empty() {
        return Ok(if total == 0 {
            "no documents".to_owned()
        } else {
            format!("no page {page}, there are {total} documents")
        });
    }

    let lines: Vec<String> = rows.iter().map(line).collect();
    let attributes = [("page", page.to_string()), ("total", total.to_string())];
    let mut reply = format!("{FRAME}\n{}", block(LIST_TAG, &attributes, &lines.join("\n")));

    if key.is_none() {
        reply.push('\n');
        reply.push_str(NO_PROJECT);
    }

    if page.saturating_mul(PAGE) < total {
        reply.push_str(&format!("\nNext page: {}", page + 1));
    }

    Ok(reply)
}

/// The page at `offset` of the documents linked to `key` (when known), then the global ones, and the
/// total of both.
fn collect(
    store: &mut dyn KnowledgeStore,
    key: Option<&str>,
    text: &str,
    offset: usize,
) -> Result<(Vec<DocumentRow>, usize), StoreError> {
    let query = |filter: RepoFilter, offset: usize, limit: usize| ListQuery {
        filter,
        format: None,
        text: text.to_owned(),
        sort: Sort::Updated,
        descending: true,
        offset,
        limit,
    };

    let mut rows = Vec::new();
    let mut linked_total = 0;

    if let Some(key) = key {
        let linked = store.list(&query(RepoFilter::Repo(key.to_owned()), offset, PAGE))?;
        linked_total = linked.total;
        rows = linked.items;
    }

    let room = PAGE - rows.len();
    let global = store.list(&query(
        RepoFilter::Global,
        offset.saturating_sub(linked_total),
        room.max(1),
    ))?;

    if room > 0 {
        rows.extend(global.items.into_iter().take(room));
    }

    Ok((rows, linked_total + global.total))
}

/// `12 | Title | markdown, 14 chunks | updated 2026-10-30 | git:h/o/r or global`.
fn line(row: &DocumentRow) -> String {
    let doc = &row.doc;
    let repos: Vec<&str> = row.links.iter().map(|link| link.repo_key.as_str()).collect();
    let place = if repos.is_empty() {
        "global".to_owned()
    } else {
        repos.join(", ")
    };

    format!(
        "{} | {} | {}, {} chunks | updated {} | {}",
        doc_ref(doc.id),
        cut(&clean::line(&doc.title), 120),
        doc.format,
        doc.chunk_count,
        date(doc.updated_at),
        cut(&place, 160)
    )
}
