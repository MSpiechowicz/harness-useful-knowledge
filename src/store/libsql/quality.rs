//! The lookups and counters behind duplicate detection and usefulness (see
//! [`crate::store::sqlite`], whose statements these are).

use super::LibsqlStore;
use super::documents::by_id;
use super::hrana::COUNT_BUDGET;
use super::sql::{Sql, Value, args};
use super::wire::execute_request;
use crate::store::quality::{TITLE_SCAN_MAX, matching_titles};
use crate::store::sqlite::sql_quality::{
    SHOWN_SQL, USEFUL_SQL, ids_json, sharing_sql, shas_json, titles_global_sql, titles_repo_sql,
};
use crate::store::{Columns, Document, StoreError, TitleScope, Visibility};

/// The documents of `scope` whose title equals `title`, by id.
pub(super) fn by_title(
    db: &mut dyn Sql,
    columns: Columns,
    title: &str,
    scope: &TitleScope,
    visibility: Visibility,
) -> Result<Vec<Document>, StoreError> {
    let rows = match scope {
        TitleScope::Global => db.query(&titles_global_sql(visibility), args![TITLE_SCAN_MAX])?,
        TitleScope::Repo(key) => db.query(&titles_repo_sql(visibility), args![TITLE_SCAN_MAX, key])?,
    };
    let titles = rows
        .iter()
        .map(|row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?)))
        .collect::<Result<Vec<_>, StoreError>>()?;

    let mut found = Vec::new();

    for id in matching_titles(titles, title) {
        if let Some((doc, _)) = by_id(db, columns, id)? {
            found.push(doc);
        }
    }

    Ok(found)
}

/// Each document with a chunk whose hash is in `shas`, with how many distinct ones.
pub(super) fn sharing(
    db: &mut dyn Sql,
    shas: &[String],
    visibility: Visibility,
) -> Result<Vec<(i64, usize)>, StoreError> {
    let wanted = shas_json(shas)?;

    if shas.is_empty() {
        return Ok(Vec::new());
    }

    db.query(&sharing_sql(visibility), args![wanted])?
        .iter()
        .map(|row| Ok((row.get(0)?, row.get(1)?)))
        .collect()
}

impl LibsqlStore {
    /// Runs a counter statement on `ids` as one request that is given up after [`COUNT_BUDGET`]
    /// and never waits for a write transaction: nothing without the counter columns. A lock another
    /// writer holds is [`StoreError::Busy`]: this count is dropped, the handle is fine.
    fn count(&mut self, sql: &str, ids: &[i64], at: Option<i64>) -> Result<(), StoreError> {
        if ids.is_empty() || !self.columns.usage {
            return Ok(());
        }

        self.check_version()?;

        let mut stream = self.client.single();
        stream.budget = Some(COUNT_BUDGET);

        let wanted = ids_json(ids);
        let arguments: Vec<Value> = match at {
            Some(at) => vec![wanted.into(), at.into()],
            None => vec![wanted.into()],
        };

        stream
            .send(vec![execute_request(sql, &arguments, false)])?
            .remove(0)
            .into_single_attempt()
            .map(drop)
    }

    pub(super) fn count_shown(&mut self, ids: &[i64]) -> Result<(), StoreError> {
        self.count(SHOWN_SQL, ids, None)
    }

    pub(super) fn count_useful(&mut self, ids: &[i64], at: i64) -> Result<(), StoreError> {
        self.count(USEFUL_SQL, ids, Some(at))
    }
}
