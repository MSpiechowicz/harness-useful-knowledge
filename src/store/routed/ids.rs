//! Ids that carry their place.
//!
//! A routed store hands out the ids of the stores it wraps with the place tagged into the two
//! bits above any id a database counts to: [`REMOTE`] or [`LOCAL`]. Everything that passes ids
//! around (documents, chunks, sections, hits) then needs no extra field, and an id given back
//! reaches the store it came from. A single store (one place in use) hands out plain ids with no
//! tag, so nothing changes for it.
//!
//! Written for people and agents, a remote id stays the plain number (`12`, `12:340`) and a local
//! one is `local:12`, `local:12:340`. Reading takes both, a plain number is the remote database's
//! (or the only store's).

use serde_json::{Value, json};

use crate::config::Place;

/// The tag of an id from this computer's file.
const LOCAL: i64 = 1 << 62;

/// The tag of an id from the remote database.
const REMOTE: i64 = 1 << 61;

/// The prefix of a local id as it is written.
pub const LOCAL_PREFIX: &str = "local:";

/// The ids a database may count to: below both tags.
const MAX: i64 = REMOTE - 1;

/// `id` of `place`, tagged.
pub fn tag(place: Place, id: i64) -> i64 {
    match place {
        Place::Local => id | LOCAL,
        Place::Remote => id | REMOTE,
    }
}

/// The place an id carries (`None` for a plain id) and the id the store itself knows.
pub fn split(id: i64) -> (Option<Place>, i64) {
    let place = if id & LOCAL != 0 {
        Some(Place::Local)
    } else if id & REMOTE != 0 {
        Some(Place::Remote)
    } else {
        None
    };

    (place, id & MAX)
}

/// The place of an id, a plain id being the remote database's.
pub fn place_of(id: i64) -> Place {
    split(id).0.unwrap_or(Place::Remote)
}

/// The id as it is written: `12`, or `local:12` for an id from this computer.
pub fn show(id: i64) -> String {
    match split(id) {
        (Some(Place::Local), plain) => format!("{LOCAL_PREFIX}{plain}"),
        (_, plain) => plain.to_string(),
    }
}

/// The id as JSON: the number, or the string `local:12`.
pub fn json(id: i64) -> Value {
    match split(id) {
        (Some(Place::Local), plain) => json!(format!("{LOCAL_PREFIX}{plain}")),
        (_, plain) => json!(plain),
    }
}

/// The place field of an object about the document `id`: `Some("local" | "remote")` when the id
/// is tagged, so only a routed store has one.
pub fn place_name(id: i64) -> Option<&'static str> {
    split(id).0.map(Place::as_str)
}

/// A document or chunk id: a positive number, or `local:` and a positive number. `None` for
/// anything else (an id as large as a tag would be mistaken for one).
pub fn parse(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    let (place, digits) = match strip_prefix(raw) {
        Some(digits) => (Some(Place::Local), digits),
        None => (None, raw),
    };

    let number = digits
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| digits.parse::<i64>().ok())
        .flatten()
        .filter(|number| (1..=MAX).contains(number))?;

    Some(place.map_or(number, |place| tag(place, number)))
}

/// What a written reference names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reference {
    Document(i64),
    /// A chunk, with the document it belongs to (both carry the same tag).
    Chunk(i64, i64),
}

/// `12`, `local:12` (a document), `12:340`, `local:12:340` (a chunk of it).
pub fn parse_reference(raw: &str) -> Option<Reference> {
    let raw = raw.trim();
    let (place, rest) = match strip_prefix(raw) {
        Some(rest) => (Some(Place::Local), rest),
        None => (None, raw),
    };

    let tagged = |number: i64| place.map_or(number, |place| tag(place, number));
    let number = |text: &str| parse(text).filter(|id| split(*id).0.is_none());

    match rest.split_once(':') {
        None => number(rest).map(|doc| Reference::Document(tagged(doc))),
        Some((doc, chunk)) => Some(Reference::Chunk(tagged(number(doc)?), tagged(number(chunk)?))),
    }
}

/// A chunk as the tools show and read it: `12:340`, `local:12:340`.
pub fn chunk_ref(doc_id: i64, chunk_id: i64) -> String {
    let (_, chunk) = split(chunk_id);

    format!("{}:{chunk}", show(doc_id))
}

fn strip_prefix(raw: &str) -> Option<&str> {
    let head = raw.get(..LOCAL_PREFIX.len())?;

    head.eq_ignore_ascii_case(LOCAL_PREFIX)
        .then(|| &raw[LOCAL_PREFIX.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tagged_id_splits_back_into_its_place_and_id() {
        let local = tag(Place::Local, 12);
        let remote = tag(Place::Remote, 12);

        assert_eq!(split(local), (Some(Place::Local), 12));
        assert_eq!(split(remote), (Some(Place::Remote), 12));
        assert_eq!(split(12), (None, 12));
        assert_ne!(local, remote);
        assert!(remote < local, "remote ids sort before local ones");
        assert_eq!(place_of(12), Place::Remote);
    }

    #[test]
    fn local_ids_are_written_with_a_prefix_and_remote_ones_plain() {
        assert_eq!(show(tag(Place::Local, 12)), "local:12");
        assert_eq!(show(tag(Place::Remote, 12)), "12");
        assert_eq!(show(12), "12");
        assert_eq!(json(tag(Place::Local, 12)), json!("local:12"));
        assert_eq!(json(tag(Place::Remote, 12)), json!(12));
        assert_eq!(chunk_ref(tag(Place::Local, 12), tag(Place::Local, 340)), "local:12:340");
        assert_eq!(chunk_ref(tag(Place::Remote, 12), tag(Place::Remote, 340)), "12:340");
        assert_eq!(chunk_ref(12, 340), "12:340");
    }

    #[test]
    fn both_forms_are_read() {
        assert_eq!(parse("12"), Some(12));
        assert_eq!(parse("local:12"), Some(tag(Place::Local, 12)));
        assert_eq!(parse(" LOCAL:12 "), Some(tag(Place::Local, 12)));

        for bad in [
            "",
            "0",
            "-1",
            "local:",
            "local:0",
            "remote:1",
            "1.5",
            "12x",
            "local:local:1",
            "+3",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }

        assert_eq!(parse(&(1_i64 << 61).to_string()), None, "as large as a tag");
    }

    #[test]
    fn references_name_documents_and_chunks_in_both_forms() {
        let local = |id| tag(Place::Local, id);

        assert_eq!(parse_reference("12"), Some(Reference::Document(12)));
        assert_eq!(parse_reference("local:12"), Some(Reference::Document(local(12))));
        assert_eq!(parse_reference("12:340"), Some(Reference::Chunk(12, 340)));
        assert_eq!(
            parse_reference("local:12:340"),
            Some(Reference::Chunk(local(12), local(340)))
        );

        for bad in ["", "12:", ":3", "12:x", "12:3:4", "local:12:local:3", "0:1"] {
            assert_eq!(parse_reference(bad), None, "{bad:?}");
        }
    }
}
