//! Which repository a call is about.
//!
//! The directory is the command's `--project` (absolute, existing) or the server's working
//! directory. Its git work tree, when there is one and it holds the directory, is the project
//! root; otherwise (a `core.worktree` or gitfile can name a work tree elsewhere) the directory
//! itself is, keyed by its path. The key of a work tree is `git:<host/owner/repo>` from the
//! normalized `origin` remote, so every clone of a repository shares its documents, and it is the
//! same key `harness-memory` gives the repository; without a usable remote it is
//! `path:<canonical root>`. A root that is the home folder or a filesystem root is no project:
//! agents started there must name the repository.
//! Results are cached per directory for the life of the process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::support::Env;
use crate::support::git::Git;
use crate::support::paths::validate_project_dir;
use crate::support::text::display_safe;

/// The identity of a repository (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectKey {
    /// `git:<host/owner/repo>` or `path:<canonical directory>`.
    pub key: String,
    /// The canonical work-tree (or directory) path.
    pub root: PathBuf,
    /// The normalized `origin` remote (`host/owner/repo`, no credentials).
    pub remote: Option<String>,
}

impl ProjectKey {
    /// The key that stands for every repository when a command reads all of them
    /// (`--all-repos`); a real key always starts with `git:` or `path:`, so it never names one.
    pub const EVERY: &'static str = "*";

    /// The key of `--all-repos`: it only reads, it has no work tree.
    pub fn every() -> Self {
        Self {
            key: Self::EVERY.to_owned(),
            root: PathBuf::new(),
            remote: None,
        }
    }

    /// Whether this is [`ProjectKey::every`].
    pub fn is_every(&self) -> bool {
        self.key == Self::EVERY
    }
}

/// Resolves and caches project identities. See the module docs.
pub struct ProjectResolver {
    env: Env,
    cwd: Option<PathBuf>,
    cache: HashMap<PathBuf, Option<ProjectKey>>,
}

impl std::fmt::Debug for ProjectResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectResolver")
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

impl ProjectResolver {
    /// A resolver for a server started in `cwd` (`None` when the working directory is unknown).
    pub fn new(env: Env, cwd: Option<PathBuf>) -> Self {
        Self {
            env,
            cwd,
            cache: HashMap::new(),
        }
    }

    /// The project of `project_path`, else of the working directory; `Ok(None)` when there is
    /// none. Fails only for an unusable `project_path`.
    pub fn resolve(&mut self, project_path: Option<&str>) -> Result<Option<ProjectKey>, String> {
        let start = match project_path.map(str::trim).filter(|raw| !raw.is_empty()) {
            Some(raw) => {
                if !Path::new(raw).is_absolute() {
                    return Err("project_path must be an absolute path".to_owned());
                }

                validate_project_dir(&self.env, Path::new(raw)).map_err(|_| {
                    "project_path must be an existing directory that is not a filesystem root".to_owned()
                })?
            }
            None => {
                let Some(cwd) = &self.cwd else {
                    return Ok(None);
                };

                match validate_project_dir(&self.env, cwd) {
                    Ok(dir) => dir,
                    Err(_) => return Ok(None),
                }
            }
        };

        if let Some(known) = self.cache.get(&start) {
            return Ok(known.clone());
        }

        let identity = self.identify(&start);
        self.cache.insert(start, identity.clone());
        Ok(identity)
    }

    fn identify(&self, dir: &Path) -> Option<ProjectKey> {
        let toplevel = self.work_tree(dir);
        let root = toplevel.clone().unwrap_or_else(|| dir.to_path_buf());

        if self.is_home(&root) || root.parent().is_none() {
            return None;
        }

        let remote = match toplevel {
            Some(_) => self
                .git(&root, &["config", "--get", "remote.origin.url"])
                .and_then(|url| normalize_remote(&url)),
            None => None,
        };

        let key = match &remote {
            Some(remote) => format!("git:{remote}"),
            None => format!("path:{}", root.display()),
        };

        Some(ProjectKey { key, root, remote })
    }

