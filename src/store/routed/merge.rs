//! Merging the pages of the document list of two places.
//!
//! Each place is asked for its first `offset + limit` rows (in pages of [`LIST_PAGE_MAX`], which
//! is what a store answers at most) and the union is sorted by the sort key, the document id
//! breaking ties in the sort's direction, as every store does. The page is cut from that.

use std::cmp::Ordering;

use super::Kinds;
use crate::limits::LIST_PAGE_MAX;
use crate::store::author_list::fold;
use crate::store::{AuthorFacet, DocumentPage, DocumentRow, KnowledgeStore, ListQuery, RepoFilter, Sort, StoreError};

/// The filter to ask a place that shows `kinds` for `wanted`; `None` when it can have no match.
pub(super) fn filter_for(wanted: &RepoFilter, kinds: Kinds) -> Option<RepoFilter> {
    match (kinds.repository, kinds.global) {
        (false, false) => None,
        (true, true) => Some(wanted.clone()),
        (false, true) => match wanted {
            RepoFilter::All | RepoFilter::Global => Some(RepoFilter::Global),
            RepoFilter::Repo(_) | RepoFilter::Linked => None,
        },
        (true, false) => match wanted {
            RepoFilter::All | RepoFilter::Linked => Some(RepoFilter::Linked),
            RepoFilter::Repo(key) => Some(RepoFilter::Repo(key.clone())),
            RepoFilter::Global => None,
        },
    }
}

/// The first `want` rows of `query` (with `filter`) in `store`'s own order, its total and its
/// authors facet.
pub(super) fn fetch(
    store: &mut dyn KnowledgeStore,
    query: &ListQuery,
    filter: RepoFilter,
    want: usize,
) -> Result<DocumentPage, StoreError> {
    let mut items: Vec<DocumentRow> = Vec::new();
    let mut total;
    let mut authors = AuthorFacet::default();

    loop {
        let page = store.list(&ListQuery {
            filter: filter.clone(),
            // The facet does not change from one page to the next: only the first one reads it.
            want_authors: query.want_authors && items.is_empty(),
            offset: items.len(),
            limit: want.saturating_sub(items.len()).clamp(1, LIST_PAGE_MAX),
            ..query.clone()
        })?;

        total = page.total;

        if items.is_empty() {
            authors = page.authors;
        }

        let got = page.items.len();
        items.extend(page.items);

        if got == 0 || items.len() >= want || items.len() >= total {
            break;
        }
    }

    Ok(DocumentPage { items, total, authors })
}

/// The order of two rows of the list. A title is compared the way every store sorts it: with ASCII
/// letters folded to lower case and the rest by bytes, so the merged pages are the pages of each
/// place's own order, also for titles that are not ASCII.
pub(super) fn compare(a: &DocumentRow, b: &DocumentRow, sort: Sort, descending: bool) -> Ordering {
    if sort == Sort::AddedBy {
        return by_author(a, b, descending);
    }

    let by_key = match sort {
        Sort::Title => a.doc.title.to_ascii_lowercase().cmp(&b.doc.title.to_ascii_lowercase()),
        Sort::Updated => a.doc.updated_at.cmp(&b.doc.updated_at),
        Sort::Size => a.doc.bytes.cmp(&b.doc.bytes),
        Sort::Format => a.doc.format.cmp(&b.doc.format),
        Sort::Chunks => a.doc.chunk_count.cmp(&b.doc.chunk_count),
        Sort::Paths => paths_of(a).cmp(&paths_of(b)),
        Sort::Repos => first_repo(a).cmp(first_repo(b)),
        Sort::Useful => useful_of(a).cmp(&useful_of(b)),
        Sort::LastUseful => last_useful_of(a).cmp(&last_useful_of(b)),
        // Compared by `by_author` above.
        Sort::AddedBy => Ordering::Equal,
    };

    let ordering = by_key.then_with(|| a.doc.id.cmp(&b.doc.id));

    if descending { ordering.reverse() } else { ordering }
}

/// The order of two rows by who added them: the name with ASCII letters folded to lower case, in the
/// direction asked, the id breaking ties the same way. A row with no known author comes last in
/// both directions, as in every store.
fn by_author(a: &DocumentRow, b: &DocumentRow, descending: bool) -> Ordering {
    let name = |row: &DocumentRow| row.doc.added_by.as_ref().map(|author| fold(&author.name));
    let directed = |ordering: Ordering| if descending { ordering.reverse() } else { ordering };

    match (name(a), name(b)) {
        (None, None) => directed(a.doc.id.cmp(&b.doc.id)),
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a_name), Some(b_name)) => directed(a_name.cmp(&b_name).then_with(|| a.doc.id.cmp(&b.doc.id))),
    }
}

/// How many path globs the links of a document carry together, as the stores sort it.
fn paths_of(row: &DocumentRow) -> usize {
    row.links.iter().map(|link| link.globs.len()).sum()
}

/// The first repository key a document links to, `""` for a global one.
fn first_repo(row: &DocumentRow) -> &str {
    row.links
        .iter()
        .map(|link| link.repo_key.as_str())
        .min()
        .unwrap_or_default()
}

/// How often a document was found useful: 0 without counters, as the stores sort it.
fn useful_of(row: &DocumentRow) -> i64 {
    row.usage.map_or(0, |usage| usage.useful)
}

/// When a document was last found useful: 0 when never or without counters, as the stores sort it.
fn last_useful_of(row: &DocumentRow) -> i64 {
    row.usage.and_then(|usage| usage.last_useful_at).unwrap_or(0)
}
