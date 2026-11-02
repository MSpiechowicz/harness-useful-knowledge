//! The columns added in place to `knowledge_documents` (the authors and the usefulness counters):
//! which the database has, and adding the missing ones when it is opened for writing. The schema
//! version stays, so an older server keeps using the database.

use super::hrana::COUNT_BUDGET;
use super::sql::{Sql, args};
use super::{LibsqlStore, Mode};
use crate::store::quality::warn_unavailable;
use crate::store::sqlite::sql_quality::{add_statements, present_sql};
use crate::store::{Columns, StoreError};

/// The names of the added columns the documents table has.
fn names(db: &mut dyn Sql) -> Result<Vec<String>, StoreError> {
    db.query(&present_sql(), args![])?
        .iter()
        .map(|row| row.get(0))
        .collect()
}

impl LibsqlStore {
    /// Which of the added columns the database has, adding the missing ones first when the open
    /// is for writing. A database that refuses them (a token without the right to change the
    /// schema) is used with what it has, with one line on stderr.
    pub(super) fn open_columns(&mut self, mode: Mode) -> Result<Columns, StoreError> {
        let mut look = self.client.single();

        if mode == Mode::Counters {
            look.budget = Some(COUNT_BUDGET);
        }

        let found = Columns::from_names(&names(&mut look)?);

        if matches!(mode, Mode::Read | Mode::Counters) || found.complete() {
            return Ok(found);
        }

        let added = self.transact(|db| {
            // Another client may have added them while this one waited for the lock.
            for statement in add_statements(&names(db)?) {
                db.batch(&statement)?;
            }

            Ok(())
        });

        match added {
            Ok(()) => Ok(Columns::from_names(&names(&mut self.client.single())?)),
            Err(error) => {
                warn_unavailable(&error);
                Ok(found)
            }
        }
    }
}
