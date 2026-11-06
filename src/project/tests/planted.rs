//! A tracked folder shaped like a bare repository (`HEAD`, `objects/`, `refs/`, `config`) is found
//! by git as a repository whose `core.worktree` and `remote.origin.url` the folder's author picks,
//! and a file the repository configuration includes is one a pull can change. The project of such
//! a folder must not be another repository, and the layouts of genuine repositories keep the keys
//! they had, since documents are stored by key.

#![cfg(unix)]

use pretty_assertions::assert_eq;

use super::*;

/// The remote of the repository that must not be borrowed.
const OFF_URL: &str = "https://example.invalid/acme/off.git";

/// The key every real layout below gets.
const KEY: &str = "git:example.invalid/acme/widget";
const URL: &str = "https://example.invalid/acme/widget.git";

fn git_here(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "protocol.file.allow=always"])
        .args(["-c", "user.name=Tests", "-c", "user.email=tests@example.com"])
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

/// A repository `path` with one commit and, when given, an `origin`.
fn repository(path: &Path, remote: Option<&str>) -> PathBuf {
    std::fs::create_dir_all(path).expect("mkdir");
    git_here(path, &["init", "-q", "-b", "main"]);
    git_here(path, &["commit", "-q", "--allow-empty", "-m", "first"]);

    if let Some(remote) = remote {
        git_here(path, &["remote", "add", "origin", remote]);
    }

    canonical(path)
}

/// Makes `dir` look like a bare repository whose configuration says the work tree is `worktree`
/// (relative to `dir`) and whose `origin` is `remote`.
fn plant(dir: &Path, worktree: &str, remote: Option<&str>) {
    for folder in ["objects/keep", "refs/keep"] {
        std::fs::create_dir_all(dir.join(folder)).expect("mkdir");
        std::fs::write(dir.join(folder).join("x"), "").expect("keep");
    }

    std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");

    let mut config = format!("[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tworktree = {worktree}\n");

    if let Some(remote) = remote {
        config.push_str(&format!("[remote \"origin\"]\n\turl = {remote}\n"));
    }

    std::fs::write(dir.join("config"), config).expect("config");
}

/// The key and the root of `folder` for a user whose home is `home`.
fn identity(home: &Path, folder: &Path) -> (String, PathBuf) {
    let project = ProjectResolver::new(env(home), None)
        .resolve(Some(folder.to_str().expect("utf-8")))
        .expect("resolves")
        .expect("a project");

    (project.key, project.root)
}

/// The path key of `folder`, which must have no remote.
fn path_key(folder: &Path) -> String {
    format!("path:{}", canonical(folder).display())
}

fn home_in(base: &Path) -> PathBuf {
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("home");
    home
}

#[test]
fn a_folder_shaped_like_a_bare_repository_is_not_another_repository() {
    for (label, worktree) in [("dot", "."), ("same-folder", "../sub")] {
        let dir = TempDir::new().expect("temp");
        let base = canonical(dir.path());
        let entry = base.join("U").join("sub");
        std::fs::create_dir_all(&entry).expect("mkdir");
        let entry = canonical(&entry);
        plant(&entry, worktree, Some(OFF_URL));

        assert_eq!(
            identity(&home_in(&base), &entry),
            (path_key(&entry), entry.clone()),
            "{label}"
        );
    }
}

#[test]
fn a_shaped_folder_below_a_repository_cannot_borrow_the_key_of_its_parent() {
    let dir = TempDir::new().expect("temp");
    let base = canonical(dir.path());
    let off = repository(&base.join("off"), Some(OFF_URL));

    // The work tree `..` of `off/L/fake` is the folder `off/L`, which is part of `off`.
    let fake = off.join("L").join("fake");
    std::fs::create_dir_all(&fake).expect("mkdir");
    plant(&fake, "..", None);
    assert_eq!(identity(&home_in(&base), &fake), (path_key(&fake), canonical(&fake)));

    // The same folder with a remote of its own.
    let vendored = off.join("vendor").join("fake");
    std::fs::create_dir_all(&vendored).expect("mkdir");
    plant(&vendored, "..", Some(OFF_URL));
    assert_eq!(
        identity(&home_in(&base), &vendored),
        (path_key(&vendored), canonical(&vendored))
    );
}

