//! `tools/call`: argument parsing, scope resolution and the compact text replies.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::reply::Capped;
use super::scope::{NO_PROJECT, repository, scope_arg, search_scope};
use super::{PATHS_MAX, QUERY_MAX, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX, Server, VECTORS_UNAVAILABLE, open_store};
use crate::search::render::{FRAME, Pointer, block, chunk_ref, words_of};
use crate::search::{self, Query, Tuning, Vectors};
use crate::store::clean;

/// The most bytes of a search reply.
const SEARCH_REPLY_MAX: usize = 24 * 1024;

/// A tool result.
pub(super) struct Reply {
    pub text: String,
    pub is_error: bool,
}

impl Reply {
    fn of(result: Result<String, String>) -> Self {
        match result {
            Ok(text) => Self { text, is_error: false },
            Err(text) => Self { text, is_error: true },
        }
    }
}

/// Runs tool `name`; `None` when there is no such tool.
pub(super) fn call(server: &mut Server, name: &str, arguments: Value) -> Option<Reply> {
    let result = match name {
        "knowledge_search" => parse(arguments).and_then(|args| search(server, args)),
        "knowledge_get" => parse(arguments).and_then(|args| super::get::get(server, args)),
        "knowledge_list" => parse(arguments).and_then(|args| super::list::list(server, args)),
        "knowledge_add" => parse(arguments).and_then(|args| super::add::add(server, args)),
        _ => return None,
    };

    Some(Reply::of(result))
}

/// Parses tool arguments, serde's message names the offending field.
pub(super) fn parse<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|error| {
        format!("invalid arguments: {error}. Check the tool's input schema in tools/list for the required fields")
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SearchArgs {
    query: String,
    project_path: Option<String>,
    paths: Option<Vec<String>>,
    limit: Option<i64>,
    scope: Option<String>,
    mode: Option<String>,
}

fn search(server: &mut Server, args: SearchArgs) -> Result<String, String> {
    let text = clean::line(&args.query);

    if text.is_empty() {
        return Err("query must have a word to search for".to_owned());
    }

    if text.chars().count() > QUERY_MAX {
        return Err(format!("query must be at most {QUERY_MAX} characters"));
    }

    let chunks = match args.mode.as_deref().map(str::trim) {
        None | Some("") | Some("pointers") => false,
        Some("chunks") => true,
        Some(_) => return Err("mode must be pointers or chunks".to_owned()),
    };

    let limit = args.limit.map_or(SEARCH_LIMIT_DEFAULT, |limit| {
        usize::try_from(limit.clamp(1, SEARCH_LIMIT_MAX as i64)).unwrap_or(SEARCH_LIMIT_DEFAULT)
    });
    let paths = paths(args.paths.unwrap_or_default())?;
    let everything = scope_arg(args.scope.as_deref())?;
    let repo = repository(server, args.project_path.as_deref())?;
    let unknown_repo = repo.is_none() && !everything;
    let scope = search_scope(repo, everything);
    let query = Query {
        text: &text,
        scope: &scope,
        paths: &paths,
        limit,
    };

    let found = {
        let Server {
            store,
            embedder,
            env,
            backend,
            routing,
            ..
        } = &mut *server;

        match open_store(store, env, backend, *routing)? {
            None => return Ok("no documents yet".to_owned()),
            Some(opened) => search::run(opened, Some(embedder.as_mut()), &query, Tuning::SEARCH),
        }
    };

    let found = found.map_err(|error| server.failed(&error))?;

    let mut reply = Capped::new(SEARCH_REPLY_MAX);

    if !found.hits.is_empty() {
        reply.push(FRAME);
    }

    if unknown_repo {
        reply.push(NO_PROJECT);
    }

    if matches!(found.vectors, Vectors::Unavailable(_)) && !server.vector_notice {
        server.vector_notice = true;
        reply.push(VECTORS_UNAVAILABLE);
    }

    if found.hits.is_empty() {
        reply.push("no matching documents");
    }

    let words = words_of(&text);

    for scored in &found.hits {
        let pointer = Pointer::of(scored, &words);

        if chunks {
            reply.push(&block(
                crate::search::render::DOC_TAG,
                &pointer.attributes(),
                &scored.hit.text,
            ));
        } else {
            reply.push(&pointer.block());
        }
    }

    let mut text = reply.finish();

    if !found.hits.is_empty() {
        let first = &found.hits[0].hit;

        text.push_str(&format!(
            "\nOpen one with knowledge_get, for example id \"{}\".",
            chunk_ref(first.doc_id, first.chunk_id)
        ));
    }

    Ok(text)
}

/// The repository-relative paths of a search: at most [`PATHS_MAX`], each on one line.
fn paths(given: Vec<String>) -> Result<Vec<String>, String> {
    if given.len() > PATHS_MAX {
        return Err(format!("paths must have at most {PATHS_MAX} entries"));
    }

    let mut paths = Vec::new();

    for path in given {
        let path = clean::line(&path);
        let path = path.trim_start_matches("./").replace('\\', "/");

        if path.chars().count() > 512 {
            return Err("each path must be at most 512 characters".to_owned());
        }

        if !path.is_empty() {
            paths.push(path);
        }
    }

    Ok(paths)
}
