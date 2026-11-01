//! `repos`, `stats` and `project-key`: what the database holds, and which repository a folder is.

use serde_json::{Value, json};

use super::{Context, Done, Failure, Reply};
use crate::backend::{self, Access, Attempt};
use crate::cli::Args;
use crate::config::{Layout, Place};
use crate::project::ProjectResolver;
use crate::store::sqlite::schema::SCHEMA_VERSION;
use crate::store::{RepoCounts, Stats, StoreError};

/// `repos --json`: `{"global": N, "repos": [{"key", "docs"}]}`, the repositories with documents,
/// most documents first.
pub fn repos(context: &Context, args: &[Option<&str>]) -> Reply {
    json_only(args)?;

    let (counts, offline) = match context.open(Access::Read)? {
        Some(mut store) => (store.repos()?, store.offline()),
        None => (RepoCounts::default(), false),
    };

    let repos: Vec<_> = counts
        .repos
        .iter()
        .map(|(key, docs)| json!({ "key": key, "docs": docs }))
        .collect();

    let mut shown = json!({ "global": counts.global, "repos": repos });

    if offline {
        shown["offline"] = json!(true);
    }

    Ok(Done::ok(shown))
}

/// `stats --json`: `documents`, `chunks`, `embeddedChunks`, `bytes`, `globalDocuments`,
/// `repositories`, `generation`, `schema` (the version a new file gets without a database) and `model` (the id
/// of the model that embedded the chunks, `null` before any was) and `held` (how many proposals wait in the
/// review queue, counted from its files, so it is right without a database). All counts are zero without a database.
///
/// `places` is `{"remote": {...}, "local": {...}}` with the same counts (not `held`) of each place in
/// use, and the top-level counts are those of the remote database, or of this computer when that is
/// the only place. A database is counted whatever the routing shows of it.
///
/// With both places in use, a remote database that cannot be reached (or is cooling down after it
/// could not be, see [`crate::store::Reach`]) is not counted: `places.remote` is `{"offline": true}`,
/// the top-level counts are this computer's, and `offline` is `true` at the top level too (it is
/// absent otherwise).
pub fn stats(context: &Context, args: &[Option<&str>]) -> Reply {
    json_only(args)?;

    let places = context.routing.places();
    let mut by_place = serde_json::Map::new();
    let mut primary: Option<Value> = None;
    let mut offline = false;

    for place in places {
        let Some(stats) = place_stats(context, place)? else {
            offline = true;
            by_place.insert(place.as_str().to_owned(), json!({ "offline": true }));
            continue;
        };

        let counts = counts(&stats);
        primary.get_or_insert_with(|| counts.clone());
        by_place.insert(place.as_str().to_owned(), counts);
    }

    let mut shown = primary.unwrap_or_else(|| {
        counts(&Stats {
            schema_version: SCHEMA_VERSION,
            ..Stats::default()
        })
    });
    shown["held"] = json!(crate::held::HeldStore::open(context.env).map_or(0, |held| held.count()));
    shown["places"] = Value::Object(by_place);

    if offline {
        shown["offline"] = json!(true);
    }

    Ok(Done::ok(shown))
}

/// The counts of one place, all zero for a database that does not exist; `None` for a remote
/// database that cannot be reached while this computer is in use too.
fn place_stats(context: &Context, place: Place) -> Result<Option<Stats>, Failure> {
    let empty = Stats {
        schema_version: SCHEMA_VERSION,
        ..Stats::default()
    };

    let store = if place == Place::Remote && context.routing.layout() == Layout::Both {
        match backend::attempt_remote(context.env, context.backend, Access::Read, true)? {
            Attempt::Opened(store) => store,
            Attempt::Offline(_) => return Ok(None),
        }
    } else {
        context.open_place(place, Access::Read)?
    };

    let Some(mut store) = store else {
        return Ok(Some(empty));
    };

    match store.stats() {
        Ok(stats) => Ok(Some(stats)),
        Err(StoreError::Unreachable(message)) if place == Place::Remote && context.routing.layout() == Layout::Both => {
            backend::reach_of(context.env, context.backend).failed(&message);
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

/// The counts of a database as `stats` prints them.
fn counts(stats: &Stats) -> Value {
    json!({
        "documents": stats.documents,
        "chunks": stats.chunks,
        "embeddedChunks": stats.embedded_chunks,
        "bytes": stats.bytes,
        "globalDocuments": stats.global_documents,
        "repositories": stats.repositories,
        "generation": stats.generation,
        "schema": stats.schema_version,
        "model": stats.model_id,
    })
}

/// `project-key --json --path PATH`: `{"key", "root", "remote"}` of the repository holding the
/// absolute folder `PATH` (the same key `harness-memory` gives it), `{"key": null}` for a folder
/// that is no project.
pub fn project_key(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json"], &["--path"]).ok_or(Failure::Usage)?;

    if !parsed.positional.is_empty() || !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let path = parsed
        .value("--path")
        .filter(|path| !path.trim().is_empty())
        .ok_or(Failure::Usage)?;
    let mut projects = ProjectResolver::new(context.env.clone(), std::env::current_dir().ok());
    let project = projects
        .resolve(Some(path))
        .map_err(|message| Failure::Failed(message.replace("project_path", "--path")))?;

    Ok(Done::ok(match project {
        Some(project) => json!({
            "key": project.key,
            "root": project.root.to_string_lossy(),
            "remote": project.remote,
        }),
        None => json!({ "key": null }),
    }))
}

/// Takes `--json` and nothing else.
fn json_only(args: &[Option<&str>]) -> Result<(), Failure> {
    let parsed = Args::parse(args, &["--json"], &[]).ok_or(Failure::Usage)?;

    if parsed.positional.is_empty() && parsed.flag("--json") {
        Ok(())
    } else {
        Err(Failure::Usage)
    }
}
