//! `docs import --ndjson` and `docs reindex --json`.
//!
//! - `docs import --ndjson < manifest.json` imports the files the manifest names and prints one JSON
//!   event per line as it goes (see [`crate::import::events`]). The manifest is
//!   `{"paths": [...], "folders": [...], "links": [{"repo", "globs"}], "replaces": ID}`, all optional,
//!   at most 4 MiB. SIGTERM, SIGINT or a closed stdout stop the import between two documents (the
//!   end of stdin does not: it carries the manifest). The exit code is 0 even when files failed,
//!   each file's status is in its event, and 1 when the manifest itself is refused.
//! - `docs reindex --json (ID... | --all)` gives the chunks that have no vector one and prints
//!   `{"documents", "embedded", "failed": [{"id", "reason"}], "cancelled", "model"}`: the documents
//!   and chunks that got a vector, in batches of 64 that are stored as they are made (ids, texts and
//!   the keyword index do not change). It fails (exit 1) while no embedder is available.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use super::{Context, Done, Failure, Reply, parse_id};
use crate::backend::Access;
use crate::cli::Args;
use crate::config::{Kind, Layout, Place};
use crate::extract::pdf::{PdfPages, WorkerProcess};
use crate::import::embed::{self, Embedder};
use crate::import::events::Events;
use crate::import::reindex::{self, Which};
use crate::import::{self, Importer, manifest};
use crate::limits::{DELETE_IDS_MAX, MANIFEST_MAX};
use crate::store::clean;
use crate::store::routed::ids;
use crate::tokens::counter_for;

/// `docs import`.
pub(super) fn import(context: &Context, args: &[Option<&str>], input: &mut dyn Read) -> Reply {
    let parsed = Args::parse(args, &["--ndjson"], &[]).ok_or(Failure::Usage)?;

    if !parsed.positional.is_empty() || !parsed.flag("--ndjson") {
        return Err(Failure::Usage);
    }

    let manifest = read_manifest(input)?;
    let pdf = WorkerProcess::current_exe().map_err(Failure::Failed)?;
    let stopped = stop_signal();
    let cancel = || stopped.load(Ordering::SeqCst);
    let mut out = std::io::stdout().lock();

    import_with(
        context,
        &manifest,
        &pdf,
        embed::default_embedder(context.env),
        &cancel,
        &mut out,
    )?;

    // The events were the output.
    Ok(Done::ok(Value::Null))
}

/// Runs an import and prints its events to `out`.
pub(super) fn import_with(
    context: &Context,
    manifest: &import::Manifest,
    pdf: &dyn PdfPages,
    mut embedder: Option<Box<dyn Embedder>>,
    cancel: &dyn Fn() -> bool,
    out: &mut dyn Write,
) -> Result<import::Report, Failure> {
    let links = clean::links(&manifest.links)?;
    let counter = counter_for(context.env);
    let place = import_place(context, manifest, Kind::of(links.len()))?;
    let mut store = context
        .open_place_to_change(place, Access::Create)?
        .ok_or_else(|| Failure::Failed("the database could not be created".to_owned()))?;

    let mut importer = Importer::new(store.as_mut(), counter.as_ref(), pdf, cancel);
    importer.embedder = embedder.as_deref_mut().map(|embedder| embedder as &mut dyn Embedder);
    importer.links = links;
    importer.replaces = manifest.replaces.map(|id| ids::split(id).1);

    // Ids are written with their place only when two places are in use.
    let tagged = (context.routing.layout() == Layout::Both).then_some(place);

    import::run(&mut importer, manifest, &mut Events::new(out).in_place(tagged)).map_err(Failure::Failed)
}

/// Where an import saves: the manifest's `place`, else the place of the document it replaces, else
/// the routing's default for documents of `kind` (the manifest's links decide it). A place the
/// routing does not keep that kind in is refused.
fn import_place(context: &Context, manifest: &import::Manifest, kind: Kind) -> Result<Place, Failure> {
    let replaced = manifest.replaces.map(ids::place_of);
    let place = manifest
        .place
        .or(replaced.filter(|_| context.routing.layout() == Layout::Both))
        .unwrap_or_else(|| context.routing.default_place(kind));

    context.routing.check_write(kind, place).map_err(Failure::Failed)?;

    Ok(place)
}

