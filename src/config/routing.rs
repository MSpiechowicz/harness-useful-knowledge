//! Where the documents are kept when a remote database is set: in it, on this computer, or in both.
//!
//! ```toml
//! [routing]
//! repository_documents = "remote"        # remote (default), local or remote+local
//! global_documents = "remote+local"
//! ```
//!
//! A document linked to a repository follows `repository_documents`, one linked to none follows
//! `global_documents`. The environment variables [`REPOSITORY_VAR`] and [`GLOBAL_VAR`] win over the
//! file. `remote+local` reads both places and saves to the remote one unless `--place local` says
//! otherwise. Without a remote database everything is local and the settings do not matter.

use super::ConfigError;

/// Environment variable with the routing of the documents linked to a repository (empty = unset).
pub const REPOSITORY_VAR: &str = "HARNESS_KNOWLEDGE_REPOSITORY_DOCUMENTS";

/// Environment variable with the routing of the documents linked to no repository (empty = unset).
pub const GLOBAL_VAR: &str = "HARNESS_KNOWLEDGE_GLOBAL_DOCUMENTS";

/// One of the two databases a document can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Place {
    /// This computer's SQLite file.
    Local,
    /// The remote (team) database.
    Remote,
}

impl Place {
    /// `local` or `remote`, as the commands and the JSON write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }

    /// Where it is, in words.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Local => "this computer",
            Self::Remote => "the remote database",
        }
    }

    /// Where it is, with its preposition: `on this computer`, `in the remote database`.
    pub fn at(self) -> &'static str {
        match self {
            Self::Local => "on this computer",
            Self::Remote => "in the remote database",
        }
    }

    /// Parses `remote` or `local` (any case, blanks ignored).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "local" => Some(Self::Local),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }
}

/// A document's kind, which picks the routing that applies to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Linked to at least one repository.
    Repository,
    /// Linked to none.
    Global,
}

impl Kind {
    /// The kind of a document with `links` links.
    pub fn of(links: usize) -> Self {
        if links == 0 { Self::Global } else { Self::Repository }
    }

    fn setting(self) -> &'static str {
        match self {
            Self::Repository => "repository_documents",
            Self::Global => "global_documents",
        }
    }
}

/// Where one kind of document is kept: [`Place`] plus `remote+local`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Where {
    #[default]
    Remote,
    Local,
    RemoteLocal,
}

impl Where {
    /// `remote`, `local` or `remote+local`, as the settings write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Local => "local",
            Self::RemoteLocal => "remote+local",
        }
    }

    /// Parses a setting (any case, blanks ignored).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "remote" => Some(Self::Remote),
            "local" => Some(Self::Local),
            "remote+local" => Some(Self::RemoteLocal),
            _ => None,
        }
    }

    /// True when documents of this kind are read from `place` and may be saved there.
    pub fn includes(self, place: Place) -> bool {
        match (self, place) {
            (Self::RemoteLocal, _) | (Self::Remote, Place::Remote) | (Self::Local, Place::Local) => true,
            (Self::Remote, Place::Local) | (Self::Local, Place::Remote) => false,
        }
    }

    /// Where a new document goes when nothing says: the remote database unless it is `local`.
    pub fn default_place(self) -> Place {
        match self {
            Self::Local => Place::Local,
            Self::Remote | Self::RemoteLocal => Place::Remote,
        }
    }
}

/// Which places are in use, which decides how ids are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Only this computer's file: one store, plain ids.
    Local,
    /// Only the remote database: one store, plain ids.
    Remote,
    /// Both: a routed store, local ids are written `local:<id>`.
    Both,
}

/// The routing of the two kinds of document. Normalised by [`super::select`]: without a remote
/// database both are [`Where::Local`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Routing {
    pub repository: Where,
    pub global: Where,
}

impl Default for Routing {
    /// No remote database: everything is local.
    fn default() -> Self {
        Self::local()
    }
}

impl Routing {
    /// Everything on this computer.
    pub fn local() -> Self {
        Self {
            repository: Where::Local,
            global: Where::Local,
        }
    }

    pub fn new(repository: Where, global: Where) -> Self {
        Self { repository, global }
    }

    /// The setting for `kind`.
    pub fn rule(self, kind: Kind) -> Where {
        match kind {
            Kind::Repository => self.repository,
            Kind::Global => self.global,
        }
    }

    /// True when documents of `kind` are read from `place`.
    pub fn reads(self, kind: Kind, place: Place) -> bool {
        self.rule(kind).includes(place)
    }

    /// True when some kind of document is in `place`.
    pub fn uses(self, place: Place) -> bool {
        self.repository.includes(place) || self.global.includes(place)
    }