#[test]
fn a_tracked_folder_that_replaced_a_submodule_cannot_reach_another_repository_in_a_fresh_clone() {
    let dir = TempDir::new().expect("temp");
    let base = canonical(dir.path());
    let source = repository(&base.join("source"), None);
    let up = repository(&base.join("up"), None);
    git_here(&up, &["submodule", "add", "-q", source.to_str().expect("text"), "sub"]);
    git_here(&up, &["commit", "-q", "-m", "add the submodule"]);

    // Upstream swaps the submodule for a tracked folder shaped like a repository.
    git_here(&up, &["rm", "-q", "-f", "sub"]);
    let _ = std::fs::remove_dir_all(up.join("sub"));
    std::fs::create_dir_all(up.join("sub")).expect("mkdir");
    plant(&up.join("sub"), ".", Some(OFF_URL));
    std::fs::write(up.join("sub").join("README"), "vendored").expect("readme");
    git_here(&up, &["add", "-A", "-f"]);
    git_here(&up, &["commit", "-q", "-m", "vendor the library"]);

    git_here(&base, &["clone", "-q", up.to_str().expect("text"), "U"]);
    let entry = canonical(&base.join("U").join("sub"));
    assert!(entry.join("config").is_file(), "the clone holds the shaped folder");

    assert_eq!(identity(&home_in(&base), &entry), (path_key(&entry), entry.clone()));
}

#[test]
fn the_layouts_of_real_repositories_keep_their_keys_and_a_submodule_is_keyed_by_its_path() {
    let dir = TempDir::new().expect("temp");
    let base = canonical(dir.path());
    let home = home_in(&base);

    // A plain checkout, and a folder inside it.
    let plain = repository(&base.join("plain"), Some(URL));
    std::fs::create_dir_all(plain.join("src/deep")).expect("mkdir");
    assert_eq!(
        identity(&home, &plain.join("src/deep")),
        (KEY.to_owned(), plain.clone())
    );

    // A linked worktree: its `.git` is a file.
    git_here(&plain, &["worktree", "add", "-q", "../linked"]);
    let linked = canonical(&base.join("linked"));
    assert!(linked.join(".git").is_file());
    assert_eq!(identity(&home, &linked), (KEY.to_owned(), linked.clone()));

    // A real submodule: a `.git` file into the superproject's `modules`.
    let source = repository(&base.join("source"), None);
    let up = repository(&base.join("up"), None);
    git_here(&up, &["submodule", "add", "-q", source.to_str().expect("text"), "sub"]);
    git_here(&up, &["commit", "-q", "-m", "add the submodule"]);
    git_here(
        &base,
        &["clone", "-q", "--recurse-submodules", up.to_str().expect("text"), "U"],
    );
    let submodule = canonical(&base.join("U").join("sub"));
    git_here(&submodule, &["remote", "set-url", "origin", URL]);
    assert!(submodule.join(".git").is_file());
    // Its remote is the superproject's to say, so a submodule is keyed by its path, as in Harness Memory.
    assert_eq!(identity(&home, &submodule), (path_key(&submodule), submodule.clone()));

    let module_worktree = base.join("sub-linked");
    git_here(
        &submodule,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            module_worktree.to_str().expect("text"),
        ],
    );
    let module_worktree = canonical(&module_worktree);
    assert_eq!(
        identity(&home, &module_worktree),
        (path_key(&module_worktree), module_worktree.clone())
    );

    // A checkout whose git folder lives elsewhere (`--separate-git-dir`).
    let separate = base.join("separate");
    std::fs::create_dir_all(&separate).expect("mkdir");
    git_here(
        &separate,
        &["init", "-q", "-b", "main", "--separate-git-dir", "../separate.git"],
    );
    git_here(&separate, &["remote", "add", "origin", URL]);
    let separate = canonical(&separate);
    assert!(separate.join(".git").is_file());
    assert_eq!(identity(&home, &separate), (KEY.to_owned(), separate.clone()));

    // A linked worktree of a bare repository.
    let bare = base.join("hub.git");
    git_here(
        &base,
        &["clone", "-q", "--bare", plain.to_str().expect("text"), "hub.git"],
    );
    git_here(&bare, &["remote", "set-url", "origin", URL]);
    git_here(&bare, &["worktree", "add", "-q", "../hubwork", "main"]);
    let hubwork = canonical(&base.join("hubwork"));
    assert_eq!(identity(&home, &hubwork), (KEY.to_owned(), hubwork.clone()));
}

