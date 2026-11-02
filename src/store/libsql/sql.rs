//! The statement runner the libSQL store is written against: values bound to a statement,
//! rows read from a result, and base64 for the blobs the server sends and takes.
//!
//! Rows come back whole as [`Value`]s. Every query of the store is bounded by a limit, so that
//! stays small.

use crate::store::StoreError;

/// A value bound to a statement or read from a row.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    /// Only the vector columns hold blobs (little-endian `f32`s).
    Blob(Vec<u8>),
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

/// A size or an ordinal: it always fits, and saturates rather than wraps if it somehow does not.
impl From<usize> for Value {
    fn from(value: usize) -> Self {
        Self::Integer(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<u64> for Value {
    fn from(value: u64) -> Self {
        Self::Integer(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<&String> for Value {
    fn from(value: &String) -> Self {
        Self::Text(value.clone())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// The arguments of a statement: `args![a, b]` binds `?1` and `?2`.
macro_rules! args {
    ($($value:expr),* $(,)?) => {
        &[$($crate::store::libsql::sql::Value::from($value)),*]
    };
}

pub(super) use args;

/// A value read from a column.
pub(super) trait FromValue: Sized {
    fn from_value(value: &Value) -> Option<Self>;
}

impl FromValue for i64 {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Integer(number) => Some(*number),
            _ => None,
        }
    }
}

impl FromValue for usize {
    fn from_value(value: &Value) -> Option<Self> {
        i64::from_value(value).and_then(|number| Self::try_from(number).ok())
    }
}

impl FromValue for u64 {
    fn from_value(value: &Value) -> Option<Self> {
        i64::from_value(value).and_then(|number| Self::try_from(number).ok())
    }
}

impl FromValue for f64 {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Real(number) => Some(*number),
            Value::Integer(number) => Some(*number as f64),
            _ => None,
        }
    }
}

impl FromValue for String {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Text(text) => Some(text.clone()),
            _ => None,
        }
    }
}

impl FromValue for Vec<u8> {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Blob(bytes) => Some(bytes.clone()),
            _ => None,
        }
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Null => Some(None),
            other => T::from_value(other).map(Some),
        }
    }
}

/// One result row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Row(Vec<Value>);

impl Row {
    pub(super) fn new(values: Vec<Value>) -> Self {
        Self(values)
    }

    /// Column `index` as `T`; a missing column or another type is a backend error.
    pub(super) fn get<T: FromValue>(&self, index: usize) -> Result<T, StoreError> {
        self.0
            .get(index)
            .and_then(T::from_value)
            .ok_or_else(|| StoreError::Backend(format!("unexpected value in column {index}")))
    }

    /// A text column that anybody with access to the database may have filled with something else:
    /// only text is read, any other value reads as none.
    pub(super) fn tolerant_text(&self, index: usize) -> Option<String> {
        match self.0.get(index) {
            Some(Value::Text(text)) => Some(text.clone()),
            _ => None,
        }
    }

    /// A counter column the same way: an integer as it is, a float (an overflow, or a hand edit) cut
    /// to the integers, anything else as 0. NULL, or a column the select list lacks, is none.
    pub(super) fn tolerant_counter(&self, index: usize) -> Option<i64> {
        match self.0.get(index)? {
            Value::Null => None,
            Value::Integer(count) => Some(*count),
            Value::Real(count) => Some(*count as i64),
            _ => Some(0),
        }
    }
}

/// What a statement without rows did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Executed {
    /// Rows inserted, updated or deleted.
    pub changes: usize,
    /// The rowid of the latest insert on the connection.
    pub last_insert_rowid: i64,
}

/// Runs statements on one connection (inside a transaction for writes).
pub(super) trait Sql {
    /// Every row of `sql`.
    fn query(&mut self, sql: &str, args: &[Value]) -> Result<Vec<Row>, StoreError>;

    /// Runs `sql`, which returns no rows.
    fn execute(&mut self, sql: &str, args: &[Value]) -> Result<Executed, StoreError>;

    /// Runs `sql` once for each of `rows` (its arguments), in order and in as few requests as
    /// fit. Stops reporting at the first failure: the caller rolls back.
    fn execute_many(&mut self, sql: &str, rows: &[Vec<Value>]) -> Result<(), StoreError>;

    /// Runs several `;`-separated statements without arguments.
    fn batch(&mut self, sql: &str) -> Result<(), StoreError>;
}

/// The first row of `sql`, if any.
pub(super) fn query_row(db: &mut dyn Sql, sql: &str, args: &[Value]) -> Result<Option<Row>, StoreError> {
    Ok(db.query(sql, args)?.into_iter().next())
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, the form the server reads in a `blob` argument.
pub(super) fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for group in bytes.chunks(3) {
        let [a, b, c] = [group[0], *group.get(1).unwrap_or(&0), *group.get(2).unwrap_or(&0)];
        let bits = u32::from(a) << 16 | u32::from(b) << 8 | u32::from(c);

        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= group.len() {
                out.push(char::from(ALPHABET[(bits >> shift & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }

    out
}

/// The bytes of standard base64, with or without padding (a server may omit it). `None` for
/// anything else.
pub(super) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let digits = text.trim_end_matches('=').as_bytes();

    if digits.len() % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity(digits.len() / 4 * 3 + 2);

    for group in digits.chunks(4) {
        let mut bits = 0u32;

        for byte in group {
            let value = ALPHABET.iter().position(|letter| letter == byte)?;
            bits = bits << 6 | u32::try_from(value).ok()?;
        }

        bits <<= 6 * (4 - group.len());

        for index in 0..group.len() - 1 {
            out.push((bits >> (16 - 8 * index) & 255) as u8);
        }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_every_length() {
        for length in 0..40_u8 {
            let bytes: Vec<u8> = (0..length).map(|n| n.wrapping_mul(37).wrapping_add(11)).collect();
            let encoded = base64_encode(&bytes);

            assert_eq!(base64_decode(&encoded).as_deref(), Some(bytes.as_slice()), "{length}");
            assert_eq!(
                base64_decode(encoded.trim_end_matches('=')).as_deref(),
                Some(bytes.as_slice()),
                "unpadded {length}"
            );
        }
    }

    #[test]
    fn base64_matches_the_server_reply_and_refuses_junk() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_decode("6tb8PZqZmT7NzEw+").map(|bytes| bytes.len()), Some(12));

        assert_eq!(base64_decode("a"), None);
        assert_eq!(base64_decode("ab$d"), None);
    }
}
