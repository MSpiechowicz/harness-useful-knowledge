//! The columns added in place to `knowledge_documents` (the authors and the usefulness counters):
//! which the database has, and adding the missing ones when it is opened for writing.
//!
//! The schema version stays, so an older server keeps using the database, and a role that may only
//! read it reads without them. The columns default to 0 or NULL, so an older server's inserts stay
//! valid and adding them rewrites no row.

use postgres::{Client, GenericClient};

use super::error::{backend, column};
use super::schema::MIGRATE_LOCK;
use crate::store::{Columns, StoreError};

/// Adds the columns; each statement leaves one that exists as it is.
const ADD: &str = "
ALTER TABLE knowledge_documents ADD COLUMN IF NOT EXISTS added_by_name TEXT;
ALTER TABLE knowledge_documents ADD COLUMN IF NOT EXISTS added_by_via TEXT;
ALTER TABLE knowledge_documents ADD COLUMN IF NOT EXISTS updated_by_name TEXT;
ALTER TABLE knowledge_documents ADD COLUMN IF NOT EXISTS updated_by_via TEXT;
ALTER TABLE knowledge_documents ADD COLUMN IF NOT EXISTS shown BIGINT NOT NULL DEFAULT 0;
ALTER TABLE knowledge_documents ADD COLUMN IF NOT EXISTS useful BIGINT NOT NULL DEFAULT 0;
ALTER TABLE knowledge_documents ADD COLUMN IF NOT EXISTS last_useful_at BIGINT;
";

/// The names of the added columns the documents table has.
const PRESENT: &str = "
SELECT attname::text FROM pg_attribute
WHERE attrelid = to_regclass('knowledge_documents') AND NOT attisdropped AND attname IN (
    'added_by_name', 'added_by_via', 'updated_by_name', 'updated_by_via', 'shown', 'useful', 'last_useful_at')
";

/// Which of the added columns the database has.
pub(super) fn present(client: &mut impl GenericClient) -> Result<Columns, StoreError> {
    let rows = client.query(PRESENT, &[]).map_err(backend)?;
    let names = rows
        .iter()
        .map(|row| column(row, 0))
        .collect::<Result<Vec<String>, _>>()?;

    Ok(Columns::from_names(&names))
}

/// Adds the columns the database lacks, in one transaction under the migration lock, after a first
/// look without it: a database that has them is only read, and servers starting together add them
/// once. A lock that cannot be had within five seconds is an error, like a refused right (the
/// caller opens without the columns and tries again at the next open).
pub(super) fn ensure(client: &mut Client) -> Result<(), StoreError> {
    if present(client)?.complete() {
        return Ok(());
    }

    let mut tx = client.transaction().map_err(backend)?;

    // `ALTER TABLE` waits for every open transaction on the table and, while it waits, queues
    // every other statement behind it: a short wait gives up instead.
    tx.batch_execute("SET LOCAL lock_timeout = '5s'").map_err(backend)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATE_LOCK])
        .map_err(backend)?;

    if !present(&mut tx)?.complete() {
        tx.batch_execute(ADD).map_err(backend)?;
    }

    tx.commit().map_err(backend)
}
