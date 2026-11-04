//! Writing and finding documents: what `put_document`, `set_links`, `rename` and `delete` do to
//! the two collections, the lookups an import makes, and how a document and a chunk are stored.

use std::cell::Cell;
use std::collections::HashMap;

use mongodb::bson::{Bson, Document as Row, doc};

use super::connection::{DUPLICATE_KEY, error_code};
use super::ctx::{Ctx, Fail, long};
use super::schema::{CHUNKS, COMMENTS, DOCUMENTS, EVENTS, FILES, REVIEW_DRAFT, REVIEW_LIVE, REVIEW_PENDING, SNAPSHOTS};
use super::{int, opt_int, strings, terms, text};
use crate::chunk::passage;
use crate::limits::{DELETE_IDS_MAX, EMBED_BATCH_MAX};
use crate::store::review::{RENAMED_COMMENT, actor};
use crate::store::{Author, Chunk, DIMS, DocUsage, Document, EventKind, Link, Origin, PendingChunk, PendingVersion};
use crate::store::{Status, StoreError, Visibility};

/// What a list sorted by title compares: the title with ASCII letters folded to lower case, like
/// every other store does (see [`crate::store::routed`]'s merge). Compared bytewise.
pub(super) fn title_key(title: &str) -> String {
    title.to_ascii_lowercase()
}

/// The fields of a document that change with an import or a rename (everything but `created_at`).
fn fields(doc: &Document, title: &str, chunks: usize, links: &[Link]) -> Row {
    let mut fields = doc! {
        "title": title,
        "title_key": title_key(title),
        "source_name": &doc.source_name,
        "source_key": &doc.source_key,
        "format": &doc.format,
        "content_sha256": &doc.content_sha256,
        "file_sha256": &doc.file_sha256,
        "bytes": i64::try_from(doc.bytes).unwrap_or(i64::MAX),
        "chunk_count": long(chunks),
        "origin": doc.origin.as_str(),
        "warnings": &doc.warnings,
        "updated_at": doc.updated_at,
        "updated_by_name": doc.updated_by.as_ref().map(|author| author.name.as_str()),
        "updated_by_via": doc.updated_by.as_ref().and_then(|author| author.via.as_deref()),
    };

    fields.extend(link_fields(links));
    fields
}

/// The links of a document and the two fields the list filters by: the repository keys and
/// whether there is none (a global document). A chunk carries the same two for the search.
pub(super) fn link_fields(links: &[Link]) -> Row {
    let stored: Vec<Row> = links
        .iter()
        .map(|link| doc! { "repo_key": &link.repo_key, "globs": &link.globs })
        .collect();
    let keys: Vec<&str> = links.iter().map(|link| link.repo_key.as_str()).collect();

    doc! { "links": stored, "repo_keys": keys, "global": links.is_empty() }
}

/// The two fields of a chunk that follow its document's links.
pub(super) fn scope_fields(links: &[Link]) -> Row {
    let keys: Vec<&str> = links.iter().map(|link| link.repo_key.as_str()).collect();

    doc! { "repo_keys": keys, "global": links.is_empty() }
}

/// A [`Document`] from a stored one.
pub(super) fn document(row: &Row) -> Result<Document, StoreError> {
    let origin = text(row, "origin")
        .and_then(|origin| Origin::parse(&origin))
        .ok_or_else(|| StoreError::Backend("a stored document has an unknown origin".to_owned()))?;

    Ok(Document {
        id: int(row, "_id"),
        title: text(row, "title").unwrap_or_default(),
        source_name: text(row, "source_name").unwrap_or_default(),
        source_key: text(row, "source_key").unwrap_or_default(),
        format: text(row, "format").unwrap_or_default(),
        content_sha256: text(row, "content_sha256").unwrap_or_default(),
        file_sha256: text(row, "file_sha256").unwrap_or_default(),
        bytes: u64::try_from(int(row, "bytes")).unwrap_or(0),
        chunk_count: usize::try_from(int(row, "chunk_count")).unwrap_or(0),
        created_at: int(row, "created_at"),
        updated_at: int(row, "updated_at"),
        origin,
        warnings: strings(row, "warnings"),
        added_by: Author::stored(text(row, "added_by_name"), text(row, "added_by_via")),
        updated_by: Author::stored(text(row, "updated_by_name"), text(row, "updated_by_via")),
        // A row from before the review existed was live.
        status: match text(row, "status") {
            Some(stored) => Status::parse(&stored)
                .ok_or_else(|| StoreError::Backend("a stored document has an unknown status".to_owned()))?,
            None => Status::Live,
        },
        pending_of: opt_int(row, "pending_of"),
        review_rev: int(row, "review_rev"),
    })
}