    /// The canonical git work tree holding `dir` (itself canonical). `None` outside git, and when
    /// git names a work tree that does not hold `dir`: a `core.worktree` or a gitfile in a folder
    /// can point anywhere, and must not give the folder another checkout's root and key.
    fn work_tree(&self, dir: &Path) -> Option<PathBuf> {
        let reported = self.git(dir, &["rev-parse", "--show-toplevel"])?;
        let top = validate_project_dir(&self.env, Path::new(&reported)).ok()?;

        dir.starts_with(&top).then_some(top)
    }

    /// The environment the server runs with.
    pub fn env(&self) -> &Env {
        &self.env
    }

    /// The server's working directory, when known.
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// The home folder, canonical when it exists.
    pub fn home(&self) -> PathBuf {
        canonical_home(&self.env)
    }

    fn is_home(&self, dir: &Path) -> bool {
        dir == self.home()
    }

    /// The first stdout line of a successful `git <args>` in `dir`; `None` when git is missing,
    /// fails or prints nothing.
    fn git(&self, dir: &Path, args: &[&str]) -> Option<String> {
        let output = Git::new(&self.env, dir).ok()?.run(args).ok()?;

        if !output.success() {
            return None;
        }

        let line = output.stdout.lines().next()?.trim().to_owned();
        (!line.is_empty()).then_some(line)
    }
}

/// The home folder of `env`, canonical when it exists.
pub fn canonical_home(env: &Env) -> PathBuf {
    validate_project_dir(env, &env.home).unwrap_or_else(|_| env.home.clone())
}

/// `host/owner/repo` from a git remote URL: scheme, credentials, port, query and a trailing
/// `.git` or `/` are dropped, the host is lowercased, and the scp form `user@host:owner/repo`
/// gives the same result as `https://host/owner/repo.git`. `None` for local paths, `file://`
/// URLs, anything unparsable, and URLs with whitespace, control or invisible format characters
/// (the key is shown to agents, so such a remote falls back to the path key).
pub fn normalize_remote(raw: &str) -> Option<String> {
    let url = raw.trim();

    if url.is_empty() || url.chars().any(char::is_whitespace) || !display_safe(url) {
        return None;
    }

    let (authority, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            if scheme.eq_ignore_ascii_case("file") {
                return None;
            }

            rest.split_once('/')?
        }
        None => {
            let (authority, path) = url.split_once(':')?;

            // A local path (`/srv/repo`, `./repo`, `C:\repo`) is not a remote host.
            if authority.contains(['/', '\\']) || authority.len() == 1 {
                return None;
            }

            (authority, path)
        }
    };

    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = strip_port(host).to_ascii_lowercase();
    let host_ok = host
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || ".-_[]:".contains(character));

    if host.is_empty() || !host_ok {
        return None;
    }

    let path = path.split(['?', '#']).next().unwrap_or_default().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');

    if path.is_empty() {
        return None;
    }

    Some(format!("{host}/{path}"))
}

