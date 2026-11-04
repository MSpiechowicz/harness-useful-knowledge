//! `docs history --json ID`: what happened to a document, newest first.
//!
//! Prints `{"events": [{"kind", "by", "at", "comment"}], "total"}`: at most 200 events (`total`
//! counts them all). `kind` is `drafted`, `edited`, `updateDrafted`, `asked`, `changesRequested`,
//! `approved`, `wentLive` or `withdrawn`, `by` the git `user.name` or `null`, `at` RFC 3339. The
//! history of a live document holds the rounds of the pending versions that went live, and while it has a pending
//! version the events of that row too, merged by time (the pending row's first on equal seconds).

use serde_json::{Value, json};

use super::review::{missing, open_to_read, reviewed_row};
use super::{Context, Done, Failure, Reply, the_id};
use crate::cli::Args;
use crate::store::review::HISTORY_MAX;
use crate::store::{History, KnowledgeStore, ReviewEvent};
use crate::support::time::rfc3339;

/// `docs history`.
pub(super) fn history(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json"], &[]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let id = the_id(&parsed)?;
    let mut store = open_to_read(context, id)?;

    history_of(store.as_mut(), id)
}

/// The history of row `id` of `store`.
pub(super) fn history_of(store: &mut dyn KnowledgeStore, id: i64) -> Reply {
    let own = store.history(id)?.ok_or_else(|| missing(id))?;
    let pending = reviewed_row(store, id).map_err(super::review::failure)?;

    let merged = if pending == id {
        own
    } else {
        let newer = store.history(pending)?.unwrap_or_default();

        merge(newer, own)
    };

    Ok(Done::ok(json!({
        "events": merged.events.iter().map(event).collect::<Vec<_>>(),
        "total": merged.total,
    })))
}

/// Two histories as one, newest first: the events of `newer` come before the ones of `older` that
/// happened in the same second.
fn merge(newer: History, older: History) -> History {
    let total = newer.total + older.total;
    let mut events: Vec<ReviewEvent> = newer.events.into_iter().chain(older.events).collect();

    // A stable sort keeps the order inside each history, and the newer one's first on a tie.
    events.sort_by_key(|event| std::cmp::Reverse(event.at));
    events.truncate(HISTORY_MAX);

    History { events, total }
}

fn event(event: &ReviewEvent) -> Value {
    json!({
        "kind": event.kind.as_str(),
        "by": event.by,
        "at": rfc3339(event.at),
        "comment": event.comment,
    })
}
