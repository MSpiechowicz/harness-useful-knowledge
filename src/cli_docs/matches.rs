//! `docs matches`: the chunks of one document that match a text (see [`super`]).

use serde_json::json;

use super::{Context, Done, Failure, Reply, not_found, parse_id};
use crate::backend::Access;
use crate::cli::Args;
use crate::search::render::snippet;
use crate::store::Detail;
use crate::store::routed::ids;

/// The most `--limit` may ask for, and its default.
const LIMIT_MAX: usize = 50;
const LIMIT_DEFAULT: usize = 20;

/// Characters of a snippet.
const SNIPPET_CHARS: usize = 240;

/// Most characters of `--query`, as in the list.
const QUERY_CHARS_MAX: usize = 200;

pub(super) fn matches(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json"], &["--query", "--limit"]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let id = match parsed.positional.as_slice() {
        [id] => parse_id(id)?,
        _ => return Err(Failure::Usage),
    };

    let text = parsed.value("--query").ok_or(Failure::Usage)?.trim().to_owned();

    if text.chars().count() > QUERY_CHARS_MAX {
        return Err(Failure::Usage);
    }

    let limit = match parsed.value("--limit") {
        Some(limit) => limit
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=LIMIT_MAX).contains(limit))
            .ok_or(Failure::Usage)?,
        None => LIMIT_DEFAULT,
    };

    let Some(mut store) = context.open(Access::Read)? else {
        return Err(not_found(id));
    };

    if store.get(id, &Detail::default())?.is_none() {
        return Err(not_found(id));
    }

    let found = store.matching_chunks(id, &text, limit)?;
    let words = words(&text);

    let matches: Vec<_> = found
        .chunks
        .iter()
        .map(|chunk| {
            json!({
                "chunkId": ids::json(chunk.chunk_id),
                "ordinal": chunk.ordinal,
                "section": chunk.heading_path,
                "snippet": snippet(&chunk.text, &words, SNIPPET_CHARS),
            })
        })
        .collect();

    Ok(Done::ok(json!({ "matches": matches, "total": found.total })))
}

/// The words of the query, lowercase, for the snippet to start at.
fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}
