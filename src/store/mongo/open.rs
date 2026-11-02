//! Opening the MongoDB store for `backend`: the boxed store, the connection and the errors an open
//! ends in (see [`MongoStore::open_checked`]).

use std::time::Duration;

use mongodb::bson::doc;
use mongodb::options::ClientOptions;
use mongodb::sync::{Client, Database};

use super::{DEFAULT_DATABASE, Mode, MongoStore, connection};
use crate::backend::{Access, OpenError, Opened, Upgrade};
use crate::config::Secrets;
use crate::store::{KnowledgeStore, StoreError};

/// The store `backend::open` hands out for a MongoDB URL: `None` when `access` does not create the
/// database and nobody wrote to it, and a server is given up on after `connect_timeout` unless the URL
/// names its own `serverSelectionTimeoutMS`. Failures are one readable line without the URL (and with every
/// secret of it scrubbed).
pub fn open_boxed(
    url: &str,
    access: Access,
    connect_timeout: Duration,
    secrets: &Secrets,
) -> Result<Option<Box<dyn KnowledgeStore>>, OpenError> {
    match open_boxed_with(url, access, connect_timeout, Upgrade::Run, secrets)? {
        Opened::Ready(store) => Ok(store),
        Opened::NeedsUpgrade => Err(OpenError::Failed(
            "cannot open the MongoDB database: it needs an upgrade".to_owned(),
        )),
    }
}

/// [`open_boxed`] that may leave an upgrade of the database to a later open (see
/// [`MongoStore::open_checked`]).
pub fn open_boxed_with(
    url: &str,
    access: Access,
    connect_timeout: Duration,
    upgrade: Upgrade,
    secrets: &Secrets,
) -> Result<Opened<Box<dyn KnowledgeStore>>, OpenError> {
    let mode = match access {
        Access::Read => Mode::ReadOnly,
        Access::Existing => Mode::Existing,
        Access::Create => Mode::Create,
        Access::Counters => Mode::Counters,
    };

    match MongoStore::open_checked(url, mode, connect_timeout, upgrade) {
        Ok(opened) => Ok(opened.map(|store| Box::new(store) as Box<dyn KnowledgeStore>)),
        Err(Opening::Missing { code, message }) => Err(OpenError::Missing { code, message }),
        Err(Opening::Failed(StoreError::Unreachable(message))) => Err(OpenError::Unreachable(
            secrets.scrub(&format!("cannot reach the MongoDB database: {message}")),
        )),
        Err(Opening::Failed(error)) => Err(OpenError::Failed(
            secrets.scrub(&format!("cannot open the MongoDB database: {error}")),
        )),
    }
}

/// Why opening stopped: the server lacks a search engine, or something else failed.
pub(super) enum Opening {
    Missing { code: &'static str, message: String },
    Failed(StoreError),
}

impl From<StoreError> for Opening {
    fn from(error: StoreError) -> Self {
        Self::Failed(error)
    }
}

impl From<Opening> for StoreError {
    fn from(opening: Opening) -> Self {
        match opening {
            Opening::Missing { message, .. } => Self::Backend(message),
            Opening::Failed(error) => error,
        }
    }
}

/// The client and the database of `url` (no request is sent yet): the URL and TLS rules applied,
/// the database from the URL path.
pub(super) fn connect_client(url: &str, selection_timeout: Duration) -> Result<(Client, Database), StoreError> {
    connect_client_for(url, selection_timeout, Mode::ReadOnly)
}

/// [`connect_client`] for a connection in `mode`: a counting one is bound to notice a server that
/// stops answering (see [`bound_counting`]).
pub(super) fn connect_client_for(
    url: &str,
    selection_timeout: Duration,
    mode: Mode,
) -> Result<(Client, Database), StoreError> {
    let (options, name) = client_options(url, selection_timeout, mode)?;
    let client = Client::with_options(options).map_err(|error| connection::connect_error(&error, url))?;
    let db = client.database(&name);

    Ok((client, db))
}

/// The options of the client for `url` in `mode` and the name of its database (nothing is sent).
pub(super) fn client_options(
    url: &str,
    selection_timeout: Duration,
    mode: Mode,
) -> Result<(ClientOptions, String), StoreError> {
    let checked = connection::check(url)?;
    let mut options = ClientOptions::parse(url)
        .run()
        .map_err(|error| connection::connect_error(&error, url))?;
    connection::enforce_tls(&mut options, &checked)?;
    connection::tls_files(&options)?;

    let name = options
        .default_database
        .clone()
        .unwrap_or_else(|| DEFAULT_DATABASE.to_owned());
    options.app_name.get_or_insert_with(|| "harness-knowledge".to_owned());
    options.server_selection_timeout.get_or_insert(selection_timeout);

    if mode == Mode::Counters {
        bound_counting(&mut options);
    }

    Ok((options, name))
}

/// How long a connection of a counting client waits for its server's heartbeat before the server
/// counts as gone.
pub(super) const COUNT_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// How often a counting client asks its server whether it is there (the least the driver allows).
pub(super) const COUNT_HEARTBEAT: Duration = Duration::from_millis(500);

/// Makes a counting client notice a server that stops answering (a half-open connection, a stopped
/// process) within about a second and a half: the driver has no timeout for the reply to a request,
/// but when the heartbeat of a server times out it interrupts the requests running on it, so a
/// counter write that waits for it ends with an error. A bound the URL names itself is kept
/// when it is shorter.
pub(super) fn bound_counting(options: &mut ClientOptions) {
    // A zero in the URL means "no limit" to the driver: it does not lift the bound.
    let shorter = |named: Option<Duration>, bound: Duration| {
        Some(
            named
                .filter(|named| !named.is_zero())
                .map_or(bound, |named| named.min(bound)),
        )
    };

    options.connect_timeout = shorter(options.connect_timeout, COUNT_CONNECT_TIMEOUT);
    options.heartbeat_freq = shorter(options.heartbeat_freq, COUNT_HEARTBEAT).map(|freq| freq.max(COUNT_HEARTBEAT));
}

/// Asks the server what it is (the first contact) and returns whether writes can run in
/// transactions: on a replica set (`setName`) or behind `mongos` (`msg: "isdbgrid"`).
pub(super) fn transactions(client: &Client) -> Result<bool, StoreError> {
    let reply = client
        .database("admin")
        .run_command(doc! { "hello": 1 })
        .run()
        .map_err(|error| connection::backend(&error))?;

    Ok(reply.contains_key("setName") || reply.get_str("msg").is_ok_and(|msg| msg == "isdbgrid"))
}
