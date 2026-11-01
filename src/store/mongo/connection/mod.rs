//! The connection URL: the scheme, where the servers are, the TLS rules and files, and error
//! messages that never carry the credentials of the URL.
//!
//! Adapted from harness-memory's MongoDB store, without the Amazon DocumentDB certificate bundle
//! (DocumentDB is not supported here).

use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

use mongodb::error::{Error, ErrorKind, WriteFailure};
use mongodb::options::{ClientOptions, ServerAddress, Tls};

use crate::config::Secrets;
use crate::store::StoreError;

/// The server error of a search command on a server without `mongot`.
pub(super) const SEARCH_NOT_ENABLED: i32 = 31082;

/// The server error of an index that exists with another definition.
pub(super) const INDEX_EXISTS: i32 = 68;

/// The server error of a collection that exists.
pub(super) const NAMESPACE_EXISTS: i32 = 48;

/// The server error of a collection that does not exist.
pub(super) const NAMESPACE_NOT_FOUND: i32 = 26;

/// What [`check`] learned from a URL.
pub(super) struct Checked {
    /// The driver talks to this machine only (see [`local_seeds`]), so TLS may be off.
    pub local: bool,
}

/// Checks the scheme and the TLS options of `url` before the driver reads it (so a refused URL
/// costs no DNS lookup). A server that is not on this machine must use TLS with certificate checks,
/// and no server gets a relative TLS file (see [`check_tls_file`]).
pub(super) fn check(url: &str) -> Result<Checked, StoreError> {
    let (srv, rest) = if let Some(rest) = url.strip_prefix("mongodb+srv://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("mongodb://") {
        (false, rest)
    } else {
        return Err(StoreError::Invalid(
            "a MongoDB URL starts with mongodb:// or mongodb+srv://".to_owned(),
        ));
    };

    let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = address.split('/').next().unwrap_or("");
    let hosts = authority.rsplit_once('@').map_or(authority, |(_, hosts)| hosts);

    let options: Vec<(String, String)> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.to_ascii_lowercase()))
        .collect();

    let single_server = options
        .iter()
        .any(|(key, value)| matches!(key.as_str(), "directconnection" | "loadbalanced") && value == "true");

    for (key, value) in query.split('&').filter_map(|pair| pair.split_once('=')) {
        check_tls_file(&key.to_ascii_lowercase(), Path::new(&percent_decode(value)))?;
    }

    let local = !srv && local_seeds(hosts, single_server);

    if local {
        return Ok(Checked { local });
    }

    for (key, value) in &options {
        if matches!(key.as_str(), "tls" | "ssl") && value == "false" {
            return Err(tls_required());
        }

        let unchecked = matches!(
            key.as_str(),
            "tlsinsecure" | "tlsallowinvalidcertificates" | "tlsallowinvalidhostnames"
        );
        if unchecked && value == "true" {
            return Err(certificates_required());
        }
    }

    Ok(Checked { local })
}

/// True when the driver can only reach this machine through `hosts` (the comma-separated seeds).
///
/// Loopback seeds alone are not enough: without `directConnection=true` or `loadBalanced=true`
/// the driver also connects to the replica-set members the server lists, which may be remote.
/// Unix socket seeds are local by themselves.
fn local_seeds(hosts: &str, single_server: bool) -> bool {
    if hosts.is_empty() {
        return false;
    }

    let mut seeds = hosts.split(',');

    if seeds.clone().all(is_socket) {
        return true;
    }

    single_server && seeds.all(is_local_host)
}

/// Turns TLS on for a remote server when the URL left it unset, and refuses options the driver
/// parsed that switch it or its certificate checks off (the authoritative check after [`check`]).
pub(super) fn enforce_tls(options: &mut ClientOptions, checked: &Checked) -> Result<(), StoreError> {
    if checked.local && single_server(options) {
        return Ok(());
    }

    match &options.tls {
        None => {
            options.tls = Some(Tls::Enabled(Default::default()));
            Ok(())
        }
        Some(Tls::Disabled) => Err(tls_required()),
        Some(Tls::Enabled(tls)) => {
            // `tlsAllowInvalidHostnames` is refused by [`check`]; rustls has no such setting.
            if tls.allow_invalid_certificates == Some(true) {
                return Err(certificates_required());
            }

            Ok(())
        }
    }
}

