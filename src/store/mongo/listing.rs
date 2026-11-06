//! Reading documents: the list with its filters and order, one document with its parts, and the
//! counts per repository and overall.

use mongodb::bson::{Bson, Document as Row, doc};

use super::ctx::{Ctx, Fail};
use super::documents::{document, links, pending_of_many, usage};
use super::schema::{CHUNKS, DOCUMENTS};
use super::{int, opt_int, terms, text};
use crate::limits::{AUTHOR_GROUPS_READ_MAX, LIST_PAGE_MAX, QUERY_TOKENS_MAX};
use crate::store::author_list::NAME_READ_MAX;
use crate::store::{AuthorFacet, AuthorFilter, Section, Sort, Stats, Status, StatusFilter, StoreError, UsefulFilter};
use crate::store::{
    Chunk, ChunkMatch, ChunkMatches, Detail, DocumentDetail, DocumentPage, DocumentRow, ListQuery, RepoCounts,
    RepoFilter,
};

/// One page of the documents `query` keeps, with the total over all pages.
pub(super) fn list(ctx: &mut Ctx<'_>, query: &ListQuery) -> Result<DocumentPage, Fail> {
    let base = conditions(ctx, query)?;
    // Only a list that asks for the facet pays for it.
    let authors = if query.want_authors {
        author_facet(ctx, &base)?
    } else {
        AuthorFacet::default()
    };
    let filter = with_author(base, query.author.as_ref());
    let total = ctx.count(DOCUMENTS, filter.clone())?;

    let key = match query.sort {
        Sort::Title => "title_key",
        Sort::Updated => "updated_at",
        Sort::Size => "bytes",
        Sort::Format => "format",
        Sort::Chunks => "chunk_count",
        // An array sorts by its smallest element ascending, an empty one (global) first.
        Sort::Repos => "repo_keys",
        // A count no field holds, and a document nobody rated has no counters (it sorts as 0): these
        // sort on a field of their own.
        Sort::Paths | Sort::Useful | Sort::LastUseful => COMPUTED_ORDER,
        // Two fields of their own, see below.
        Sort::AddedBy => AUTHOR_RANK,
    };
    let direction = if query.descending { -1 } else { 1 };
    let sort = if query.sort == Sort::AddedBy {
        // No known author comes last in both directions.
        doc! { AUTHOR_RANK: 1, AUTHOR_ORDER: direction, "_id": direction }
    } else {
        doc! { key: direction, "_id": direction }
    };

    let skip = u64::try_from(query.offset).unwrap_or(u64::MAX);
    let limit = i64::try_from(query.limit.min(LIST_PAGE_MAX)).unwrap_or(0);

    // A limit of 0 asks for no limit in a query.
    let rows = if limit == 0 {
        Vec::new()
    } else if key == COMPUTED_ORDER || key == AUTHOR_RANK {
        ctx.aggregate(DOCUMENTS, computed_pipeline(query.sort, filter, sort, skip, limit))?
    } else {
        ctx.find(DOCUMENTS, filter, (sort, skip, limit), Row::new())?
    };

    let docs = rows
        .iter()
        .map(|row| Ok((document(row)?, links(row), usage(row))))
        .collect::<Result<Vec<_>, StoreError>>()?;

    // The pending version of a live document is shown on its row.
    let ids: Vec<i64> = docs
        .iter()
        .filter(|(doc, _, _)| doc.status.is_live())
        .map(|(doc, _, _)| doc.id)
        .collect();
    let mut pending = pending_of_many(ctx, &ids)?;

    let items = docs
        .into_iter()
        .map(|(doc, links, usage)| DocumentRow {
            pending: pending.remove(&doc.id),
            doc,
            links,
            usage,
        })
        .collect();

    Ok(DocumentPage {
        items,
        total: usize::try_from(total).unwrap_or(usize::MAX),
        authors,
    })
}

/// The field a page is sorted on for the sorts by a computed value.
const COMPUTED_ORDER: &str = "computed_order";

/// The fields of the sort by who added a document: 0 for a document with a known author and 1 for
/// one without, and the name with ASCII letters folded to lower case.
const AUTHOR_RANK: &str = "author_rank";
const AUTHOR_ORDER: &str = "author_order";

