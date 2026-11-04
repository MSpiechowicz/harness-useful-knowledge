//! The lookups and counters behind duplicate detection and usefulness: titles in a scope, the
//! documents that share chunks (both only of the documents the visibility lets through), how often a
//! live document was shown or helped, and whether this role
//! may change documents.

use postgres::GenericClient;

use super::documents::{by_id, seen};
use super::error::{backend, column, count, int};
use super::{PostgresStore, schema};
use crate::limits::SHARED_SHAS_MAX;
use crate::store::quality::{TITLE_SCAN_MAX, matching_titles};
use crate::store::{Columns, Document, StoreError, TitleScope, Visibility};

/// How long a counter statement may wait for a lock or run: a busy database drops the count.
const COUNT_STATEMENT_LIMIT: &str = "500ms";

/// The documents of `scope` whose title equals `title`, by id.
pub(super) fn by_title(
    client: &mut impl GenericClient,
    columns: Columns,
    title: &str,
    scope: &TitleScope,
    visibility: Visibility,
) -> Result<Vec<Document>, StoreError> {
    let limit = int(TITLE_SCAN_MAX);
    let seen = seen(visibility);
    let rows = match scope {
        TitleScope::Global => client.query(
            &format!(
                "SELECT d.id, d.title FROM knowledge_documents d
                 WHERE {seen} AND NOT EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id)
                 ORDER BY d.id LIMIT $1"
            ),
            &[&limit],
        ),
        TitleScope::Repo(key) => client.query(
            &format!(
                "SELECT d.id, d.title FROM knowledge_documents d
                 WHERE {seen} AND EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id AND l.repo_key = $2)
                 ORDER BY d.id LIMIT $1"
            ),
            &[&limit, key],
        ),
    }
    .map_err(backend)?;

    let titles = rows
        .iter()
        .map(|row| Ok((column::<i64>(row, 0)?, column::<String>(row, 1)?)))
        .collect::<Result<Vec<_>, StoreError>>()?;

    let mut found = Vec::new();

    for id in matching_titles(titles, title) {
        if let Some((doc, _)) = by_id(client, columns, id)? {
            found.push(doc);
        }
    }

    Ok(found)
}

/// Each document with a chunk whose hash is in `shas`, with how many distinct ones.
pub(super) fn sharing(
    client: &mut impl GenericClient,
    shas: &[String],
    visibility: Visibility,
) -> Result<Vec<(i64, usize)>, StoreError> {
    if shas.len() > SHARED_SHAS_MAX {
        return Err(StoreError::Invalid(format!(
            "at most {SHARED_SHAS_MAX} chunk hashes at once"
        )));
    }

    if shas.is_empty() {
        return Ok(Vec::new());
    }

    let rows = client
        .query(
            &format!(
                "SELECT c.doc_id, count(DISTINCT c.chunk_sha) FROM knowledge_chunks c
                 JOIN knowledge_documents d ON d.id = c.doc_id
                 WHERE {} AND c.chunk_sha = ANY($1) GROUP BY c.doc_id ORDER BY c.doc_id",
                seen(visibility)
            ),
            &[&shas],
        )
        .map_err(backend)?;

    rows.iter().map(|row| Ok((column(row, 0)?, count(row, 1)?))).collect()
}

/// Whether this role may update documents; `None` when the database cannot tell.
pub(super) fn may_update(client: &mut impl GenericClient) -> Option<bool> {
    let row = client
        .query_one(
            "SELECT has_table_privilege(current_user, 'knowledge_documents', 'UPDATE')",
            &[],
        )
        .ok()?;

    column(&row, 0).ok()
}

impl PostgresStore {
    /// Runs the counter statement `sql` on `ids` once: it takes no write lock (an increment is
    /// atomic on its own), only the migration lock, shared, and no statement of it waits or runs
    /// longer than [`COUNT_STATEMENT_LIMIT`], so a busy database drops the count instead of holding
    /// the caller up. It is not tried again, and a closed connection is not opened again for it.
    /// Nothing without the counter columns.
    pub(super) fn count(&mut self, sql: &str, ids: &[i64], at: Option<i64>) -> Result<(), StoreError> {
        if self.newer {
            return Err(StoreError::NewerSchema);
        }

        if ids.is_empty() || !self.columns.usage {
            return Ok(());
        }

        if self.client.is_closed() {
            return Err(StoreError::Backend("the connection is closed".to_owned()));
        }

        let mut tx = self.client.transaction().map_err(backend)?;
        tx.batch_execute(&format!(
            "SET LOCAL lock_timeout = '{COUNT_STATEMENT_LIMIT}'; SET LOCAL statement_timeout = '{COUNT_STATEMENT_LIMIT}'"
        ))
        .map_err(backend)?;
        tx.execute("SELECT pg_advisory_xact_lock_shared($1)", &[&schema::MIGRATE_LOCK])
            .map_err(backend)?;

        if schema::stored_version(&mut tx)?.is_some_and(|found| found > schema::SCHEMA_VERSION) {
            self.newer = true;
            return Err(StoreError::NewerSchema);
        }

        match at {
            Some(at) => tx.execute(sql, &[&ids, &at]),
            None => tx.execute(sql, &[&ids]),
        }
        .map_err(backend)?;

        tx.commit().map_err(backend)
    }

    pub(super) fn count_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        self.count(
            "UPDATE knowledge_documents
             SET shown = CASE WHEN shown < 9223372036854775807 THEN shown + 1 ELSE shown END
             WHERE status = 'live' AND id = ANY($1::bigint[])",
            ids,
            None,
        )
    }

    pub(super) fn count_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.count(
            "UPDATE knowledge_documents
             SET useful = CASE WHEN useful < 9223372036854775807 THEN useful + 1 ELSE useful END, last_useful_at = $2
             WHERE status = 'live' AND id = ANY($1::bigint[])",
            ids,
            Some(at),
        )
    }
}
