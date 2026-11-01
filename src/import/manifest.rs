//! The manifest an import reads from stdin: what to import and where the documents link to.

use serde::Deserialize;

use crate::store::Link;

/// `{"paths": ["/docs/a.md"], "folders": ["/docs"], "links": [{"repo": "git:h/o/r", "globs": ["src/**"]}],
/// "replaces": 12}`. Every field is optional.
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
    /// The id of a document the one file of this import replaces in place, whatever its source.
    #[serde(default)]
    pub replaces: Option<i64>,
}

/// The manifest in `text`.
pub fn parse(text: &str) -> Result<Manifest, String> {
    let manifest: Manifest = serde_json::from_str(text).map_err(|_| {
        "the manifest must be JSON like {\"paths\": [...], \"folders\": [...], \"links\": [{\"repo\": \"git:host/owner/repo\", \
         \"globs\": [...]}]}"
            .to_owned()
    })?;

    if manifest.replaces.is_some_and(|id| id < 1) {
        return Err("replaces must be a document id".to_owned());
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
}
