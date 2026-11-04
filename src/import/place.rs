//! Where an import or an approval saves a document.
//!
//! The place comes from three things: the one asked for (`--place`, the manifest's `place`), the
//! document the import replaces (which lives in a place of its own) and the routing's default for the
//! kind of document. A document it replaces is in the place its id carries, and it can only be
//! replaced there: a different place asked for is refused, never followed by changing another
//! document that happens to have the same number.
//!
//! An id written without a place is the remote database's while both places are in use, and the
//! only place in use otherwise (see [`crate::store::routed::ids`]). A proposal that is held for
//! later is given its place when it is held ([`hold_replaces`]), because the routing can change
//! before it is approved, and the same number would then name a document in the other place.

use crate::config::{Kind, Place, Routing};
use crate::store::routed::ids;

/// The place of the document `id` names while `routing` is in use: the place it carries, else the
/// remote database's when both places are in use, else the only place.
pub fn place_in_use(routing: Routing, id: i64) -> Place {
    match (ids::split(id).0, routing.places().as_slice()) {
        (Some(place), _) => place,
        (None, [only]) => *only,
        (None, _) => Place::Remote,
    }
}

/// `id` with its place made explicit for a proposal that waits to be approved: tagged with the place
/// it names under `routing` now, even when only one place is in use. An id that names a place that
/// is not in use is refused.
pub fn hold_replaces(routing: Routing, id: i64) -> Result<i64, String> {
    let place = place_in_use(routing, id);

    if !routing.places().contains(&place) {
        return Err(format!(
            "replaces names a document {}, but that place is not in use",
            place.at()
        ));
    }

    Ok(ids::tag(place, ids::split(id).1))
}

/// The place a document of `kind` is saved in, or why it cannot be.
pub fn write_place(
    routing: Routing,
    requested: Option<Place>,
    replaces: Option<i64>,
    kind: Kind,
) -> Result<Place, String> {
    let replaced = replaces.map(|id| place_in_use(routing, id));

    if let Some(replaced) = replaced {
        if !routing.places().contains(&replaced) {
            return Err(format!(
                "replaces names a document {}, but that place is not in use",
                replaced.at()
            ));
        }

        if let Some(requested) = requested
            && requested != replaced
        {
            return Err(format!(
                "replaces names a document {}, but place is {}",
                replaced.at(),
                requested.as_str()
            ));
        }
    }

    let place = requested.or(replaced).unwrap_or_else(|| routing.default_place(kind));

    routing.check_write(kind, place)?;

    Ok(place)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Where;

    const BOTH: Routing = Routing {
        repository: Where::RemoteLocal,
        global: Where::RemoteLocal,
    };

    fn local(id: i64) -> i64 {
        ids::tag(Place::Local, id)
    }

    #[test]
    fn a_replacement_is_saved_where_the_document_it_replaces_is() {
        assert_eq!(write_place(BOTH, None, Some(local(3)), Kind::Global), Ok(Place::Local));
        assert_eq!(
            write_place(BOTH, None, Some(3), Kind::Global),
            Ok(Place::Remote),
            "a plain id is the remote database's"
        );
        assert_eq!(
            write_place(BOTH, Some(Place::Local), Some(local(3)), Kind::Global),
            Ok(Place::Local)
        );
        assert_eq!(write_place(BOTH, None, None, Kind::Global), Ok(Place::Remote));
    }

    #[test]
    fn a_place_that_disagrees_with_the_document_it_replaces_is_refused() {
        let refused = write_place(BOTH, Some(Place::Remote), Some(local(3)), Kind::Global).expect_err("refused");
        assert!(
            refused.contains("on this computer") && refused.contains("remote"),
            "{refused}"
        );

        let refused = write_place(BOTH, Some(Place::Local), Some(3), Kind::Global).expect_err("refused");
        assert!(
            refused.contains("in the remote database") && refused.contains("local"),
            "{refused}"
        );
    }

    #[test]
    fn an_id_naming_the_only_place_in_use_is_accepted_and_one_naming_the_other_is_refused() {
        let remote_only = Routing::new(Where::Remote, Where::Remote);

        assert_eq!(
            write_place(Routing::local(), None, Some(local(3)), Kind::Global),
            Ok(Place::Local)
        );
        assert_eq!(
            write_place(remote_only, None, Some(ids::tag(Place::Remote, 3)), Kind::Global),
            Ok(Place::Remote)
        );

        let refused =
            write_place(Routing::local(), None, Some(ids::tag(Place::Remote, 3)), Kind::Global).expect_err("refused");
        assert!(
            refused.contains("not in use") && refused.contains("remote"),
            "{refused}"
        );

        let refused = write_place(remote_only, None, Some(local(3)), Kind::Global).expect_err("refused");
        assert!(
            refused.contains("not in use") && refused.contains("this computer"),
            "{refused}"
        );

        for routing in [Routing::local(), remote_only] {
            assert!(
                write_place(routing, None, Some(3), Kind::Global).is_ok(),
                "a plain id is the only place's"
            );
        }
    }

    #[test]
    fn a_held_replacement_is_given_its_place_when_it_is_held() {
        let remote_only = Routing::new(Where::Remote, Where::Remote);

        assert_eq!(hold_replaces(Routing::local(), 12), Ok(local(12)));
        assert_eq!(hold_replaces(remote_only, 12), Ok(ids::tag(Place::Remote, 12)));
        assert_eq!(
            hold_replaces(BOTH, 12),
            Ok(ids::tag(Place::Remote, 12)),
            "a plain id is remote's"
        );
        assert_eq!(hold_replaces(BOTH, local(12)), Ok(local(12)));
        assert!(
            hold_replaces(remote_only, local(12)).is_err(),
            "this computer is not in use"
        );
        assert!(hold_replaces(Routing::local(), ids::tag(Place::Remote, 12)).is_err());
    }

    #[test]
    fn a_held_replacement_follows_its_place_when_the_routing_changes_and_is_refused_when_it_cannot() {
        let remote_only = Routing::new(Where::Remote, Where::Remote);
        let held = hold_replaces(Routing::local(), 12).expect("held under local");

        // Remote plus local: still this computer's document 12, never the remote's.
        assert_eq!(write_place(BOTH, None, Some(held), Kind::Global), Ok(Place::Local));

        // Remote alone: this computer is no longer used, so there is nothing it may replace.
        assert!(write_place(remote_only, None, Some(held), Kind::Global).is_err());
        assert!(write_place(remote_only, Some(Place::Remote), Some(held), Kind::Global).is_err());
    }
}