fn read_manifest(input: &mut dyn Read) -> Result<import::Manifest, Failure> {
    let mut text = String::new();
    let read = input.take(MANIFEST_MAX + 1).read_to_string(&mut text);

    if read.is_err() || text.len() as u64 > MANIFEST_MAX {
        return Err(Failure::Failed(format!(
            "the manifest must be UTF-8 text of at most {} MiB",
            MANIFEST_MAX / (1024 * 1024)
        )));
    }

    manifest::parse(&text).map_err(Failure::Failed)
}

/// A flag that SIGTERM, SIGHUP and SIGINT (Ctrl-C) set.
fn stop_signal() -> Arc<AtomicBool> {
    let stopped = Arc::new(AtomicBool::new(false));
    let handler = Arc::clone(&stopped);

    // Fails only when a handler is already set: then that one rules.
    let _ = ctrlc::set_handler(move || handler.store(true, Ordering::SeqCst));

    stopped
}

/// `docs reindex`.
pub(super) fn reindex(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json", "--all"], &[]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let which = match (parsed.flag("--all"), parsed.positional.is_empty()) {
        (true, true) => Which::All,
        (false, false) if parsed.positional.len() <= DELETE_IDS_MAX => {
            let mut ids: Vec<i64> = Vec::new();

            for raw in &parsed.positional {
                let id = parse_id(raw)?;

                if !ids.contains(&id) {
                    ids.push(id);
                }
            }

            Which::Ids(ids)
        }
        _ => return Err(Failure::Usage),
    };

    let Some(mut embedder) = embed::default_embedder(context.env) else {
        return Err(Failure::Failed(
            "no embedder is available: install the embedding model first".to_owned(),
        ));
    };

    let stopped = stop_signal();

    reindex_with(context, &which, embedder.as_mut(), &|| stopped.load(Ordering::SeqCst))
}

