//! `check --json`: what the selected database can do.
//!
//! For the local file: `{"ok": true, "backend": "sqlite", "schema": N, "capabilities": {"keyword":
//! true, "vector": true, "vectorIndex": "ready"}, "model": {"dims": 384, "id"?}}`. `schema` is the
//! version of the file, or the version a new file gets when there is none yet (a check never
//! creates it). `model.id` is there once chunks were embedded. The vectors are searched in the
//! process, so there is no index to build and the vector index is always ready.
//!
//! A SQLite without FTS5 cannot keep the keyword index: `ok` is false, `capabilities.keyword` too,
//! `problem` is `no-fts5` with a `message`, and the exit code is 3. A team database has no store
//! yet and exits 2 (see [`crate::backend`]).

use serde_json::{Value, json};

use super::{Context, Done, Failure, Reply};
use crate::backend::{self, Access};
use crate::cli::Args;
use crate::config::Backend;
use crate::store::DIMS;
use crate::store::sqlite::{fts5_available, schema::SCHEMA_VERSION};

pub fn check(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json"], &[]).ok_or(Failure::Usage)?;

    if !parsed.positional.is_empty() || !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    match context.backend {
        Backend::Sqlite => local(context),
        other => Err(Failure::Unsupported(format!(
            "{} is not supported by this build yet, use the local database",
            other.describe()
        ))),
    }
}

fn local(context: &Context) -> Reply {
    let name = backend::name(context.backend);

    if !fts5_available() {
        let message = "this SQLite build has no FTS5 full-text search, so the keyword index cannot be kept";

        return Ok(Done {
            json: json!({
                "ok": false,
                "backend": name,
                "schema": SCHEMA_VERSION,
                "capabilities": capabilities(false),
                "model": { "dims": DIMS },
                "problem": "no-fts5",
                "message": message,
            }),
            code: 3,
        });
    }

    let (schema, model_id) = match context.open(Access::Read)? {
        Some(mut store) => {
            let schema = store
                .meta("schema_version")?
                .and_then(|version| version.parse::<i64>().ok());
            (schema.unwrap_or(SCHEMA_VERSION), store.meta("model_id")?)
        }
        None => (SCHEMA_VERSION, None),
    };

    let mut model = json!({ "dims": DIMS });

    if let Some(id) = model_id {
        model["id"] = Value::String(id);
    }

    Ok(Done::ok(json!({
        "ok": true,
        "backend": name,
        "schema": schema,
        "capabilities": capabilities(true),
        "model": model,
    })))
}

fn capabilities(keyword: bool) -> Value {
    json!({ "keyword": keyword, "vector": true, "vectorIndex": "ready" })
}
