//! What `check` asks a MongoDB server: is it reachable, does the user get in, does it run the
//! search engine (`mongot`), and how far the two search indexes have got. Nothing is created or
//! changed: a database nobody wrote to yet is reported as it is.

use mongodb::bson::{Document, doc};
use mongodb::error::{Error, ErrorKind};
use mongodb::sync::Client;

use super::connection::{self, SEARCH_NOT_ENABLED, error_code};
use super::schema::{self, IndexState, KEYWORD_INDEX, VECTOR_INDEX, state_of};
use super::{connect_client, text};
use crate::store::StoreError;

/// The server error of a user that may not run a command.
const UNAUTHORIZED: i32 = 13;

/// The server error of a failed login.
const AUTHENTICATION_FAILED: i32 = 18;

/// A reason `check` reports, with its problem code (`unreachable`, `auth`, `no-mongot` or
/// `index-building`) and a message that carries no credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub code: &'static str,
    pub message: String,
    /// A setting to fix, not a server that is down: the TLS check refused the server, or the URL's
    /// rules refused to connect at all. Reported as `unreachable` like a connection that did not
    /// work, but there is no outage to wait out.
    pub refused: bool,
}

/// A server that answered, and what it can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The schema version of the database, `None` when nothing was written to it yet.
    pub schema: Option<i64>,
    /// The model that embedded the chunks, once one has.
    pub model_id: Option<String>,
    pub keyword: IndexState,
    pub vector: IndexState,
}

impl Probe {
    /// The search indexes are there to answer queries, or there is no database to index yet (its
    /// first import creates them).
    pub fn ready(&self) -> bool {
        self.schema.is_none() || (self.keyword == IndexState::Ready && self.vector == IndexState::Ready)
    }

    /// `index-building` while an index that exists, or the one a database of ours lacks, cannot
    /// answer.
    pub fn problem(&self) -> Option<Problem> {
        if self.ready() {
            return None;
        }

        let missing = self.keyword == IndexState::Missing || self.vector == IndexState::Missing;
        let message = if missing {
            "the search indexes of the knowledge database are missing, the next import creates them"
        } else {
            "the search indexes of the knowledge database are still being built, try again in a moment"
        };

        Some(Problem {
            code: "index-building",
            message: message.to_owned(),
            refused: false,
        })
    }

    /// How the vector index is: `ready`, `building` or `missing`. A database nobody wrote to has
    /// nothing to build, so it counts as ready like the local file does.
    pub fn vector_index(&self) -> &'static str {
        match self.vector {
            _ if self.schema.is_none() => "ready",
            IndexState::Ready => "ready",
            IndexState::Building => "building",
            IndexState::Missing => "missing",
        }
    }
}

/// Asks the server of `url`.
pub fn probe(url: &str) -> Result<Probe, Problem> {
    let (client, db) = connect_client(url, super::SERVER_SELECTION_TIMEOUT).map_err(refused)?;

    hello(&client).map_err(|error| classify(&error))?;

    let schema = schema::version(&db).map_err(|error| classify(&error))?;
    let states = schema::search_states(&db).map_err(|error| classify(&error))?;

    let model_id = if schema > 0 {
        db.collection::<Document>(schema::META)
            .find_one(doc! { "_id": "meta:model_id" })
            .run()
            .map_err(|error| classify(&error))?
            .and_then(|row| text(&row, "value"))
    } else {
        None
    };

    Ok(Probe {
        schema: (schema > 0).then_some(schema),
        model_id,
        keyword: state_of(&states, KEYWORD_INDEX),
        vector: state_of(&states, VECTOR_INDEX),
    })
}

fn hello(client: &Client) -> mongodb::error::Result<Document> {
    client.database("admin").run_command(doc! { "hello": 1 }).run()
}

/// A URL the rules refuse (TLS, relative files) or the driver cannot read.
fn refused(error: StoreError) -> Problem {
    Problem {
        code: "unreachable",
        message: error.to_string(),
        refused: true,
    }
}

/// The problem code of a driver error: a login or a privilege that fails is `auth`, a server
/// without `mongot` is `no-mongot`, anything else (no route, no answer, a timeout) `unreachable`.
fn classify(error: &Error) -> Problem {
    let code = match error_code(error) {
        Some(SEARCH_NOT_ENABLED) => "no-mongot",
        Some(UNAUTHORIZED | AUTHENTICATION_FAILED) => "auth",
        _ => match error.kind.as_ref() {
            ErrorKind::Authentication { .. } => "auth",
            _ => "unreachable",
        },
    };

    let message = match code {
        "auth" if error_code(error).is_some() => {
            "the database user may not read the search indexes of the knowledge database".to_owned()
        }
        _ => connection::message(error),
    };

    let refused = code == "unreachable" && connection::tls_failure(error);

    Problem { code, message, refused }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(schema: Option<i64>, keyword: IndexState, vector: IndexState) -> Probe {
        Probe {
            schema,
            model_id: None,
            keyword,
            vector,
        }
    }

    #[test]
    fn a_database_nobody_wrote_to_is_ready_whatever_its_indexes() {
        let fresh = probe(None, IndexState::Missing, IndexState::Missing);

        assert!(fresh.ready());
        assert_eq!(fresh.problem(), None);
        assert_eq!(fresh.vector_index(), "ready");
    }

    #[test]
    fn indexes_that_are_pending_or_missing_report_index_building() {
        let building = probe(Some(1), IndexState::Ready, IndexState::Building);
        assert_eq!(building.problem().expect("problem").code, "index-building");
        assert_eq!(building.vector_index(), "building");
        assert!(
            building
                .problem()
                .expect("problem")
                .message
                .contains("still being built")
        );

        let missing = probe(Some(1), IndexState::Missing, IndexState::Ready);
        assert_eq!(missing.problem().expect("problem").code, "index-building");
        assert!(missing.problem().expect("problem").message.contains("missing"));

        let ready = probe(Some(1), IndexState::Ready, IndexState::Ready);
        assert_eq!(ready.problem(), None);
        assert_eq!(ready.vector_index(), "ready");
    }
}
