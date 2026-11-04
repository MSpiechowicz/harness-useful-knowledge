//! `docs download --json ID --to PATH [--live]`: writes the original file of a document.
//!
//! Prints `{"path", "original"}`. The file is written `0600` through a temporary file renamed into
//! place, and a symbolic link (or anything that is not a regular file) at `PATH` is refused. The
//! original is the file as it was imported, kept next to the document (see
//! [`crate::store::KnowledgeStore::file`]). A document that has none (it was imported before
//! originals were kept, or its text was changed since) writes its stored text instead, as Markdown
//! (`.md`) or, for plain text and PDF, as text (`.txt`): `original` is `false` and `path` ends in that
//! extension (`PATH` with its extension replaced, unless it is `.md` or `.txt` already). A file that
//! exists at that other path is refused (`refused`), never replaced: only `PATH` was confirmed.
//!
//! `ID` is a row: a pending version has a file of its own, and a live row that has a pending version
//! downloads the pending version's, `--live` the live document's instead (for a pending row, the
//! live document it replaces).

use std::path::{Path, PathBuf};

use serde_json::json;

use super::review::{missing, open_to_read, refused};
use super::{Context, Done, Failure, Reply, the_id};
use crate::cli::Args;
use crate::store::{Detail, KnowledgeStore, SectionText};
use crate::support::private::write_atomic;

/// The prefix of the temporary file next to the one that is written.
const TEMP_PREFIX: &str = ".harness-knowledge-download-";

/// `docs download`.
pub(super) fn download(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json", "--live"], &["--to"]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let id = the_id(&parsed)?;
    let to = parsed
        .value("--to")
        .filter(|to| !to.trim().is_empty())
        .ok_or(Failure::Usage)?;
    let mut store = open_to_read(context, id)?;

    download_on(store.as_mut(), id, Path::new(to), parsed.flag("--live"))
}

/// The row whose file is written: see the module docs.
fn chosen(store: &mut dyn KnowledgeStore, id: i64, live: bool) -> Result<i64, Failure> {
    let detail = Detail {
        drafts: true,
        ..Detail::default()
    };
    let found = store.get(id, &detail)?.ok_or_else(|| missing(id))?;

    if let Some(live_id) = found.doc.pending_of {
        return Ok(if live { live_id } else { id });
    }

    if !found.doc.status.is_live() || live {
        return Ok(id);
    }

    Ok(match found.pending {
        Some(pending) => pending.id,
        None => store.pending_of(id)?.map_or(id, |pending| pending.id),
    })
}

/// Writes the file of row `id` of `store` to `to`.
pub(super) fn download_on(store: &mut dyn KnowledgeStore, id: i64, to: &Path, live: bool) -> Reply {
    let row = chosen(store, id, live)?;

    if let Some(file) = store.file(row)? {
        write_file(to, &file.bytes)?;

        return Ok(Done::ok(json!({ "path": to.to_string_lossy(), "original": true })));
    }

    let detail = Detail {
        drafts: true,
        ..Detail::default()
    };
    let found = store.get(row, &detail)?.ok_or_else(|| missing(row))?;
    let sections = store.section_texts(row)?.ok_or_else(|| missing(row))?;
    let plain = matches!(found.doc.format.as_str(), "text" | "pdf");
    let text = stored_text(&found.doc.title, &sections, plain);
    let path = with_extension(to, if plain { "txt" } else { "md" });

    // The person confirmed `to`, not the name this made of it: a file that is there is not replaced.
    if path != to && std::fs::symlink_metadata(&path).is_ok() {
        return Err(refused(
            "refused",
            &format!(
                "there is no original to write, so the text would go to {}, which exists already: it is not overwritten",
                path.to_string_lossy()
            ),
        ));
    }

    write_file(&path, text.as_bytes())?;

    Ok(Done::ok(json!({ "path": path.to_string_lossy(), "original": false })))
}

/// `path` as a `.md` or `.txt` file: the extension of `extension`, kept when it is one of the two.
fn with_extension(path: &Path, extension: &str) -> PathBuf {
    let kept = path
        .extension()
        .and_then(|own| own.to_str())
        .is_some_and(|own| own.eq_ignore_ascii_case("md") || own.eq_ignore_ascii_case("txt"));

    if kept {
        path.to_path_buf()
    } else {
        path.with_extension(extension)
    }
}

/// The text of a document from its sections: the title, then each section under its heading
/// (`#` marks for each level below the title unless `plain`), separated by blank lines.
fn stored_text(title: &str, sections: &[SectionText], plain: bool) -> String {
    let mut parts: Vec<String> = Vec::new();

    parts.push(if plain { title.to_owned() } else { format!("# {title}") });

    for section in sections {
        if !section.heading.is_empty() {
            let levels = section.heading.split(" > ").count();
            let last = section.heading.rsplit(" > ").next().unwrap_or_default();

            parts.push(if plain {
                last.to_owned()
            } else {
                format!("{} {last}", "#".repeat((levels + 1).min(6)))
            });
        }

        if !section.text.is_empty() {
            parts.push(section.text.clone());
        }
    }

    let mut text = parts.join("\n\n");
    text.push('\n');

    text
}

/// Writes `bytes` to `path` (`0600`), refusing a symbolic link or a thing that is not a file there.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(refused(
                "refused",
                "the file to write is a symbolic link or not a regular file: it is not overwritten",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Failure::Failed(format!("the file to write cannot be read: {error}"))),
    }

    let folder = match path.parent() {
        Some(folder) if !folder.as_os_str().is_empty() => folder,
        _ => Path::new("."),
    };

    write_atomic(folder, path, bytes, TEMP_PREFIX, "downloaded file").map_err(Failure::Failed)
}
