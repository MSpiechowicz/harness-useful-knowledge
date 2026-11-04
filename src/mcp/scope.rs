//! Where a tool call reads or writes: the `scope` argument and the repository `project_path` names
//! (else the one the server runs in).

use super::Server;
use crate::project::ProjectKey;
use crate::store::Scope;

/// Sent with a search that sees only global documents because no repository is known.
pub(super) const NO_PROJECT: &str = "(repository unknown, so only global documents are searched. Pass project_path, the \
     absolute path of the repository you are working in, for this repository's documents)";

/// The repository `project_path` names, else the one the server runs in; `None` outside any.
/// A `project_path` that is not an absolute existing folder is an error that says how to fix it.
pub(super) fn repository(server: &mut Server, project_path: Option<&str>) -> Result<Option<ProjectKey>, String> {
    server.projects.resolve(project_path).map_err(|reason| {
        format!(
            "{reason} (got {:?}). Pass project_path as the absolute path of the repository you are working in, \
             its git root",
            project_path.unwrap_or_default()
        )
    })
}

/// `repo` or `all`, the default `repo`; blanks around it and case are ignored.
pub(super) fn scope_arg(raw: Option<&str>) -> Result<bool, String> {
    let given = raw
        .map(|raw| raw.trim().to_ascii_lowercase())
        .filter(|raw| !raw.is_empty());

    match given.as_deref().unwrap_or("repo") {
        "repo" => Ok(false),
        "all" => Ok(true),
        _ => Err("scope must be repo or all".to_owned()),
    }
}

/// What a search may see: the global documents and those linked to `repo`, or with `everything`
/// every document (the repository still boosts its own). Always the live documents: an agent never
/// sees a draft (see [`Scope::with_drafts`], which this never calls).
pub(super) fn search_scope(repo: Option<ProjectKey>, everything: bool) -> Scope {
    let mut scope = Scope::from_repo(repo);
    scope.include_other_repos = everything;

    scope
}
