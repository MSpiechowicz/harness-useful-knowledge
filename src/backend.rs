//! Opens the knowledge store the settings select.
//!
//! The local SQLite file works. The team databases (PostgreSQL, MongoDB, libSQL) are not built
//! yet: opening one is an [`OpenError::NotBuilt`] that says so, and each gets its store later.

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
    /// This build has no store for the selected database yet.
    #[error("{0}")]
    NotBuilt(String),
    /// The local SQLite has no FTS5, so there is no keyword index.
    #[error("this SQLite build has no FTS5 full-text search")]
    NoFts5,
    /// The database could not be opened.
    #[error("{0}")]
    Failed(String),
}

/// The store of `backend`, or `None` when `access` does not create the database and it does not
/// exist (callers answer as for an empty one).
pub fn open(env: &Env, backend: &Backend, access: Access) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    match backend {
        Backend::Sqlite => open_local(env, access),
        other => Err(OpenError::NotBuilt(format!(
            "{} is not supported by this build yet, use the local database",
            other.describe()
        ))),
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
        Err(StoreError::NoFts5) => Err(OpenError::NoFts5),
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
    fn a_team_database_is_not_built_yet() {
        let env = Env::for_tests("/home/u");

        for url in [
            "postgres://u:secret@db/k",
            "mongodb://u:secret@db/k",
            "libsql://db.example.com",
        ] {
            let backend = crate::config::backend_for(Some(url.to_owned()), None).expect("backend");
            let error = open(&env, &backend, Access::Read).err().expect("not built");

            assert!(matches!(&error, OpenError::NotBuilt(message) if message.contains("not supported")));
            assert!(!error.to_string().contains("secret"), "{error}");
        }
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
