//! Opens the knowledge store the settings select: the local SQLite file or a team database
//! (PostgreSQL, MongoDB, libSQL).
//!
//! A database that lacks a capability the knowledge needs (pgvector, `mongot`, vectors in libSQL,
//! FTS5) is an [`OpenError::Missing`] with the `check` problem code, which every command answers
//! with exit 3.

use crate::config::Backend;
use crate::location::{DIR_VAR, database_path, override_is_relative};
use crate::store::{KnowledgeStore, SqliteStore, StoreError};
use crate::support::Env;

/// What a command needs from the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads only: a database that does not exist yet is not created, [`open`] answers `None`.
    Read,
    /// Reads and changes what is there: a database that does not exist is not created either.
    Existing,
    /// Reads and writes, creating the database when it does not exist (an import).
    Create,
}

/// Why no store was opened. Messages carry no credentials.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OpenError {
    /// The database lacks a capability: `code` is the problem code of `check` (`no-fts5`,
    /// `no-pgvector`, `pgvector-no-privilege`, `pgvector-too-old`, `index-building`, `no-mongot`
    /// or `libsql-no-vector`).
    #[error("{message}")]
    Missing { code: &'static str, message: String },
    /// The database could not be opened.
    #[error("{0}")]
    Failed(String),
}

impl OpenError {
    /// A SQLite (the local file or a libSQL server) without FTS5.
    pub fn no_fts5() -> Self {
        Self::Missing {
            code: "no-fts5",
            message: "this SQLite build has no FTS5 full-text search, so the keyword index cannot be kept".to_owned(),
        }
    }
}

/// The store of `backend`, or `None` when `access` does not create the database and it does not
/// exist (callers answer as for an empty one).
pub fn open(env: &Env, backend: &Backend, access: Access) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    match backend {
        Backend::Sqlite => open_local(env, access),
        Backend::Postgres { url } => crate::store::postgres::open_boxed(url, access, &backend.secrets()),
        Backend::Libsql { url, token } => {
            crate::store::libsql::open_boxed(url, token.as_deref(), access, &backend.secrets())
        }
        Backend::Mongodb { url } => crate::store::mongo::open_boxed(url, access, &backend.secrets()),
    }
}

/// The name of the backend, as `check` reports it.
pub fn name(backend: &Backend) -> &'static str {
    match backend {
        Backend::Sqlite => "sqlite",
        Backend::Postgres { .. } => "postgres",
        Backend::Mongodb { .. } => "mongodb",
        Backend::Libsql { .. } => "libsql",
    }
}

fn open_local(env: &Env, access: Access) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    let Some(path) = database_path(env) else {
        return Err(OpenError::Failed("the data folder is unknown".to_owned()));
    };

    if override_is_relative(env) {
        return Err(OpenError::Failed(format!("{DIR_VAR} must be an absolute folder")));
    }

    let opened = match access {
        Access::Read => SqliteStore::open_read_only(&path),
        Access::Existing => SqliteStore::open_existing(&path),
        Access::Create => SqliteStore::open(&path).map(Some),
    };

    match opened {
        Ok(store) => Ok(store.map(|store| Box::new(store) as Box<dyn KnowledgeStore>)),
        Err(StoreError::NoFts5) => Err(OpenError::no_fts5()),
        Err(error) => Err(OpenError::Failed(format!("cannot open the local database: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::FILE_NAME;

    fn env_in(dir: &std::path::Path) -> Env {
        Env::for_tests("/home/u").with_var(DIR_VAR, dir.to_string_lossy())
    }

    #[test]
    fn reading_and_changing_never_create_the_local_database_and_creating_does() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("knowledge");
        let env = env_in(&folder);

        assert!(open(&env, &Backend::Sqlite, Access::Read).expect("read").is_none());
        assert!(
            open(&env, &Backend::Sqlite, Access::Existing)
                .expect("existing")
                .is_none()
        );
        assert!(!folder.exists());

        assert!(open(&env, &Backend::Sqlite, Access::Create).expect("create").is_some());
        assert!(folder.join(FILE_NAME).is_file());

        assert!(open(&env, &Backend::Sqlite, Access::Read).expect("read").is_some());
    }

    #[test]
    fn a_relative_folder_override_is_refused() {
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, "relative");

        assert!(matches!(
            open(&env, &Backend::Sqlite, Access::Read),
            Err(OpenError::Failed(_))
        ));
    }
}