    /// The places in use.
    pub fn layout(self) -> Layout {
        match (self.uses(Place::Local), self.uses(Place::Remote)) {
            (true, true) => Layout::Both,
            (true, false) => Layout::Local,
            _ => Layout::Remote,
        }
    }

    /// The places in use, remote first.
    pub fn places(self) -> Vec<Place> {
        [Place::Remote, Place::Local]
            .into_iter()
            .filter(|place| self.uses(*place))
            .collect()
    }

    /// Where a new document of `kind` goes when `--place` is not given.
    pub fn default_place(self, kind: Kind) -> Place {
        self.rule(kind).default_place()
    }

    /// `Ok` when a document of `kind` may be saved in `place`, else the message that says why.
    pub fn check_write(self, kind: Kind, place: Place) -> Result<(), String> {
        if self.reads(kind, place) {
            return Ok(());
        }

        if place == Place::Remote && !self.uses(Place::Remote) && self.repository == self.global {
            return Err("no remote database is configured, so documents are kept on this computer".to_owned());
        }

        let which = match kind {
            Kind::Repository => "documents linked to a repository",
            Kind::Global => "documents linked to no repository",
        };
        let rule = self.rule(kind);
        let kept = match rule {
            Where::Remote => Place::Remote.at(),
            Where::Local => Place::Local.at(),
            Where::RemoteLocal => "in both places",
        };

        Err(format!(
            "{which} are kept {kept} ({} is \"{}\"), not {}. Change the routing in the settings to save them there",
            kind.setting(),
            rule.as_str(),
            place.at(),
        ))
    }
}

/// The [`Where`] a setting names, from the environment variable `var`, else `file`; `Remote` when
/// neither is set. The error names where the unknown value came from.
pub(super) fn setting(var: Option<&str>, var_name: &str, file: Option<&str>, key: &str) -> Result<Where, ConfigError> {
    let (raw, origin) = match var {
        Some(raw) => (Some(raw), var_name.to_owned()),
        None => (file, format!("{key} in the config file")),
    };

    match raw {
        None => Ok(Where::Remote),
        Some(raw) => Where::parse(raw).ok_or(ConfigError::Routing(origin)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_in_any_case_and_other_words_are_refused() {
        assert_eq!(Where::parse(" Remote+Local "), Some(Where::RemoteLocal));
        assert_eq!(Where::parse("LOCAL"), Some(Where::Local));
        assert_eq!(Where::parse("both"), None);
        assert_eq!(Where::parse(""), None);

        let refused = setting(None, GLOBAL_VAR, Some("everywhere"), "global_documents").expect_err("refused");
        assert!(refused.to_string().contains("global_documents in the config file"));
        assert!(refused.to_string().contains("remote+local"));

        let refused = setting(Some("x"), GLOBAL_VAR, Some("remote"), "global_documents").expect_err("refused");
        assert!(refused.to_string().contains(GLOBAL_VAR));

        assert_eq!(setting(None, GLOBAL_VAR, None, "k").ok(), Some(Where::Remote));
        assert_eq!(
            setting(Some("local"), GLOBAL_VAR, Some("remote"), "k").ok(),
            Some(Where::Local),
            "the variable wins"
        );
    }

    #[test]
    fn the_layout_follows_the_places_the_two_kinds_use() {
        let layout = |repository, global| Routing::new(repository, global).layout();

        assert_eq!(layout(Where::Remote, Where::Remote), Layout::Remote);
        assert_eq!(layout(Where::Local, Where::Local), Layout::Local);
        assert_eq!(layout(Where::Remote, Where::Local), Layout::Both);
        assert_eq!(layout(Where::RemoteLocal, Where::Remote), Layout::Both);
        assert_eq!(layout(Where::Local, Where::RemoteLocal), Layout::Both);
        assert_eq!(Routing::default().layout(), Layout::Local);
    }

    #[test]
    fn a_new_document_goes_remote_unless_its_kind_is_local() {
        let routing = Routing::new(Where::RemoteLocal, Where::Local);

        assert_eq!(routing.default_place(Kind::Repository), Place::Remote);
        assert_eq!(routing.default_place(Kind::Global), Place::Local);
        assert!(routing.check_write(Kind::Repository, Place::Local).is_ok());
        assert!(routing.check_write(Kind::Global, Place::Remote).is_err());
    }

    #[test]
    fn a_write_the_routing_does_not_allow_says_why() {
        let routing = Routing::new(Where::Remote, Where::Local);
        let refused = routing
            .check_write(Kind::Repository, Place::Local)
            .expect_err("refused");

        assert!(refused.contains("repository_documents"), "{refused}");
        assert!(refused.contains("on this computer"), "{refused}");

        let none = Routing::local()
            .check_write(Kind::Global, Place::Remote)
            .expect_err("refused");
        assert!(none.contains("no remote database"), "{none}");
    }
}
