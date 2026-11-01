//! `knowledge_add`: an agent's proposal for a document. It never writes the database: the proposal
//! is snapshotted into the review queue ([`crate::held`]) and waits for the person to approve it.
//!
//! The text is the agent's `content`, or the text of a file named by `path`, which must be inside
//! the project, a regular file reached without a symbolic link and not hidden (no dot in the name of
//! the file or of a folder on the way), at most [`CONTENT_MAX`] bytes of UTF-8. A text that holds a
//! credential is refused.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use super::scope::repository;
use super::{CONTENT_MAX, Server};
use crate::extract::{self, Format};
use crate::held::{HeldItem, HoldError, Proposal, Source};
use crate::project::ProjectKey;
use crate::store::{Link, clean};
use crate::support::redact::has_credentials;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AddArgs {
    title: String,
    content: Option<String>,
    path: Option<String>,
    format: Option<String>,
    project_path: Option<String>,
    links: Option<Vec<LinkArg>>,
    replaces: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkArg {
    repo: Option<String>,
    globs: Option<Vec<String>>,
}

/// Said to the agent when the proposal is held.
const SENT: &str = "Sent to review in Harness Desktop → Knowledge.";

/// The formats an agent may name, with the extension a text of that format is imported under.
const FORMATS: [(&str, &str); 6] = [
    ("markdown", "md"),
    ("text", "txt"),
    ("html", "html"),
    ("asciidoc", "adoc"),
    ("rst", "rst"),
    ("openapi", "yaml"),
];

pub(super) fn add(server: &mut Server, args: AddArgs) -> Result<String, String> {
    let title = clean::title(&args.title).map_err(|error| error.to_string())?;

    if args.replaces.is_some_and(|id| id < 1) {
        return Err("replaces must be the id of a document".to_owned());
    }

    let repo = repository(server, args.project_path.as_deref())?;
    let links = links(args.links, repo.as_ref())?;

    let (content, source_path, name, format) = match (args.content, args.path) {
        (Some(content), None) => {
            let format = named_format(args.format.as_deref())?.unwrap_or("markdown");
            let name = file_name(&title, extension(format));

            (content, None, name, format.to_owned())
        }
        (None, Some(path)) => {
            let root = project_root(server, args.project_path.as_deref(), repo.as_ref())?;
            let (content, shown, file) = read_project_file(&root, &path)?;
            let format = match named_format(args.format.as_deref())? {
                Some(format) => format.to_owned(),
                None => format_of_file(&file, &content)?,
            };

            (content, Some(shown), file, format)
        }
        _ => return Err("give either content or path, not both and not neither".to_owned()),
    };

    check_text(&content)?;

    let Some(held) = &server.held else {
        return Err("the review queue cannot be reached: the data folder is unknown".to_owned());
    };

    let item = HeldItem::new(
        Source {
            client: server.client.clone().unwrap_or_else(|| "unknown".to_owned()),
            session: server.session.clone(),
            project_path: args
                .project_path
                .map(|path| path.trim().to_owned())
                .filter(|path| !path.is_empty()),
            repo_key: repo.map(|repo| repo.key),
        },
        Proposal {
            title,
            format,
            name,
            source_path,
            content,
            links,
            replaces: args.replaces,
        },
    );

    match held.hold(item) {
        Ok(id) => Ok(format!("held:{id}\n{SENT}")),
        Err(HoldError::Limit) => Err(
            "the review queue is full or this document is too large. Ask the user to review the waiting proposals in \
             Harness Desktop → Knowledge"
                .to_owned(),
        ),
        Err(HoldError::Failed(reason)) => Err(format!("the proposal could not be held: {reason}")),
    }
}

/// The links of the proposal: the given ones (a link without a repository is the project's), else
/// the project's repository, none when it has no repository (a global document).
fn links(given: Option<Vec<LinkArg>>, repo: Option<&ProjectKey>) -> Result<Vec<Link>, String> {
    let own = repo.filter(|repo| !repo.is_every()).map(|repo| repo.key.clone());

    let links: Vec<Link> = match given {
        None => own
            .map(|repo_key| Link {
                repo_key,
                globs: Vec::new(),
            })
            .into_iter()
            .collect(),
        Some(given) => {
            let mut links = Vec::new();

            for link in given {
                let repo_key = match link.repo.or_else(|| own.clone()) {
                    Some(repo_key) => repo_key,
                    None => {
                        return Err(
                            "a link without repo needs a known repository: pass project_path or name the repo"
                                .to_owned(),
                        );
                    }
                };

                links.push(Link {
                    repo_key,
                    globs: link.globs.unwrap_or_default(),
                });
            }

            links
        }
    };

    clean::links(&links).map_err(|error| error.to_string())
}

/// The format an agent named, as one of [`FORMATS`]; `None` when it named none.
fn named_format(raw: Option<&str>) -> Result<Option<&'static str>, String> {
    let Some(raw) = raw
        .map(|raw| raw.trim().to_ascii_lowercase())
        .filter(|raw| !raw.is_empty())
    else {
        return Ok(None);
    };

    FORMATS
        .iter()
        .find(|(name, _)| *name == raw)
        .map(|(name, _)| Some(*name))
        .ok_or_else(|| "format must be markdown, text, html, asciidoc, rst or openapi".to_owned())
}

