//! The pgvector extension: which version the server has, whether this role can add it, and what
//! its absence is called in a `check` ([`Problem`]).
//!
//! The extension is looked for in the order a person would ask: already installed in this
//! database (`pg_extension`), else installable on this server (`pg_available_extensions`), else
//! created (`CREATE EXTENSION IF NOT EXISTS vector`). Only the last one changes anything. The HNSW
//! index needs pgvector 0.5, iterative index scans (a filtered search that keeps looking until it
//! has enough rows) 0.8.
//!
//! The extension lives in the schema that was first in the `search_path` when it was created. A
//! session whose `search_path` does not list that schema (another schema of the same database)
//! gets it appended by [`reach`], so the vector type and its operators resolve everywhere.

use std::fmt;

use postgres::GenericClient;

use super::error::{backend, column, described};
use crate::store::StoreError;

/// The first pgvector with the HNSW index.
const HNSW_MIN: Version = Version(0, 5, 0);

/// The first pgvector whose HNSW scans can continue until a filter is satisfied.
const ITERATIVE_MIN: Version = Version(0, 8, 0);

/// A pgvector version (`0.8.7`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// The version in `text`: three numbers separated by dots, a missing one is 0 and anything
    /// after the digits of one (`0.8.0-dev`) is dropped. `None` when there is no number at all.
    pub fn parse(text: &str) -> Option<Self> {
        let mut numbers = text.trim().split('.').map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u32>().ok()
        });

        let major = numbers.next().flatten()?;
        let minor = numbers.next().flatten().unwrap_or(0);
        let patch = numbers.next().flatten().unwrap_or(0);

        Some(Self(major, minor, patch))
    }

    /// True when this version has the HNSW index.
    pub fn has_hnsw(self) -> bool {
        self >= HNSW_MIN
    }

    /// True when a filtered HNSW scan can run on until it has enough rows.
    pub fn has_iterative_scan(self) -> bool {
        self >= ITERATIVE_MIN
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Why the database cannot serve knowledge, with the `check` problem code of each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The server has no pgvector to install.
    NoPgvector,
    /// pgvector is installable here, but not by this role.
    NoPrivilege,
    /// The pgvector there is has no HNSW index.
    TooOld(Version),
    /// The HNSW index exists but is not valid yet (a build is running or was interrupted).
    IndexBuilding,
    /// The server did not answer (the reason, without the URL).
    Unreachable(String),
    /// The server answered, but the TLS check refused it (a certificate, a server that does not
    /// speak TLS): a setting to fix, reported like an unreachable server (the reason, without the
    /// URL).
    Refused(String),
    /// The server refused the login.
    Auth,
}

impl Problem {
    /// The problem code of `check --json`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoPgvector => "no-pgvector",
            Self::NoPrivilege => "pgvector-no-privilege",
            Self::TooOld(_) => "pgvector-too-old",
            Self::IndexBuilding => "index-building",
            Self::Unreachable(_) | Self::Refused(_) => "unreachable",
            Self::Auth => "auth",
        }
    }

    /// True for a capability the database lacks (exit 3 of a command), false for a server that
    /// could not be reached or refused the login.
    pub fn is_missing(&self) -> bool {
        !matches!(self, Self::Unreachable(_) | Self::Refused(_) | Self::Auth)
    }

    /// What to tell the person, in a sentence or two. It never holds the URL or a password.
    pub fn message(&self) -> String {
        match self {
            Self::NoPgvector => "the pgvector extension is not installed on this PostgreSQL server, so vectors cannot \
                                 be searched. Use a server with pgvector (the pgvector/pgvector image or the \
                                 pgvector package) at version 0.5 or newer"
                .to_owned(),
            Self::NoPrivilege => "pgvector is available on this server, but the database role may not create it. Ask \
                                  an administrator to run CREATE EXTENSION vector in this database"
                .to_owned(),
            Self::TooOld(version) => {
                format!("pgvector {version} has no HNSW index. Update the extension to version {HNSW_MIN} or newer")
            }
            Self::IndexBuilding => "the vector index is still being built, try again in a moment".to_owned(),
            Self::Unreachable(reason) | Self::Refused(reason) => format!("the database cannot be reached ({reason})"),
            Self::Auth => "the database refused the login, check the user and password in the database URL".to_owned(),
        }
    }
}

/// A [`Problem`] or a store failure: what an open or a probe can end in.
#[derive(Debug)]
pub enum Fault {
    Problem(Problem),
    Store(StoreError),
}

