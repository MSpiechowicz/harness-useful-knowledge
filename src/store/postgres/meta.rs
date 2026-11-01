//! The `knowledge_meta` table: the schema version, the vector size, the model that made the
//! vectors and the write counter.

use postgres::GenericClient;

use super::error::{backend, column};
use crate::store::StoreError;

/// The keys the store keeps itself.
const RESERVED: [&str; 3] = ["generation", "schema_version", "dims"];

pub(super) fn get(client: &mut impl GenericClient, key: &str) -> Result<Option<String>, StoreError> {
    let row = client
        .query_opt("SELECT value FROM knowledge_meta WHERE key = $1", &[&key])
        .map_err(backend)?;

    row.map(|row| column(&row, 0)).transpose()
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

pub(super) fn set(client: &mut impl GenericClient, key: &str, value: &str) -> Result<(), StoreError> {
    client
        .execute(
            "INSERT INTO knowledge_meta (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            &[&key, &value],
        )
        .map_err(backend)?;

    Ok(())
}

/// The write counter.
pub(super) fn generation(client: &mut impl GenericClient) -> Result<i64, StoreError> {
    let text = get(client, "generation")?.unwrap_or_default();

    text.parse()
        .map_err(|_| StoreError::Backend("the stored write counter is not a number".to_owned()))
}

/// Counts one more write.
pub(super) fn bump(client: &mut impl GenericClient) -> Result<(), StoreError> {
    client
        .execute(
            "UPDATE knowledge_meta SET value = (value::bigint + 1)::text WHERE key = 'generation'",
            &[],
        )
        .map_err(backend)?;

    Ok(())
}