fn extension(format: &str) -> &'static str {
    FORMATS
        .iter()
        .find(|(name, _)| *name == format)
        .map_or("md", |(_, extension)| *extension)
}

/// The format of the file `name` that holds `content`: not a PDF, which is binary.
fn format_of_file(name: &str, content: &str) -> Result<String, String> {
    let head = &content.as_bytes()[..content.len().min(8192)];

    match extract::detect(name, head) {
        Some(Format::Pdf) => Err("a PDF cannot be proposed: ask the user to import it in Harness Desktop".to_owned()),
        Some(format) => Ok(format.as_str().to_owned()),
        None => Err("the file is not a text document".to_owned()),
    }
}

/// A file name for a document called `title`: lowercase ASCII words joined by dashes, with `extension`.
fn file_name(title: &str, extension: &str) -> String {
    let mut slug = String::new();

    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }

    let slug: String = slug.trim_end_matches('-').chars().take(60).collect();
    let slug = if slug.is_empty() { "document".to_owned() } else { slug };

    format!("{slug}.{extension}")
}

/// The text is text: not blank, no NUL, no credential.
fn check_text(content: &str) -> Result<(), String> {
    if content.trim().is_empty() {
        return Err("the document has no text".to_owned());
    }

    if content.len() > CONTENT_MAX {
        return Err(format!(
            "the document is larger than {} KiB: ask the user to import it in Harness Desktop",
            CONTENT_MAX / 1024
        ));
    }

    if content.contains('\0') {
        return Err("the document is binary, only text can be proposed".to_owned());
    }

    if has_credentials(content) {
        return Err("the text holds what looks like a credential: remove it and propose the document again".to_owned());
    }

    Ok(())
}

/// The canonical root a `path` must stay inside: the repository root, else the project folder.
fn project_root(server: &mut Server, project_path: Option<&str>, repo: Option<&ProjectKey>) -> Result<PathBuf, String> {
    let root = match repo.filter(|repo| !repo.is_every()) {
        Some(repo) => repo.root.clone(),
        None => {
            let Some(folder) = project_path.map(str::trim).filter(|folder| !folder.is_empty()) else {
                return Err("path needs project_path, the folder it is inside".to_owned());
            };

            crate::support::paths::validate_project_dir(&server.env, Path::new(folder))
                .map_err(|_| "project_path must be an existing folder that is not a filesystem root".to_owned())?
        }
    };

    super::folder::check(&server.env, &root)?;

    Ok(root)
}

/// The text of the file `path` inside `root`, with the path as shown (relative to `root`) and the
/// file's name. The path is resolved lexically against the root, and the file is read only when
/// resolving it through the file system gives the same path: a symbolic link anywhere on it, or a
/// `..` that leaves the root, is refused.
fn read_project_file(root: &Path, path: &str) -> Result<(String, String, String), String> {
    let outside = || "path must name a file inside the project".to_owned();
    let raw = Path::new(path.trim());
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };

    let mut lexical = PathBuf::new();

    for component in joined.components() {
        match component {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }

    let relative = lexical.strip_prefix(root).map_err(|_| outside())?.to_path_buf();

    if relative.as_os_str().is_empty() {
        return Err(outside());
    }

    let hidden = relative
        .components()
        .any(|component| component.as_os_str().to_string_lossy().starts_with('.'));

    if hidden {
        return Err("hidden files and folders (.env, .ssh, .git) are never proposed".to_owned());
    }

    let metadata = std::fs::symlink_metadata(&lexical).map_err(|_| "the file does not exist".to_owned())?;

    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("path must be a regular file, a symbolic link is not followed".to_owned());
    }

    let real = dunce::canonicalize(&lexical).map_err(|_| "the file cannot be read".to_owned())?;

    if real != lexical || !real.starts_with(root) {
        return Err("path must name a file inside the project, a symbolic link is not followed".to_owned());
    }

    if metadata.len() > CONTENT_MAX as u64 {
        return Err(format!(
            "the file is larger than {} KiB: ask the user to import it in Harness Desktop",
            CONTENT_MAX / 1024
        ));
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, nix::libc::O_NOFOLLOW);

    let file = options
        .open(&lexical)
        .map_err(|_| "the file cannot be read".to_owned())?;
    let mut bytes = Vec::new();
    file.take(CONTENT_MAX as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "the file cannot be read".to_owned())?;

    if bytes.len() > CONTENT_MAX {
        return Err(format!(
            "the file is larger than {} KiB: ask the user to import it in Harness Desktop",
            CONTENT_MAX / 1024
        ));
    }

    let content = String::from_utf8(bytes).map_err(|_| "the file is not UTF-8 text".to_owned())?;
    let name = lexical
        .file_name()
        .map(|name| clean::line(&name.to_string_lossy()))
        .filter(|name| !name.is_empty())
        .ok_or_else(outside)?;

    Ok((content, relative.to_string_lossy().replace('\\', "/"), name))
}
