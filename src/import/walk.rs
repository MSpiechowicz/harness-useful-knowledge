//! Finds the files an import takes: the manifest's paths and the documents inside its folders.
//!
//! A folder is walked 16 levels deep. Symbolic links are never followed (neither files nor folders
//! inside it), hidden entries (a leading dot, `.git` among them), `node_modules`, `target` and
//! Confluence's `attachments` are not entered, and a file whose extension is not a documentation
//! format is passed over without a word (a folder of source code is not 5000 skipped files).
//!
//! A document's source key is its path inside the chosen folder, led by the folder's own name
//! (`docs/guide/install.md`), or for a chosen file its folder's name and its name, and led by the
//! scope of the import: the first linked repository (`git:github.com/o/r/docs/guide/install.md`),
//! else `global`. Two same-named folders of different repositories so keep their own documents.
//! It never holds the absolute path: a team database is shared between computers, and re-importing
//! the same folder from another place must find the same documents.

use std::fs;
use std::path::{Path, PathBuf};

use super::manifest::Manifest;
use crate::digest::sha256_hex;
use crate::extract::format_of_name;
use crate::limits::{FILES_PER_IMPORT_MAX, WALK_DEPTH_MAX};

/// Folders an import never enters.
const SKIPPED_FOLDERS: [&str; 3] = ["node_modules", "target", "attachments"];

/// Why an entry of the manifest gives no file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    Skipped(String),
    Failed(String),
}

/// One file to import, or why it cannot be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    /// The path as events show it.
    pub display: String,
    pub name: String,
    pub source_key: String,
    pub problem: Option<Problem>,
}

/// Everything the manifest names, in order: its paths, then its folders.
pub fn plan(manifest: &Manifest) -> Result<Vec<Candidate>, String> {
    let mut found = Vec::new();

    for raw in &manifest.paths {
        found.push(file(Path::new(raw)));
        check_count(found.len())?;
    }

    for raw in &manifest.folders {
        folder(Path::new(raw), &mut found)?;
    }

    let scope = key_scope(manifest);

    for candidate in found.iter_mut().filter(|candidate| candidate.problem.is_none()) {
        candidate.source_key = format!("{scope}/{}", candidate.source_key);
    }

    Ok(found)
}

/// What leads every source key of the import: the first linked repository, else `global`. A
/// `path:` repository key holds an absolute path, so only its hash is used.
fn key_scope(manifest: &Manifest) -> String {
    match manifest.links.first() {
        Some(link) if link.repo_key.starts_with("git:") => link.repo_key.clone(),
        Some(link) => format!("path-{}", &sha256_hex(link.repo_key.as_bytes())[..12]),
        None => "global".to_owned(),
    }
}

fn check_count(count: usize) -> Result<(), String> {
    if count > FILES_PER_IMPORT_MAX {
        return Err(format!(
            "an import takes at most {FILES_PER_IMPORT_MAX} files, choose fewer or smaller folders"
        ));
    }

    Ok(())
}

fn problem(path: &Path, problem: Problem) -> Candidate {
    Candidate {
        path: path.to_path_buf(),
        display: path.to_string_lossy().into_owned(),
        name: file_name(path),
        source_key: String::new(),
        problem: Some(problem),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The folder's name for a source key; empty for a filesystem root.
fn folder_name(path: &Path) -> String {
    file_name(path)
}

/// A file the manifest names.
fn file(path: &Path) -> Candidate {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => {
            return problem(
                path,
                Problem::Failed("the file does not exist or cannot be read".to_owned()),
            );
        }
    };

    if metadata.file_type().is_symlink() {
        return problem(path, Problem::Skipped("symbolic links are not followed".to_owned()));
    }

    if metadata.is_dir() {
        return problem(path, Problem::Failed("this is a folder, not a file".to_owned()));
    }

    if !metadata.is_file() {
        return problem(path, Problem::Skipped("not a regular file".to_owned()));
    }

    let name = file_name(path);
    let parent = path.parent().map(folder_name).unwrap_or_default();

    Candidate {
        path: path.to_path_buf(),
        display: path.to_string_lossy().into_owned(),
        source_key: key(&parent, &name),
        name,
        problem: None,
    }
}

fn key(folder: &str, relative: &str) -> String {
    if folder.is_empty() {
        relative.to_owned()
    } else {
        format!("{folder}/{relative}")
    }
}

/// A folder the manifest names: its documents are added to `found`.
fn folder(path: &Path, found: &mut Vec<Candidate>) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => {
            found.push(problem(
                path,
                Problem::Failed("the folder does not exist or cannot be read".to_owned()),
            ));
            return Ok(());
        }
    };

    if metadata.file_type().is_symlink() {
        found.push(problem(
            path,
            Problem::Skipped("symbolic links are not followed".to_owned()),
        ));
        return Ok(());
    }

    if !metadata.is_dir() {
        found.push(problem(
            path,
            Problem::Failed("this is a file, not a folder".to_owned()),
        ));
        return Ok(());
    }

    let root = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = folder_name(&root);

    walk(&root, &name, 0, found)
}

