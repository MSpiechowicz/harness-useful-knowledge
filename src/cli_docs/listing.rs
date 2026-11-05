//! The arguments of `docs list`.

use super::Failure;
use crate::cli::Args;
use crate::config::Place;
use crate::limits::{LIST_PAGE_DEFAULT, LIST_PAGE_MAX};
use crate::store::{ListQuery, RepoFilter, Sort, StatusFilter, UsefulFilter, clean};

/// The query of `docs list` and the place `--place` narrows it to.
pub(super) fn parse_list(args: &[Option<&str>]) -> Result<(ListQuery, Option<Place>), Failure> {
    let valued = [
        "--repo",
        "--format",
        "--query",
        "--sort",
        "--page",
        "--per-page",
        "--place",
        "--useful",
        "--status",
    ];
    let parsed = Args::parse(args, &["--json", "--global", "--all", "--desc"], &valued).ok_or(Failure::Usage)?;

    if !parsed.positional.is_empty() || !parsed.flag("--json") {
        return Err(Failure::Usage);
    }

    let filter = match (parsed.value("--repo"), parsed.flag("--global"), parsed.flag("--all")) {
        (None, false, _) => RepoFilter::All,
        (None, true, false) => RepoFilter::Global,
        (Some(key), false, false) => RepoFilter::Repo(clean::repo_key(key).map_err(|_| Failure::Usage)?.to_owned()),
        _ => return Err(Failure::Usage),
    };

    let format = match parsed.value("--format") {
        Some(format) => Some(parse_format(format)?),
        None => None,
    };

    let text = parsed.value("--query").unwrap_or_default().trim().to_owned();

    if text.chars().count() > QUERY_CHARS_MAX {
        return Err(Failure::Usage);
    }

    let sort = match parsed.value("--sort") {
        None | Some("updated") => Sort::Updated,
        Some("title") => Sort::Title,
        Some("size") => Sort::Size,
        Some("format") => Sort::Format,
        Some("chunks") => Sort::Chunks,
        Some("paths") => Sort::Paths,
        Some("repos") => Sort::Repos,
        Some("useful") => Sort::Useful,
        Some("lastUseful") => Sort::LastUseful,
        Some(_) => return Err(Failure::Usage),
    };

    let useful = match parsed.value("--useful") {
        None | Some("any") => None,
        Some(raw) => Some(UsefulFilter::parse(raw).ok_or(Failure::Usage)?),
    };

    let status = match parsed.value("--status") {
        None => StatusFilter::All,
        Some(raw) => StatusFilter::parse(raw).ok_or(Failure::Usage)?,
    };

    let page = match parsed.value("--page") {
        Some(page) => page
            .parse::<usize>()
            .ok()
            .filter(|page| *page >= 1)
            .ok_or(Failure::Usage)?,
        None => 1,
    };

    let per_page = match parsed.value("--per-page") {
        Some(per_page) => per_page
            .parse::<usize>()
            .ok()
            .filter(|per_page| (1..=LIST_PAGE_MAX).contains(per_page))
            .ok_or(Failure::Usage)?,
        None => LIST_PAGE_DEFAULT,
    };

    let place = match parsed.value("--place") {
        Some(place) => Some(Place::parse(place).ok_or(Failure::Usage)?),
        None => None,
    };

    let query = ListQuery {
        filter,
        format,
        text,
        useful,
        status,
        sort,
        descending: parsed.flag("--desc"),
        offset: (page - 1).checked_mul(per_page).ok_or(Failure::Usage)?,
        limit: per_page,
    };

    Ok((query, place))
}

/// Most characters of `--query`.
const QUERY_CHARS_MAX: usize = 200;

/// A format name: lowercase letters, digits, `-` and `_`, 1 to 32 characters.
fn parse_format(raw: &str) -> Result<String, Failure> {
    let fine = (1..=32).contains(&raw.len())
        && raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');

    if fine { Ok(raw.to_owned()) } else { Err(Failure::Usage) }
}