/// The fields [`computed_pipeline`] adds for `sort`: [`COMPUTED_ORDER`] (the path globs of all the
/// links together, or a counter, a missing one being 0), or the two of the author.
fn computed_fields(sort: Sort) -> Row {
    if sort == Sort::AddedBy {
        return doc! {
            AUTHOR_RANK: { "$cond": [named_author(), 0_i64, 1_i64] },
            AUTHOR_ORDER: { "$cond": [named_author(), { "$toLower": cut_name() }, ""] },
        };
    }

    if sort == Sort::Paths {
        let globs = doc! { "$size": { "$ifNull": ["$$link.globs", []] } };
        let paths = doc! { "$sum": { "$map": { "input": { "$ifNull": ["$links", []] }, "as": "link", "in": globs } } };

        return doc! { COMPUTED_ORDER: paths };
    }

    let counter = if sort == Sort::Useful {
        "$useful"
    } else {
        "$last_useful_at"
    };

    doc! { COMPUTED_ORDER: { "$cond": [{ "$in": [{ "$type": counter }, ["int", "long", "double", "decimal"]] }, counter, 0_i64] } }
}

/// The name of a document cut to [`NAME_READ_MAX`] characters: a longer one is not a name, and
/// reaches this process cut, as a sort key or a group.
fn cut_name() -> Row {
    doc! { "$substrCP": ["$added_by_name", 0_i64, i64::try_from(NAME_READ_MAX).unwrap_or(i64::MAX)] }
}

/// True for a document whose `added_by_name` is a name: text, and not empty.
fn named_author() -> Row {
    doc! { "$and": [
        { "$eq": [{ "$type": "$added_by_name" }, "string"] },
        { "$ne": ["$added_by_name", ""] },
    ] }
}

/// `filter` and the match of the author filter. Kept apart from [`conditions`]: the facet of the
/// authors honours every filter but this one, and `conditions` runs reads of its own.
fn with_author(filter: Row, author: Option<&AuthorFilter>) -> Row {
    let matching = match author {
        None => return filter,
        Some(AuthorFilter::Named(name)) => doc! { "added_by_name": name },
        Some(AuthorFilter::Unknown) => doc! { "added_by_name": { "$in": [Bson::Null, ""] } },
    };

    doc! { "$and": [filter, matching] }
}

/// How many documents of `filter` each person added, and how many have no known author. A name that
/// is missing, empty or not text is no known author (one group), the groups are cut to what one read
/// takes with the unknown one first, and sorted again last as DocumentDB keeps the order of a `$sort`
/// only when it is the last stage.
fn author_facet(ctx: &mut Ctx<'_>, filter: &Row) -> Result<AuthorFacet, Fail> {
    let order = doc! { "unknown": -1, "count": -1, "_id": 1 };
    let pipeline = vec![
        doc! { "$match": filter.clone() },
        doc! { "$group": {
            "_id": { "$cond": [named_author(), cut_name(), Bson::Null] },
            "count": { "$sum": 1_i64 },
        } },
        doc! { "$addFields": { "unknown": { "$cond": [{ "$eq": ["$_id", Bson::Null] }, 1_i64, 0_i64] } } },
        doc! { "$sort": order.clone() },
        doc! { "$limit": i64::try_from(AUTHOR_GROUPS_READ_MAX).unwrap_or(i64::MAX) },
        doc! { "$sort": order },
    ];

    let groups = ctx
        .aggregate(DOCUMENTS, pipeline)?
        .iter()
        .map(|row| {
            (
                text(row, "_id"),
                usize::try_from(int(row, "count")).unwrap_or(usize::MAX),
            )
        })
        .collect::<Vec<_>>();

    Ok(AuthorFacet::from_groups(groups))
}

/// The pipeline of a page sorted by a computed field. The page is sorted again at the end: Amazon
/// DocumentDB keeps the order of a `$sort` only when it is the last stage.
fn computed_pipeline(sort: Sort, filter: Row, order: Row, skip: u64, limit: i64) -> Vec<Row> {
    vec![
        doc! { "$match": filter },
        doc! { "$addFields": computed_fields(sort) },
        doc! { "$sort": order.clone() },
        doc! { "$skip": i64::try_from(skip).unwrap_or(i64::MAX) },
        doc! { "$limit": limit },
        doc! { "$sort": order },
    ]
}

