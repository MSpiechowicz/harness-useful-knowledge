//! The path globs of a link: which files of a repository a document is about.
//!
//! A glob is matched against repository-relative paths with `/` separators. `*` and `?` stay
//! inside one path segment and `**` crosses segments, so `src/*.rs` is the Rust files directly in
//! `src` and `src/**/*.rs` all of those below it. As in a `.gitignore`, a glob without a `/`
//! matches at any depth (`*.rs` is every Rust file), a leading `/` anchors it to the root and a
//! trailing `/` stands for everything below that folder.

use globset::{GlobBuilder, GlobMatcher};

use super::StoreError;

/// The matcher of `pattern`.
fn matcher(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    let rooted = pattern.strip_prefix('/');
    let mut glob = rooted.unwrap_or(pattern).to_owned();

    if glob.ends_with('/') {
        glob.push_str("**");
    }

    if rooted.is_none() && !glob.contains('/') {
        glob.insert_str(0, "**/");
    }

    let glob = GlobBuilder::new(&glob).literal_separator(true).build()?;
    Ok(glob.compile_matcher())
}

/// Fails when `pattern` is not a glob.
pub fn validate(pattern: &str) -> Result<(), StoreError> {
    matcher(pattern)
        .map(drop)
        .map_err(|_| StoreError::Invalid(format!("not a valid glob: {}", pattern.escape_debug())))
}

/// `path` with `/` separators and no leading `./` or `/`.
fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_start_matches("./");

    path.trim_start_matches('/').to_owned()
}

/// True when any of `globs` matches any of `paths`. A glob that does not compile matches nothing.
pub fn any_match(globs: &[String], paths: &[String]) -> bool {
    let paths: Vec<String> = paths.iter().map(|path| normalize(path)).collect();

    globs
        .iter()
        .filter_map(|glob| matcher(glob).ok())
        .any(|matcher| paths.iter().any(|path| matcher.is_match(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(glob: &str, path: &str) -> bool {
        any_match(&[glob.to_owned()], &[path.to_owned()])
    }

    #[test]
    fn a_star_stays_in_a_segment_and_two_stars_cross() {
        assert!(matches("src/*.rs", "src/main.rs"));
        assert!(!matches("src/*.rs", "src/api/mod.rs"));
        assert!(matches("src/**/*.rs", "src/api/mod.rs"));
        assert!(matches("src/**", "src/a/b/c.md"));
        assert!(!matches("src/**", "docs/a.md"));
    }

    #[test]
    fn a_glob_without_a_slash_matches_at_any_depth() {
        assert!(matches("*.rs", "main.rs"));
        assert!(matches("*.rs", "src/deep/main.rs"));
        assert!(!matches("*.rs", "main.py"));
    }

    #[test]
    fn a_leading_slash_anchors_and_a_trailing_slash_means_below() {
        assert!(matches("/Cargo.toml", "Cargo.toml"));
        assert!(!matches("/Cargo.toml", "sub/Cargo.toml"));
        assert!(matches("docs/", "docs/a/b.md"));
        assert!(!matches("docs/", "other/docs/b.md"));
    }

    #[test]
    fn paths_are_normalised() {
        assert!(matches("src/**", "./src/a.rs"));
        assert!(matches("src/**", "/src/a.rs"));
        assert!(matches("src/**", "src\\a.rs"));
    }

    #[test]
    fn invalid_globs_are_refused_and_match_nothing() {
        assert!(validate("src/**").is_ok());
        assert!(validate("[unclosed").is_err());
        assert!(!matches("[unclosed", "a"));
        assert!(!any_match(&[], &["a".to_owned()]));
    }
}
