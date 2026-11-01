//! The manifest an import reads from stdin: what to import and where the documents link to.

use serde::{Deserialize, Deserializer};

use crate::config::Place;
use crate::store::Link;
use crate::store::routed::ids;

/// `{"paths": ["/docs/a.md"], "folders": ["/docs"], "links": [{"repo": "git:h/o/r", "globs": ["src/**"]}],
/// "replaces": 12, "place": "remote"}`. Every field is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Files to import, as absolute paths.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Folders to import every document of (see [`super::walk`]).
    #[serde(default)]
    pub folders: Vec<String>,
    /// The repositories every imported document links to, besides the links it already has.
    #[serde(default)]
    pub links: Vec<Link>,
    /// The id of a document the one file of this import replaces in place, whatever its source: a
    /// number, or `"local:12"` for a document on this computer (see [`ids`]).
    #[serde(default, deserialize_with = "document_id")]
    pub replaces: Option<i64>,
    /// Where the documents are saved, `remote` or `local`: the routing's default for them when
    /// not given, and the place of the document `replaces` names.
    #[serde(default, deserialize_with = "place")]
    pub place: Option<Place>,
}

/// A document id written as a number or as text (`"12"`, `"local:12"`).
pub(crate) fn document_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Written {
        Number(i64),
        Text(String),
    }

    let bad = || serde::de::Error::custom("replaces must be a document id");

    match Option::<Written>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Written::Number(number)) => ids::parse(&number.to_string()).map(Some).ok_or_else(bad),
        Some(Written::Text(text)) => ids::parse(&text).map(Some).ok_or_else(bad),
    }
}

/// `"remote"` or `"local"`.
fn place<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Place>, D::Error> {
    match Option::<String>::deserialize(deserializer)? {
        None => Ok(None),
        Some(text) => Place::parse(&text)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("place must be remote or local")),
    }
}

/// The manifest in `text`.
pub fn parse(text: &str) -> Result<Manifest, String> {
    let manifest: Manifest = serde_json::from_str(text).map_err(|_| {
        "the manifest must be JSON like {\"paths\": [...], \"folders\": [...], \"links\": [{\"repo\": \"git:host/owner/repo\", \
         \"globs\": [...]}], \"place\": \"remote\" or \"local\"}"
            .to_owned()
    })?;

    if let (Some(id), Some(place)) = (manifest.replaces, manifest.place)
        && ids::place_of(id) != place
    {
        return Err(format!(
            "replaces names a document {}, but place is {}",
            if place == Place::Local {
                "of the remote database"
            } else {
                "on this computer"
            },
            place.as_str()
        ));
    }

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_is_optional_and_unknown_ones_are_refused() {
        assert_eq!(parse("{}").expect("empty"), Manifest::default());

        let full = parse(
            r#"{"paths":["/a.md"],"folders":["/d"],"links":[{"repo":"git:h/o/r","globs":["src/**"]}],"replaces":3}"#,
        )
        .expect("full");
        assert_eq!(full.paths, ["/a.md"]);
        assert_eq!(full.links[0].repo_key, "git:h/o/r");
        assert_eq!(full.replaces, Some(3));

        assert!(parse(r#"{"files":[]}"#).is_err());
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"replaces":0}"#).is_err());
    }

    #[test]
    fn the_place_and_the_id_of_a_local_document_are_read_and_must_agree() {
        let local = parse(r#"{"replaces":"local:3","place":"local"}"#).expect("local");
        assert_eq!(local.place, Some(Place::Local));
        assert_eq!(local.replaces, Some(ids::tag(Place::Local, 3)));

        assert_eq!(
            parse(r#"{"place":" Remote "}"#).expect("remote").place,
            Some(Place::Remote)
        );
        assert_eq!(parse(r#"{"replaces":"7"}"#).expect("text").replaces, Some(7));
        assert_eq!(parse(r#"{"replaces":"local:3"}"#).expect("no place").place, None);

        assert!(parse(r#"{"place":"cloud"}"#).is_err());
        assert!(parse(r#"{"replaces":"local:3","place":"remote"}"#).is_err());
        assert!(parse(r#"{"replaces":3,"place":"local"}"#).is_err());
        assert!(parse(r#"{"replaces":"x"}"#).is_err());
    }
}
