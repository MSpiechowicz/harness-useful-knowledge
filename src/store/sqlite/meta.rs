//! The `knowledge_meta` table: the schema version, the vector size, the model that made the
//! vectors and the write counter.

use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::store::StoreError;

/// The keys the store keeps itself.
const RESERVED: [&str; 3] = ["generation", "schema_version", "dims"];

pub(super) fn get(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row("SELECT value FROM knowledge_meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

/// Fails for a key the store keeps itself.
pub(super) fn check_settable(key: &str) -> Result<(), StoreError> {
    if RESERVED.contains(&key) {
        return Err(StoreError::Invalid(format!("the meta key {key} belongs to the store")));
    }

    if key.is_empty() || key.len() > 64 {
        return Err(StoreError::Invalid("a meta key is 1-64 characters".to_owned()));
    }

    Ok(())
}

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