impl From<StoreError> for Fault {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<Problem> for Fault {
    fn from(problem: Problem) -> Self {
        Self::Problem(problem)
    }
}

impl From<Fault> for StoreError {
    fn from(fault: Fault) -> Self {
        match fault {
            Fault::Problem(problem) => Self::Backend(problem.message()),
            Fault::Store(error) => error,
        }
    }
}

/// The version of pgvector installed in this database, if any.
pub(super) fn installed(client: &mut impl GenericClient) -> Result<Option<Version>, StoreError> {
    let row = client
        .query_opt("SELECT extversion FROM pg_extension WHERE extname = 'vector'", &[])
        .map_err(backend)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let text: String = column(&row, 0)?;

    Version::parse(&text)
        .map(Some)
        .ok_or_else(|| StoreError::Backend("the pgvector version is not a version number".to_owned()))
}

/// pgvector as the server offers it, not yet installed in this database.
pub(super) struct Available {
    pub version: Version,
    /// This role may run `CREATE EXTENSION vector`: a superuser, or a role with `CREATE` on the
    /// database when the extension is trusted or needs no superuser.
    pub installable: bool,
}

/// pgvector's default version on this server and whether this role may install it; `None` when
/// the server has no pgvector.
pub(super) fn available(client: &mut impl GenericClient) -> Result<Option<Available>, StoreError> {
    let row = client
        .query_opt(
            "SELECT e.default_version,
                    r.rolsuper OR ((NOT v.superuser OR v.trusted)
                                   AND has_database_privilege(current_database(), 'CREATE'))
             FROM pg_available_extensions e
             JOIN pg_available_extension_versions v ON v.name = e.name AND v.version = e.default_version
             JOIN pg_roles r ON r.rolname = current_user
             WHERE e.name = 'vector'",
            &[],
        )
        .map_err(backend)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let text: String = column(&row, 0)?;
    let version = Version::parse(&text)
        .ok_or_else(|| StoreError::Backend("the pgvector version is not a version number".to_owned()))?;

    Ok(Some(Available {
        version,
        installable: column(&row, 1)?,
    }))
}

/// pgvector installed in this database, adding it when it is not (see the module docs).
pub(super) fn ensure(client: &mut impl GenericClient) -> Result<Version, Fault> {
    if let Some(version) = installed(client)? {
        return if version.has_hnsw() {
            Ok(version)
        } else {
            Err(Problem::TooOld(version).into())
        };
    }

    match available(client)? {
        None => return Err(Problem::NoPgvector.into()),
        Some(offered) if !offered.version.has_hnsw() => return Err(Problem::TooOld(offered.version).into()),
        Some(_) => {}
    }

    if let Err(error) = client.batch_execute("CREATE EXTENSION IF NOT EXISTS vector") {
        return Err(match error.as_db_error().map(|db| db.code().code()) {
            Some("42501") => Problem::NoPrivilege.into(),
            Some("0A000") => Problem::NoPgvector.into(),
            Some(code) => StoreError::Backend(described(code)).into(),
            None => backend(error).into(),
        });
    }

    let version = installed(client)?.ok_or_else(|| StoreError::Backend("pgvector was not created".to_owned()))?;

    if version.has_hnsw() {
        Ok(version)
    } else {
        Err(Problem::TooOld(version).into())
    }
}

/// Appends the schema pgvector lives in to the session's `search_path` when it is not listed, so
/// the `vector` type and its operators resolve. The first entry (where the tables go) is kept.
pub(super) fn reach(client: &mut impl GenericClient) -> Result<(), StoreError> {
    let row = client
        .query_opt(
            "SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
             WHERE e.extname = 'vector' AND n.nspname <> ALL (current_schemas(false))",
            &[],
        )
        .map_err(backend)?;

    let Some(row) = row else {
        return Ok(());
    };

    let schema: String = column(&row, 0)?;

    client
        .execute(
            "SELECT set_config('search_path', current_setting('search_path') || ', ' || quote_ident($1), false)",
            &[&schema],
        )
        .map_err(backend)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_compare_and_name_the_features_they_have() {
        assert_eq!(Version::parse("0.8.7"), Some(Version(0, 8, 7)));
        assert_eq!(Version::parse(" 0.5 "), Some(Version(0, 5, 0)));
        assert_eq!(Version::parse("0.8.0-dev"), Some(Version(0, 8, 0)));
        assert_eq!(Version::parse("1"), Some(Version(1, 0, 0)));
        assert_eq!(Version::parse("vector"), None);
        assert_eq!(Version::parse(""), None);

        assert!(!Version(0, 4, 4).has_hnsw());
        assert!(Version(0, 5, 0).has_hnsw());
        assert!(Version(0, 5, 1).has_hnsw() && !Version(0, 5, 1).has_iterative_scan());
        assert!(!Version(0, 7, 4).has_iterative_scan());
        assert!(Version(0, 8, 0).has_iterative_scan());
        assert!(
            Version(0, 10, 0).has_iterative_scan(),
            "versions compare as numbers, not text"
        );
        assert_eq!(Version(0, 8, 7).to_string(), "0.8.7");
    }

    #[test]
    fn every_problem_has_a_code_and_a_message_without_secrets() {
        let problems = [
            (Problem::NoPgvector, "no-pgvector"),
            (Problem::NoPrivilege, "pgvector-no-privilege"),
            (Problem::TooOld(Version(0, 4, 4)), "pgvector-too-old"),
            (Problem::IndexBuilding, "index-building"),
            (Problem::Unreachable("Connection refused".to_owned()), "unreachable"),
            (Problem::Auth, "auth"),
        ];

        for (problem, code) in problems {
            assert_eq!(problem.code(), code);
            assert!(!problem.message().is_empty());
            assert!(!problem.message().contains(';'), "{}", problem.message());
        }

        assert!(Problem::TooOld(Version(0, 4, 4)).message().contains("0.4.4"));
        assert!(Problem::TooOld(Version(0, 4, 4)).message().contains("0.5.0"));
    }
}
