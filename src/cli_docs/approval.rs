//! `approval get --json` and `approval set --json on|off`: team approval, a setting of the team
//! database (see [`crate::review`]).
//!
//! Both print `{"approval": bool, "place": "remote"|"local", "writable": bool|null, "changedBy":
//! string|null, "changedAt": string|null}`: `place` is `remote` while a team database is in use,
//! else `local` (where approval is always off), `writable` whether the user may change the setting
//! (`null` when that is not known, and for this computer), and `changedBy` and `changedAt` who
//! last switched it (a git `user.name`) and when (RFC 3339, UTC), `null` while unknown. `set` is
//! for a user who may write to the team database (`readOnly` otherwise, and `refused` with no team
//! database) and has a git `user.name` (`noName` otherwise), applies at once, records who and when
//! with the switch and changes no document. A team database that cannot be reached is
//! `unreachable`, never this computer's answer.

use serde_json::json;

use super::review::{failure, refused};
use super::{Context, Done, Failure, Reply};
use crate::backend::Access;
use crate::cli::Args;
use crate::config::Place;
use crate::review::{self, ApprovalState};

/// `approval get|set`, as the person at the terminal.
pub(super) fn approval(context: &Context, args: &[Option<&str>]) -> Reply {
    // Only a switch needs to know who the person is.
    let actor = match args {
        [Some("set"), ..] => super::review::actor(context),
        _ => None,
    };

    approval_as(context, args, actor.as_deref())
}

/// [`approval`] for the person named `actor`.
pub(super) fn approval_as(context: &Context, args: &[Option<&str>], actor: Option<&str>) -> Reply {
    match args {
        [Some("get"), rest @ ..] => get(context, rest),
        [Some("set"), rest @ ..] => set(context, rest, actor),
        _ => Err(Failure::Usage),
    }
}

fn shown(state: &ApprovalState) -> Done {
    Done::ok(json!({
        "approval": state.approval,
        "place": state.place.as_str(),
        "writable": state.writable,
        "changedBy": state.changed_by,
        "changedAt": state.changed_at,
    }))
}

fn get(context: &Context, args: &[Option<&str>]) -> Reply {
    let parsed = Args::parse(args, &["--json"], &[]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") || !parsed.positional.is_empty() {
        return Err(Failure::Usage);
    }

    let remote = context.routing.uses(Place::Remote);
    let state = if remote {
        // The remote database alone: never this computer's file.
        match context.open_place(Place::Remote, Access::Read)? {
            Some(mut store) => review::approval_state(store.as_mut(), true).map_err(failure)?,
            None => ApprovalState::unset(),
        }
    } else {
        ApprovalState::local()
    };

    Ok(shown(&state))
}

fn set(context: &Context, args: &[Option<&str>], actor: Option<&str>) -> Reply {
    let parsed = Args::parse(args, &["--json"], &[]).ok_or(Failure::Usage)?;

    if !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let on = match parsed.positional.as_slice() {
        ["on"] => true,
        ["off"] => false,
        _ => return Err(Failure::Usage),
    };

    if !context.routing.uses(Place::Remote) {
        return Err(refused(
            "refused",
            "there is no team database: team approval is a setting of the shared database",
        ));
    }

    // The team database is created by the first switch when it has no tables yet.
    let mut store = context
        .open_place_to_change(Place::Remote, Access::Create)?
        .ok_or_else(|| Failure::Failed("the team database could not be created".to_owned()))?;
    let state = review::set_approval(store.as_mut(), true, on, actor).map_err(failure)?;

    Ok(shown(&state))
}
