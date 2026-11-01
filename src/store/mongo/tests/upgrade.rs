//! The machinery of a schema upgrade: a field filled in pages of 500, recorded version by version,
//! and run again after a stop. There is no upgrade after version 1 yet, so these run a test one
//! through [`schema::migrate_with`].

use mongodb::bson::{Document, doc};
use mongodb::sync::Database;

use super::{insert, store};
use crate::store::mongo::migrate::{PAGE, update_in_pages};
use crate::store::mongo::schema::{self, CHUNKS, META};
use crate::store::testing::Seed;

/// Gives every chunk that has none a `marker`, one page of updates at a time.
fn mark_chunks(db: &Database) -> mongodb::error::Result<()> {
    let chunks = db.collection::<Document>(CHUNKS);
    let mut updates = Vec::new();

    for row in chunks.find(doc! { "marker": { "$exists": false } }).run()? {
        let row = row?;
        let id = row.get_i64("_id").unwrap_or_default();
        let text = row.get_str("text").unwrap_or_default().len() as i64;

        updates.push((doc! { "_id": id }, doc! { "$set": { "marker": text } }));
    }

    update_in_pages(db, CHUNKS, updates)
}

fn version(db: &Database) -> i64 {
    db.collection::<Document>(META)
        .find_one(doc! { "_id": "schema" })
        .run()
        .expect("find")
        .expect("schema")
        .get_i64("version")
        .expect("version")
}

#[test]
fn team_mongo_an_upgrade_fills_a_field_in_pages_and_records_the_version_it_reached() {
    let Some((db, mut store)) = store() else { return };

    // Three pages and a part of one.
    let chunks = PAGE * 2 + 120;
    let mut seed = Seed::new("Big");

    for n in 0..chunks {
        seed = seed.chunk(&format!("Part {n}"), "text of the part");
    }

    insert(&mut store, &seed);
    drop(store);

    let raw = db.raw();
    assert_eq!(db.count(CHUNKS, doc! { "marker": { "$exists": true } }), 0);
    assert_eq!(version(&raw), 1);

    let newer = schema::migrate_with(&raw, &[mark_chunks]).expect("the upgrade runs");

    assert!(!newer);
    assert_eq!(
        db.count(CHUNKS, doc! { "marker": { "$exists": true } }),
        chunks as u64,
        "every chunk has it"
    );
    assert_eq!(version(&raw), 2, "the version it reached is recorded");

    // Run again, it finds nothing to do.
    assert!(!schema::migrate_with(&raw, &[mark_chunks]).expect("again"));
    assert_eq!(version(&raw), 2);
}

#[test]
fn team_mongo_a_database_that_a_later_server_upgraded_is_left_alone() {
    let Some((db, store)) = store() else { return };
    drop(store);
    let raw = db.raw();

    // The later server knows two upgrades after version 1.
    assert!(!schema::migrate_with(&raw, &[mark_chunks, mark_chunks]).expect("upgrades"));
    assert_eq!(version(&raw), 3);

    // This one knows one, so what it finds is newer, and it changes nothing.
    assert!(schema::migrate_with(&raw, &[mark_chunks]).expect("looks"));
    assert_eq!(version(&raw), 3);
}