/// The match that keeps the documents of `filter` by usefulness (nothing for no filter). A
/// document nobody rated has no `useful` field, which counts as 0: `$not` of `$gte` also matches a
/// missing field (and one that holds no number). `never` needs the document shown often enough.
fn useful_match(filter: Option<UsefulFilter>) -> Option<Row> {
    match filter? {
        UsefulFilter::Ever => Some(doc! { "useful": { "$gte": 1_i64 } }),
        UsefulFilter::Never => Some(doc! {
            "shown": { "$gte": UsefulFilter::SHOWN_MIN },
            "useful": { "$not": { "$gte": 1_i64 } },
        }),
    }
}

/// The ids of the live documents that have a pending version (in `state`, when one is named).
fn with_pending(ctx: &mut Ctx<'_>, state: Option<Status>) -> Result<Vec<i64>, Fail> {
    let mut filter = doc! { "pending_of": { "$ne": Bson::Null } };

    if let Some(state) = state {
        filter.insert("status", state.as_str());
    }

    let rows = ctx.find(DOCUMENTS, filter, (doc! { "_id": 1 }, 0, 0), doc! { "pending_of": 1 })?;

    Ok(rows.iter().filter_map(|row| opt_int(row, "pending_of")).collect())
}

/// The condition that keeps the rows `status` lists: a live document counts for the state of its
/// pending version too, as that is the state the person waits on.
fn status_condition(ctx: &mut Ctx<'_>, status: StatusFilter) -> Result<Row, Fail> {
    Ok(match status {
        StatusFilter::All => doc! { "pending_of": Bson::Null },
        StatusFilter::Live => doc! { "status": "live" },
        StatusFilter::Pending => doc! { "status": "live", "_id": { "$in": with_pending(ctx, None)? } },
        StatusFilter::Draft | StatusFilter::Waiting | StatusFilter::ChangesRequested => {
            let wanted = status.state().map_or("draft", Status::as_str);
            let live = with_pending(ctx, status.state())?;

            doc! { "$or": [
                { "pending_of": Bson::Null, "status": wanted },
                { "status": "live", "_id": { "$in": live } },
            ] }
        }
    })
}

/// The filter of `query`: the state, the repository, the format, how useful and the text.
fn conditions(ctx: &mut Ctx<'_>, query: &ListQuery) -> Result<Row, Fail> {
    let mut all: Vec<Row> = vec![status_condition(ctx, query.status)?];

    match &query.filter {
        RepoFilter::All => {}
        RepoFilter::Global => all.push(doc! { "global": true }),
        RepoFilter::Linked => all.push(doc! { "global": false }),
        RepoFilter::Repo(key) => all.push(doc! { "repo_keys": key }),
    }

    if let Some(format) = &query.format {
        all.push(doc! { "format": format });
    }

    if let Some(useful) = useful_match(query.useful) {
        all.push(useful);
    }

    let wanted = query.text.trim();

    if !wanted.is_empty() {
        all.push(text_condition(ctx, wanted)?);
    }

    Ok(match all.len() {
        0 => Row::new(),
        _ => doc! { "$and": all },
    })
}

/// Keeps the documents whose title contains `wanted` (any case), or that have a chunk whose
/// heading or text has its words next to each other in that order, the last as a prefix.
///
/// One word is a prefix of a word of a chunk's text or title (the `terms` arrays decide). Several
/// words are matched by the server against the chunk's `phrase_text` (see [`terms::phrase_text`]),
/// so every chunk is looked at and nothing is cut: the total and the pages are exact. The `terms`
/// index narrows the chunks first by the words of two letters or more (a one-letter prefix matches
/// nearly every chunk and narrows nothing, a phrase of one-letter words alone scans the chunks).
fn text_condition(ctx: &mut Ctx<'_>, wanted: &str) -> Result<Row, Fail> {
    let in_title = doc! { "title": { "$regex": regex::escape(wanted), "$options": "i" } };

    let phrase = terms::phrase_words(wanted);
    let words: Vec<String> = terms::of_text(wanted).into_iter().take(QUERY_TOKENS_MAX).collect();

    if words.is_empty() {
        return Ok(in_title);
    }

    let chunk_filter = if phrase.len() == 1 {
        // The word is a prefix of a word of the chunk's text or of its title.
        let prefix = doc! { "$regex": format!("^{}", regex::escape(&words[0])) };

        doc! { "$or": [{ "terms": prefix.clone() }, { "title_terms": prefix }] }
    } else {
        let mut each: Vec<Row> = phrase
            .iter()
            .filter(|word| word.chars().count() >= 2)
            .map(|word| doc! { "terms": { "$regex": format!("^{}", regex::escape(word)) } })
            .collect();
        each.push(doc! { "phrase_text": { "$regex": terms::phrase_regex(&phrase) } });

        doc! { "$and": each }
    };

    let found = ctx.aggregate(
        CHUNKS,
        vec![doc! { "$match": chunk_filter }, doc! { "$group": { "_id": "$doc_id" } }],
    )?;
    let ids: Vec<i64> = found.iter().map(|group| int(group, "_id")).collect();

    Ok(doc! { "$or": [in_title, { "_id": { "$in": ids } }] })
}

