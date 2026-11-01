//! Where an import or an approval saves a document.
//!
//! The place comes from three things: the one asked for (`--place`, the manifest's `place`), the
//! document the import replaces (which lives in a place of its own) and the routing's default for the
//! kind of document. A document it replaces is in the place its id carries, a plain id being the
//! remote database's (see [`crate::store::routed::ids`]), and it can only be replaced there: a
//! different place asked for is refused, never followed by changing another document that happens
//! to have the same number. Ids that carry a place exist only while both places are in use.

use crate::config::{Kind, Layout, Place, Routing};
use crate::store::routed::ids;

/// The place a document of `kind` is saved in, or why it cannot be.
pub fn write_place(
    routing: Routing,
    requested: Option<Place>,
    replaces: Option<i64>,
    kind: Kind,
) -> Result<Place, String> {
    let both = routing.layout() == Layout::Both;
    let replaced = replaces.map(ids::place_of);

    if let Some(id) = replaces {
        if !both && ids::split(id).0.is_some() {
            return Err(
                "replaces names a document by its place (local:N), which only exists while this computer and a remote \
                 database are both in use"
                    .to_owned(),
            );
        }

        if let (true, Some(requested), Some(replaced)) = (both, requested, replaced)
            && requested != replaced
        {
            return Err(format!(
                "replaces names a document {}, but place is {}",
                replaced.at(),
                requested.as_str()
            ));
        }
    }

    let place = requested
        .or(replaced.filter(|_| both))
        .unwrap_or_else(|| routing.default_place(kind));

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
    fn an_id_with_a_place_is_refused_while_one_place_is_in_use() {
        for routing in [Routing::local(), Routing::new(Where::Remote, Where::Remote)] {
            let refused = write_place(routing, None, Some(local(3)), Kind::Global).expect_err("refused");
            assert!(refused.contains("both in use"), "{refused}");

            assert!(
                write_place(routing, None, Some(3), Kind::Global).is_ok(),
                "a plain id is fine"
            );
        }
    }
}