/// Applies the TLS file rules to the parsed options (the authoritative check after [`check`]), for
/// every server, this machine included:
///
/// - `tlsCertificateKeyFile` must be an absolute path ([`check_tls_file`]).
/// - `tlsCAFile` must be an absolute path of an existing file.
///
/// Without a `tlsCAFile` the Mozilla roots compiled into the driver (`webpki-roots`) are trusted.
pub(super) fn tls_files(options: &ClientOptions) -> Result<(), StoreError> {
    let Some(Tls::Enabled(tls)) = options.tls.as_ref() else {
        return Ok(());
    };

    if let Some(key_file) = &tls.cert_key_file_path {
        check_tls_file("tlscertificatekeyfile", key_file)?;
    }

    let Some(named) = tls.ca_file_path.as_deref() else {
        return Ok(());
    };

    check_tls_file("tlscafile", named)?;

    if !named.is_file() {
        return Err(StoreError::Invalid(format!(
            "the CA file {} that tlsCAFile names does not exist: fix the path",
            named.display()
        )));
    }

    Ok(())
}

/// Refuses a relative `tlsCertificateKeyFile` or `tlsCAFile` (`key`, lowercase). The server runs
/// inside the project, so a relative path would read a file the repository could have planted
/// there.
fn check_tls_file(key: &str, path: &Path) -> Result<(), StoreError> {
    let option = match key {
        "tlscafile" => "tlsCAFile",
        "tlscertificatekeyfile" => "tlsCertificateKeyFile",
        _ => return Ok(()),
    };

    if path.is_absolute() {
        return Ok(());
    }

    Err(StoreError::Invalid(format!(
        "{option} in the URL must be an absolute path, not {}, because the server runs inside the project",
        path.display()
    )))
}

/// True when the parsed options keep the driver on the seeds: a direct or load-balanced
/// connection, or Unix sockets only.
fn single_server(options: &ClientOptions) -> bool {
    if options.direct_connection == Some(true) || options.load_balanced == Some(true) {
        return true;
    }

    !options.hosts.is_empty() && options.hosts.iter().all(is_socket_address)
}

#[cfg(unix)]
fn is_socket_address(address: &ServerAddress) -> bool {
    matches!(address, ServerAddress::Unix { .. })
}

#[cfg(not(unix))]
fn is_socket_address(_address: &ServerAddress) -> bool {
    false
}

fn tls_required() -> StoreError {
    StoreError::Invalid(
        "TLS is required for a MongoDB server that is not on this machine: remove tls=false (or ssl=false) \
         from the URL"
            .to_owned(),
    )
}

fn certificates_required() -> StoreError {
    StoreError::Invalid(
        "certificate checks are required for a MongoDB server that is not on this machine: remove \
         tlsInsecure, tlsAllowInvalidCertificates and tlsAllowInvalidHostnames from the URL"
            .to_owned(),
    )
}

/// True for a Unix socket path (`%2F...sock`).
fn is_socket(host: &str) -> bool {
    let lower = host.trim().to_ascii_lowercase();
    lower.starts_with("%2f") || lower.ends_with(".sock")
}

/// True for `localhost`, a loopback address or a Unix socket path.
fn is_local_host(host: &str) -> bool {
    if is_socket(host) {
        return true;
    }

    let lower = host.trim().to_ascii_lowercase();

    let name = match lower.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or(""),
        None => lower.rsplit_once(':').map_or(lower.as_str(), |(name, _)| name),
    };

    if name == "localhost" {
        return true;
    }

    if let Ok(address) = name.parse::<Ipv4Addr>() {
        return address.is_loopback();
    }

    name.parse::<Ipv6Addr>().is_ok_and(|address| address.is_loopback())
}

/// A driver error while reading the URL or creating the client, as a message without the URL's
/// credentials. Only messages the driver builds from the URL and local settings are shown; DNS
/// answers (SRV and TXT records) and server replies are not.
pub(super) fn connect_error(error: &Error, url: &str) -> StoreError {
    let detail = match error.kind.as_ref() {
        ErrorKind::InvalidArgument { message, .. } => Some(format!("invalid MongoDB URL: {message}")),
        ErrorKind::InvalidTlsConfig { message, .. } => Some(format!("invalid TLS settings: {message}")),
        _ => None,
    };

    match detail {
        Some(detail) => StoreError::Backend(Secrets::of(url, None).scrub(&detail).chars().take(200).collect()),
        None => backend(error),
    }
}

