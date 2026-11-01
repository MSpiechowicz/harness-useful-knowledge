//! Upgrading a database written by an older server.
//!
//! Version 2 adds the field the phrase filter matches on (`phrase_text` of a chunk, see
//! [`super::terms::phrase_text`]) and re-keys the titles (`title_key` folds ASCII letters only, like
//! every other store, see [`super::documents::title_key`]). Both are derived from fields that are
//! already stored, and writing them again gives the same result, so a migration that stopped half
//! way is simply run again.

use mongodb::bson::{Document, doc};
use mongodb::sync::Database;

use super::documents::title_key;
use super::schema::{CHUNKS, DOCUMENTS};
use super::terms;

/// Version 2: the folded title key of every document and the phrase text of every chunk.
pub(super) fn upgrade_v2(db: &Database) -> mongodb::error::Result<()> {
    let documents = db.collection::<Document>(DOCUMENTS);

    for row in documents
        .find(doc! {})
        .projection(doc! { "title": 1, "title_key": 1 })
        .run()?
    {
        let row = row?;
        let title = row.get_str("title").unwrap_or_default();
        let key = title_key(title);

        if row.get_str("title_key").ok() != Some(key.as_str()) {
            documents
                .update_one(doc! { "_id": row.get("_id") }, doc! { "$set": { "title_key": key } })
                .run()?;
        }
    }

    let chunks = db.collection::<Document>(CHUNKS);
    let missing = doc! { "phrase_text": { "$exists": false } };

    for row in chunks
        .find(missing)
        .projection(doc! { "heading_path": 1, "text": 1 })
        .run()?
    {
        let row = row?;
        let phrase = terms::phrase_text(
            row.get_str("heading_path").unwrap_or_default(),
            row.get_str("text").unwrap_or_default(),
        );

        chunks
            .update_one(
                doc! { "_id": row.get("_id") },
                doc! { "$set": { "phrase_text": phrase } },
            )
            .run()?;
    }

    Ok(())
}
