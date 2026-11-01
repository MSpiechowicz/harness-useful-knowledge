//! Which database the server uses: the local SQLite file (the default) or a team database.
//!
//! The team database comes from [`URL_VAR`], else from `url` in the `[database]` table of the
//! config file (`<config dir>/harness-knowledge/config.toml`, or the file [`CONFIG_VAR`] names):
//!
//! ```toml
//! [database]
//! url = "postgres://user:password@db.example.com/knowledge"
//! token = "only for libsql"
//! ```
//!
//! The scheme picks the [`Backend`]:
//!
//! | Scheme | Backend |
//! | --- | --- |
//! | none set | local SQLite file (see [`crate::location`]) |
//! | `postgres://`, `postgresql://` | PostgreSQL (pgvector) |
//! | `mongodb://`, `mongodb+srv://` | MongoDB (Atlas, or 8.2+ with mongot) |
//! | `libsql://`, `https://`, `http://` (`http` for this machine only) | libSQL (remote SQLite) |
//!
//! The scheme is matched and passed on in lowercase (`POSTGRES://…` works), the rest of the URL is
//! kept as written. Amazon DocumentDB endpoints are refused: it is not supported yet.
//!
//! [`TOKEN_VAR`] or `token` is the libSQL auth token.
//!
//! The URL and the token are secrets: errors show a [`redact_url`] form and pass through
//! [`Secrets::scrub`]. On Unix a config file other users can change, or that another user owns, is
//! refused, and one other users can read is reported as a warning.

mod file;
mod secrets;

use std::path::PathBuf;

use crate::support::{Env, OsKind};
use file::{ConfigRead, DatabaseTable, parse_config, permission_warning, read_config};
pub use secrets::{Secrets, redact_url};

/// Environment variable holding the team database URL (empty = unset). The URL can carry the
/// password, so it is a secret.
pub const URL_VAR: &str = "HARNESS_KNOWLEDGE_DATABASE_URL";

/// Environment variable holding the libSQL auth token (empty = unset).
pub const TOKEN_VAR: &str = "HARNESS_KNOWLEDGE_DATABASE_TOKEN";

/// Environment variable naming the config file instead of the default one. It must be an absolute
/// path and the file must exist.
pub const CONFIG_VAR: &str = "HARNESS_KNOWLEDGE_CONFIG";

/// The config file name inside `<config dir>/harness-knowledge/`.
pub const CONFIG_FILE: &str = "config.toml";

/// The domains of Amazon DocumentDB cluster endpoints, instance-based and elastic, in the global
/// and the China partitions.
const DOCUMENTDB_DOMAINS: [&str; 4] = [
    ".docdb.amazonaws.com",
    ".docdb-elastic.amazonaws.com",
    ".docdb.amazonaws.com.cn",
    ".docdb-elastic.amazonaws.com.cn",
];

/// The selected database.
#[derive(Clone, PartialEq, Eq)]
pub enum Backend {
    /// The local SQLite file (see [`crate::location`]).
    Sqlite,
    /// A PostgreSQL team database.
    Postgres { url: String },
    /// A MongoDB team database.
    Mongodb { url: String },
    /// A libSQL (remote SQLite, `sqld`) team database.
    Libsql { url: String, token: Option<String> },
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

impl Backend {
    /// The backend's name and its [`redact_url`]: `PostgreSQL at postgres://alice@db:5432/knowledge`.
    pub fn describe(&self) -> String {
        match self {
            Self::Sqlite => "the local database".to_owned(),
            Self::Postgres { url } => format!("PostgreSQL at {}", redact_url(url)),
            Self::Mongodb { url } => format!("MongoDB at {}", redact_url(url)),
            Self::Libsql { url, .. } => format!("libSQL at {}", redact_url(url)),
        }
    }

    /// The secrets of this backend, for scrubbing messages.
    pub fn secrets(&self) -> Secrets {
        match self {
            Self::Sqlite => Secrets::default(),
            Self::Postgres { url } | Self::Mongodb { url } => Secrets::of(url, None),
            Self::Libsql { url, token } => Secrets::of(url, token.as_deref()),
        }
    }
}

/// The settings the server starts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The database to use.
    pub backend: Backend,
    /// Problems that do not stop the server (a config file other users can read), for stderr.
    pub warnings: Vec<String>,
}

/// Why no backend could be selected. Messages never hold the URL or the token.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{CONFIG_VAR} must be an absolute path to the config file")]
    ConfigRelative,
    #[error("the config file {0} does not exist ({CONFIG_VAR} names it)")]
    ConfigMissing(PathBuf),
    #[error("cannot read the config file {path}: {reason}")]
    ConfigUnreadable { path: PathBuf, reason: String },
    #[error(
        "the config file {0} can be changed by other users, who could point the server at their own database; \
         run chmod 600 on it"
    )]
    ConfigWritable(PathBuf),
    #[error(
        "the config file {0} belongs to another user, who could point the server at their own database; use a \
         file you own"
    )]
    ConfigNotOwned(PathBuf),
    #[error("the config file {path} is not valid: {reason}")]
    ConfigInvalid { path: PathBuf, reason: String },
    #[error(
        "the database URL must start with postgres://, postgresql://, libsql://, https://, http:// (this machine \
         only), mongodb:// or mongodb+srv://{0}"
    )]
    Scheme(String),
    #[error("an http:// database URL is only allowed for this machine (localhost), use https:// or libsql://")]
    PlainHttp,
    #[error("a libSQL URL takes no user, password or query: give the token in {TOKEN_VAR} or token instead")]
    LibsqlUrlSecrets,
    #[error(
        "Amazon DocumentDB is not supported yet, use MongoDB (Atlas, or 8.2+ with mongot), PostgreSQL with \
         pgvector, or libSQL"
    )]
    DocumentDb,
}

