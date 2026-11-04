//! What the commands share: their options, the settings, the repository `--project` names and the
//! paging options of the lists.
//!
//! Usage errors exit 2, failures (settings, database) exit 1, a missing capability of the
//! database (`check`'s problem codes) exit 3. A failure ends stderr with the line
//! that says why (`harness-knowledge: …`, after any warning), so a caller may show the last line.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::config::{self, Selection};
use crate::limits::{LIST_PAGE_DEFAULT, LIST_PAGE_MAX};
use crate::project::{ProjectKey, ProjectResolver};
use crate::support::Env;

/// The options of the lists' paging: `--offset N`, `--limit N`, `--query TEXT`, `--order asc|desc`.
pub const PAGE_OPTIONS: [&str; 4] = ["--offset", "--limit", "--query", "--order"];

/// An option that is blank or names more than one repository: a usage error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadOption;

/// The arguments of a command, read against the options it takes.
#[derive(Debug, Default)]
pub struct Args<'a> {
    /// The arguments that do not start with `-`, in order.
    pub positional: Vec<&'a str>,
    flags: Vec<&'a str>,
    values: Vec<(&'a str, &'a str)>,
}

impl<'a> Args<'a> {
    /// Reads `args`: the `flags` stand alone, the `valued` options take a value (`--name VALUE`
    /// or `--name=VALUE`). `None` for an unknown or repeated option, a missing value, or an
    /// argument that is not UTF-8.
    pub fn parse(args: &[Option<&'a str>], flags: &[&str], valued: &[&str]) -> Option<Self> {
        let mut parsed = Self::default();
        let mut rest = args.iter().copied();

        while let Some(arg) = rest.next() {
            let arg = arg?;

            if !arg.starts_with('-') {
                parsed.positional.push(arg);
                continue;
            }

            if flags.contains(&arg) {
                if parsed.flag(arg) {
                    return None;
                }

                parsed.flags.push(arg);
                continue;
            }

            let (name, value) = match arg.split_once('=') {
                Some((name, value)) => (name, value),
                None => (arg, rest.next().flatten()?),
            };

            if !valued.contains(&name) || parsed.value(name).is_some() {
                return None;
            }

            parsed.values.push((name, value));
        }

        Some(parsed)
    }

    /// True when the flag `name` was given.
    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains(&name)
    }

    /// The value of the option `name`, when given.
    pub fn value(&self, name: &str) -> Option<&'a str> {
        self.values
            .iter()
            .find(|(given, _)| *given == name)
            .map(|(_, value)| *value)
    }

    /// The path of `--project`; `Err` when it is blank.
    pub fn project(&self) -> Result<Option<String>, BadOption> {
        match self.value("--project") {
            Some(path) if path.trim().is_empty() => Err(BadOption),
            Some(path) => Ok(Some(path.to_owned())),
            None => Ok(None),
        }
    }

    /// The repositories `--project`, `--repo` and `--all-repos` pick (at most one of them, not
    /// blank); `Err` otherwise. Without any it is the repository of the working directory.
    pub fn repos(&self) -> Result<Repos, BadOption> {
        let every = self.flag("--all-repos");
        let key = self.value("--repo");
        let project = self.value("--project");

        match (every, key, project) {
            (true, None, None) => Ok(Repos::Every),
            // A real key starts with `git:` or `path:`; anything else (`*`, a blank) is no repository.
            (false, Some(key), None) if key.starts_with("git:") || key.starts_with("path:") => {
                Ok(Repos::Key(key.to_owned()))
            }
            (false, None, _) => self.project().map(Repos::Project),
            _ => Err(BadOption),
        }
    }
}

/// The valued options that name the repositories a command reads, with the flag `--all-repos`
/// (see [`Args::repos`]).
pub const PICK_VALUED: [&str; 2] = ["--project", "--repo"];

/// Which repositories a command sees besides the documents linked to none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repos {
    /// `--project PATH`: the repository of that directory, else of the working directory.
    Project(Option<String>),
    /// `--repo KEY`: the repository with that key (`git:host/owner/repo`), wherever it is checked out.
    Key(String),
    /// `--all-repos`: every repository.
    Every,
}

