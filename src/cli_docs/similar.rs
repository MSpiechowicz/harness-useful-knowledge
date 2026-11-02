//! `docs similar`: which stored documents the files at hand are probably versions of.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use super::{Context, Done, Failure, Reply, json as shown};
use crate::backend::Access;
use crate::cli::Args;
use crate::config::Place;
use crate::extract::pdf::{PdfPages, WorkerProcess};
use crate::import::manifest::Manifest;
use crate::import::similar::{self, Match};
use crate::import::walk::{self, Candidate};
use crate::limits::FILES_PER_IMPORT_MAX;
use crate::store::routed::ids;
use crate::store::{Link, TitleScope, clean};
use crate::support::time::rfc3339;
use crate::tokens::counter_for;

/// `docs similar --json [--repo KEY | --global] PATH...`: `{"similar": [{"path", "match"}]}`, one
/// entry per file in the order of the paths (a folder gives its files, as an import walks it). A
/// `match` is `null` or the stored document the file is probably a version of:
/// `{"id", "title", "place", "reason", "sharedChunks", "chunks", "addedBy", "updatedBy", "updatedAt"}`
/// (see [`similar::find`] for the `reason`s and [`shown::author`] for the authors). `chunks` counts
/// the file's distinct chunks, `sharedChunks` how many of them the document has. A file that cannot
/// be read has `"match": null` and an `"error"`. `--repo KEY` reads the files as a link to that
/// repository would, `--global` (the default) as an import without links. Read only.
pub(super) fn similar(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json", "--global"], &["--repo"]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json")
        || parsed.positional.is_empty()
        || parsed.positional.len() > FILES_PER_IMPORT_MAX
        || (parsed.flag("--global") && parsed.value("--repo").is_some())
    {
        return Err(Failure::Usage);
    }

    let (links, scope) = match parsed.value("--repo") {
        Some(key) => {
            let key = clean::repo_key(key).map_err(|_| Failure::Usage)?.to_owned();
            let link = Link {
                repo_key: key.clone(),
                globs: Vec::new(),
            };

            (vec![link], TitleScope::Repo(key))
        }
        None => (Vec::new(), TitleScope::Global),
    };

    let mut files = Vec::new();

    for raw in &parsed.positional {
        files.extend(expand(raw, &links)?);

        if files.len() > FILES_PER_IMPORT_MAX {
            return Err(too_many());
        }
    }

    let pdf = WorkerProcess::current_exe().map_err(Failure::Failed)?;
    let counter = counter_for(context.env);
    let mut store = context.open(Access::Read)?;
    let mut items = Vec::new();

    for file in &files {
        let probe = similar::probe(file, &pdf as &dyn PdfPages, counter.as_ref(), scope.clone());
        let item = match (probe, store.as_mut()) {
            (Err(reason), _) => json!({ "path": file.display, "match": null, "error": reason }),
            (Ok(_), None) => json!({ "path": file.display, "match": null }),
            (Ok(probe), Some(store)) => {
                let found = similar::find(store.as_mut(), &probe)?;

                json!({ "path": file.display, "match": found.map(|found| matched(context, &found)) })
            }
        };

        items.push(item);
    }

    let mut shown = json!({ "similar": items });

    if store.as_ref().is_some_and(|store| store.offline()) {
        shown["offline"] = json!(true);
    }

    Ok(Done::ok(shown))
}

fn too_many() -> Failure {
    Failure::Failed(format!(
        "at most {FILES_PER_IMPORT_MAX} files are compared at once, choose fewer or smaller folders"
    ))
}

/// The files `raw` names: itself, or a folder's documents as an import walks them.
fn expand(raw: &str, links: &[Link]) -> Result<Vec<Candidate>, Failure> {
    let is_folder = fs::symlink_metadata(Path::new(raw)).is_ok_and(|metadata| metadata.is_dir());
    let mut manifest = Manifest {
        links: links.to_vec(),
        ..Manifest::default()
    };

    if is_folder {
        manifest.folders.push(raw.to_owned());
    } else {
        manifest.paths.push(raw.to_owned());
    }

    walk::plan(&manifest).map_err(|_| too_many())
}

/// The `match` of a document found.
fn matched(context: &Context, found: &Match) -> Value {
    let doc = &found.doc;
    // Ids carry their place when both are in use, else the only place in use is the one.
    let place = ids::split(doc.id)
        .0
        .or_else(|| context.routing.places().first().copied())
        .unwrap_or(Place::Remote);

    json!({
        "id": ids::json(doc.id),
        "title": doc.title,
        "place": place.as_str(),
        "reason": found.reason.as_str(),
        "sharedChunks": found.shared,
        "chunks": found.chunks,
        "addedBy": shown::author(doc.added_by.as_ref()),
        "updatedBy": shown::author(doc.updated_by.as_ref()),
        "updatedAt": rfc3339(doc.updated_at),
    })
}
