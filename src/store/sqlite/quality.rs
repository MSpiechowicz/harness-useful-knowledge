//! The lookups and counters behind duplicate detection and usefulness: titles in a scope, the
//! documents that share chunks, and the counters of how often a document was shown or helped.

use std::time::Duration;

use rusqlite::{Connection, params};

use super::SqliteStore;
use super::documents::by_id;
use super::sql_quality::{SHOWN_SQL, USEFUL_SQL, ids_json, sharing_sql, shas_json, titles_global_sql, titles_repo_sql};
use crate::store::quality::{TITLE_SCAN_MAX, matching_titles};
use crate::store::{Columns, Document, StoreError, TitleScope, Visibility};

/// The documents of `scope` whose title equals `title`, by id.
pub(super) fn by_title(
    conn: &Connection,
    columns: Columns,
    title: &str,
    scope: &TitleScope,
    visibility: Visibility,
) -> Result<Vec<Document>, StoreError> {
    let limit = i64::try_from(TITLE_SCAN_MAX).unwrap_or(i64::MAX);
    let map = |row: &rusqlite::Row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?));

    let titles: Vec<(i64, String)> = match scope {
        TitleScope::Global => conn
            .prepare(&titles_global_sql(visibility))?
            .query_map([limit], map)?
            .collect::<Result<_, _>>()?,
        TitleScope::Repo(key) => conn
            .prepare(&titles_repo_sql(visibility))?
            .query_map(params![limit, key], map)?
            .collect::<Result<_, _>>()?,
    };

    let mut found = Vec::new();

    for id in matching_titles(titles, title) {
        if let Some((doc, _)) = by_id(conn, columns, id)? {
            found.push(doc);
        }
    }

    Ok(found)
}

/// Each document with a chunk whose hash is in `shas`, with how many distinct ones.
pub(super) fn sharing(
    conn: &Connection,
    shas: &[String],
    visibility: Visibility,
) -> Result<Vec<(i64, usize)>, StoreError> {
    let wanted = shas_json(shas)?;

    if shas.is_empty() {
        return Ok(Vec::new());
    }

    let mut statement = conn.prepare(&sharing_sql(visibility))?;
    let rows = statement.query_map([wanted], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, usize>(1)?)))?;

    Ok(rows.collect::<Result<_, _>>()?)
}

impl SqliteStore {
    /// Runs a counter statement on `ids` once: it never waits for the write lock another process
    /// holds (a busy file fails at once), and does nothing without the counter columns.
    pub(super) fn count(&mut self, sql: &str, ids: &[i64], at: Option<i64>) -> Result<(), StoreError> {
        if ids.is_empty() || !self.columns.usage {
            return Ok(());
        }

        self.conn()?;
        let wanted = ids_json(ids);

        // No busy handler: a lock another process holds fails the statement at once.
        self.conn.busy_timeout(Duration::ZERO)?;

        let done = match at {
            Some(at) => self.conn.execute(sql, params![wanted, at]),
            None => self.conn.execute(sql, params![wanted]),
        };

        // The waiting of every other statement is back, whatever this one did.
        let _ = self.conn.busy_timeout(self.busy);

        // Only here is a held lock the expected answer: everywhere else it stays a backend error.
        match done {
            Ok(_) => Ok(()),
            Err(error) if super::is_busy(&error) => Err(StoreError::Busy),
            Err(error) => Err(StoreError::from(error)),
        }
    }

    pub(super) fn count_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        self.count(SHOWN_SQL, ids, None)
    }

    pub(super) fn count_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.count(USEFUL_SQL, ids, Some(at))
    }
}
