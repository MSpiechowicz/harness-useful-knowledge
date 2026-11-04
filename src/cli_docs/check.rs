//! `check --json`: what the selected database can do.
//!
//! For the local file: `{"ok": true, "backend": "sqlite", "schema": N, "capabilities": {"keyword":
//! true, "vector": true, "vectorIndex": "ready"}, "model": {"dims": 384, "id"?}}`. `schema` is the
//! version of the file, or the version a new file gets when there is none yet (a check never
//! creates it). `model.id` is there once chunks were embedded. The vectors are searched in the
//! process, so there is no index to build and the vector index is always ready.
//!
//! A SQLite without FTS5 cannot keep the keyword index: `ok` is false, `capabilities.keyword` too,
//! `problem` is `no-fts5` with a `message`, and the exit code is 3. The team databases print the
//! same JSON with their own problem codes, also exit 3.
//!
//! A database from an older server that was not upgraded yet is `problem` `not-upgraded` (exit 3)
//! whatever the backend, with a `message` that says to run `harness-knowledge upgrade`: its search
//! indexes and tables are those of the older version, so nothing else about it is reported.

use serde_json::{Value, json};

use super::{Context, Done, Failure, Reply, upgrade};
use crate::backend::{self, Access};
use crate::cli::Args;
use crate::config::{Backend, Layout, Place};
use crate::store::DIMS;
use crate::store::libsql::{self, Trouble};
use crate::store::postgres::{self, Fault, Problem};
use crate::store::sqlite::{fts5_available, schema::SCHEMA_VERSION};

/// `check --json` reports each place in use under `places` (`{"remote": {...}, "local": {...}}`, each
/// as described above), and the top-level fields are those of the remote database, or of this
/// computer when that is the only place, unless the remote database is fine and this computer is
/// not: then the problem is the one reported at the top and in the exit code.
pub fn check(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json"], &[]).ok_or(Failure::Usage)?;

    if !parsed.positional.is_empty() || !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let mut reports: Vec<(Place, Done)> = Vec::new();

    for place in context.routing.places() {
        let checked = check_place(context, place)?;

        if place == Place::Remote && context.routing.layout() == Layout::Both {
            note_reach(context, &checked);
        }

        reports.push((place, checked.done));
    }

    let first = reports.iter().position(|(_, done)| done.code != 0).unwrap_or(0);
    let mut shown = reports[first].1.clone();

    let places: serde_json::Map<String, Value> = reports
        .into_iter()
        .map(|(place, done)| (place.as_str().to_owned(), done.json))
        .collect();
    shown.json["places"] = Value::Object(places);

    Ok(shown)
}

/// What a check of one place found, and whether the server could not be reached at all.
struct Checked {
    done: Done,
    /// The server did not answer (no route, nothing listening, a timeout). False for one that
    /// answered, or whose TLS the client refused (a setting to fix, nothing to wait out).
    unreachable: bool,
}

impl From<Done> for Checked {
    fn from(done: Done) -> Self {
        Self {
            done,
            unreachable: false,
        }
    }
}

impl Checked {
    fn unreachable(done: Done) -> Self {
        Self {
            done,
            unreachable: true,
        }
    }
}

/// Records what the check found out about reaching the remote database, which every other command
/// goes by while this computer stands in for it (see [`crate::store::Reach`]): a server that could
/// not be reached starts the cooldown, any other answer ends it. The check itself is never skipped.
/// A server whose TLS is refused answered (it is a setting to fix, see
/// [`crate::store::StoreError::Unreachable`]), so it starts no cooldown, though the check reports it
/// as `unreachable` like any connection that did not work.
fn note_reach(context: &Context, checked: &Checked) {
    let reach = backend::reach_of(context.env, context.backend);

    if !checked.unreachable {
        reach.reached();
        return;
    }

    reach.failed(
        checked.done.json["message"]
            .as_str()
            .unwrap_or("the remote database is unreachable"),
    );
}

