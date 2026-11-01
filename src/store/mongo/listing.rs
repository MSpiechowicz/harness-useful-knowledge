//! Reading documents: the list with its filters and order, one document with its parts, and the
//! counts per repository and overall.

use mongodb::bson::{Bson, Document as Row, doc};

use super::ctx::{Ctx, Fail};
use super::documents::{document, links};
use super::schema::{CHUNKS, DOCUMENTS};
use super::{int, terms, text};
use crate::limits::{LIST_PAGE_MAX, QUERY_TOKENS_MAX};
use crate::store::{
    Chunk, ChunkMatch, ChunkMatches, Detail, DocumentDetail, DocumentPage, DocumentRow, ListQuery, RepoCounts,
    RepoFilter,
};
use crate::store::{Section, Sort, Stats, StoreError};

/// One page of the documents `query` keeps, with the total over all pages.
pub(super) fn list(ctx: &mut Ctx<'_>, query: &ListQuery) -> Result<DocumentPage, Fail> {
    let filter = conditions(ctx, query)?;
    let total = ctx.count(DOCUMENTS, filter.clone())?;

    let key = match query.sort {
        Sort::Title => "title_key",
        Sort::Updated => "updated_at",
        Sort::Size => "bytes",
        Sort::Format => "format",
        Sort::Chunks => "chunk_count",
        // An array sorts by its smallest element ascending, an empty one (global) first.
        Sort::Repos => "repo_keys",
    };
    let direction = if query.descending { -1 } else { 1 };
    let sort = doc! { key: direction, "_id": direction };

    let skip = u64::try_from(query.offset).unwrap_or(u64::MAX);
    let limit = i64::try_from(query.limit.min(LIST_PAGE_MAX)).unwrap_or(0);

    // A limit of 0 asks for no limit in a query.
    let rows = if limit == 0 {
        Vec::new()
    } else {
        ctx.find(DOCUMENTS, filter, (sort, skip, limit), Row::new())?
    };

    let items = rows
        .iter()
        .map(|row| {
            Ok(DocumentRow {
                doc: document(row)?,
                links: links(row),
            })
        })
        .collect::<Result<_, StoreError>>()?;

    Ok(DocumentPage {
        items,
        total: usize::try_from(total).unwrap_or(usize::MAX),
    })
}

/// The filter of `query`: the repository, the format and the text.
fn conditions(ctx: &mut Ctx<'_>, query: &ListQuery) -> Result<Row, Fail> {
    let mut all: Vec<Row> = Vec::new();

    match &query.filter {
        RepoFilter::All => {}
        RepoFilter::Global => all.push(doc! { "global": true }),
        RepoFilter::Linked => all.push(doc! { "global": false }),
        RepoFilter::Repo(key) => all.push(doc! { "repo_keys": key }),
    }

    if let Some(format) = &query.format {
        all.push(doc! { "format": format });
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

/// The most chunks a phrase filter checks the text of: the chunks that have every word.
const PHRASE_CANDIDATES_MAX: i64 = 5000;

/// Keeps the documents whose title contains `wanted` (any case), or that have a chunk whose
/// heading or text has its words next to each other in that order, the last as a prefix.
///
/// One word is a prefix of a word of a chunk's text or title (the `terms` arrays decide). Several
/// words cannot be asked of the arrays, so the chunks that have every word are the candidates
/// (at most [`PHRASE_CANDIDATES_MAX`]) and their heading and text are checked for the phrase.
fn text_condition(ctx: &mut Ctx<'_>, wanted: &str) -> Result<Row, Fail> {
    let in_title = doc! { "title": { "$regex": regex::escape(wanted), "$options": "i" } };

    let phrase = terms::phrase_words(wanted);
    let words: Vec<String> = terms::of_text(wanted).into_iter().take(QUERY_TOKENS_MAX).collect();

    if words.is_empty() {
        return Ok(in_title);
    }

    let ids: Vec<i64> = if phrase.len() == 1 {
        // The word is a prefix of a word of the chunk's text or of its title.
        let prefix = doc! { "$regex": format!("^{}", regex::escape(&words[0])) };
        let either = doc! { "$or": [{ "terms": prefix.clone() }, { "title_terms": prefix }] };
        let found = ctx.aggregate(
            CHUNKS,
            vec![doc! { "$match": either }, doc! { "$group": { "_id": "$doc_id" } }],
        )?;

        found.iter().map(|group| int(group, "_id")).collect()
    } else {
        let each: Vec<Row> = words
            .iter()
            .map(|word| doc! { "terms": { "$regex": format!("^{}", regex::escape(word)) } })
            .collect();
        let projection = doc! { "doc_id": 1, "heading_path": 1, "text": 1 };
        let candidates = ctx.find(
            CHUNKS,
            doc! { "$and": each },
            (doc! { "_id": 1 }, 0, PHRASE_CANDIDATES_MAX),
            projection,
        )?;

        let mut ids: Vec<i64> = candidates
            .iter()
            .filter(|chunk| has_phrase_in(chunk, &phrase))
            .map(|chunk| int(chunk, "doc_id"))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };

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
        doc: document(&row)?,
        links: links(&row),
        sections,
        chunk,
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
    let global = ctx.count(DOCUMENTS, doc! { "global": true })?;

    let groups = ctx.aggregate(
        DOCUMENTS,
        vec![
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
        vec![doc! { "$group": { "_id": Bson::Null, "documents": { "$sum": 1_i64 }, "bytes": { "$sum": "$bytes" } } }],
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
        global_documents: count(ctx, DOCUMENTS, doc! { "global": true })?,
        repositories: repositories.first().map_or(0, |found| size(found, "repositories")),
        generation,
        schema_version,
        model_id,
    })
}

fn count(ctx: &mut Ctx<'_>, collection: &str, filter: Row) -> Result<usize, Fail> {
    Ok(usize::try_from(ctx.count(collection, filter)?).unwrap_or(usize::MAX))
}

/// A stored count or position.
fn size(row: &Row, key: &str) -> usize {
    usize::try_from(int(row, key)).unwrap_or(0)
}
