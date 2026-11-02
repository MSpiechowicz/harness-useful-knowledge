//! The columns added in place to `knowledge_documents` (the authors and the usefulness counters):
//! which a file has, and adding the missing ones when the file is opened for writing.
//!
//! No schema version changes, so an older server keeps using the file: it never names the columns,
//! and a read-only open (which refuses only a `user_version` it cannot read) reads without them.

use rusqlite::{Connection, TransactionBehavior};

use super::sql_quality::{add_statements, present_sql};
use crate::store::quality::warn_unavailable;
use crate::store::{Columns, StoreError};

/// The names of the added columns the documents table has.
fn names(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement = conn.prepare(&present_sql())?;

    statement.query_map([], |row| row.get(0))?.collect()
}

/// Which of the added columns the file has.
pub(super) fn present(conn: &Connection) -> rusqlite::Result<Columns> {
    Ok(Columns::from_names(&names(conn)?))
}

/// Adds the columns the file lacks, in one `IMMEDIATE` transaction. A file that has them is only
/// read, so servers opening it together add them once.
fn ensure(conn: &mut Connection) -> rusqlite::Result<()> {
    if present(conn)?.complete() {
        return Ok(());
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    // Another server may have added them while this one waited for the lock.
    for statement in add_statements(&names(&tx)?) {
        tx.execute_batch(&statement)?;
    }

    tx.commit()
}

/// Adds the missing columns and reports which the file has now. A file that refuses them (a
/// read-only folder) is used with what it has, with one line on stderr.
pub(super) fn install(conn: &mut Connection) -> Columns {
    if let Err(error) = ensure(conn) {
        warn_unavailable(&StoreError::from(error));
    }

    present(conn).unwrap_or(Columns::NONE)
}