/// The summary of a pending row.
pub(super) fn pending_version(doc: &Document) -> PendingVersion {
    PendingVersion {
        id: doc.id,
        status: doc.status,
        review_rev: doc.review_rev,
        updated_by: doc.updated_by.clone(),
        updated_at: doc.updated_at,
    }
}

/// The pending versions of the live documents `ids`, by live id.
pub(super) fn pending_of_many(ctx: &mut Ctx<'_>, ids: &[i64]) -> Result<HashMap<i64, PendingVersion>, Fail> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows = ctx.find(
        DOCUMENTS,
        doc! { "pending_of": { "$in": ids } },
        (doc! { "_id": 1 }, 0, 0),
        Row::new(),
    )?;
    let mut found = HashMap::new();

    for row in &rows {
        let pending = document(row)?;

        if let Some(live) = pending.pending_of {
            found.insert(live, pending_version(&pending));
        }
    }

    Ok(found)
}

/// The `review` a chunk of a document in `status` (a pending version or not) carries: waiting and
/// changes requested are `draft` too, so a change between those states rewrites no chunk.
pub(super) fn review_of(status: Status, pending: bool) -> &'static str {
    if pending {
        REVIEW_PENDING
    } else if status.is_live() {
        REVIEW_LIVE
    } else {
        REVIEW_DRAFT
    }
}

/// The condition that keeps the rows `visibility` lets a lookup see.
pub(super) fn seen(visibility: Visibility) -> Row {
    match visibility {
        Visibility::Live => doc! { "status": "live" },
        Visibility::WithDrafts => doc! { "pending_of": Bson::Null },
    }
}

/// The `review` values of the chunks `visibility` lets a lookup see.
pub(super) fn seen_chunks(visibility: Visibility) -> Row {
    match visibility {
        Visibility::Live => doc! { "review": REVIEW_LIVE },
        Visibility::WithDrafts => doc! { "review": { "$in": [REVIEW_LIVE, REVIEW_DRAFT] } },
    }
}

/// The review columns of a stored row.
pub(super) struct RowState {
    pub status: Status,
    pub pending_of: Option<i64>,
    pub review_rev: i64,
    pub format: String,
}

/// The review fields of document `id`; `None` when there is no such document.
pub(super) fn row_state(ctx: &mut Ctx<'_>, id: i64) -> Result<Option<RowState>, Fail> {
    let projection = doc! { "status": 1, "pending_of": 1, "review_rev": 1, "format": 1 };
    let Some(row) = ctx.find_one(DOCUMENTS, doc! { "_id": id }, projection)? else {
        return Ok(None);
    };

    let status = match text(&row, "status") {
        Some(stored) => Status::parse(&stored)
            .ok_or_else(|| StoreError::Backend("a stored document has an unknown status".to_owned()))?,
        None => Status::Live,
    };

    Ok(Some(RowState {
        status,
        pending_of: opt_int(&row, "pending_of"),
        review_rev: int(&row, "review_rev"),
        format: text(&row, "format").unwrap_or_default(),
    }))
}

/// Records an event of document `id`.
pub(super) fn record(
    ctx: &mut Ctx<'_>,
    id: i64,
    kind: EventKind,
    (by, comment): (Option<&str>, Option<&str>),
    at: i64,
) -> Result<(), Fail> {
    let event = ctx.reserve_ids("events", 1)?;

    ctx.insert_many(
        EVENTS,
        vec![doc! { "_id": event, "doc_id": id, "kind": kind.as_str(), "by": by, "comment": comment, "at": at }],
    )
}

