//! What a failed statement says, kept free of anything stored or secret.
//!
//! The database is shared, so its rows and errors are not trusted: a NULL or a value of another
//! type is an error instead of a panic, and errors carry the SQLSTATE with a fixed description,
//! never the server's message, stored text, the URL or a password.

use postgres::Row;
use postgres::types::FromSql;

use crate::store::StoreError;

/// A [`StoreError::Backend`] for a client error, or [`StoreError::Unreachable`] when the server
/// could not be reached (see [`unreachable`]). A database error becomes its SQLSTATE with a fixed
/// description (see [`described`]): the server's message, detail and hint can quote stored values
/// or text a teammate wrote, so none of them is kept. Any other error is the error and its
/// causes, at most 200 characters.
pub(super) fn backend(error: postgres::Error) -> StoreError {
    let message = match error.as_db_error() {
        Some(db) => described(db.code().code()),
        None => with_causes(&error).chars().take(200).collect(),
    };

    if unreachable(&error) {
        StoreError::Unreachable(message)
    } else {
        StoreError::Backend(message)
    }
}

/// Whether `error` says the server could not be reached: a connection that was refused, timed out,
/// could not be resolved or was lost (an I/O error, or a closed connection), or a server that
/// says it is shutting down or the connection failed (see [`unreachable_code`]). A certificate the
/// TLS check refuses is a setting to fix, not an unreachable server.
pub(super) fn unreachable(error: &postgres::Error) -> bool {
    if let Some(db) = error.as_db_error() {
        return unreachable_code(db.code().code());
    }

    if error.is_closed() {
        return true;
    }

    let mut cause = std::error::Error::source(error);

    while let Some(inner) = cause {
        if let Some(io) = inner.downcast_ref::<std::io::Error>() {
            let refused_by_tls = io.get_ref().is_some_and(|inner| inner.is::<rustls::Error>());

            return !refused_by_tls;
        }

        if inner.is::<rustls::Error>() {
            return false;
        }

        cause = inner.source();
    }

    false
}

/// Whether the SQLSTATE `code` says the server could not be reached: the connection exceptions of
/// class 08 and a server that is shutting down or starting up (57P01 to 57P03). 08P01 is a protocol
/// violation, a client or server that speaks wrongly, which another attempt does not mend: it is a
/// failure, not an unreachable server.
fn unreachable_code(code: &str) -> bool {
    (code.starts_with("08") && code != "08P01") || matches!(code, "57P01" | "57P02" | "57P03")
}

/// Whether `error` says the server ended this session (an administrator or a restart terminated
/// it: SQLSTATE 57P01, 57P02, 57P03). The client may not have noticed the closed socket yet, so
/// this counts as a lost connection like [`postgres::Client::is_closed`].
pub(super) fn ended(error: &StoreError) -> bool {
    matches!(error, StoreError::Backend(message) | StoreError::Unreachable(message)
        if ["57P01", "57P02", "57P03"].iter().any(|code| message.contains(&format!("SQLSTATE {code}"))))
}

/// A fixed description of the SQLSTATE `code`, followed by the code, or `database error <code>`
/// for a code without one. A code that is not five letters or digits is not shown.
pub(super) fn described(code: &str) -> String {
    let valid = code.len() == 5 && code.bytes().all(|byte| byte.is_ascii_alphanumeric());

    if !valid {
        return "database error (an invalid SQLSTATE)".to_owned();
    }

    let known = match code {
        "0A000" => "the database lacks a feature the knowledge store needs",
        "22021" => "the database refused a character in the text",
        "23505" => "a row with this key already exists",
        "25P03" => "the server ended a transaction that sat idle too long",
        "3D000" => "the database does not exist",
        "3F000" => "the schema does not exist",
        "40001" | "40P01" => "the server stopped a transaction that conflicted with another, try again",
        "42501" => "the database role lacks a permission the knowledge store needs",
        "42P01" | "42703" | "42883" | "42704" => {
            "the knowledge tables do not match this server (a table, column, type or function is missing)"
        }
        "53300" => "the database server has too many connections",
        "54000" => "a text is too large to index",
        "55P03" => "a database lock was not granted in time",
        "57014" => "a database statement ran too long and was cancelled",
        "57P01" | "57P02" | "57P03" => "the database server is shutting down or starting up",
        _ => match &code[..2] {
            "08" => "the connection to the database failed",
            "22" => "the database refused a value",
            "23" => "a write breaks a database constraint",
            "28" => "the database refused the login",
            "53" => "the database server is out of resources",
            _ => return format!("database error {code}"),
        },
    };

    format!("{known} (SQLSTATE {code})")
}

/// Column `index` of `row`. A NULL or a value of another type (a shared database can hold rows
/// changed by hand) is a [`StoreError::Backend`] naming the column position, never the value.
pub(super) fn column<'a, T: FromSql<'a>>(row: &'a Row, index: usize) -> Result<T, StoreError> {
    row.try_get(index).map_err(|_| {
        StoreError::Backend(format!(
            "a stored row holds a NULL or an unexpected type (result column {index})"
        ))
    })
}

/// Column `index` of `row`, a non-negative `BIGINT`, as a count.
pub(super) fn count(row: &Row, index: usize) -> Result<usize, StoreError> {
    let value: i64 = column(row, index)?;

    usize::try_from(value)
        .map_err(|_| StoreError::Backend(format!("a stored count is negative (result column {index})")))
}

/// `error` followed by its causes (`error connecting to server: Connection refused`).
pub(super) fn with_causes(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();

    while let Some(inner) = cause {
        let text = inner.to_string();

        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }

        cause = inner.source();
    }

    message
}

/// `value` as a `BIGINT` parameter.
pub(super) fn int(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_errors_are_described_by_their_code_only() {
        assert_eq!(
            described("55P03"),
            "a database lock was not granted in time (SQLSTATE 55P03)"
        );
        assert_eq!(
            described("08006"),
            "the connection to the database failed (SQLSTATE 08006)"
        );
        assert_eq!(described("28P01"), "the database refused the login (SQLSTATE 28P01)");
        assert_eq!(described("XX000"), "database error XX000");
    }

    #[test]
    fn a_lost_or_refused_connection_is_unreachable_and_a_protocol_violation_is_not() {
        for code in ["08000", "08001", "08003", "08004", "08006", "57P01", "57P02", "57P03"] {
            assert!(unreachable_code(code), "{code}");
        }

        for code in ["08P01", "28P01", "42P01", "57014", "53300", "XX000"] {
            assert!(!unreachable_code(code), "{code}");
        }
    }

    #[test]
    fn a_code_that_is_not_a_sqlstate_is_not_shown() {
        assert_eq!(described("leaked secret"), "database error (an invalid SQLSTATE)");
        assert_eq!(described("ab\ncd"), "database error (an invalid SQLSTATE)");
        assert_eq!(described("é1234"), "database error (an invalid SQLSTATE)");
    }
}