fn walk(dir: &Path, relative: &str, depth: usize, found: &mut Vec<Candidate>) -> Result<(), String> {
    let mut entries: Vec<_> = match fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(std::result::Result::ok).collect(),
        Err(_) => {
            found.push(problem(dir, Problem::Failed("the folder cannot be read".to_owned())));
            return Ok(());
        }
    };

    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();

        if name.starts_with('.') || SKIPPED_FOLDERS.contains(&name.as_str()) {
            continue;
        }

        // `file_type` does not follow a symbolic link: a link is neither a file nor a folder here.
        let Ok(kind) = entry.file_type() else {
            continue;
        };

        if kind.is_dir() && depth < WALK_DEPTH_MAX {
            walk(&entry.path(), &key(relative, &name), depth + 1, found)?;
        } else if kind.is_file() && format_of_name(&name).is_some() {
            found.push(Candidate {
                path: entry.path(),
                display: entry.path().to_string_lossy().into_owned(),
                source_key: key(relative, &name),
                name,
                problem: None,
            });
            check_count(found.len())?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().expect("parent")).expect("folders");
        fs::write(path, "# text\n\nbody").expect("write");
    }

    fn keys(candidates: &[Candidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|candidate| candidate.source_key.clone())
            .collect()
    }

    fn folder_manifest(root: &Path) -> Manifest {
        Manifest {
            folders: vec![root.to_string_lossy().into_owned()],
            ..Manifest::default()
        }
    }

    #[test]
    fn a_folder_walk_skips_what_is_not_documentation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("docs");

        for path in [
            "b.md",
            "a/guide.md",
            "a/notes.TXT",
            "a/deep/spec.yaml",
            ".git/config.md",
            "node_modules/pkg/README.md",
            "target/doc/index.html",
            ".hidden/secret.md",
            ".dotfile.md",
            "attachments/1/scan.pdf",
            "src/main.rs",
            "image.png",
        ] {
            touch(&root.join(path));
        }

        let found = plan(&folder_manifest(&root)).expect("plan");

        assert_eq!(
            keys(&found),
            [
                "global/docs/a/deep/spec.yaml",
                "global/docs/a/guide.md",
                "global/docs/a/notes.TXT",
                "global/docs/b.md"
            ]
        );
        assert!(found.iter().all(|candidate| candidate.problem.is_none()));
        assert_eq!(found[3].name, "b.md");
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_never_followed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let outside = dir.path().join("outside");
        let root = dir.path().join("docs");
        touch(&outside.join("secret.md"));
        touch(&root.join("real.md"));

        std::os::unix::fs::symlink(&outside, root.join("linked-folder")).expect("folder link");
        std::os::unix::fs::symlink(outside.join("secret.md"), root.join("linked-file.md")).expect("file link");

        let found = plan(&folder_manifest(&root)).expect("plan");
        assert_eq!(keys(&found), ["global/docs/real.md"]);

        // A link given as a path or as the folder itself is reported, not followed.
        let manifest = Manifest {
            paths: vec![root.join("linked-file.md").to_string_lossy().into_owned()],
            folders: vec![root.join("linked-folder").to_string_lossy().into_owned()],
            ..Manifest::default()
        };
        let found = plan(&manifest).expect("plan");

        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|candidate| matches!(
            &candidate.problem,
            Some(Problem::Skipped(reason)) if reason.contains("symbolic")
        )));
    }

    #[test]
    fn the_walk_stops_at_sixteen_levels() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("docs");
        let mut nested = root.clone();

        touch(&root.join("level0.md"));

        for level in 1..=18 {
            nested = nested.join(format!("d{level}"));
            touch(&nested.join(format!("level{level}.md")));
        }

        let found = plan(&folder_manifest(&root)).expect("plan");
        let names: Vec<&str> = found.iter().map(|candidate| candidate.name.as_str()).collect();

        assert!(names.contains(&"level16.md"));
        assert!(!names.contains(&"level17.md"));
        assert_eq!(names.len(), 17);
    }

    #[test]
    fn a_chosen_file_is_keyed_by_its_folder_and_name_never_its_absolute_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("handbook").join("start.md");
        touch(&path);

        let manifest = Manifest {
            paths: vec![path.to_string_lossy().into_owned()],
            ..Manifest::default()
        };
        let found = plan(&manifest).expect("plan");

        assert_eq!(keys(&found), ["global/handbook/start.md"]);
        assert_eq!(found[0].display, path.to_string_lossy());
    }

    #[test]
    fn same_named_folders_of_different_repositories_get_different_keys() {
        let dir = tempfile::tempdir().expect("temp dir");
        let a = dir.path().join("a").join("docs");
        let b = dir.path().join("b").join("docs");
        touch(&a.join("guide.md"));
        touch(&b.join("guide.md"));

        let linked = |root: &Path, repo: &str| Manifest {
            links: vec![crate::store::Link {
                repo_key: repo.to_owned(),
                globs: Vec::new(),
            }],
            ..folder_manifest(root)
        };

        let first = keys(&plan(&linked(&a, "git:github.com/o/a")).expect("plan"));
        let second = keys(&plan(&linked(&b, "git:github.com/o/b")).expect("plan"));
        let local = keys(&plan(&linked(&b, "path:/home/me/project")).expect("plan"));

        assert_eq!(first, ["git:github.com/o/a/docs/guide.md"]);
        assert_eq!(second, ["git:github.com/o/b/docs/guide.md"]);
        assert!(
            local[0].starts_with("path-") && local[0].ends_with("/docs/guide.md"),
            "{local:?}"
        );
        assert!(!local[0].contains("/home"), "no absolute path: {local:?}");
    }

    #[test]
    fn more_than_five_thousand_files_are_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("docs");
        fs::create_dir_all(&root).expect("folder");

        for n in 0..=FILES_PER_IMPORT_MAX {
            fs::write(root.join(format!("f{n:05}.md")), "x").expect("write");
        }

        let error = plan(&folder_manifest(&root)).expect_err("too many");
        assert!(error.contains("5000"), "{error}");

        fs::remove_file(root.join(format!("f{FILES_PER_IMPORT_MAX:05}.md"))).expect("remove");
        assert_eq!(
            plan(&folder_manifest(&root)).expect("exactly the limit").len(),
            FILES_PER_IMPORT_MAX
        );
    }
}
