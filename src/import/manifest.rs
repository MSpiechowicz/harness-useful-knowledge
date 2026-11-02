//! The manifest an import reads from stdin: what to import and where the documents link to.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

use crate::config::Place;
use crate::limits::FILES_PER_IMPORT_MAX;
use crate::store::Link;
use crate::store::routed::ids;

/// `{"paths": ["/docs/a.md"], "folders": ["/docs"], "links": [{"repo": "git:h/o/r", "globs": ["src/**"]}],
/// "replaces": 12, "replacing": {"/docs/a.md": "local:7"}, "place": "remote"}`. Every field is optional.
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
    /// The document each of several files replaces in place, by the file's path: a number, or
    /// `"local:12"` for a document on this computer. Every key is one of `paths`, the ids differ, and
    /// the place of each file is the place of its id. Not combined with `replaces`.
    #[serde(default, deserialize_with = "replacing")]
    pub replacing: BTreeMap<String, i64>,
    /// Where the documents are saved, `remote` or `local`: the routing's default for them when
    /// not given, and the place of the document `replaces` names.
    #[serde(default, deserialize_with = "place")]
    pub place: Option<Place>,
}

/// A document id as it is written: a number or text (`"12"`, `"local:12"`).
#[derive(Deserialize)]
#[serde(untagged)]
enum Written {
    Number(i64),
    Text(String),
}

impl Written {
    fn id(&self) -> Option<i64> {
        match self {
            Self::Number(number) => ids::parse(&number.to_string()),
            Self::Text(text) => ids::parse(text),
        }
    }
}

/// A document id written as a number or as text (`"12"`, `"local:12"`).
pub(crate) fn document_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
    let bad = || serde::de::Error::custom("replaces must be a document id");

    match Option::<Written>::deserialize(deserializer)? {
        None => Ok(None),
        Some(written) => written.id().map(Some).ok_or_else(bad),
    }
}

/// An object of file paths and the document ids they replace.
fn replacing<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<String, i64>, D::Error> {
    let written = BTreeMap::<String, Written>::deserialize(deserializer)?;

    written
        .into_iter()
        .map(|(path, written)| {
            written
                .id()
                .map(|id| (path, id))
                .ok_or_else(|| serde::de::Error::custom("replacing must map file paths to document ids"))
        })
        .collect()
}

/// `path` the way the walk names a folder: its canonical form, else as it is.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl Manifest {
    /// The document each file of `replacing` replaces, by the file's canonical path.
    pub fn replacing_by_path(&self) -> HashMap<PathBuf, i64> {
        self.replacing
            .iter()
            .map(|(path, id)| (canonical(Path::new(path)), *id))
            .collect()
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

    if let Some(id) = manifest.replaces {
        check_place(manifest.place, id)?;
    }

    check_replacing(&manifest)?;

    Ok(manifest)
}

/// Refuses a `replacing` that cannot be carried out: with `replaces`, too many files, a key that
/// is not one of `paths` (compared as canonical paths), two keys that are the same file, an id
/// twice, or an id of another place than `place`.
fn check_replacing(manifest: &Manifest) -> Result<(), String> {
    if manifest.replacing.is_empty() {
        return Ok(());
    }

    if manifest.replaces.is_some() {
        return Err("replacing cannot be combined with replaces".to_owned());
    }

    if manifest.replacing.len() > FILES_PER_IMPORT_MAX {
        return Err(format!(
            "replacing takes at most {FILES_PER_IMPORT_MAX} files, choose fewer"
        ));
    }

    let paths: HashSet<PathBuf> = manifest.paths.iter().map(|path| canonical(Path::new(path))).collect();
    let mut files = HashSet::new();
    let mut taken = HashSet::new();

    for (path, id) in &manifest.replacing {
        let file = canonical(Path::new(path));

        if !paths.contains(&file) {
            return Err(format!("replacing names {path}, which is not one of paths"));
        }

        if !files.insert(file) {
            return Err(format!("replacing names {path} twice"));
        }

        if !taken.insert(*id) {
            return Err(format!(
                "replacing names document {} for more than one file",
                ids::show(*id)
            ));
        }

        check_place(manifest.place, *id)?;
    }

    Ok(())
}

