//! The checks every backend runs on its input before it opens a write transaction, so a rejected
//! call never holds the lock.
//!
//! A title becomes one line without control or invisible characters (see `display_safe`), so a
//! document cannot hide or reorder text on screen. Links are checked whole: counts first (an
//! oversized list costs nothing), then each repository key and glob. Repeats of a repository
//! merge into one link.

use std::collections::HashSet;

use super::{Chunk, DIMS, Link, StoreError, glob};
use crate::limits::{CHUNKS_PER_DOC_MAX, EMBED_BATCH_MAX, GLOB_MAX, GLOBS_MAX, LINKS_MAX, REPO_KEY_MAX, TITLE_MAX};
use crate::support::text::{display_safe, is_format_character};

/// A document title (1 to [`TITLE_MAX`] characters, one line).
pub fn title(raw: &str) -> Result<String, StoreError> {
    let title = line(raw);
    let length = title.chars().count();

    if length == 0 || length > TITLE_MAX {
        return Err(invalid(format!(
            "the title must be 1-{TITLE_MAX} characters (got {length})"
        )));
    }

    Ok(title)
}

/// `text` on one line: whitespace collapsed to single spaces, control and invisible characters
/// dropped, no leading or trailing space.
pub fn line(text: &str) -> String {
    let visible: String = text
        .chars()
        .filter_map(|character| {
            if character.is_whitespace() {
                Some(' ')
            } else if !character.is_control() && !is_format_character(character) {
                Some(character)
            } else {
                None
            }
        })
        .collect();

    visible.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A repository key: `git:` or `path:` and something after it, shown safely (a `git:` key has no
/// whitespace).
pub fn repo_key(raw: &str) -> Result<&str, StoreError> {
    let rest = raw.strip_prefix("git:").or_else(|| raw.strip_prefix("path:"));
    let is_git = raw.starts_with("git:");

    let fine = rest.is_some_and(|rest| !rest.is_empty())
        && raw.chars().count() <= REPO_KEY_MAX
        && display_safe(raw)
        && !(is_git && raw.chars().any(char::is_whitespace));

    if fine {
        Ok(raw)
    } else {
        Err(invalid(format!(
            "a repository is git:host/owner/repo or path:/folder, at most {REPO_KEY_MAX} characters"
        )))
    }
}

/// Whether `key` names a folder on this computer (`path:`), which no team database may store.
pub fn is_local_path(key: &str) -> bool {
    key.starts_with(LOCAL_PATH_PREFIX)
}

/// The start of a repository key that is a folder on this computer.
pub const LOCAL_PATH_PREFIX: &str = "path:";

/// The comment of the event a document gets when the folder links it had are taken out of a team
/// database and nothing else links it: it is a draft then, not a global document.
pub const FOLDER_LINK_REMOVED: &str = "folder link removed";

/// [`FOLDER_LINK_REMOVED`] for a live document that had a pending update, which went with it.
pub const FOLDER_LINK_REMOVED_PENDING: &str = "folder link removed, its pending update was dropped";

/// What a writer says on opening a team database that held folders of this computer: `removed`
/// links were taken out, or `left` could not be (the database is read-only for this user, a writer
/// will). Never a key.
pub fn report_local_paths(removed: usize, left: usize) {
    if removed > 0 {
        eprintln!("harness-knowledge: removed {removed} link(s) to folders of this computer from the team database");
    }

    if left > 0 {
        eprintln!(
            "harness-knowledge: {left} link(s) to folders of this computer remain in the team database: it is read-only for this user, a teammate who can write will remove them"
        );
    }
}

/// Whether `key` is a source key built from a folder of this computer (`path-` and a hash, see
/// `import::walk`), which no team database is asked about.
pub fn is_local_source_key(key: &str) -> bool {
    key.starts_with(LOCAL_SOURCE_PREFIX)
}

/// The start of a source key that stands for a folder of this computer.
pub const LOCAL_SOURCE_PREFIX: &str = "path-";

/// Refuses `links` that name a folder on this computer, for a team database: only a `git:` key
/// leaves this computer. The message names the folder (the last part of its path) and never the
/// path itself.
pub fn refuse_local_paths(links: &[Link]) -> Result<(), StoreError> {
    let Some(link) = links.iter().find(|link| is_local_path(&link.repo_key)) else {
        return Ok(());
    };

    Err(StoreError::LocalPath(local_path_message(&link.repo_key)))
}

/// Why a team document cannot be linked to the `path:` key `key`, naming only its folder.
pub fn local_path_message(key: &str) -> String {
    let rest = key.strip_prefix(LOCAL_PATH_PREFIX).unwrap_or(key);
    let folder = rest
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .map(line)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "the folder".to_owned());

    format!(
        "{folder} has no git remote, so a team document cannot be linked to it (only a git: project key leaves this computer)"
    )
}

/// [`links`] for a team database: the same checks, and a repository that is a folder on this
/// computer refused (see [`refuse_local_paths`]), before anything is written.
pub fn team_links(raw: &[Link]) -> Result<Vec<Link>, StoreError> {
    let checked = links(raw)?;
    refuse_local_paths(&checked)?;

    Ok(checked)
}

/// `links` checked and merged: at most [`LINKS_MAX`] repositories, each with at most
/// [`GLOBS_MAX`] globs of 1 to [`GLOB_MAX`] characters that compile (see [`glob`]).
pub fn links(links: &[Link]) -> Result<Vec<Link>, StoreError> {
    if links.len() > LINKS_MAX {
        return Err(invalid(format!("a document links to at most {LINKS_MAX} repositories")));
    }

    let mut merged: Vec<Link> = Vec::new();

    for link in links {
        let key = repo_key(&link.repo_key)?;

        if link.globs.len() > GLOBS_MAX {
            return Err(invalid(format!("a link has at most {GLOBS_MAX} globs")));
        }

        let at = match merged.iter().position(|seen| seen.repo_key == key) {
            Some(at) => at,
            None => {
                merged.push(Link {
                    repo_key: key.to_owned(),
                    globs: Vec::new(),
                });
                merged.len() - 1
            }
        };

        for raw in &link.globs {
            let pattern = raw.trim();

            if pattern.is_empty() || pattern.chars().count() > GLOB_MAX || !display_safe(pattern) {
                return Err(invalid(format!("a glob is 1-{GLOB_MAX} visible characters")));
            }

            glob::validate(pattern)?;

            if !merged[at].globs.iter().any(|seen| seen == pattern) {
                merged[at].globs.push(pattern.to_owned());
            }
        }

        if merged[at].globs.len() > GLOBS_MAX {
            return Err(invalid(format!("a link has at most {GLOBS_MAX} globs")));
        }
    }

    Ok(merged)
}

/// `chunks` of one document checked: at most [`CHUNKS_PER_DOC_MAX`], one position each, and a
/// vector only with its model (and the other way round), [`DIMS`] finite floats.
pub fn chunks(chunks: &[Chunk]) -> Result<(), StoreError> {
    if chunks.len() > CHUNKS_PER_DOC_MAX {
        return Err(invalid(format!("a document has at most {CHUNKS_PER_DOC_MAX} chunks")));
    }

    let mut ordinals: HashSet<usize> = HashSet::new();

    for chunk in chunks {
        if !ordinals.insert(chunk.ordinal) {
            return Err(invalid(format!("two chunks have the position {}", chunk.ordinal)));
        }

        match (&chunk.embedding, &chunk.model_id) {
            (None, None) => {}
            (Some(vector), Some(model)) if !model.is_empty() => {
                if vector.len() != DIMS || vector.iter().any(|value| !value.is_finite()) {
                    return Err(invalid(format!("an embedding is {DIMS} finite numbers")));
                }
            }
            _ => return Err(invalid("a chunk has an embedding and a model, or neither".to_owned())),
        }
    }

    Ok(())
}

/// `embeddings` of existing chunks, to be stored for `model_id`: a model, at most
/// [`EMBED_BATCH_MAX`] of them, [`DIMS`] finite floats each.
pub fn embeddings(model_id: &str, embeddings: &[(i64, Vec<f32>)]) -> Result<(), StoreError> {
    if model_id.is_empty() {
        return Err(invalid("an embedding needs the model that made it".to_owned()));
    }

    if embeddings.len() > EMBED_BATCH_MAX {
        return Err(invalid(format!("at most {EMBED_BATCH_MAX} embeddings at once")));
    }

    for (_, vector) in embeddings {
        if vector.len() != DIMS || vector.iter().any(|value| !value.is_finite()) {
            return Err(invalid(format!("an embedding is {DIMS} finite numbers")));
        }
    }

    Ok(())
}

fn invalid(message: String) -> StoreError {
    StoreError::Invalid(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(repo: &str, globs: &[&str]) -> Link {
        Link {
            repo_key: repo.to_owned(),
            globs: globs.iter().map(|glob| (*glob).to_owned()).collect(),
        }
    }

    #[test]
    fn a_title_is_one_visible_line_of_at_most_300_characters() {
        assert_eq!(title("  API\n\tguide\u{202e} ").expect("title"), "API guide");
        assert!(title(" \n ").is_err());
        assert!(title(&"a".repeat(TITLE_MAX)).is_ok());
        assert!(title(&"a".repeat(TITLE_MAX + 1)).is_err());
    }

    #[test]
    fn repository_keys_are_git_or_path_keys() {
        assert!(repo_key("git:github.com/o/r").is_ok());
        assert!(repo_key("path:/home/u/my project").is_ok());

        for bad in ["", "git:", "path:", "github.com/o/r", "*", "git:a b", "git:a\u{7}b"] {
            assert!(repo_key(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn links_are_bounded_and_repeats_merge() {
        let merged = links(&[
            link("git:h/o/r", &["src/**", " src/** "]),
            link("path:/w", &[]),
            link("git:h/o/r", &["docs/*.md"]),
        ])
        .expect("links");

        assert_eq!(
            merged,
            [link("git:h/o/r", &["src/**", "docs/*.md"]), link("path:/w", &[])]
        );

        let many: Vec<Link> = (0..=LINKS_MAX).map(|n| link(&format!("git:h/o/r{n}"), &[])).collect();
        assert!(links(&many).is_err());

        let globs: Vec<String> = (0..=GLOBS_MAX).map(|n| format!("a{n}/**")).collect();
        let too_many = Link {
            repo_key: "git:h/o/r".to_owned(),
            globs,
        };
        assert!(links(&[too_many]).is_err());

        assert!(links(&[link("git:h/o/r", &[&"a".repeat(GLOB_MAX + 1)])]).is_err());
        assert!(links(&[link("git:h/o/r", &["a".repeat(GLOB_MAX).as_str()])]).is_ok());
        assert!(links(&[link("git:h/o/r", &[" "])]).is_err());
        assert!(links(&[link("git:h/o/r", &["[unclosed"])]).is_err());
    }

    #[test]
    fn chunks_need_unique_positions_and_whole_embeddings() {
        let chunk = |ordinal: usize| Chunk {
            id: 0,
            doc_id: 0,
            ordinal,
            heading_path: String::new(),
            text: "text".to_owned(),
            chunk_sha: "sha".to_owned(),
            tokens: 1,
            char_start: 0,
            char_end: 4,
            embedding: None,
            model_id: None,
        };

        assert!(chunks(&[chunk(0), chunk(1)]).is_ok());
        assert!(chunks(&[chunk(0), chunk(0)]).is_err());

        let vector_without_model = Chunk {
            embedding: Some(vec![0.0; DIMS]),
            ..chunk(0)
        };
        assert!(chunks(std::slice::from_ref(&vector_without_model)).is_err());

        let whole = Chunk {
            model_id: Some("e5".to_owned()),
            ..vector_without_model
        };
        assert!(chunks(std::slice::from_ref(&whole)).is_ok());

        let short = Chunk {
            embedding: Some(vec![0.0; DIMS - 1]),
            ..whole.clone()
        };
        assert!(chunks(&[short]).is_err());

        let nan = Chunk {
            embedding: Some(vec![f32::NAN; DIMS]),
            ..whole
        };
        assert!(chunks(&[nan]).is_err());
    }
}
