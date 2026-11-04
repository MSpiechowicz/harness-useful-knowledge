//! The failures every backend reports.

/// Store failures; every message is safe to show to the agent.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The input breaks a limit or a rule.
    #[error("{0}")]
    Invalid(String),
    /// A team database was asked to store a repository key that is a folder on this computer
    /// (`path:`): only a `git:` key leaves it. The message names the folder, never its path.
    #[error("{0}")]
    LocalPath(String),
    /// The document or chunk does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The database was written by a newer server; nothing is read or written.
    #[error("the knowledge database is newer than this server")]
    NewerSchema,
    /// This SQLite build has no FTS5, so there is no keyword index.
    #[error("this SQLite build has no FTS5 full-text search")]
    NoFts5,
    /// The database lacks a capability (a `check` problem code such as `no-pgvector`): a command
    /// answers with exit 3.
    #[error("{message}")]
    Missing { code: &'static str, message: String },
    /// The remote database could not be reached (connect timeout, refused connection, DNS, a lost
    /// connection). A routed store answers from this computer meanwhile (see [`reach`]).
    #[error("{0}")]
    Unreachable(String),
    /// Another process holds the lock a single-attempt write needed (a counter), so it was not
    /// made. The handle is fine and may be used again.
    #[error("the knowledge database is busy")]
    Busy,
    /// The database failed (the message carries no stored text).
    #[error("knowledge database error: {0}")]
    Backend(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        if super::sqlite::is_missing_fts5(&error) {
            return Self::NoFts5;
        }

        let message = match &error {
            rusqlite::Error::SqliteFailure(code, _) => format!("{:?}", code.code),
            other => other.to_string(),
        };

        Self::Backend(message.chars().take(200).collect())
    }
}