impl From<Option<&str>> for Repos {
    fn from(project: Option<&str>) -> Self {
        Self::Project(project.map(str::to_owned))
    }
}

impl Repos {
    /// The repository this picks: `Some(None)` for none (outside a repository, only the global
    /// documents), `Some(Some(key))` for one, `None`, with the reason printed, when `--project`
    /// names a wrong path. `--all-repos` gives [`ProjectKey::every`].
    pub fn resolve(&self, env: &Env) -> Option<Option<ProjectKey>> {
        match self {
            Self::Project(path) => repository(env, path.as_deref()),
            Self::Key(key) => Some(Some(ProjectKey {
                key: key.clone(),
                root: PathBuf::new(),
                remote: None,
            })),
            Self::Every => Some(Some(ProjectKey::every())),
        }
    }
}

/// The paging options of a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub offset: usize,
    /// 1 to [`LIST_PAGE_MAX`], [`LIST_PAGE_DEFAULT`] when not given.
    pub limit: usize,
    /// Blank when not given.
    pub query: String,
    /// `--order`: `Some(true)` for `desc`, `None` when not given (the sort's own order).
    pub descending: Option<bool>,
}

impl Page {
    /// The paging options of `args`; `None` when one is not a number, the limit is out of range,
    /// or the order is neither `asc` nor `desc`.
    pub fn of(args: &Args) -> Option<Self> {
        let offset = match args.value("--offset") {
            Some(offset) => offset.parse::<usize>().ok()?,
            None => 0,
        };

        let limit = match args.value("--limit") {
            Some(limit) => limit
                .parse::<usize>()
                .ok()
                .filter(|limit| (1..=LIST_PAGE_MAX).contains(limit))?,
            None => LIST_PAGE_DEFAULT,
        };

        let descending = match args.value("--order") {
            Some("asc") => Some(false),
            Some("desc") => Some(true),
            Some(_) => return None,
            None => None,
        };

        Some(Self {
            offset,
            limit,
            query: args.value("--query").unwrap_or_default().trim().to_owned(),
            descending,
        })
    }
}

/// Prints `object` with `json`, else `line`.
pub fn print_either(json: bool, object: serde_json::Value, line: &str) {
    if json {
        println!("{object}");
    } else {
        println!("{line}");
    }
}

/// Prints the usage of `commands`; exit 2.
pub fn usage(commands: &str) -> ExitCode {
    eprintln!("usage: harness-knowledge {commands}");
    ExitCode::from(2)
}

/// Prints a failed read or write of the knowledge (a team database that cannot be reached, a refused
/// input, ...) as `{"error": "failed", "message"}` on stdout, so Harness Desktop can show the
/// message instead of waiting for a result, and the message on stderr; exit 1. `message` must carry
/// no credentials.
pub fn failed(message: &str) -> ExitCode {
    println!("{}", serde_json::json!({ "error": "failed", "message": message }));
    eprintln!("harness-knowledge: {message}");
    ExitCode::FAILURE
}

/// Prints a remote database that cannot be reached as `{"error": "unreachable", "message"}` on stdout
/// and the message on stderr; exit 1. Harness Desktop matches on the code. `message` must carry no
/// credentials.
pub fn unreachable(message: &str) -> ExitCode {
    println!("{}", serde_json::json!({ "error": "unreachable", "message": message }));
    eprintln!("harness-knowledge: {message}");
    ExitCode::FAILURE
}

/// Prints a refusal of a review command as `{"error": CODE, "message"}` on stdout and the message on
/// stderr; exit 1. `code` is one of [`crate::review::Code::as_str`]. Harness Desktop matches on the
/// code. `message` must carry no credentials.
pub fn refused(code: &str, message: &str) -> ExitCode {
    println!("{}", serde_json::json!({ "error": code, "message": message }));
    eprintln!("harness-knowledge: {message}");
    ExitCode::FAILURE
}

/// Prints a command this build does not support as `{"error": "unsupported", "message"}` on stdout
/// and the message on stderr; exit 2.
pub fn unsupported(message: &str) -> ExitCode {
    println!("{}", serde_json::json!({ "error": "unsupported", "message": message }));
    eprintln!("harness-knowledge: {message}");
    ExitCode::from(2)
}

