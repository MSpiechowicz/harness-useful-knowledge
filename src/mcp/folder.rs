//! Which folders `knowledge_add` may read a file from: a project, never a place that holds the
//! person's other files or the system's.
//!
//! A project is a git work tree or a folder with one of the files projects keep at their root
//! ([`MARKERS`]). The home folder, a filesystem root and the system folders are not projects, and
//! neither is a folder with none of the markers (`~/.ssh`, `/etc/ssl`): an agent that names one as
//! its `project_path` would otherwise read any file in it that has no dot in its name.

use std::path::Path;

use crate::project::canonical_home;
use crate::support::Env;

/// Files and folders at the root of a project.
const MARKERS: [&str; 20] = [
    ".git",
    ".hg",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "requirements.txt",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Gemfile",
    "composer.json",
    "mix.exs",
    "CMakeLists.txt",
    "Makefile",
    "deno.json",
    "pubspec.yaml",
    "Package.swift",
    "project.clj",
];

/// Folders that are no project, and what is inside them.
#[cfg(unix)]
const SYSTEM_TREES: [&str; 9] = [
    "/etc",
    "/proc",
    "/sys",
    "/dev",
    "/boot",
    "/root",
    "/System",
    "/Library",
    "/private/etc",
];

/// Folders that are no project themselves (projects live below them).
#[cfg(unix)]
const SYSTEM_FOLDERS: [&str; 16] = [
    "/usr",
    "/usr/local",
    "/var",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/opt",
    "/srv",
    "/mnt",
    "/media",
    "/run",
    "/tmp",
    "/home",
    "/Users",
    "/Applications",
];

/// `Ok` when `root` (canonical) may be read from as a project, else why not.
pub(super) fn check(env: &Env, root: &Path) -> Result<(), String> {
    let refused = || {
        "project_path must be the root of a project: a git work tree or a folder with a project file such as \
         Cargo.toml or package.json (the home folder and system folders are refused)"
            .to_owned()
    };

    if root == canonical_home(env) || root.parent().is_none() || is_system(root) || holds_secrets(root) {
        return Err(refused());
    }

    if MARKERS.iter().any(|marker| root.join(marker).exists()) {
        return Ok(());
    }

    Err(refused())
}

/// Folders that keep keys and passwords, refused even when they are git work trees
/// (`~/.password-store` is one).
const SECRET_FOLDERS: [&str; 6] = [".ssh", ".gnupg", ".aws", ".password-store", ".kube", ".docker"];

fn holds_secrets(root: &Path) -> bool {
    root.components().any(|part| {
        SECRET_FOLDERS
            .iter()
            .any(|secret| part.as_os_str().to_string_lossy().eq_ignore_ascii_case(secret))
    })
}

#[cfg(unix)]
fn is_system(root: &Path) -> bool {
    SYSTEM_TREES.iter().any(|tree| root.starts_with(tree))
        || SYSTEM_FOLDERS.iter().any(|folder| root == Path::new(folder))
}

#[cfg(windows)]
fn is_system(root: &Path) -> bool {
    ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramData"]
        .iter()
        .filter_map(std::env::var_os)
        .any(|folder| root.starts_with(dunce::canonicalize(&folder).unwrap_or_else(|_| folder.into())))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn folders_that_keep_keys_are_refused_even_as_projects() {
        assert!(super::holds_secrets(Path::new("/home/u/.password-store")));
        assert!(super::holds_secrets(Path::new("/home/u/.ssh/keys")));
        assert!(super::holds_secrets(Path::new("/Users/u/.SSH")));
        assert!(!super::holds_secrets(Path::new("/home/u/code/sshd-config")));
    }
}