/// A driver error as a short message: its kind and server error code (with the code name when it
/// is a plain word), never server text or the URL. A server without `mongot` is named as such.
///
/// A server that cannot be reached (no usable server in time, a network error, a host that does
/// not resolve) is [`StoreError::Unreachable`].
pub(super) fn backend(error: &Error) -> StoreError {
    match error.kind.as_ref() {
        ErrorKind::ServerSelection { .. } | ErrorKind::Io(_) | ErrorKind::DnsResolve { .. } => {
            StoreError::Unreachable(message(error))
        }
        _ => StoreError::Backend(message(error)),
    }
}

/// The server error code of `error`, if any.
pub(super) fn error_code(error: &Error) -> Option<i32> {
    match error.kind.as_ref() {
        ErrorKind::Command(command) => Some(command.code),
        ErrorKind::Write(WriteFailure::WriteError(write)) => Some(write.code),
        ErrorKind::Write(WriteFailure::WriteConcernError(concern)) => Some(concern.code),
        _ => None,
    }
}

/// The capability the server of `error` lacks, as its `check` problem code and the text to show:
/// only a server without `mongot` (`no-mongot`).
pub(super) fn missing(error: &Error) -> Option<(&'static str, String)> {
    (error_code(error) == Some(SEARCH_NOT_ENABLED)).then(|| ("no-mongot", NO_MONGOT.to_owned()))
}

/// The text shown for a server without `mongot`.
pub(super) const NO_MONGOT: &str = "this MongoDB server has no search engine (mongot), use MongoDB Atlas or \
                                    MongoDB 8.2 or newer with mongot";

/// The message of [`backend`].
pub(super) fn message(error: &Error) -> String {
    if error_code(error) == Some(SEARCH_NOT_ENABLED) {
        return NO_MONGOT.to_owned();
    }

    match error.kind.as_ref() {
        ErrorKind::Authentication { .. } => {
            "authentication failed (check the user, the password and authSource)".to_owned()
        }
        ErrorKind::ServerSelection { .. } => "cannot reach a usable MongoDB server (timed out)".to_owned(),
        ErrorKind::Io(io) => format!("network error ({:?})", io.kind()),
        ErrorKind::DnsResolve { .. } => "cannot resolve the MongoDB host".to_owned(),
        ErrorKind::InvalidTlsConfig { .. } => "invalid TLS settings".to_owned(),
        ErrorKind::InvalidArgument { .. } => "invalid MongoDB settings".to_owned(),
        ErrorKind::Command(command) => coded("MongoDB command failed", Some(&command.code_name), command.code),
        ErrorKind::Write(WriteFailure::WriteError(write)) => {
            coded("write failed", write.code_name.as_deref(), write.code)
        }
        ErrorKind::Write(WriteFailure::WriteConcernError(concern)) => {
            coded("write concern failed", Some(&concern.code_name), concern.code)
        }
        ErrorKind::Transaction { .. } => "transaction failed".to_owned(),
        ErrorKind::IncompatibleServer { .. } => "the MongoDB server version is not supported".to_owned(),
        ErrorKind::SessionsNotSupported => "the MongoDB server does not support sessions".to_owned(),
        _ => "MongoDB request failed".to_owned(),
    }
}

/// `what` with the server's error code, and its code name when that is 1 to 40 ASCII letters
/// (anything else the server sent is left out).
fn coded(what: &str, code_name: Option<&str>, code: i32) -> String {
    let plain = |name: &&str| (1..=40).contains(&name.len()) && name.bytes().all(|byte| byte.is_ascii_alphabetic());

    match code_name.filter(plain) {
        Some(name) => format!("{what}: {name} ({code})"),
        None => format!("{what} ({code})"),
    }
}

/// `%XX` escapes decoded (invalid escapes and non-UTF-8 results are kept as they are).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| text.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());

        match escaped {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }

    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

#[cfg(test)]
mod tests;
