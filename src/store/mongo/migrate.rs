//! What an upgrade of the MongoDB schema writes with: updates sent in pages.
//!
//! An upgrade that fills a field on every chunk of a real-sized database must not cost one round
//! trip per chunk (millions of them, far past any bound an open has). [`update_in_pages`] sends
//! them as `update` commands of [`PAGE`] updates each, which every server version runs.

use mongodb::bson::{Bson, Document, doc};
use mongodb::error::Error;
use mongodb::sync::Database;

/// How many updates one `update` command carries.
pub(super) const PAGE: usize = 500;

/// Applies `updates` (each a filter and an update document, matching one document) to
/// `collection`, [`PAGE`] to a command. A page runs unordered, so one failed update does not stop
/// the others of its page, and the first failure is the error once the page is done.
///
/// A filter names `_id` and every field the new value was derived from (the title for a title key),
/// so a row that someone changed meanwhile is left for the next run instead of getting a value
/// derived from what it no longer holds.
pub(super) fn update_in_pages(
    db: &Database,
    collection: &str,
    updates: impl IntoIterator<Item = (Document, Document)>,
) -> mongodb::error::Result<()> {
    let mut page: Vec<Bson> = Vec::with_capacity(PAGE);

    for (filter, update) in updates {
        page.push(Bson::Document(doc! { "q": filter, "u": update, "multi": false }));

        if page.len() == PAGE {
            send(db, collection, std::mem::take(&mut page))?;
        }
    }

    if !page.is_empty() {
        send(db, collection, page)?;
    }

    Ok(())
}

fn send(db: &Database, collection: &str, updates: Vec<Bson>) -> mongodb::error::Result<()> {
    let reply = db
        .run_command(doc! { "update": collection, "updates": updates, "ordered": false })
        .run()?;

    match reply.get_array("writeErrors") {
        Ok(failed) if !failed.is_empty() => Err(Error::custom("an update of the upgrade failed")),
        _ => Ok(()),
    }
}
