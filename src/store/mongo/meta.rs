//! The `knowledge_meta` collection as the trait sees it: the schema version, the vector size, the
//! write counter, and the keys a caller sets (stored as `meta:<key>`, so none can touch a counter).

use mongodb::bson::{Document as Row, doc};

use super::ctx::{Ctx, Fail};
use super::int;
use super::schema::META;
use crate::store::StoreError;

/// The keys the store keeps itself.
const RESERVED: [&str; 3] = ["generation", "schema_version", "dims"];

/// The document of `key` in the meta collection and the field that holds its value.
fn place(key: &str) -> (String, &'static str) {
    match key {
        "schema_version" => ("schema".to_owned(), "version"),
        "dims" => ("schema".to_owned(), "dims"),
        "generation" => ("generation".to_owned(), "value"),
        other => (format!("meta:{other}"), "value"),
    }
}

pub(super) fn get(ctx: &mut Ctx<'_>, key: &str) -> Result<Option<String>, Fail> {
    let (id, field) = place(key);
    let found = ctx.find_one(META, doc! { "_id": id }, doc! { field: 1 })?;

    Ok(found.and_then(|row| value(&row, field)))
}

/// A stored value as text: the counters are numbers, a caller's keys strings.
fn value(row: &Row, field: &str) -> Option<String> {
    match row.get(field)? {
        mongodb::bson::Bson::String(text) => Some(text.clone()),
        _ => Some(int(row, field).to_string()),
    }
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

pub(super) fn set(ctx: &mut Ctx<'_>, key: &str, value: &str) -> Result<(), Fail> {
    let (id, _) = place(key);
    ctx.update_one(META, doc! { "_id": id }, doc! { "$set": { "value": value } }, true)?;

    Ok(())
}

/// The write counter.
pub(super) fn generation(ctx: &mut Ctx<'_>) -> Result<i64, Fail> {
    Ok(get(ctx, "generation")?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0))
}
