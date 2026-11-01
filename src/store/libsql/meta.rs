//! The `knowledge_meta` table: the schema version, the vector size, the model that made the
//! vectors and the write counter.

use super::sql::{Sql, args, query_row};
use crate::store::StoreError;

pub(super) fn get(db: &mut dyn Sql, key: &str) -> Result<Option<String>, StoreError> {
    let row = query_row(db, "SELECT value FROM knowledge_meta WHERE key = ?1", args![key])?;

    row.map(|row| row.get(0)).transpose()
}

pub(super) fn set(db: &mut dyn Sql, key: &str, value: &str) -> Result<(), StoreError> {
    db.execute(
        "INSERT INTO knowledge_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        args![key, value],
    )?;

    Ok(())
}

/// The write counter.
pub(super) fn generation(db: &mut dyn Sql) -> Result<i64, StoreError> {
    let row = query_row(
        db,
        "SELECT CAST(value AS INTEGER) FROM knowledge_meta WHERE key = 'generation'",
        args![],
    )?;

    row.map_or(Ok(0), |row| row.get(0))
}

/// Counts one more write.
pub(super) fn bump(db: &mut dyn Sql) -> Result<(), StoreError> {
    db.execute(
        "UPDATE knowledge_meta SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT) WHERE key = 'generation'",
        args![],
    )?;

    Ok(())
}
