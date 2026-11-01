//! Reading the config file: the checks that only its owner may change it (a file others can change
//! could point the server at their own database), and the TOML parse that never quotes the URL.

use std::path::Path;

use serde::Deserialize;

use super::ConfigError;
use super::secrets::Secrets;

/// The config file. Unknown keys are refused, so a misspelt `url` fails loudly instead of quietly
/// using the local database.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConfigFile {
    pub database: Option<DatabaseTable>,
    pub routing: Option<RoutingTable>,
}

/// The `[routing]` table of the config file (see [`super::routing`]).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RoutingTable {
    pub repository_documents: Option<String>,
    pub global_documents: Option<String>,
}

/// The `[database]` table of the config file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DatabaseTable {
    pub url: Option<String>,
    pub token: Option<String>,
}

/// Why [`read_config`] gave no text.
pub(super) enum ConfigRead {
    Missing,
    Refused(ConfigError),
}

/// The text of the config file at `path`, after checking (on Unix) that only its owner, the user
/// running the server, may change it. The checks look at the opened file, so a file swapped in
/// between the check and the read is not used.
pub(super) fn read_config(path: &Path) -> Result<String, ConfigRead> {
    use std::io::Read;

    let unreadable = |error: std::io::Error| {
        ConfigRead::Refused(ConfigError::ConfigUnreadable {
            path: path.to_owned(),
            reason: error.kind().to_string(),
        })
    };

    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(ConfigRead::Missing),
        Err(error) => return Err(unreadable(error)),
    };

    let metadata = file.metadata().map_err(unreadable)?;
    check_config_owner(path, &metadata).map_err(ConfigRead::Refused)?;

    let mut text = String::new();
    file.read_to_string(&mut text).map_err(unreadable)?;
    Ok(text)
}

/// Refuses a config file the group or others may write, or that another user owns (Unix only).
#[cfg(unix)]
fn check_config_owner(path: &Path, metadata: &std::fs::Metadata) -> Result<(), ConfigError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if writable_by_others(metadata.permissions().mode()) {
        return Err(ConfigError::ConfigWritable(path.to_owned()));
    }

    if nix::unistd::geteuid().as_raw() != metadata.uid() {
        return Err(ConfigError::ConfigNotOwned(path.to_owned()));
    }

    Ok(())
}

#[cfg(not(unix))]
fn check_config_owner(_path: &Path, _metadata: &std::fs::Metadata) -> Result<(), ConfigError> {
    Ok(())
}

/// Parses the config file. A TOML error is reported by its message and line only: the parser's
/// own report quotes the line, which can hold the URL.
pub(super) fn parse_config(path: &Path, text: &str) -> Result<ConfigFile, ConfigError> {
    toml::from_str(text).map_err(|error: toml::de::Error| {
        let line = error
            .span()
            .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
        let message = error.message().lines().next().unwrap_or("invalid TOML").to_owned();
        let reason = match line {
            Some(line) => format!("{message} (line {line})"),
            None => message,
        };

        ConfigError::ConfigInvalid {
            path: path.to_owned(),
            reason: Secrets::of(text, None).scrub(&reason),
        }
    })
}

/// The warning for a config file other users can read (Unix; `None` elsewhere or when private).
pub(super) fn permission_warning(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(path).ok()?.permissions().mode();
        readable_by_others(mode).then(|| {
            format!(
                "{} can be read by other users and may hold the database password, run chmod 600 on it",
                path.display()
            )
        })
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// True when the group or everyone may read a file with Unix `mode`.
#[cfg(unix)]
fn readable_by_others(mode: u32) -> bool {
    mode & 0o044 != 0
}

/// True when the group or everyone may write a file with Unix `mode`.
#[cfg(unix)]
fn writable_by_others(mode: u32) -> bool {
    mode & 0o022 != 0
}
