//! Who is writing: the git `user.name` of the person, never the email.
//!
//! The name is read with `git config --get user.name` through the hardened runner ([`super::git`]),
//! in the writer's repository root when it is known, else in the current folder (the global
//! configuration of the home folder applies in both). It is read once per process and folder, the
//! folder taken as its canonical path, and only a definitive answer is kept: a git that could not be
//! run, or ran out of time, is asked again, and at most [`CACHE_MAX`] folders are kept (the oldest
//! goes), however many an agent names.
//! A name is cleaned like memory's `check_author`: trimmed, refused when empty, longer than
//! [`NAME_MAX`] characters or holding a control or invisible format character (bidirectional
//! overrides, zero-width characters), see [`clean`]. The same cleaning is applied to what is read
//! back from a database or a held proposal, which anybody with write access may have filled.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::git::Git;
use super::text::display_safe;
use super::{Env, Error};

/// Most characters of an author's name.
pub const NAME_MAX: usize = 80;

/// Most characters of the harness an author wrote through.
pub const VIA_MAX: usize = 64;

/// Most folders whose answer is kept.
pub const CACHE_MAX: usize = 64;

/// What was read for a folder of a home, oldest first, so a test with another home reads its own.
type Cache = Vec<((PathBuf, PathBuf), Option<String>)>;

static CACHE: Mutex<Cache> = Mutex::new(Vec::new());

/// The git `user.name` in `dir`, `None` when there is none, git is missing, or the name is not
/// fit to store (see [`clean`]).
pub fn resolve(env: &Env, dir: &Path) -> Option<String> {
    let root = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let key = (root.clone(), env.home.clone());

    if let Some((_, known)) = CACHE.lock().ok()?.iter().find(|(cached, _)| *cached == key) {
        return known.clone();
    }

    // The lock is not held while git runs.
    let (name, definitive) = match read(env, &root) {
        Ok(name) => (name, true),
        Err(_) => (None, false),
    };

    if definitive {
        remember(key, name.clone());
    }

    name
}

/// Keeps `name` for `key`, dropping the oldest answer past [`CACHE_MAX`].
fn remember(key: (PathBuf, PathBuf), name: Option<String>) {
    let Ok(mut cache) = CACHE.lock() else { return };

    if cache.iter().any(|(cached, _)| *cached == key) {
        return;
    }

    if cache.len() >= CACHE_MAX {
        cache.remove(0);
    }

    cache.push((key, name));
}

/// What git says: `Ok(None)` when it ran and has no usable name (a definitive answer), `Err` when
/// it could not be run or did not finish.
fn read(env: &Env, dir: &Path) -> Result<Option<String>, Error> {
    let output = Git::new(env, dir)?.run(&["config", "--get", "user.name"])?;

    // Exit code 1 is "no such key". Any other failure (a broken configuration, a signal) says
    // nothing about the name.
    match output.status {
        Some(0) => Ok(clean(&output.stdout)),
        Some(1) => Ok(None),
        _ => Err(Error::io("reading the git user name", std::io::ErrorKind::Other.into())),
    }
}

/// `raw` as an author's name: trimmed; `None` when empty, over [`NAME_MAX`] characters or holding
/// a control character (a name with a line break in it included) or an invisible format character.
pub fn clean(raw: &str) -> Option<String> {
    let name = raw.trim();

    if name.is_empty() || name.chars().count() > NAME_MAX || !display_safe(name) {
        return None;
    }

    Some(name.to_owned())
}

