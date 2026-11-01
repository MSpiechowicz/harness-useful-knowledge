//! What `check` asks of a PostgreSQL database: can it be reached, is pgvector there (or
//! installable by this role), is the vector index built. Nothing is created or changed, and a
//! read-only role can run it.

use postgres::GenericClient;

use super::error::{backend, column};
use super::schema::{self, HNSW_INDEX};
use super::vector::{self, Fault, Problem};
use super::{connect, session};
use crate::store::StoreError;

/// A database that can serve knowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The schema version stored, `None` for a database without the knowledge tables yet.
    pub schema: Option<i64>,
    /// The model that made the vectors, once one has.
    pub model_id: Option<String>,
}

/// Probes the database at `url`. A problem comes back as [`Fault::Problem`] with its `check`
/// code, anything else (a URL that does not parse, a refused TLS setup) as [`Fault::Store`].
pub fn probe(url: &str) -> Result<Probe, Fault> {
    let settings = connect::parse(url).map_err(|error| StoreError::Backend(error.to_string()))?;

    let mut client = match session::dial(&settings) {
        Ok(client) => client,
        Err(session::Dial::Tls(message)) => return Err(StoreError::Backend(message).into()),
        Err(session::Dial::Connect(error)) => return Err(session::problem(&error).into()),
    };

    check_vector(&mut client)?;

    let schema = schema::stored_version(&mut client)?;

    if schema.is_none() {
        return Ok(Probe { schema, model_id: None });
    }

    check_index(&mut client)?;

    Ok(Probe {
        schema,
        model_id: super::meta::get(&mut client, "model_id")?,
    })
}

/// pgvector is installed with HNSW, or is offered here to a role that may install it.
fn check_vector(client: &mut impl GenericClient) -> Result<(), Fault> {
    if let Some(version) = vector::installed(client)? {
        return if version.has_hnsw() {
            Ok(())
        } else {
            Err(Problem::TooOld(version).into())
        };
    }

    let Some(offered) = vector::available(client)? else {
        return Err(Problem::NoPgvector.into());
    };

    if !offered.version.has_hnsw() {
        return Err(Problem::TooOld(offered.version).into());
    }

    if !offered.installable {
        return Err(Problem::NoPrivilege.into());
    }

    Ok(())
}

/// The HNSW index, when the tables exist, is valid: an index being built (or one whose build was
/// interrupted) is invalid, and answers nothing.
fn check_index(client: &mut impl GenericClient) -> Result<(), Fault> {
    let row = client
        .query_opt(
            "SELECT indisvalid FROM pg_index WHERE indexrelid = to_regclass($1)",
            &[&HNSW_INDEX],
        )
        .map_err(backend)?;

    match row {
        Some(row) if !column::<bool>(&row, 0)? => Err(Problem::IndexBuilding.into()),
        _ => Ok(()),
    }
}