/// True when the heading path or the text of `chunk` holds the `phrase`.
fn has_phrase_in(chunk: &Row, phrase: &[String]) -> bool {
    ["heading_path", "text"]
        .iter()
        .any(|field| text(chunk, field).is_some_and(|value| terms::has_phrase(&value, phrase)))
}

/// The chunks of document `id` whose heading path or text holds the words of `wanted` as a
/// phrase, by position, at most `limit`. The chunks of one document are few, so they are read and
/// checked here, with the folding of the `terms` arrays.
pub(super) fn matching_chunks(ctx: &mut Ctx<'_>, id: i64, wanted: &str, limit: usize) -> Result<ChunkMatches, Fail> {
    let phrase = terms::phrase_words(wanted);

    if phrase.is_empty() {
        return Ok(ChunkMatches::default());
    }

    let projection = doc! { "ordinal": 1, "heading_path": 1, "text": 1 };
    let rows = ctx.find(CHUNKS, doc! { "doc_id": id }, (doc! { "ordinal": 1 }, 0, 0), projection)?;

    let all: Vec<ChunkMatch> = rows
        .iter()
        .filter(|row| has_phrase_in(row, &phrase))
        .map(|row| ChunkMatch {
            chunk_id: int(row, "_id"),
            ordinal: size(row, "ordinal"),
            heading_path: text(row, "heading_path").unwrap_or_default(),
            text: text(row, "text").unwrap_or_default(),
        })
        .collect();

    Ok(ChunkMatches {
        total: all.len(),
        chunks: all.into_iter().take(limit).collect(),
    })
}

/// Document `id` with its links, and what `detail` asks for.
pub(super) fn get(ctx: &mut Ctx<'_>, id: i64, detail: &Detail) -> Result<Option<DocumentDetail>, Fail> {
    let Some(row) = ctx.find_one(DOCUMENTS, doc! { "_id": id }, Row::new())? else {
        return Ok(None);
    };

    let doc = document(&row)?;

    // Agents read live documents only: a draft or a pending version is not there for them.
    if !detail.drafts && !doc.status.is_live() {
        return Ok(None);
    }

    let pending = if detail.drafts {
        pending_of_many(ctx, &[id])?.remove(&id)
    } else {
        None
    };

    let sections = if detail.sections {
        Some(sections(ctx, id)?)
    } else {
        None
    };

    let chunk = match detail.chunk {
        Some(chunk_id) => Some(chunk(ctx, id, chunk_id)?),
        None => None,
    };

    Ok(Some(DocumentDetail {
        doc,
        links: links(&row),
        usage: usage(&row),
        sections,
        chunk,
        pending,
    }))
}

fn sections(ctx: &mut Ctx<'_>, id: i64) -> Result<Vec<Section>, Fail> {
    let projection = doc! { "ordinal": 1, "heading_path": 1, "tokens": 1, "char_start": 1, "char_end": 1 };
    let rows = ctx.find(CHUNKS, doc! { "doc_id": id }, (doc! { "ordinal": 1 }, 0, 0), projection)?;

    Ok(rows
        .iter()
        .map(|row| Section {
            chunk_id: int(row, "_id"),
            ordinal: size(row, "ordinal"),
            heading_path: text(row, "heading_path").unwrap_or_default(),
            tokens: size(row, "tokens"),
            char_start: size(row, "char_start"),
            char_end: size(row, "char_end"),
        })
        .collect())
}

