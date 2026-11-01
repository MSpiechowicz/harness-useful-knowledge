//! The `knowledge_meta` table: the schema version, the vector size, the model that made the
//! vectors and the write counter.

use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::store::StoreError;

pub(super) fn get(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row("SELECT value FROM knowledge_meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

pub(super) use super::sql_shared::check_settable;

pub(super) fn set(tx: &Transaction, key: &str, value: &str) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO knowledge_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;

    Ok(())
}

/// The write counter.
pub(super) fn generation(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "SELECT CAST(value AS INTEGER) FROM knowledge_meta WHERE key = 'generation'",
        [],
        |row| row.get(0),
    )?)
}

/// Counts one more write.
pub(super) fn bump(tx: &Transaction) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE knowledge_meta SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT) WHERE key = 'generation'",
        [],
    )?;

    Ok(())
}