/// Prints a missing capability of the database (`code` is a `check` problem code such as `no-fts5`) as
/// `{"error": "missing", "code", "message"}` on stdout and the message on stderr; exit 3.
pub fn missing_capability(code: &str, message: &str) -> ExitCode {
    println!(
        "{}",
        serde_json::json!({ "error": "missing", "code": code, "message": message })
    );
    eprintln!("harness-knowledge: {message}");
    ExitCode::from(3)
}

/// The environment and the settings, after printing their warnings; `None`, with the reason
/// printed, when the home folder is unknown or the settings are wrong.
pub fn settings() -> Option<(Env, Selection)> {
    let Ok(env) = Env::from_process() else {
        eprintln!("harness-knowledge: the home folder is unknown");
        return None;
    };

    let selection = match config::select(&env) {
        Ok(selection) => selection,
        Err(error) => {
            eprintln!("harness-knowledge: {error}");
            return None;
        }
    };

    for warning in &selection.warnings {
        eprintln!("harness-knowledge: {warning}");
    }

    Some((env, selection))
}

/// The repository of `project` (else of the working directory), found the way the MCP server
/// finds it: `Some(None)` outside one, `None`, with the reason printed, when the path is wrong.
pub fn repository(env: &Env, project: Option<&str>) -> Option<Option<ProjectKey>> {
    let mut projects = ProjectResolver::new(env.clone(), std::env::current_dir().ok());

    match projects.resolve(project) {
        Ok(repo) => Some(repo),
        Err(message) => {
            eprintln!("harness-knowledge: {}", message.replace("project_path", "--project"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(raw: &[&'a str]) -> Vec<Option<&'a str>> {
        raw.iter().map(|arg| Some(*arg)).collect()
    }

    #[test]
    fn options_take_values_either_way_and_never_twice() {
        let raw = args(&["list", "--json", "--project", "/w", "--query=a b", "7"]);
        let parsed = Args::parse(&raw, &["--json"], &["--project", "--query"]).expect("valid");

        assert_eq!(parsed.positional, ["list", "7"]);
        assert!(parsed.flag("--json"));
        assert_eq!(parsed.project(), Ok(Some("/w".to_owned())));
        assert_eq!(parsed.value("--query"), Some("a b"));

        for bad in [
            &["--json", "--json"][..],
            &["--project"],
            &["--project", "/a", "--project=/b"],
            &["--yaml"],
            &["--json=1"],
        ] {
            assert!(
                Args::parse(&args(bad), &["--json"], &["--project"]).is_none(),
                "{bad:?}"
            );
        }

        let blank = args(&["--project", " "]);
        assert_eq!(
            Args::parse(&blank, &[], &["--project"]).expect("parsed").project(),
            Err(BadOption)
        );
        assert!(Args::parse(&[None], &[], &[]).is_none(), "not UTF-8");
    }

    #[test]
    fn paging_has_defaults_and_bounds() {
        let none = Args::parse(&[], &[], &PAGE_OPTIONS).expect("empty");
        assert_eq!(
            Page::of(&none),
            Some(Page {
                offset: 0,
                limit: LIST_PAGE_DEFAULT,
                query: String::new(),
                descending: None
            })
        );

        let raw = args(&[
            "--offset", "400", "--limit", "200", "--query", " Cache ", "--order", "asc",
        ]);
        let page = Page::of(&Args::parse(&raw, &[], &PAGE_OPTIONS).expect("valid")).expect("page");
        assert_eq!(
            (page.offset, page.limit, page.query.as_str(), page.descending),
            (400, 200, "Cache", Some(false))
        );

        for bad in [
            &["--limit", "0"][..],
            &["--limit", "201"],
            &["--offset", "-1"],
            &["--offset", "x"],
            &["--order", "up"],
        ] {
            let parsed = Args::parse(&args(bad), &[], &PAGE_OPTIONS).expect("parsed");
            assert!(Page::of(&parsed).is_none(), "{bad:?}");
        }
    }
}