/// The config folder: `$XDG_CONFIG_HOME` (or `~/.config`) on Linux, `~/Library/Application
/// Support` on macOS, `%APPDATA%` on Windows. Relative values do not count (the server runs inside
/// the project, so they would read a file from the repository).
pub fn config_dir(env: &Env) -> Option<PathBuf> {
    let rooted = |name| env.var(name).map(PathBuf::from).filter(|dir| dir.has_root());

    match env.os {
        OsKind::Linux => Some(rooted("XDG_CONFIG_HOME").unwrap_or_else(|| env.home.join(".config"))),
        OsKind::MacOs => Some(env.home.join("Library").join("Application Support")),
        OsKind::Windows => rooted("APPDATA"),
    }
}

/// The config file and whether [`CONFIG_VAR`] named it (then it has to exist).
pub fn config_path(env: &Env) -> Result<Option<(PathBuf, bool)>, ConfigError> {
    if let Some(raw) = env.var(CONFIG_VAR) {
        let path = env.expand_home(raw);

        if !path.has_root() {
            return Err(ConfigError::ConfigRelative);
        }

        return Ok(Some((path, true)));
    }

    Ok(config_dir(env)
        .map(|dir| dir.join("harness-knowledge").join(CONFIG_FILE))
        .map(|path| (path, false)))
}

/// Picks the backend from the environment and the config file (see the module docs).
pub fn select(env: &Env) -> Result<Selection, ConfigError> {
    let mut warnings: Vec<String> = Vec::new();
    let mut table = DatabaseTable::default();

    if let Some((path, named)) = config_path(env)? {
        match read_config(&path) {
            Ok(text) => {
                table = parse_config(&path, &text)?.database.unwrap_or_default();
                warnings.extend(permission_warning(&path));
            }
            Err(ConfigRead::Missing) if !named => {}
            Err(ConfigRead::Missing) => return Err(ConfigError::ConfigMissing(path)),
            Err(ConfigRead::Refused(error)) => return Err(error),
        }
    }

    let url = env.var(URL_VAR).map(str::to_owned).or(table.url);
    let token = env.var(TOKEN_VAR).map(str::to_owned).or(table.token);
    let backend = backend_for(url, token)?;

    Ok(Selection { backend, warnings })
}

/// The backend for a URL (none or blank: [`Backend::Sqlite`]) and a token.
pub fn backend_for(url: Option<String>, token: Option<String>) -> Result<Backend, ConfigError> {
    let Some(url) = url.map(|url| url.trim().to_owned()).filter(|url| !url.is_empty()) else {
        return Ok(Backend::Sqlite);
    };

    let token = token
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty());

    // The stores only take lowercase schemes, so `POSTGRES://` is passed on as `postgres://`.
    let (scheme, url) = match url.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = scheme.to_ascii_lowercase();
            let url = format!("{scheme}://{rest}");
            (scheme, url)
        }
        None => (String::new(), url),
    };

    match scheme.as_str() {
        "postgres" | "postgresql" => Ok(Backend::Postgres { url }),
        "mongodb" | "mongodb+srv" if is_documentdb_url(&url) => Err(ConfigError::DocumentDb),
        "mongodb" | "mongodb+srv" => Ok(Backend::Mongodb { url }),
        "libsql" | "https" | "http" if carries_secrets(&url) => Err(ConfigError::LibsqlUrlSecrets),
        "libsql" | "https" => Ok(Backend::Libsql { url, token }),
        "http" if is_local_http(&url) => Ok(Backend::Libsql { url, token }),
        "http" => Err(ConfigError::PlainHttp),
        "" => Err(ConfigError::Scheme(String::new())),
        other if other.len() <= 16 && other.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) => {
            Err(ConfigError::Scheme(format!(" (got {other}://)")))
        }
        _ => Err(ConfigError::Scheme(String::new())),
    }
}

/// True when a host of the `mongodb://` or `mongodb+srv://` URL is an Amazon DocumentDB cluster
/// endpoint (`<cluster>.cluster-<id>.<region>.docdb.amazonaws.com` and the like).
fn is_documentdb_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let address = rest.split(['?', '#']).next().unwrap_or_default();
    let authority = address.split('/').next().unwrap_or_default();
    let hosts = authority.rsplit_once('@').map_or(authority, |(_, hosts)| hosts);

    hosts.split(',').any(is_documentdb_host)
}

/// True for a DocumentDB endpoint name, with or without a port.
fn is_documentdb_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => name,
        _ => host.as_str(),
    };
    let name = name.trim_end_matches('.');

    DOCUMENTDB_DOMAINS.iter().any(|domain| name.ends_with(domain))
}

/// True when a URL has a user or password, a query or a fragment (where a token could hide).
fn carries_secrets(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();

    authority.contains('@') || rest.contains(['?', '#'])
}

/// True when an `http://` URL points at this machine (`localhost` or a loopback address).
fn is_local_http(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();

    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };

    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests;