/// `host` without a numeric `:port`.
fn strip_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) && !name.ends_with(':') => {
            name
        }
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    use super::*;

    /// An environment with `home` as the home folder and the real `PATH` (for git).
    fn env(home: &Path) -> Env {
        Env::for_tests(home).with_var("PATH", std::env::var("PATH").unwrap_or_default())
    }

    fn canonical(path: &Path) -> PathBuf {
        dunce::canonicalize(path).expect("canonical")
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    /// A new repository `parent/name` (with a `src/deep` folder) whose `origin` is `remote`.
    fn repo_with_remote(parent: &Path, name: &str, remote: &str) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(dir.join("src/deep")).expect("mkdir");
        git(&dir, &["init", "-q"]);
        git(&dir, &["remote", "add", "origin", remote]);
        dir
    }

    #[test]
    fn https_with_credentials_and_scp_remotes_share_one_key() {
        let home = TempDir::new().expect("home");
        let work = TempDir::new().expect("work");
        let https = repo_with_remote(work.path(), "a", "https://bob:s3cret@GitHub.com/Acme/widget.git/");
        let scp = repo_with_remote(work.path(), "b", "git@github.com:Acme/widget.git");

        let mut resolver = ProjectResolver::new(env(home.path()), Some(https.join("src/deep")));
        let from_https = resolver.resolve(None).expect("resolves").expect("a project");
        let from_scp = resolver
            .resolve(Some(scp.to_str().expect("utf-8")))
            .expect("resolves")
            .expect("a project");

        assert_eq!(from_https.key, "git:github.com/Acme/widget");
        assert_eq!(from_scp.key, from_https.key);
        assert_eq!(from_https.root, canonical(&https), "the work tree, not the subfolder");
        assert_eq!(from_https.remote.as_deref(), Some("github.com/Acme/widget"));

        let shown = format!("{from_https:?}");
        assert!(!shown.contains("bob") && !shown.contains("s3cret"), "{shown}");
    }

    #[test]
    fn a_folder_outside_git_is_keyed_by_its_path() {
        let home = TempDir::new().expect("home");
        let work = TempDir::new().expect("work");
        let plain = work.path().join("notes");
        std::fs::create_dir(&plain).expect("mkdir");

        let mut resolver = ProjectResolver::new(env(home.path()), Some(plain.clone()));
        let project = resolver.resolve(None).expect("resolves").expect("a project");

        assert_eq!(project.key, format!("path:{}", canonical(&plain).display()));
        assert_eq!(project.remote, None);
    }

    #[test]
    fn a_repository_without_a_remote_is_keyed_by_its_work_tree() {
        let home = TempDir::new().expect("home");
        let work = TempDir::new().expect("work");
        let dir = work.path().join("local");
        std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
        git(&dir, &["init", "-q"]);

        let mut resolver = ProjectResolver::new(env(home.path()), Some(dir.join("sub")));
        let project = resolver.resolve(None).expect("resolves").expect("a project");

        assert_eq!(project.key, format!("path:{}", canonical(&dir).display()));
    }

    #[test]
    fn the_home_folder_and_a_filesystem_root_are_no_project() {
        let home = TempDir::new().expect("home");

        let mut at_home = ProjectResolver::new(env(home.path()), Some(home.path().to_path_buf()));
        assert_eq!(at_home.resolve(None).expect("resolves"), None);

        let root = if cfg!(windows) { "C:\\" } else { "/" };
        let mut at_root = ProjectResolver::new(env(home.path()), Some(PathBuf::from(root)));
        assert_eq!(at_root.resolve(None).expect("resolves"), None);

        let mut nowhere = ProjectResolver::new(env(home.path()), None);
        assert_eq!(nowhere.resolve(None).expect("resolves"), None);
    }

    #[test]
    fn project_path_must_be_an_absolute_existing_folder() {
        let home = TempDir::new().expect("home");
        let mut resolver = ProjectResolver::new(env(home.path()), Some(home.path().to_path_buf()));

        assert!(resolver.resolve(Some("relative/dir")).is_err());
        assert!(resolver.resolve(Some("/definitely/not/here/xyz")).is_err());

        let file = home.path().join("file.txt");
        std::fs::write(&file, "x").expect("write");
        assert!(resolver.resolve(Some(file.to_str().expect("utf-8"))).is_err());
    }

    #[test]
    fn remotes_normalize_to_host_owner_repo() {
        let cases = [
            ("https://github.com/o/r.git", Some("github.com/o/r")),
            ("https://user:token@GITHUB.com/o/r", Some("github.com/o/r")),
            ("ssh://git@github.com:22/o/r.git", Some("github.com/o/r")),
            (
                "git@gitlab.example.com:group/sub/r.git",
                Some("gitlab.example.com/group/sub/r"),
            ),
            ("https://host/o/r/?ref=x#frag", Some("host/o/r")),
            ("/srv/git/r.git", None),
            ("./r", None),
            ("C:\\repos\\r", None),
            ("file:///srv/git/r.git", None),
            ("https://host", None),
            ("", None),
        ];

        for (raw, expected) in cases {
            assert_eq!(normalize_remote(raw).as_deref(), expected, "{raw}");
        }
    }

    #[test]
    fn the_every_key_names_no_repository() {
        let every = ProjectKey::every();

        assert!(every.is_every());
        assert!(!every.key.starts_with("git:") && !every.key.starts_with("path:"));
    }
}