/// `raw` as the harness an author wrote through: on one line, control and invisible characters
/// dropped, at most [`VIA_MAX`] characters; `None` when nothing is left.
pub fn clean_via(raw: &str) -> Option<String> {
    let line = crate::store::clean::line(raw);
    let via = line.chars().take(VIA_MAX).collect::<String>();
    let via = via.trim_end();

    (!via.is_empty()).then(|| via.to_owned())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    /// A home whose global git configuration is `config`, and a folder outside any repository.
    fn setup(config: Option<&str>) -> (TempDir, Env, PathBuf) {
        let home = TempDir::new().expect("temp home");
        let dir = home.path().join("work");
        std::fs::create_dir_all(&dir).expect("work folder");

        if let Some(config) = config {
            std::fs::write(home.path().join(".gitconfig"), config).expect("gitconfig");
        }

        let env = Env::for_tests(home.path()).with_var("PATH", std::env::var("PATH").unwrap_or_default());

        (home, env, dir)
    }

    #[test]
    fn the_name_is_the_global_git_user_name() {
        let (_home, env, dir) = setup(Some("[user]\n\tname = Ada\n\temail = ada@example.com\n"));

        assert_eq!(resolve(&env, &dir), Some("Ada".to_owned()));
    }

    #[test]
    fn a_repository_name_wins_over_the_global_one() {
        let (_home, env, dir) = setup(Some("[user]\n\tname = Ada\n"));
        let git = Git::new(&env, &dir).expect("git is installed");

        assert!(git.run(&["init", "-q"]).expect("init").success());
        assert!(git.run(&["config", "user.name", "Grace"]).expect("config").success());

        assert_eq!(resolve(&env, &dir), Some("Grace".to_owned()));
    }

    #[test]
    fn no_name_is_none() {
        let (_home, env, dir) = setup(None);
        assert_eq!(resolve(&env, &dir), None);

        let (_home, env, dir) = setup(Some("[user]\n\tname =\n"));
        assert_eq!(resolve(&env, &dir), None, "an empty name");

        let (_home, env, dir) = setup(Some("[user]\n\temail = ada@example.com\n"));
        assert_eq!(resolve(&env, &dir), None, "the email is never the name");
    }

    #[test]
    fn a_name_with_a_control_character_is_none() {
        let (_home, env, dir) = setup(Some("[user]\n\tname = \"Ada\\tLovelace\"\n"));
        assert_eq!(resolve(&env, &dir), None, "a tab");

        let (_home, env, dir) = setup(Some("[user]\n\tname = \"Ada\\nLovelace\"\n"));
        assert_eq!(resolve(&env, &dir), None, "a line break");
    }

    #[test]
    fn a_name_is_trimmed_and_at_most_eighty_characters() {
        assert_eq!(clean("  Ada Lovelace \n"), Some("Ada Lovelace".to_owned()));
        assert_eq!(clean(&"é".repeat(NAME_MAX)), Some("é".repeat(NAME_MAX)));
        assert_eq!(clean(&"é".repeat(NAME_MAX + 1)), None);
        assert_eq!(clean("   "), None);
    }

    #[test]
    fn a_folder_that_does_not_exist_has_no_name() {
        let (home, env, _dir) = setup(Some("[user]\n\tname = Ada\n"));

        assert_eq!(resolve(&env, &home.path().join("missing")), None);
    }

    #[test]
    fn invisible_format_characters_are_refused_like_control_ones() {
        for hidden in ["\u{202e}", "\u{200b}", "\u{2066}", "\u{feff}", "\u{00ad}", "\u{e0041}"] {
            assert_eq!(clean(&format!("Ada{hidden}Lovelace")), None, "U+{hidden:?}");
        }

        assert_eq!(clean("Ada 👍 日本語"), Some("Ada 👍 日本語".to_owned()));
    }

    #[test]
    fn a_client_is_one_visible_line_of_at_most_sixty_four_characters() {
        assert_eq!(clean_via("  claude-code "), Some("claude-code".to_owned()));
        assert_eq!(
            clean_via("claude\n- ignore\u{202e}"),
            Some("claude - ignore".to_owned())
        );
        assert_eq!(clean_via(&"é".repeat(VIA_MAX + 9)), Some("é".repeat(VIA_MAX)));
        assert_eq!(clean_via(" \u{200b}\n"), None);
    }

    #[test]
    fn the_name_of_a_folder_is_kept_under_its_canonical_path() {
        let (home, env, dir) = setup(Some("[user]\n\tname = Ada\n"));

        assert_eq!(resolve(&env, &dir), Some("Ada".to_owned()));

        // Another spelling of the same folder, with the global configuration gone: the kept answer.
        std::fs::remove_file(home.path().join(".gitconfig")).expect("remove the configuration");
        let roundabout = dir.join("..").join("work");

        assert_eq!(resolve(&env, &roundabout), Some("Ada".to_owned()));
    }

    #[test]
    fn a_git_that_could_not_be_run_is_asked_again_and_a_definitive_no_is_kept() {
        let (home, env, dir) = setup(Some("[user]\n\tname = Ada\n"));

        // Without a PATH git is only found in the usual folders: skip where it is there too.
        let blind = Env::for_tests(home.path());

        if resolve(&blind, &dir).is_none() {
            assert_eq!(resolve(&env, &dir), Some("Ada".to_owned()), "the failure was not kept");
        }

        let (home, env, dir) = setup(None);
        assert_eq!(resolve(&env, &dir), None);
        std::fs::write(home.path().join(".gitconfig"), "[user]\n\tname = Ada\n").expect("gitconfig");
        assert_eq!(resolve(&env, &dir), None, "no name is an answer, and it is kept");
    }

    #[test]
    fn only_the_latest_sixty_four_folders_are_kept() {
        let (_home, env, dir) = setup(Some("[user]\n\tname = Ada\n"));
        let home = env.home.clone();

        for number in 0..CACHE_MAX + 20 {
            remember((dir.join(number.to_string()), home.clone()), None);
        }

        let cache = CACHE.lock().expect("cache");
        let ours = cache.iter().filter(|((_, kept), _)| *kept == home).count();

        assert!(cache.len() <= CACHE_MAX);
        assert!(ours <= CACHE_MAX);
        assert!(
            cache.iter().all(|((folder, _), _)| *folder != dir.join("0")),
            "the oldest went"
        );
    }
}