/// Reindexes `which` with `embedder`, in each place in use: a document by id in its own place,
/// `--all` in every place.
pub(super) fn reindex_with(
    context: &Context,
    which: &Which,
    embedder: &mut dyn Embedder,
    cancel: &dyn Fn() -> bool,
) -> Reply {
    let model = embedder.model_id().to_owned();
    let both = context.routing.layout() == Layout::Both;
    let mut done = reindex::Reindexed::default();

    for place in context.routing.places() {
        let wanted = match which {
            Which::All => Which::All,
            Which::Ids(all) => {
                let own: Vec<i64> = all
                    .iter()
                    .filter(|id| ids::place_of(**id) == place || (!both && ids::split(**id).0.is_none()))
                    .map(|id| ids::split(*id).1)
                    .collect();

                if own.is_empty() {
                    continue;
                }

                Which::Ids(own)
            }
        };

        let Some(mut store) = context.open_place_to_change(place, Access::Existing)? else {
            continue;
        };

        let part = reindex::run(store.as_mut(), embedder, &wanted, cancel)?;
        done.documents += part.documents;
        done.embedded += part.embedded;
        done.cancelled |= part.cancelled;
        done.failed.extend(part.failed.into_iter().map(|(id, reason)| {
            let id = if both { ids::tag(place, id) } else { id };
            (id, reason)
        }));
    }

    Ok(Done::ok(json!({
        "documents": done.documents,
        "embedded": done.embedded,
        "failed": done
            .failed
            .iter()
            .map(|(id, reason)| json!({ "id": ids::json(*id), "reason": reason }))
            .collect::<Vec<_>>(),
        "cancelled": done.cancelled,
        "model": model,
    })))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::config::{Backend, Routing};
    use crate::import::EmbedError;
    use crate::location::DIR_VAR;
    use crate::store::testing::{MODEL, fake_vector};
    use crate::support::Env;

    struct NoPdf;

    impl PdfPages for NoPdf {
        fn pages(&self, _path: &std::path::Path) -> Result<Vec<String>, String> {
            Err("no PDF reader in this test".to_owned())
        }
    }

    struct Fixed;

    impl Embedder for Fixed {
        fn model_id(&self) -> &str {
            MODEL
        }

        fn embed_passages(&mut self, passages: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(passages
                .iter()
                .map(|passage| fake_vector(passage.len() as u64))
                .collect())
        }
    }

    fn lines(output: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(output)
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON line"))
            .collect()
    }

    fn args<'a>(raw: &[&'a str]) -> Vec<Option<&'a str>> {
        raw.iter().map(|arg| Some(*arg)).collect()
    }

    #[test]
    fn an_import_creates_the_database_prints_events_and_lists_the_document() {
        let root = tempfile::tempdir().expect("temp dir");
        let folder = root.path().join("knowledge");
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, folder.to_string_lossy());
        let context = Context::new(&env, &Backend::Sqlite, Routing::local());
        let file = root.path().join("guide.md");
        std::fs::write(&file, "# Guide\n\nHow the gateway routes requests.").expect("write");

        let manifest = import::Manifest {
            paths: vec![file.to_string_lossy().into_owned()],
            ..import::Manifest::default()
        };
        let mut out: Vec<u8> = Vec::new();

        let report = import_with(&context, &manifest, &NoPdf, None, &|| false, &mut out).expect("imports");
        let events = lines(&out);

        assert_eq!(report.counts.added, 1);
        assert_eq!(report.pending, 1);
        assert_eq!(events.first().map(|event| event["type"].clone()), Some(json!("plan")));
        assert_eq!(events.last().map(|event| event["type"].clone()), Some(json!("done")));
        assert_eq!(events[1]["status"], "added");
        assert_eq!(events[1]["embedding"], "pending");

        let listed =
            super::super::docs_reply(&context, &args(&["list", "--json"]), &mut std::io::empty()).expect("list");
        assert_eq!(listed.json["total"], 1);
        assert_eq!(listed.json["items"][0]["title"], "Guide");
        assert_eq!(listed.json["items"][0]["chunkCount"], 1);

        // Reindexing gives the chunk its vector.
        let reindexed = reindex_with(&context, &Which::All, &mut Fixed, &|| false).expect("reindex");
        assert_eq!(
            reindexed.json,
            json!({ "documents": 1, "embedded": 1, "failed": [], "cancelled": false, "model": MODEL })
        );

        let stats = super::super::overview::stats(&context, &args(&["--json"])).expect("stats");
        assert_eq!(stats.json["embeddedChunks"], 1);
        assert_eq!(stats.json["model"], MODEL);
    }

    #[test]
    fn a_reindex_of_nothing_reports_zero_and_bad_arguments_are_usage_errors() {
        let root = tempfile::tempdir().expect("temp dir");
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, root.path().join("none").to_string_lossy());
        let context = Context::new(&env, &Backend::Sqlite, Routing::local());

        let empty = reindex_with(&context, &Which::All, &mut Fixed, &|| false).expect("reindex");
        assert_eq!(empty.json["documents"], 0);
        assert!(
            !root.path().join("none").exists(),
            "a reindex never creates the database"
        );

        let docs = |raw: &[&str]| super::super::docs_reply(&context, &args(raw), &mut std::io::empty());

        for bad in [
            &["reindex", "--json"][..],
            &["reindex", "--json", "--all", "3"],
            &["reindex", "--all"],
            &["reindex", "--json", "x"],
            &["reindex", "--json", "0"],
            &["import"],
            &["import", "--ndjson", "extra"],
        ] {
            assert_eq!(docs(bad).expect_err("usage"), Failure::Usage, "{bad:?}");
        }

        // Without an embedder a reindex says so.
        assert!(
            matches!(docs(&["reindex", "--json", "--all"]), Err(Failure::Failed(message)) if message.contains("no embedder"))
        );
    }

    #[test]
    fn a_bad_or_oversized_manifest_fails_before_anything_is_imported() {
        assert!(matches!(
            read_manifest(&mut "not json".as_bytes()),
            Err(Failure::Failed(_))
        ));
        assert!(matches!(
            read_manifest(&mut "{\"files\": []}".as_bytes()),
            Err(Failure::Failed(_))
        ));

        let big = format!("{{\"paths\": [\"{}\"]}}", "a".repeat(MANIFEST_MAX as usize));
        assert!(
            matches!(read_manifest(&mut big.as_bytes()), Err(Failure::Failed(message)) if message.contains("4 MiB"))
        );

        assert_eq!(
            read_manifest(&mut "{}".as_bytes()).expect("empty"),
            import::Manifest::default()
        );
    }

    #[test]
    fn links_that_break_a_rule_fail_the_import_up_front() {
        let root = tempfile::tempdir().expect("temp dir");
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, root.path().join("k").to_string_lossy());
        let context = Context::new(&env, &Backend::Sqlite, Routing::local());
        let manifest = import::Manifest {
            links: vec![crate::store::Link {
                repo_key: "not-a-key".to_owned(),
                globs: Vec::new(),
            }],
            ..import::Manifest::default()
        };

        let failure = import_with(&context, &manifest, &NoPdf, None, &|| false, &mut Vec::new()).expect_err("refused");

        assert!(matches!(failure, Failure::Failed(message) if message.contains("repository")));
        assert!(!root.path().join("k").exists());
    }
}
