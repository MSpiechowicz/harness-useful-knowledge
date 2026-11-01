//! One attempt's access to the database: inside a transaction when a session is set.

use mongodb::action::Action;
use mongodb::bson::{Bson, Document, doc};
use mongodb::error::Error;
use mongodb::options::ReturnDocument;
use mongodb::sync::{ClientSession, Collection, Database};

use super::schema::META;
use crate::store::StoreError;

/// Why one attempt of an operation stopped.
pub(super) enum Fail {
    /// A final answer for the caller (not found, invalid).
    Store(StoreError),
    /// A driver error, retried when transient (see [`super::retryable`]).
    Db(Error),
}

impl From<StoreError> for Fail {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<Error> for Fail {
    fn from(error: Error) -> Self {
        Self::Db(error)
    }
}

impl Fail {
    /// The error for the caller; a driver error as [`super::connection::backend`] puts it.
    pub(super) fn into_store(self) -> StoreError {
        match self {
            Self::Store(error) => error,
            Self::Db(error) => super::connection::backend(&error),
        }
    }
}

pub(super) struct Ctx<'a> {
    db: &'a Database,
    session: Option<&'a mut ClientSession>,
}

impl<'a> Ctx<'a> {
    pub(super) fn new(db: &'a Database, session: Option<&'a mut ClientSession>) -> Self {
        Self { db, session }
    }

    fn collection(&self, name: &str) -> Collection<Document> {
        self.db.collection(name)
    }

    pub(super) fn find_one(
        &mut self,
        collection: &str,
        filter: Document,
        projection: Document,
    ) -> Result<Option<Document>, Fail> {
        let found = self
            .collection(collection)
            .find_one(filter)
            .projection(projection)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(found)
    }

    /// The documents matching `filter` in `sort` order, after skipping `skip`, at most `limit`
    /// (0: no limit), with only the fields of `projection`.
    pub(super) fn find(
        &mut self,
        collection: &str,
        filter: Document,
        (sort, skip, limit): (Document, u64, i64),
        projection: Document,
    ) -> Result<Vec<Document>, Fail> {
        let collection = self.collection(collection);
        let action = collection
            .find(filter)
            .sort(sort)
            .skip(skip)
            .limit(limit)
            .projection(projection);

        let found = match self.session.as_deref_mut() {
            Some(session) => {
                let mut cursor = action.session(&mut *session).run()?;
                cursor.iter(session).collect::<Result<Vec<_>, _>>()?
            }
            None => action.run()?.collect::<Result<Vec<_>, _>>()?,
        };

        Ok(found)
    }

    pub(super) fn aggregate(&mut self, collection: &str, pipeline: Vec<Document>) -> Result<Vec<Document>, Fail> {
        let collection = self.collection(collection);
        let action = collection.aggregate(pipeline);

        let found = match self.session.as_deref_mut() {
            Some(session) => {
                let mut cursor = action.session(&mut *session).run()?;
                cursor.iter(session).collect::<Result<Vec<_>, _>>()?
            }
            None => action.run()?.collect::<Result<Vec<_>, _>>()?,
        };

        Ok(found)
    }

    pub(super) fn insert_many(&mut self, collection: &str, documents: Vec<Document>) -> Result<(), Fail> {
        if documents.is_empty() {
            return Ok(());
        }

        self.collection(collection)
            .insert_many(documents)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(())
    }

    /// Updates the first match (inserting a document when `upsert` and none matches); the number
    /// of matched documents.
    pub(super) fn update_one(
        &mut self,
        collection: &str,
        filter: Document,
        update: Document,
        upsert: bool,
    ) -> Result<u64, Fail> {
        let result = self
            .collection(collection)
            .update_one(filter, update)
            .upsert(upsert)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(result.matched_count + u64::from(result.upserted_id.is_some()))
    }

    pub(super) fn update_many(&mut self, collection: &str, filter: Document, update: Document) -> Result<u64, Fail> {
        let result = self
            .collection(collection)
            .update_many(filter, update)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(result.matched_count)
    }

    pub(super) fn delete_many(&mut self, collection: &str, filter: Document) -> Result<u64, Fail> {
        let result = self
            .collection(collection)
            .delete_many(filter)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(result.deleted_count)
    }

    pub(super) fn count(&mut self, collection: &str, filter: Document) -> Result<u64, Fail> {
        let count = self
            .collection(collection)
            .count_documents(filter)
            .optional(self.session.as_deref_mut(), |action, session| action.session(session))
            .run()?;

        Ok(count)
    }

    /// Reserves `count` consecutive ids of the counter `name` and returns the first. Taken outside
    /// the transaction, so concurrent writers do not conflict on the counter. An id of an attempt
    /// that is retried is skipped, never reused.
    pub(super) fn reserve_ids(&mut self, name: &str, count: usize) -> Result<i64, Fail> {
        let count = i64::try_from(count).unwrap_or(i64::MAX);

        let counter = self
            .collection(META)
            .find_one_and_update(
                doc! { "_id": format!("counter:{name}") },
                doc! { "$inc": { "value": count } },
            )
            .upsert(true)
            .return_document(ReturnDocument::After)
            .projection(doc! { "value": 1 })
            .run()?;

        let last = counter.as_ref().map_or(count, |counter| super::int(counter, "value"));

        Ok(last - count + 1)
    }

    /// Counts one more write (see [`crate::store::KnowledgeStore::generation`]).
    pub(super) fn bump_generation(&mut self) -> Result<(), Fail> {
        self.update_one(
            META,
            doc! { "_id": "generation" },
            doc! { "$inc": { "value": 1_i64 } },
            true,
        )?;

        Ok(())
    }
}

/// A BSON integer from a count.
pub(super) fn long(value: usize) -> Bson {
    Bson::Int64(i64::try_from(value).unwrap_or(i64::MAX))
}