/// Chunk `chunk_id` of document `id`, whole and without its vector.
fn chunk(ctx: &mut Ctx<'_>, id: i64, chunk_id: i64) -> Result<Chunk, Fail> {
    let filter = doc! { "_id": chunk_id, "doc_id": id };
    let found = ctx.find_one(CHUNKS, filter, doc! { "embedding": 0, "terms": 0, "title_terms": 0 })?;

    let row = found.ok_or_else(|| StoreError::NotFound(format!("chunk {chunk_id} is not in document {id}")))?;

    Ok(Chunk {
        id: int(&row, "_id"),
        doc_id: int(&row, "doc_id"),
        ordinal: size(&row, "ordinal"),
        heading_path: text(&row, "heading_path").unwrap_or_default(),
        text: text(&row, "text").unwrap_or_default(),
        chunk_sha: text(&row, "chunk_sha").unwrap_or_default(),
        tokens: size(&row, "tokens"),
        char_start: size(&row, "char_start"),
        char_end: size(&row, "char_end"),
        embedding: None,
        model_id: None,
    })
}

pub(super) fn repos(ctx: &mut Ctx<'_>) -> Result<RepoCounts, Fail> {
    // A pending version is not a document of a repository: the live one is.
    let global = ctx.count(DOCUMENTS, doc! { "global": true, "pending_of": Bson::Null })?;

    let groups = ctx.aggregate(
        DOCUMENTS,
        vec![
            doc! { "$match": { "pending_of": Bson::Null } },
            doc! { "$unwind": "$repo_keys" },
            doc! { "$group": { "_id": "$repo_keys", "docs": { "$sum": 1_i64 } } },
            doc! { "$sort": { "docs": -1, "_id": 1 } },
        ],
    )?;

    Ok(RepoCounts {
        global: usize::try_from(global).unwrap_or(usize::MAX),
        repos: groups
            .iter()
            .filter_map(|group| Some((text(group, "_id")?, size(group, "docs"))))
            .collect(),
    })
}

pub(super) fn stats(
    ctx: &mut Ctx<'_>,
    model_id: Option<String>,
    generation: i64,
    schema_version: i64,
) -> Result<Stats, Fail> {
    let totals = ctx.aggregate(
        DOCUMENTS,
        vec![
            doc! { "$match": { "pending_of": Bson::Null } },
            doc! { "$group": { "_id": Bson::Null, "documents": { "$sum": 1_i64 }, "bytes": { "$sum": "$bytes" } } },
        ],
    )?;
    let (documents, bytes) = totals
        .first()
        .map_or((0, 0), |totals| (size(totals, "documents"), int(totals, "bytes")));

    let mut embedded = doc! { "embedding": { "$exists": true } };

    if let Some(model) = &model_id {
        embedded.insert("model_id", model);
    }

    let repositories = ctx.aggregate(
        DOCUMENTS,
        vec![
            doc! { "$match": { "pending_of": Bson::Null } },
            doc! { "$unwind": "$repo_keys" },
            doc! { "$group": { "_id": "$repo_keys" } },
            doc! { "$count": "repositories" },
        ],
    )?;

    Ok(Stats {
        documents,
        chunks: count(ctx, CHUNKS, Row::new())?,
        embedded_chunks: count(ctx, CHUNKS, embedded)?,
        bytes: u64::try_from(bytes).unwrap_or(0),
        global_documents: count(ctx, DOCUMENTS, doc! { "global": true, "pending_of": Bson::Null })?,
        repositories: repositories.first().map_or(0, |found| size(found, "repositories")),
        generation,
        schema_version,
        model_id,
        drafts: count(ctx, DOCUMENTS, doc! { "status": "draft" })?,
        waiting: count(ctx, DOCUMENTS, doc! { "status": "waiting" })?,
        changes_requested: count(ctx, DOCUMENTS, doc! { "status": "changesRequested" })?,
        pending: count(ctx, DOCUMENTS, doc! { "pending_of": { "$ne": Bson::Null } })?,
    })
}

fn count(ctx: &mut Ctx<'_>, collection: &str, filter: Row) -> Result<usize, Fail> {
    Ok(usize::try_from(ctx.count(collection, filter)?).unwrap_or(usize::MAX))
}

/// A stored count or position.
fn size(row: &Row, key: &str) -> usize {
    usize::try_from(int(row, key)).unwrap_or(0)
}