/// The counters of a stored document. The fields are schemaless, so a document no counter has
/// touched reads as 0 (there is never a database without them).
pub(super) fn usage(row: &Row) -> Option<DocUsage> {
    Some(DocUsage {
        shown: int(row, "shown"),
        useful: int(row, "useful"),
        last_useful_at: row.get("last_useful_at").and_then(|at| match at {
            Bson::Int64(at) => Some(*at),
            Bson::Int32(at) => Some(i64::from(*at)),
            _ => None,
        }),
    })
}

/// How many links point at a folder of this computer (a `path:` key).
pub(super) fn count_local_paths(ctx: &mut Ctx<'_>) -> Result<usize, Fail> {
    let rows = ctx.find(
        DOCUMENTS,
        doc! { "repo_keys": { "$regex": "^path:" } },
        (doc! { "_id": 1 }, 0, 0),
        doc! { "links": 1 },
    )?;

    Ok(rows.iter().map(|row| local_links(row).count()).sum())
}

/// The links of a stored document that point at a folder of this computer.
fn local_links(row: &Row) -> impl Iterator<Item = Link> {
    links(row)
        .into_iter()
        .filter(|link| crate::store::clean::is_local_path(&link.repo_key))
}

/// Takes the links to folders of this computer, which a team database never keeps, out of every
/// document and its chunks; the number of links taken out. A live document that no link is left
/// on becomes a draft (a document with no link is global, which would widen who sees it), with an
/// `edited` event and a new revision, and its chunks leave the agents' search.
pub(super) fn drop_local_paths(ctx: &mut Ctx<'_>) -> Result<(usize, bool), Fail> {
    let rows = ctx.find(
        DOCUMENTS,
        doc! { "repo_keys": { "$regex": "^path:" } },
        (doc! { "_id": 1 }, 0, 0),
        doc! { "links": 1, "status": 1, "pending_of": 1 },
    )?;

    let mut removed = 0;

    for row in &rows {
        let id = int(row, "_id");
        removed += local_links(row).count();
        let kept: Vec<Link> = links(row)
            .into_iter()
            .filter(|link| !crate::store::clean::is_local_path(&link.repo_key))
            .collect();

        set_links(ctx, id, &kept)?;

        if !kept.is_empty() {
            continue;
        }

        // A pending version has no links of its own: the live document's win.
        let Some(state) = row_state(ctx, id)?.filter(|state| state.pending_of.is_none()) else {
            continue;
        };
        let mut comment = crate::store::clean::FOLDER_LINK_REMOVED;

        // Nothing links it, and a document with no link is global: it is a draft that is looked at
        // again (a waiting or sent-back row needs a new ask), its chunks leave the agents' search,
        // and a pending update of a live one goes, so no pending version points at a draft.
        if state.status != Status::Draft {
            ctx.update_one(
                DOCUMENTS,
                doc! { "_id": id },
                doc! { "$set": { "status": Status::Draft.as_str() }, "$inc": { "review_rev": 1_i64 } },
                false,
            )?;
            ctx.update_many(
                CHUNKS,
                doc! { "doc_id": id },
                doc! { "$set": { "review": REVIEW_DRAFT } },
            )?;
        }

        if state.status == Status::Live
            && let Some(pending) = ctx.find_one(DOCUMENTS, doc! { "pending_of": id }, doc! { "_id": 1 })?
        {
            delete(ctx, &[int(&pending, "_id")])?;
            comment = crate::store::clean::FOLDER_LINK_REMOVED_PENDING;
        }

        record(
            ctx,
            id,
            EventKind::Edited,
            (None, Some(comment)),
            crate::import::document::now(),
        )?;
    }

    Ok((removed, removed > 0))
}

