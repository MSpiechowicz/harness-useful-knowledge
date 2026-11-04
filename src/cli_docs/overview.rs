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
/// Every place's counts carry its own `generation` (it changes with every write there), and
/// `generations` is `{"remote": N, "local": M}` of the places that were counted, so a client sees a
/// change in either place without reading `places`. A remote database that is offline has none.
///
/// With both places in use, a remote database that cannot be reached (or is cooling down after it
/// could not be, see [`crate::store::Reach`]) is not counted: `places.remote` is `{"offline": true}`,
/// the top-level counts are this computer's, and `offline` is `true` at the top level too (it is
/// absent otherwise).
///
/// `remoteWritable` is whether the remote database lets this user change documents: `true` or
/// `false` as the database says (PostgreSQL and MongoDB report the user's privileges), `null` when
/// no remote database is in use, it cannot be reached, or it does not say. The desktop then keeps
/// the actions enabled and shows the error of the one that fails.
///
/// Drafts: every count has `drafts`, `waiting`, `changesRequested` (rows in that state, a pending
/// version of a live document included) and `pending` (live documents that have a pending version),
/// and `documents` counts every row that is not a pending version, drafts included. At the top
/// level `drafts`, `changesRequested` and `pending` are those of the primary place like the other
/// counts, `waiting` is the sum over every place counted (what the team waits to approve: it is not
/// narrowed to what waits for the current user), and `approval` is whether team approval is on in
/// the remote database, `null` with no remote, while it cannot be reached or cannot say.
///
/// `me` (top level only) is the person at the terminal: `{"name": <git user.name or null>}`, the name
/// of the working directory's git configuration, so a viewer can tell their own documents.
///
/// `waitingForYou` (top level only) is how many of the waiting documents of every place counted this
/// person could approve: not written by them and not asked for by them (the `selfApproval` rule of
/// [`crate::review`], names compared trimmed and case-insensitive, the name the git `user.name` of
/// the working directory). It is `0` without a name. Each place is scanned for at most
/// [`crate::review::WAITING_SCAN_MAX`] waiting rows, so it is a lower bound past that many in one
/// database. A place whose rows cannot be read counts 0 rather than failing `stats`.
pub fn stats(context: &Context, args: &[Option<&str>]) -> Reply {
    let actor = super::review::actor(context);

    stats_as(context, args, actor.as_deref())
}

/// [`stats`] for the person named `actor`.
pub(super) fn stats_as(context: &Context, args: &[Option<&str>], actor: Option<&str>) -> Reply {
    json_only(args)?;

    let places = context.routing.places();
    let mut by_place = serde_json::Map::new();
    let mut generations = serde_json::Map::new();
    let mut primary: Option<Value> = None;
    let mut offline = false;
    let mut remote_writable: Option<bool> = None;
    let mut approval: Option<bool> = None;
    let mut waiting = 0;
    let mut waiting_for_you = 0;

    for place in places {
        let Some(Counted {
            stats,
            writable,
            approval: on,
            for_you,
        }) = place_stats(context, place, actor)?
        else {
            offline = true;
            by_place.insert(place.as_str().to_owned(), json!({ "offline": true }));
            continue;
        };

        generations.insert(place.as_str().to_owned(), json!(stats.generation));

        if place == Place::Remote {
            remote_writable = writable;
            approval = on;
        }

        waiting += stats.waiting;
        waiting_for_you += for_you;

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
    shown["generations"] = Value::Object(generations);
    shown["remoteWritable"] = json!(remote_writable);
    shown["waiting"] = json!(waiting);
    shown["waitingForYou"] = json!(waiting_for_you);
    shown["me"] = json!({ "name": crate::store::review::actor(actor) });
    shown["approval"] = json!(approval);

    if offline {
        shown["offline"] = json!(true);
    }

    Ok(Done::ok(shown))
}

/// What one place counts, and for the remote one whether it lets this user write and whether team
/// approval is on there.
struct Counted {
    stats: Stats,
    writable: Option<bool>,
    approval: Option<bool>,
    /// Of its waiting documents, how many the current person could approve.
    for_you: usize,
}

/// The counts of one place, all zero for a database that does not exist; `None` for a remote
/// database that cannot be reached while this computer is in use too.
fn place_stats(context: &Context, place: Place, actor: Option<&str>) -> Result<Option<Counted>, Failure> {
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
        return Ok(Some(Counted {
            stats: empty,
            writable: None,
            approval: None,
            for_you: 0,
        }));
    };

    match store.stats() {
        Ok(stats) => {
            let (writable, approval) = if place == Place::Remote {
                // A setting that cannot be read is not known: no word about it.
                let approval = crate::review::approval_in(store.as_mut(), Place::Remote).ok();

                (store.remote_writable(), approval)
            } else {
                (None, None)
            };

            let for_you = if stats.waiting > 0 {
                let state = crate::review::PlaceState {
                    place,
                    generation: stats.generation,
                    waiting: stats.waiting,
                };
                let folder = crate::embed::data_folder(context.env);

                crate::review::waiting_for_cached(store.as_mut(), folder.as_deref(), state, actor).unwrap_or(0)
            } else {
                0
            };

            Ok(Some(Counted {
                stats,
                writable,
                approval,
                for_you,
            }))
        }
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
        "drafts": stats.drafts,
        "waiting": stats.waiting,
        "changesRequested": stats.changes_requested,
        "pending": stats.pending,
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
