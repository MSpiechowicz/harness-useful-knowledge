//! Opening one session: the TLS policy of the URL, the session limits and the pgvector schema.

use postgres::config::SslMode;
use postgres::{Client, NoTls};

use super::connect::{self, Settings};
use super::error::{backend, described, refused_by_tls, with_causes};
use super::vector::{self, Problem};
use crate::store::StoreError;

/// Session limits: how long a statement waits for a lock or runs, and how long a transaction may
/// sit idle before the server ends it.
const SESSION: &str = "SET lock_timeout = '10s'; SET statement_timeout = '30s'; \
                       SET idle_in_transaction_session_timeout = '60s';";

/// Why no connection was made.
pub(super) enum Dial {
    /// The TLS setup failed (an unreadable `sslrootcert`, no trusted roots).
    Tls(String),
    /// The server did not accept the connection.
    Connect(postgres::Error),
}

/// Connects with the TLS policy of `settings`.
pub(super) fn dial(settings: &Settings) -> Result<Client, Dial> {
    match settings.config.get_ssl_mode() {
        SslMode::Disable => settings.config.connect(NoTls).map_err(Dial::Connect),
        mode => {
            let required = mode == SslMode::Require;
            let tls = connect::tls(settings.root_cert.as_ref(), required).map_err(Dial::Tls)?;

            settings.config.connect(tls).map_err(Dial::Connect)
        }
    }
}

/// Opens a session, sets the session limits and makes pgvector reachable.
pub(super) fn open(settings: &Settings) -> Result<Client, StoreError> {
    let mut client = match dial(settings) {
        Ok(client) => client,
        Err(Dial::Tls(message)) => return Err(StoreError::Backend(message)),
        Err(Dial::Connect(error)) => return Err(backend(error)),
    };

    client.batch_execute(SESSION).map_err(backend)?;
    vector::reach(&mut client)?;

    Ok(client)
}

/// What a failed connection is called in a `check`: a refused login is [`Problem::Auth`], a server
/// whose TLS the client refuses [`Problem::Refused`], anything else (no route, nothing listening, a
/// database that is not there) [`Problem::Unreachable`] with the reason, which carries no URL.
pub(super) fn problem(error: &postgres::Error) -> Problem {
    match error.as_db_error() {
        Some(db) if db.code().code().starts_with("28") => Problem::Auth,
        Some(db) => Problem::Unreachable(described(db.code().code())),
        None if refused_by_tls(error) => Problem::Refused(with_causes(error).chars().take(200).collect()),
        None => Problem::Unreachable(with_causes(error).chars().take(200).collect()),
    }
}