/// The check of one place.
fn check_place(context: &Context, place: Place) -> Result<Checked, Failure> {
    match (place, context.backend) {
        (Place::Local, _) => local(context).map(Checked::from),
        (Place::Remote, Backend::Sqlite) => Err(Failure::Failed("no remote database is configured".to_owned())),
        (Place::Remote, Backend::Postgres { url }) => postgres_check(context, url),
        (Place::Remote, Backend::Libsql { url, token }) => libsql_check(context, url, token.as_deref()),
        (Place::Remote, Backend::Mongodb { url }) => mongodb_check(context, url),
    }
}

fn local(context: &Context) -> Reply {
    let name = "sqlite";

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

    let opened = match context.open_place(Place::Local, Access::Read) {
        Err(failure) if is_not_upgraded(&failure) => {
            return Ok(not_upgraded(name, SCHEMA_VERSION, capabilities(false)));
        }
        other => other?,
    };

    let (schema, model_id) = match opened {
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

/// The report of a database from an older server that was not upgraded yet: `problem` is
/// `not-upgraded`, and the message says to run `harness-knowledge upgrade`. Exit 3.
fn not_upgraded(name: &str, schema: i64, capabilities: Value) -> Done {
    Done {
        json: json!({
            "ok": false,
            "backend": name,
            "schema": schema,
            "capabilities": capabilities,
            "model": { "dims": DIMS },
            "problem": "not-upgraded",
            "message": upgrade::NOT_UPGRADED_MESSAGE,
        }),
        code: 3,
    }
}

/// Whether `failure` is a database that was not upgraded yet.
fn is_not_upgraded(failure: &Failure) -> bool {
    matches!(failure, Failure::Failed(message) if message.contains(upgrade::NOT_UPGRADED))
}

/// `check` of a PostgreSQL team database (see [`postgres::probe`], which changes nothing).
///
/// Same JSON as the local file, with `schema` the version stored (or the one a new database gets).
/// A problem prints `ok: false`, `problem` (`no-pgvector`, `pgvector-no-privilege`,
/// `pgvector-too-old`, `index-building`, `unreachable` or `auth`) and a `message`, and exits 3.
/// `capabilities` then says what is known to work: the keyword index when the server answered, the
/// vector search only when pgvector and its index are fine, and `vectorIndex` is `building` or
/// `missing`.
fn postgres_check(context: &Context, url: &str) -> Result<Checked, Failure> {
    let secrets = context.backend.secrets();
    let name = backend::name(context.backend);

    match postgres::probe(url) {
        Ok(found)
            if found
                .schema
                .is_some_and(|version| version < postgres::schema::SCHEMA_VERSION) =>
        {
            let capabilities = json!({ "keyword": true, "vector": false, "vectorIndex": "missing" });

            Ok(not_upgraded(name, postgres::schema::SCHEMA_VERSION, capabilities).into())
        }
        Ok(found) => {
            let mut model = json!({ "dims": DIMS });

            if let Some(id) = found.model_id {
                model["id"] = Value::String(id);
            }

            Ok(Done::ok(json!({
                "ok": true,
                "backend": name,
                "schema": found.schema.unwrap_or(postgres::schema::SCHEMA_VERSION),
                "capabilities": { "keyword": true, "vector": true, "vectorIndex": "ready" },
                "model": model,
            }))
            .into())
        }
        Err(Fault::Problem(problem)) => {
            let reached = problem.is_missing();
            let unreachable = matches!(problem, Problem::Unreachable(_));
            let index = if problem == Problem::IndexBuilding {
                "building"
            } else {
                "missing"
            };

            let done = Done {
                json: json!({
                    "ok": false,
                    "backend": name,
                    "schema": postgres::schema::SCHEMA_VERSION,
                    "capabilities": { "keyword": reached, "vector": false, "vectorIndex": index },
                    "model": { "dims": DIMS },
                    "problem": problem.code(),
                    "message": secrets.scrub(&problem.message()),
                }),
                code: 3,
            };

            Ok(Checked { done, unreachable })
        }
        Err(Fault::Store(error)) => Err(Failure::Failed(secrets.scrub(&error.to_string()))),
    }
}

/// `check` of a libSQL team database (see [`libsql::probe`], which changes nothing).
///
/// Same JSON as the local file, with `schema` the version stored (or the one a new database gets).
/// A problem prints `ok: false`, `problem` (`libsql-no-vector`, `no-fts5`, `unreachable` or `auth`)
/// and a `message`, and exits 3. `capabilities` then says what the server answered: `keyword` and
/// `vector` as probed (both false when it could not be asked), and `vectorIndex` is `missing`
/// when vectors do not work.
fn libsql_check(context: &Context, url: &str, token: Option<&str>) -> Result<Checked, Failure> {
    let secrets = context.backend.secrets();
    let name = backend::name(context.backend);

    let problem = |code: &str, message: &str, keyword: bool, vector: bool| Done {
        json: json!({
            "ok": false,
            "backend": name,
            "schema": libsql::SCHEMA_VERSION,
            "capabilities": {
                "keyword": keyword,
                "vector": vector,
                "vectorIndex": if vector { "ready" } else { "missing" },
            },
            "model": { "dims": DIMS },
            "problem": code,
            "message": secrets.scrub(message),
        }),
        code: 3,
    };

    let found = match libsql::probe(url, token) {
        Ok(found) => found,
        Err(Trouble::Auth) => {
            let message = "the libSQL server refused the token, check the token of the database settings";
            return Ok(problem("auth", message, false, false).into());
        }
        Err(Trouble::Unreachable(message)) => {
            return Ok(Checked::unreachable(problem("unreachable", &message, false, false)));
        }
        Err(Trouble::Refused(message)) => return Ok(problem("unreachable", &message, false, false).into()),
        Err(Trouble::Invalid(message)) => return Err(Failure::Failed(secrets.scrub(&message))),
    };

    if !found.vector {
        return Ok(problem("libsql-no-vector", libsql::NO_VECTOR_MESSAGE, found.keyword, false).into());
    }

    if !found.keyword {
        let message = "this libSQL server has no FTS5 full-text search, so the keyword index cannot be kept";
        return Ok(problem("no-fts5", message, false, true).into());
    }

    let opened = match context.open_place(Place::Remote, Access::Read) {
        Err(failure) if is_not_upgraded(&failure) => {
            let capabilities = json!({ "keyword": true, "vector": true, "vectorIndex": "ready" });

            return Ok(not_upgraded(name, libsql::SCHEMA_VERSION, capabilities).into());
        }
        other => other?,
    };

    let (schema, model_id) = match opened {
        Some(mut store) => {
            let schema = store
                .meta("schema_version")?
                .and_then(|version| version.parse::<i64>().ok());
            (schema.unwrap_or(libsql::SCHEMA_VERSION), store.meta("model_id")?)
        }
        None => (libsql::SCHEMA_VERSION, None),
    };

    let mut model = json!({ "dims": DIMS });

    if let Some(id) = model_id {
        model["id"] = Value::String(id);
    }

    Ok(Done::ok(json!({
        "ok": true,
        "backend": name,
        "schema": schema,
        "capabilities": { "keyword": true, "vector": true, "vectorIndex": "ready" },
        "model": model,
    }))
    .into())
}

/// `check` of a MongoDB team database (see [`crate::store::mongo::probe`], which changes nothing).
///
/// Same JSON as the local file, with `schema` the version stored (or the one a new database gets).
/// A problem prints `ok: false`, `problem` (`no-mongot`, `index-building`, `unreachable` or `auth`)
/// and a `message`, and exits 3. `capabilities` then says what is known to work: with `no-mongot`
/// neither search, with `index-building` the keyword search when its index answers, and
/// `vectorIndex` is `building` or `missing`. A database nobody wrote to yet is ready: its first
/// import creates the two search indexes.
fn mongodb_check(context: &Context, url: &str) -> Result<Checked, Failure> {
    use crate::store::mongo::probe::probe;
    use crate::store::mongo::schema::{IndexState, SCHEMA_VERSION as MONGO_SCHEMA};

    let secrets = context.backend.secrets();
    let name = backend::name(context.backend);

    let problem = |code: &str, message: &str, keyword: bool, vector: (bool, &str)| Done {
        json: json!({
            "ok": false,
            "backend": name,
            "schema": MONGO_SCHEMA,
            "capabilities": { "keyword": keyword, "vector": vector.0, "vectorIndex": vector.1 },
            "model": { "dims": DIMS },
            "problem": code,
            "message": secrets.scrub(message),
        }),
        code: 3,
    };

    let found = match probe(url) {
        Ok(found) => found,
        Err(error) => {
            let done = problem(error.code, &error.message, false, (false, "missing"));
            let unreachable = error.code == "unreachable" && !error.refused;

            return Ok(Checked { done, unreachable });
        }
    };

    // The search indexes of an older database are missing: say what is behind it.
    if found.schema.is_some_and(|version| version < MONGO_SCHEMA) {
        let capabilities = json!({ "keyword": false, "vector": false, "vectorIndex": "missing" });

        return Ok(not_upgraded(name, MONGO_SCHEMA, capabilities).into());
    }

    if let Some(error) = found.problem() {
        let keyword = found.keyword == IndexState::Ready;
        let vector = (false, found.vector_index());

        return Ok(problem(error.code, &error.message, keyword, vector).into());
    }

    let mut model = json!({ "dims": DIMS });

    if let Some(id) = found.model_id {
        model["id"] = Value::String(id);
    }

    Ok(Done::ok(json!({
        "ok": true,
        "backend": name,
        "schema": found.schema.unwrap_or(MONGO_SCHEMA),
        "capabilities": { "keyword": true, "vector": true, "vectorIndex": "ready" },
        "model": model,
    }))
    .into())
}

#[cfg(test)]
mod postgres_tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::config::{Routing, Where};
    use crate::support::Env;

    /// A server with pgvector, and one without, for the `team_*` tests (they skip when unset).
    const URL_VAR: &str = "HARNESS_KNOWLEDGE_TEST_POSTGRES_URL";
    const PLAIN_URL_VAR: &str = "HARNESS_KNOWLEDGE_TEST_POSTGRES_PLAIN_URL";

    fn check_url(url: &str) -> Done {
        let env = Env::for_tests("/home/u");
        let backend = crate::config::backend_for(Some(url.to_owned()), None).expect("backend");
        let context = Context::new(&env, &backend, Routing::new(Where::Remote, Where::Remote));

        check(&context, &[Some("--json")]).expect("a check answers")
    }

    fn url_of(name: &str) -> Option<String> {
        let url = std::env::var(name).ok().filter(|url| !url.trim().is_empty());

        if url.is_none() {
            eprintln!("skipped: set {name} to run the PostgreSQL check tests");
        }

        url
    }

    #[test]
    fn an_unreachable_postgres_server_exits_3_with_its_problem_and_no_password() {
        let done = check_url("postgres://someone:hunter2-secret@127.0.0.1:1/knowledge?sslmode=disable");

        assert_eq!(done.code, 3);
        assert_eq!(done.json["ok"], json!(false));
        assert_eq!(done.json["backend"], json!("postgres"));
        assert_eq!(done.json["problem"], json!("unreachable"));
        assert_eq!(
            done.json["capabilities"],
            json!({ "keyword": false, "vector": false, "vectorIndex": "missing" })
        );
        assert!(!done.json.to_string().contains("hunter2-secret"), "{}", done.json);
    }

    #[test]
    fn team_a_postgres_server_with_pgvector_checks_ready() {
        let Some(url) = url_of(URL_VAR) else { return };
        let done = check_url(&url);

        assert_eq!(done.code, 0, "{}", done.json);
        assert_eq!(done.json["ok"], json!(true));
        assert_eq!(done.json["backend"], json!("postgres"));
        assert_eq!(
            done.json["capabilities"],
            json!({ "keyword": true, "vector": true, "vectorIndex": "ready" })
        );
        assert_eq!(done.json["model"], json!({ "dims": 384 }));
    }

    #[test]
    fn team_a_postgres_server_without_pgvector_exits_3_with_no_pgvector() {
        let Some(url) = url_of(PLAIN_URL_VAR) else { return };
        let done = check_url(&url);

        assert_eq!(done.code, 3, "{}", done.json);
        assert_eq!(done.json["ok"], json!(false));
        assert_eq!(done.json["problem"], json!("no-pgvector"));
        assert_eq!(
            done.json["capabilities"],
            json!({ "keyword": true, "vector": false, "vectorIndex": "missing" })
        );
        assert!(
            done.json["message"]
                .as_str()
                .is_some_and(|message| message.contains("pgvector"))
        );
    }
}