/// Refuses a `place` that is not the place the document `id` is in.
fn check_place(place: Option<Place>, id: i64) -> Result<(), String> {
    match place {
        Some(place) if ids::place_of(id) != place => Err(format!(
            "replaces names a document {}, but place is {}",
            if place == Place::Local {
                "of the remote database"
            } else {
                "on this computer"
            },
            place.as_str()
        )),
        _ => Ok(()),
    }
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

    #[test]
    fn replacing_maps_files_of_paths_to_document_ids_of_either_place() {
        let manifest = parse(r#"{"paths":["/a.md","/b.md","/c.md"],"replacing":{"/a.md":"local:12","/b.md":7}}"#)
            .expect("replacing");

        assert_eq!(manifest.replacing["/a.md"], ids::tag(Place::Local, 12));
        assert_eq!(manifest.replacing["/b.md"], 7);
        assert_eq!(manifest.replacing.len(), 2, "/c.md replaces nothing");

        assert_eq!(parse(r#"{"replacing":{}}"#).expect("empty").replacing.len(), 0);
        assert_eq!(parse(r#"{"paths":["/a.md"]}"#).expect("none").replacing.len(), 0);
    }

    #[test]
    fn replacing_refuses_what_cannot_be_carried_out() {
        for refused in [
            // A key that is not one of paths (a folder's file included).
            r#"{"paths":["/a.md"],"replacing":{"/b.md":1}}"#,
            r#"{"folders":["/d"],"replacing":{"/d/a.md":1}}"#,
            // An id twice, in either spelling of the remote place.
            r#"{"paths":["/a.md","/b.md"],"replacing":{"/a.md":1,"/b.md":"1"}}"#,
            // Not combined with replaces.
            r#"{"paths":["/a.md"],"replacing":{"/a.md":1},"replaces":2}"#,
            // Not document ids.
            r#"{"paths":["/a.md"],"replacing":{"/a.md":0}}"#,
            r#"{"paths":["/a.md"],"replacing":{"/a.md":"x"}}"#,
            r#"{"paths":["/a.md"],"replacing":{"/a.md":null}}"#,
            r#"{"paths":["/a.md"],"replacing":["/a.md"]}"#,
            // A place that is not the place of the document.
            r#"{"paths":["/a.md"],"replacing":{"/a.md":"local:1"},"place":"remote"}"#,
            r#"{"paths":["/a.md"],"replacing":{"/a.md":1},"place":"local"}"#,
        ] {
            assert!(parse(refused).is_err(), "{refused}");
        }

        assert!(parse(r#"{"paths":["/a.md"],"replacing":{"/a.md":"local:1"},"place":"local"}"#).is_ok());
    }

    #[test]
    fn replacing_compares_the_files_as_canonical_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("sub")).expect("folder");
        std::fs::write(dir.path().join("a.md"), "# A").expect("write");
        std::fs::write(dir.path().join("b.md"), "# B").expect("write");

        let plain = dir.path().join("a.md").to_string_lossy().into_owned();
        let roundabout = dir
            .path()
            .join("sub")
            .join("..")
            .join("a.md")
            .to_string_lossy()
            .into_owned();
        let other = dir.path().join("b.md").to_string_lossy().into_owned();
        let text = |paths: &[&str], replacing: &[(&str, u32)]| {
            serde_json::json!({ "paths": paths, "replacing": replacing.iter().cloned().collect::<std::collections::BTreeMap<_, _>>() })
                .to_string()
        };

        // The key spells the file another way than paths does.
        let manifest = parse(&text(&[&plain, &other], &[(&roundabout, 1)])).expect("same file");
        assert_eq!(
            manifest.replacing_by_path().get(&canonical(&dir.path().join("a.md"))),
            Some(&1)
        );

        // Two spellings of one file are two entries for it.
        assert!(parse(&text(&[&plain], &[(&plain, 1), (&roundabout, 2)])).is_err());
    }

    #[test]
    fn replacing_takes_at_most_as_many_files_as_an_import_does() {
        let paths: Vec<String> = (0..=FILES_PER_IMPORT_MAX).map(|n| format!("/f{n}.md")).collect();
        let replacing: BTreeMap<&String, usize> = paths.iter().zip(1..).collect();
        let text = serde_json::json!({ "paths": paths, "replacing": replacing }).to_string();

        assert!(parse(&text).is_err());
    }
}