#[test]
fn a_genuine_bare_repository_is_a_project_by_its_path_never_by_its_remote() {
    let dir = TempDir::new().expect("temp");
    let base = canonical(dir.path());
    let home = home_in(&base);

    let origin = repository(&base.join("origin"), None);
    git_here(
        &base,
        &["clone", "-q", "--bare", origin.to_str().expect("text"), "hub.git"],
    );
    let bare = canonical(&base.join("hub.git"));
    git_here(&bare, &["remote", "set-url", "origin", URL]);

    for folder in [bare.clone(), bare.join("refs")] {
        assert_eq!(identity(&home, &folder), (path_key(&folder), folder.clone()));
    }
}

#[test]
fn an_included_configuration_file_cannot_change_the_key() {
    let dir = TempDir::new().expect("temp");
    let base = canonical(dir.path());
    let home = home_in(&base);

    // A tracked file (it arrives with a pull) that names another remote, included from `.git/config`.
    let planted = base.join("tracked.cfg");
    std::fs::write(&planted, format!("[remote \"origin\"]\n\turl = {OFF_URL}\n")).expect("file");
    let include = |repo: &Path| {
        git_here(
            repo,
            &["config", "--local", "include.path", planted.to_str().expect("text")],
        )
    };

    // The remote of the repository wins, and a repository with none stays keyed by its path.
    let plain = repository(&base.join("plain"), Some(URL));
    include(&plain);
    assert_eq!(identity(&home, &plain), (KEY.to_owned(), plain.clone()));

    let none = repository(&base.join("none"), None);
    include(&none);
    assert_eq!(identity(&home, &none), (path_key(&none), none.clone()));

    // A linked worktree reads the configuration shared with its repository.
    git_here(&plain, &["worktree", "add", "-q", "../linked"]);
    let linked = canonical(&base.join("linked"));
    assert_eq!(identity(&home, &linked), (KEY.to_owned(), linked));
}

#[test]
fn a_remote_whose_authority_git_and_this_parser_could_read_differently_keys_the_folder_by_its_path() {
    let dir = TempDir::new().expect("temp");
    let base = canonical(dir.path());
    let home = home_in(&base);

    let urls = [
        "http://127.0.0.1:9?@example.invalid/acme/off",
        "http://127.0.0.1:9#@example.invalid/acme/off",
        "http://127.0.0.1:9\\@example.invalid/acme/off",
    ];

    for (index, url) in urls.into_iter().enumerate() {
        let repo = repository(&base.join(format!("odd{index}")), Some(url));
        assert_eq!(identity(&home, &repo), (path_key(&repo), repo.clone()), "{url}");
    }
}

#[test]
fn a_leftover_submodule_is_keyed_by_its_path_when_git_finds_no_superproject() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = canonical(dir.path());
    let home = home_in(&base);

    let source = repository(&base.join("source"), None);
    let up = repository(&base.join("up"), None);
    git_here(&up, &["submodule", "add", "-q", source.to_str().expect("text"), "sub"]);
    git_here(&up, &["commit", "-q", "-m", "add the submodule"]);
    git_here(
        &up,
        &["config", "-f", ".git/modules/sub/config", "remote.origin.url", URL],
    );
    git_here(&up, &["rm", "-q", "--cached", "sub"]);
    git_here(&up, &["commit", "-q", "-m", "drop the gitlink"]);

    let leftover = canonical(&up.join("sub"));
    assert!(leftover.join(".git").is_file());
    assert_eq!(identity(&home, &leftover), (path_key(&leftover), leftover.clone()));
}