/// The links of a stored document, by repository key.
pub(super) fn links(row: &Row) -> Vec<Link> {
    let mut links: Vec<Link> = row
        .get_array("links")
        .map(|stored| {
            stored
                .iter()
                .filter_map(Bson::as_document)
                .filter_map(|link| {
                    Some(Link {
                        repo_key: text(link, "repo_key")?,
                        globs: strings(link, "globs"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    links.sort_by(|a, b| a.repo_key.cmp(&b.repo_key));
    links
}

/// A stored chunk of document `doc_id` (titled `title`, linked as `links`).
fn chunk_row(
    id: i64,
    doc_id: i64,
    (title, updated_at, review): (&str, i64, &str),
    chunk: &Chunk,
    links: &[Link],
) -> Row {
    let mut row = doc! {
        "_id": id,
        "doc_id": doc_id,
        "ordinal": long(chunk.ordinal),
        "title": title,
        "title_terms": terms::of_text(title),
        "heading_path": &chunk.heading_path,
        "text": &chunk.text,
        "terms": terms::of_text(&format!("{} {}", chunk.heading_path, chunk.text)),
        "phrase_text": terms::phrase_text(&chunk.heading_path, &chunk.text),
        "chunk_sha": &chunk.chunk_sha,
        "tokens": long(chunk.tokens),
        "char_start": long(chunk.char_start),
        "char_end": long(chunk.char_end),
        "updated_at": updated_at,
        "review": review,
    };

    row.extend(scope_fields(links));

    if let (Some(embedding), Some(model)) = (&chunk.embedding, &chunk.model_id) {
        let vector: Vec<f64> = embedding.iter().map(|value| f64::from(*value)).collect();
        row.insert("embedding", vector);
        row.insert("model_id", model);
    }

    row
}

/// Stores `doc` (whose cleaned title is `title`) under `id` with `chunks` and `links`: inserted
/// for a document that has no id yet, else replacing that document's fields, chunks and links
/// (`created_at` stays). The chunks of an earlier attempt of the same write are removed first, so
/// a retry outside a transaction starts clean.
///
/// A new row starts in the state `doc.status` names (a pending version also names its live
/// document). A replaced row keeps its state, except that a waiting row is a draft again, takes the
/// next revision and loses its original file; a row that is not live records the event.
pub(super) fn put(
    ctx: &mut Ctx<'_>,
    id: i64,
    doc: &Document,
    title: &str,
    (chunks, links): (&[Chunk], &[Link]),
    begun: &Cell<bool>,
) -> Result<i64, Fail> {
    let fresh = doc.id == 0;
    let by = actor(doc.updated_by.as_ref().map(|author| author.name.as_str()));

    let stored = if fresh {
        check_new_row(ctx, doc)?;
        None
    } else {
        Some(row_state(ctx, id)?.ok_or_else(|| not_found(id))?)
    };

    let (status, pending) = stored
        .as_ref()
        .map_or((doc.status, doc.pending_of.is_some()), |stored| {
            (stored.status, stored.pending_of.is_some())
        });

    let written = match &stored {
        None => {
            // An update keeps who added the document, so the author is set on insert only.
            let added = doc.added_by.as_ref();
            let mut on_insert = doc! {
                "created_at": doc.created_at,
                "added_by_name": added.map(|author| author.name.as_str()),
                "added_by_via": added.and_then(|author| author.via.as_deref()),
                "status": doc.status.as_str(),
                "review_rev": 0_i64,
            };

            if let Some(live) = doc.pending_of {
                on_insert.insert("pending_of", live);
            }

            let update = doc! { "$set": fields(doc, title, chunks.len(), links), "$setOnInsert": on_insert };

            ctx.update_one(DOCUMENTS, doc! { "_id": id }, update, true)
        }
        Some(stored) => {
            // What waited for approval is no longer what was asked: it is a draft again.
            let status = if stored.status == Status::Waiting {
                Status::Draft
            } else {
                stored.status
            };
            let mut set = fields(doc, title, chunks.len(), links);
            set.insert("status", status.as_str());
            let update = doc! { "$set": set, "$inc": { "review_rev": 1_i64 } };

            ctx.update_one(
                DOCUMENTS,
                doc! { "_id": id, "review_rev": stored.review_rev },
                update,
                false,
            )
        }
    };

    let matched = match written {
        Ok(matched) => matched,
        // Two writers raced for the one pending version of a live document.
        Err(Fail::Db(error)) if error_code(&error) == Some(DUPLICATE_KEY) => {
            return Err(StoreError::Invalid("the document already has a pending version".to_owned()).into());
        }
        Err(other) => return Err(other),
    };

    if matched == 0 {
        // The row was changed or deleted between reading it and writing it.
        return Err(if stored.is_some() {
            StoreError::Busy
        } else {
            not_found(id)
        }
        .into());
    }

    // From here a failure outside a transaction leaves a part behind (see `undo_put`).
    begun.set(true);

    ctx.delete_many(CHUNKS, doc! { "doc_id": id })?;

    if stored.is_some() {
        // Its content changed, so a stored file no longer is the original.
        ctx.delete_many(FILES, doc! { "doc_id": id })?;
    }

    if !status.is_live() {
        let kind = match (&stored, pending) {
            (Some(_), _) => EventKind::Edited,
            (None, true) => EventKind::UpdateDrafted,
            (None, false) => EventKind::Drafted,
        };

        record(ctx, id, kind, (by.as_deref(), None), doc.updated_at)?;
    }

    let review = review_of(status, pending);
    let first = match chunks.len() {
        0 => 0,
        count => ctx.reserve_ids("chunks", count)?,
    };
    let rows: Vec<Row> = chunks
        .iter()
        .zip(first..)
        .map(|(chunk, chunk_id)| chunk_row(chunk_id, id, (title, doc.updated_at, review), chunk, links))
        .collect();

    // One request per slice, as a document of 4000 chunks with vectors is some 15 MB.
    for slice in rows.chunks(500) {
        ctx.insert_many(CHUNKS, slice.to_vec())?;
    }

    Ok(id)
}

/// Refuses a new row that cannot be stored: a pending version needs a live document without one.
fn check_new_row(ctx: &mut Ctx<'_>, doc: &Document) -> Result<(), Fail> {
    let Some(live) = doc.pending_of else {
        return Ok(());
    };

    if doc.status.is_live() {
        return Err(StoreError::Invalid("a pending version is never live".to_owned()).into());
    }

    match row_state(ctx, live)? {
        None => return Err(not_found(live).into()),
        Some(target) if !target.status.is_live() || target.pending_of.is_some() => {
            return Err(StoreError::Invalid(format!("document {live} is not live")).into());
        }
        Some(_) => {}
    }

    let taken = ctx.find_one(DOCUMENTS, doc! { "pending_of": live }, doc! { "_id": 1 })?;

    if taken.is_some() {
        return Err(StoreError::Invalid(format!("document {live} already has a pending version")).into());
    }

    Ok(())
}

/// What a failed put leaves behind outside a transaction, cleaned up as far as the database
/// answers: a new document goes with its chunks, a replaced one keeps its old fields but loses
/// its content hash and its chunks, so the next import of the file stores it again.
pub(super) fn undo_put(ctx: &mut Ctx<'_>, id: i64, fresh: bool) {
    let _ = ctx.delete_many(CHUNKS, doc! { "doc_id": id });

    if fresh {
        let _ = ctx.delete_many(DOCUMENTS, doc! { "_id": id });
        let _ = ctx.delete_many(EVENTS, doc! { "doc_id": id });
    } else {
        let blank = doc! { "$set": { "content_sha256": "", "file_sha256": "", "chunk_count": 0_i64 } };
        let _ = ctx.update_one(DOCUMENTS, doc! { "_id": id }, blank, false);
    }
}

/// Replaces the links of document `id`; false when there is none.
pub(super) fn set_links(ctx: &mut Ctx<'_>, id: i64, links: &[Link]) -> Result<bool, Fail> {
    let update = doc! { "$set": link_fields(links) };

    if ctx.update_one(DOCUMENTS, doc! { "_id": id }, update, false)? == 0 {
        return Ok(false);
    }

    ctx.update_many(CHUNKS, doc! { "doc_id": id }, doc! { "$set": scope_fields(links) })?;

    Ok(true)
}

/// Retitles document `id` (to the cleaned `title`) and its chunks; false when there is none. A row
/// that is not live takes it as a change (see [`retitle`]), and so does the pending version of a
/// live one.
pub(super) fn rename(ctx: &mut Ctx<'_>, id: i64, title: &str, by: Option<&str>, at: i64) -> Result<bool, Fail> {
    let Some(state) = row_state(ctx, id)? else {
        return Ok(false);
    };

    retitle(ctx, id, &state, title, by, at)?;

    if !state.status.is_live() {
        return Ok(true);
    }

    let pending = ctx.find_one(DOCUMENTS, doc! { "pending_of": id }, doc! { "_id": 1 })?;

    if let Some(pending) = pending {
        let pending = int(&pending, "_id");

        if let Some(state) = row_state(ctx, pending)? {
            retitle(ctx, pending, &state, title, by, at)?;
        }
    }

    Ok(true)
}

/// Gives the row `id`, in `state`, the title and its chunks. A live row is only retitled. A row that
/// is not live has a new revision, a waiting one is a draft again (what was asked is no longer what
/// is there) and the `edited` event says what changed. Nothing when the title already is `title`.
fn retitle(ctx: &mut Ctx<'_>, id: i64, state: &RowState, title: &str, by: Option<&str>, at: i64) -> Result<(), Fail> {
    // The renamer goes on the `edited` event, not on the row: who wrote the content stays as it was.
    let renamer = actor(by);
    let mut set = doc! { "title": title, "title_key": title_key(title), "updated_at": at };
    let mut filter = doc! { "_id": id, "title": { "$ne": title } };
    let mut update = Row::new();

    if state.status.is_live() {
        update.insert("$set", set);
    } else {
        // The revision is part of the condition, so two writers cannot both move it on from one.
        filter.insert("review_rev", state.review_rev);

        let status = if state.status == Status::Waiting {
            Status::Draft
        } else {
            state.status
        };
        set.insert("status", status.as_str());

        update.insert("$set", set);
        update.insert("$inc", doc! { "review_rev": 1_i64 });
    }

    if ctx.update_one(DOCUMENTS, filter, update, false)? == 0 {
        // Unchanged (a title that already is), or changed meanwhile.
        return match row_state(ctx, id)? {
            Some(now) if now.review_rev != state.review_rev => Err(StoreError::Busy.into()),
            _ => Ok(()),
        };
    }

    let chunks = doc! { "$set": { "title": title, "title_terms": terms::of_text(title) } };
    ctx.update_many(CHUNKS, doc! { "doc_id": id }, chunks)?;

    if !state.status.is_live() {
        record(
            ctx,
            id,
            EventKind::Edited,
            (renamer.as_deref(), Some(RENAMED_COMMENT)),
            at,
        )?;
    }

    Ok(())
}

/// Deletes the documents with their pending versions and everything that belongs to them (chunks,
/// events, comments, files, snapshots); how many of `ids` existed.
pub(super) fn delete(ctx: &mut Ctx<'_>, ids: &[i64]) -> Result<usize, Fail> {
    if ids.len() > DELETE_IDS_MAX {
        return Err(StoreError::Invalid(format!("at most {DELETE_IDS_MAX} documents at once")).into());
    }

    let deleted = ctx.delete_many(DOCUMENTS, doc! { "_id": { "$in": ids } })?;

    // A live document takes its pending version with it.
    let pending = ctx.find(
        DOCUMENTS,
        doc! { "pending_of": { "$in": ids } },
        (doc! { "_id": 1 }, 0, 0),
        doc! { "_id": 1 },
    )?;
    let mut all: Vec<i64> = ids.to_vec();
    all.extend(pending.iter().map(|row| int(row, "_id")));

    if !pending.is_empty() {
        ctx.delete_many(DOCUMENTS, doc! { "pending_of": { "$in": ids } })?;
    }

    for collection in [CHUNKS, EVENTS, COMMENTS, FILES, SNAPSHOTS] {
        ctx.delete_many(collection, doc! { "doc_id": { "$in": all.as_slice() } })?;
    }

    Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
}

/// The oldest document `visibility` lets through whose `field` (`content_sha256` or `source_key`)
/// is `value`.
pub(super) fn find(
    ctx: &mut Ctx<'_>,
    field: &str,
    value: &str,
    visibility: Visibility,
) -> Result<Option<Document>, Fail> {
    let mut filter = doc! { field: value };
    filter.extend(seen(visibility));

    let found = ctx.find(DOCUMENTS, filter, (doc! { "_id": 1 }, 0, 1), Row::new())?;

    Ok(found.first().map(document).transpose()?)
}

/// The embeddings of chunks made by `model_id`, by chunk hash.
pub(super) fn reusable_embeddings(
    ctx: &mut Ctx<'_>,
    model_id: &str,
    shas: &[String],
) -> Result<HashMap<String, Vec<f32>>, Fail> {
    if shas.is_empty() {
        return Ok(HashMap::new());
    }

    let filter = doc! { "model_id": model_id, "chunk_sha": { "$in": shas }, "embedding": { "$exists": true } };
    let rows = ctx.find(
        CHUNKS,
        filter,
        (doc! { "_id": 1 }, 0, 0),
        doc! { "chunk_sha": 1, "embedding": 1 },
    )?;

    let mut found: HashMap<String, Vec<f32>> = HashMap::new();

    for row in &rows {
        if let (Some(sha), Some(vector)) = (text(row, "chunk_sha"), embedding(row)) {
            found.entry(sha).or_insert(vector);
        }
    }

    Ok(found)
}

/// The next `limit` chunks after chunk `after` that have no embedding of `model_id` (a chunk
/// without an embedding has no `model_id` either, and `$ne` matches a missing field).
pub(super) fn pending_chunks(
    ctx: &mut Ctx<'_>,
    model_id: &str,
    after: i64,
    limit: usize,
) -> Result<Vec<PendingChunk>, Fail> {
    let filter = doc! { "_id": { "$gt": after }, "model_id": { "$ne": model_id } };
    let wanted = i64::try_from(limit.min(EMBED_BATCH_MAX)).unwrap_or(i64::MAX);
    let projection = doc! { "doc_id": 1, "title": 1, "heading_path": 1, "text": 1 };
    let rows = ctx.find(CHUNKS, filter, (doc! { "_id": 1 }, 0, wanted), projection)?;

    Ok(rows
        .iter()
        .map(|row| PendingChunk {
            chunk_id: int(row, "_id"),
            doc_id: int(row, "doc_id"),
            embed_text: passage(
                &text(row, "title").unwrap_or_default(),
                &text(row, "heading_path").unwrap_or_default(),
                &text(row, "text").unwrap_or_default(),
            ),
        })
        .collect())
}

/// Stores the embeddings; the number of chunks that existed.
pub(super) fn set_embeddings(ctx: &mut Ctx<'_>, model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<usize, Fail> {
    let mut found = 0;

    for (chunk_id, vector) in embeddings {
        let vector: Vec<f64> = vector.iter().map(|value| f64::from(*value)).collect();
        let update = doc! { "$set": { "embedding": vector, "model_id": model_id } };

        found += ctx.update_one(CHUNKS, doc! { "_id": *chunk_id }, update, false)?;
    }

    Ok(usize::try_from(found).unwrap_or(usize::MAX))
}

/// The embedding of a stored chunk; `None` unless it is exactly [`DIMS`] numbers.
fn embedding(row: &Row) -> Option<Vec<f32>> {
    let stored = row.get_array("embedding").ok()?;

    if stored.len() != DIMS {
        return None;
    }

    stored
        .iter()
        .map(|value| value.as_f64().map(|float| float as f32))
        .collect()
}

pub(super) fn not_found(id: i64) -> StoreError {
    StoreError::NotFound(format!("document {id} not found"))
}
