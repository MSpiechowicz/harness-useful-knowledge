//! `upgrade --json`: brings the databases in use to the schema of this server.
//!
//! Prints `{"upgraded": bool, "waiting": bool}`. Each place in use is opened to read first: one that
//! says it is "not upgraded yet" is opened again to change what is there (`Access::Existing`, which
//! upgrades it, see [`crate::backend`]). `upgraded` is true when a database was upgraded, `waiting`
//! when one needs an upgrade this user cannot make because a permission is missing (a read-only
//! role): it waits for a teammate's update. Any other failure (a timeout, a database that does not
//! match) is a failure with its message, not a wait. A database that does not exist yet, or is
//! current, is neither. A team database that cannot be reached is `unreachable` once the places
//! that can be reached were upgraded.

use serde_json::json;

use super::{Context, Done, Failure, Reply};
use crate::backend::{Access, OpenError};
use crate::cli::Args;
use crate::config::Place;

/// What a server says while a database is not upgraded yet (Harness Desktop reads these words).
pub(super) const NOT_UPGRADED: &str = "not upgraded yet";

/// The sentence of a database from an older server that was not upgraded yet.
pub(super) const NOT_UPGRADED_MESSAGE: &str =
    "the knowledge database is not upgraded yet: it is from an older server, run `harness-knowledge upgrade`";

/// `message` of a failed open, with the stores' "has not been upgraded yet" said as
/// [`NOT_UPGRADED_MESSAGE`] (the words Harness Desktop matches, and what to do).
pub(super) fn worded(message: String) -> String {
    if message.contains("been upgraded yet") {
        NOT_UPGRADED_MESSAGE.to_owned()
    } else {
        message
    }
}

/// What `upgrade` found in one place.
enum Found {
    Current,
    Upgraded,
    Waiting,
}

/// `upgrade`.
pub(super) fn upgrade(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json"], &[]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") || !parsed.positional.is_empty() {
        return Err(Failure::Usage);
    }

    let (mut upgraded, mut waiting) = (false, false);
    let mut unreachable = None;

    for place in context.routing.places() {
        match upgrade_place(context, place) {
            Ok(Found::Upgraded) => upgraded = true,
            Ok(Found::Waiting) => waiting = true,
            Ok(Found::Current) => {}
            Err(Failure::Unreachable(message)) => unreachable = unreachable.or(Some(message)),
            Err(other) => return Err(other),
        }
    }

    if let Some(message) = unreachable {
        return Err(Failure::Unreachable(message));
    }

    Ok(Done::ok(json!({ "upgraded": upgraded, "waiting": waiting })))
}

fn upgrade_place(context: &Context, place: Place) -> Result<Found, Failure> {
    match context.open_place(place, Access::Read) {
        Ok(_) => return Ok(Found::Current),
        Err(Failure::Failed(message)) if message.contains(NOT_UPGRADED) => {}
        Err(other) => return Err(other),
    }

    match crate::backend::open_place(context.env, context.backend, place, Access::Existing) {
        Ok(_) => Ok(Found::Upgraded),
        Err(OpenError::Failed(message)) if is_permission(&message) => {
            eprintln!(
                "harness-knowledge: the {} database waits for an upgrade this user cannot make: {message}",
                place.as_str()
            );
            Ok(Found::Waiting)
        }
        // A timeout, a database that does not match, anything but a missing permission is a failure
        // the person has to see: it would not be fixed by waiting for a teammate.
        Err(OpenError::Failed(message)) => Err(Failure::Failed(worded(message))),
        Err(other) => Err(other.into()),
    }
}

/// Whether a failed upgrade says the user lacks the permission to make it: a role without the
/// privilege (PostgreSQL SQLSTATE 42501), a read-only or unwritable file (SQLite), a refused token
/// or a database that is not writable (libSQL), a user that is not authorized (MongoDB).
fn is_permission(message: &str) -> bool {
    let message = message.to_lowercase();

    [
        "lacks a permission",
        "sqlstate 42501",
        "permission denied",
        "permissiondenied",
        "readonly",
        "read-only",
        "read only",
        "not authorized",
        "unauthorized",
        "forbidden",
    ]
    .iter()
    .any(|sign| message.contains(sign))
}

#[cfg(test)]
mod tests {
    use super::is_permission;

    #[test]
    fn only_a_missing_permission_is_a_wait() {
        for message in [
            "the database role lacks a permission the knowledge store needs (SQLSTATE 42501)",
            "knowledge database error: ReadOnly",
            "knowledge database error: the database was opened read-only",
            "knowledge database error: PermissionDenied",
            "the server answered 403 Forbidden",
            "command createIndexes requires authentication: Unauthorized",
        ] {
            assert!(is_permission(message), "{message}");
        }

        for message in [
            "upgrading the knowledge database is still running, try again later",
            "the remote database did not answer in time",
            "the knowledge database could not be upgraded",
            "knowledge database error: duplicate column name: status",
            "the database server has too many connections (SQLSTATE 53300)",
        ] {
            assert!(!is_permission(message), "{message}");
        }
    }
}
