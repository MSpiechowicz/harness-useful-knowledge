//! How a write runs: in a transaction when the deployment has them, retried from the start on
//! transient errors and write conflicts.

use std::thread;
use std::time::Duration;

use mongodb::error::{Error, TRANSIENT_TRANSACTION_ERROR, UNKNOWN_TRANSACTION_COMMIT_RESULT};
use mongodb::options::{ReadConcern, WriteConcern};

use super::connection;
use super::ctx::{Ctx, Fail};
use super::{MongoStore, StoreError};

/// Attempts of one write before it gives up on concurrent changes.
const WRITE_ATTEMPTS: u32 = 20;

/// Attempts to commit a transaction whose outcome is unknown.
const COMMIT_ATTEMPTS: u32 = 3;

impl MongoStore {
    /// Runs `op` as one write that counts in the generation: in a transaction when the deployment
    /// has them, retried from the start on transient errors and write conflicts.
    pub(super) fn write<T>(&mut self, mut op: impl FnMut(&mut Ctx<'_>) -> Result<T, Fail>) -> Result<T, StoreError> {
        self.write_if(|ctx| op(ctx).map(|done| (done, true)))
    }

    /// [`Self::write`] for an `op` that says whether it changed anything: a refusal changes nothing
    /// and does not count as a write.
    pub(super) fn write_if<T>(
        &mut self,
        mut op: impl FnMut(&mut Ctx<'_>) -> Result<(T, bool), Fail>,
    ) -> Result<T, StoreError> {
        if self.read_only {
            return Err(StoreError::Backend(
                "the database was opened for reading only".to_owned(),
            ));
        }

        self.check_version()?;

        let mut counted = |ctx: &mut Ctx<'_>| {
            let (done, changed) = op(ctx)?;

            if changed {
                ctx.bump_generation()?;
            }

            Ok(done)
        };

        for attempt in 0..WRITE_ATTEMPTS {
            if attempt > 0 {
                thread::sleep(backoff(attempt));
            }

            let outcome = if self.transactions {
                self.in_transaction(&mut counted)
            } else {
                counted(&mut Ctx::new(&self.db, None))
            };

            match outcome {
                Ok(value) => return Ok(value),
                Err(Fail::Store(error)) => return Err(error),
                Err(Fail::Db(error)) if retryable(&error) => {}
                Err(Fail::Db(error)) => return Err(connection::backend(&error)),
            }
        }

        Err(StoreError::Backend(
            "too many concurrent changes to the same data, try again".to_owned(),
        ))
    }

    fn in_transaction<T>(&self, op: &mut impl FnMut(&mut Ctx<'_>) -> Result<T, Fail>) -> Result<T, Fail> {
        let mut session = self.client.start_session().run()?;
        session
            .start_transaction()
            .read_concern(ReadConcern::snapshot())
            .write_concern(WriteConcern::majority())
            .run()?;

        let value = match op(&mut Ctx::new(&self.db, Some(&mut session))) {
            Ok(value) => value,
            Err(fail) => {
                // Dropping the session aborts too, an abort failure changes nothing here.
                let _ = session.abort_transaction().run();
                return Err(fail);
            }
        };

        let mut attempt = 0;
        loop {
            attempt += 1;

            match session.commit_transaction().run() {
                Ok(()) => return Ok(value),
                Err(error) if error.contains_label(UNKNOWN_TRANSACTION_COMMIT_RESULT) && attempt < COMMIT_ATTEMPTS => {}
                Err(error) => return Err(Fail::Db(error)),
            }
        }
    }
}

/// A transient transaction error or a write conflict: the next attempt starts from a fresh read.
fn retryable(error: &Error) -> bool {
    error.contains_label(TRANSIENT_TRANSACTION_ERROR) || connection::error_code(error) == Some(112)
}

/// A growing pause between attempts, spread by the thread so racing writers drift apart.
fn backoff(attempt: u32) -> Duration {
    let spread = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        thread::current().id().hash(&mut hasher);
        attempt.hash(&mut hasher);
        hasher.finish() % 8
    };

    Duration::from_millis(u64::from(attempt.min(6)) * 4 + spread)
}
