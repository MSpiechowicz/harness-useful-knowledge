//! Part three: the repository and glob boosts. A cross-repository scenario (every document linked to
//! its own repository) shows what a boost costs a question whose answer is in another repository,
//! and a glob scenario (the hooks and the routines document answer the same questions) shows what
//! a glob that matches the file being edited is worth, and what it costs the other questions.

use super::Lab;
use super::recall::position;
use crate::project::ProjectKey;
use crate::search::tests::queries::{CASES, Case};
use crate::search::{CANDIDATES, QueryEmbedder};
use crate::store::fuse::{Scored, fuse};
use crate::store::{KnowledgeStore, Link, Scope, glob};

const DESKTOP: &str = "git:github.com/mspiechowicz/harness-useful-desktop";
const MEMORY: &str = "git:github.com/mspiechowicz/harness-useful-memory";
const SKILLS: &str = "git:github.com/mspiechowicz/harness-useful-skills";

/// The scope of a search from `repo` that sees every document.
fn from(repo: &str) -> Scope {
    Scope {
        repo: Some(ProjectKey {
            key: repo.to_owned(),
            root: std::path::PathBuf::new(),
            remote: None,
        }),
        include_other_repos: true,
        drafts: false,
    }
}

/// `fused` boosted as `rank` boosts it (without its floor), best first.
fn boosted(fused: Vec<Scored>, paths: &[String], linked: f64, glob_boost: f64) -> Vec<Scored> {
    let mut boosted: Vec<Scored> = fused
        .into_iter()
        .map(|scored| {
            let boost = match &scored.hit.repo_globs {
                None => 1.0,
                Some(globs) if glob::any_match(globs, paths) => glob_boost,
                Some(_) => linked,
            };

            Scored {
                score: scored.score * boost,
                ..scored
            }
        })
        .collect();

    boosted.sort_by(|a, b| b.score.total_cmp(&a.score));
    boosted
}

/// The fused candidates of `text` as seen from `scope`.
fn candidates(lab: &mut Lab, text: &str, scope: &Scope) -> Vec<Scored> {
    let embedding = lab.real.embed_query(text).expect("query vector");
    let keyword = lab.store.keyword_top_k(text, scope, CANDIDATES).expect("keyword");
    let vector = lab.store.vector_top_k(&embedding, scope, CANDIDATES).expect("vector");

    fuse(keyword, vector, CANDIDATES * 2)
}

pub(super) fn run(lab: &mut Lab) {
    let documents = lab.all_documents();

    repositories(lab, &documents);
    globs(lab, &documents);

    for (id, _) in documents {
        lab.store.set_links(id, &[]).expect("clear the links");
    }
}

fn link(repo: &str, globs: &[&str]) -> Vec<Link> {
    vec![Link {
        repo_key: repo.to_owned(),
        globs: globs.iter().map(|glob| (*glob).to_owned()).collect(),
    }]
}

/// The repository a document belongs to, by its source key.
fn repo_of(name: &str) -> &'static str {
    // The source key is the path of the copy: `<temp folder>/corpus/memory/docs/hybrid-search.md`.
    let name = name.rsplit_once("corpus/").map_or(name, |(_, relative)| relative);

    if name.starts_with("memory/") {
        MEMORY
    } else if name.starts_with("skills/") {
        SKILLS
    } else {
        DESKTOP
    }
}

/// The repositories of the documents `case` expects.
fn expected_repos(case: &Case) -> Vec<&'static str> {
    case.expect.iter().map(|name| repo_of(name)).collect()
}

fn repositories(lab: &mut Lab, documents: &[(i64, String)]) {
    for (id, name) in documents {
        lab.store.set_links(*id, &link(repo_of(name), &[])).expect("link");
    }

    eprintln!("\nrepository boost: every document linked to its own repository, scope sees all");
    eprintln!(
        "  boost | right repo: found@3 first | wrong repo: found@3 first (of {})",
        CASES.len()
    );

    for boost in [1.0, 1.02, 1.1, 1.25, 1.5, 2.0] {
        let mut right = (0, 0);
        let mut wrong = (0, 0);

        for case in CASES {
            let repos = expected_repos(case);
            let other = [DESKTOP, MEMORY, SKILLS]
                .into_iter()
                .find(|repo| !repos.contains(repo))
                .expect("a repository without the answer");

            for (context, tally) in [(repos[0], &mut right), (other, &mut wrong)] {
                let found = candidates(lab, case.query, &from(context));
                let ranked = lab.documents(&boosted(found, &[], boost, boost));
                let at = position(&ranked, case);

                tally.0 += usize::from(at.is_some_and(|at| at <= 3));
                tally.1 += usize::from(at == Some(1));
            }
        }

        eprintln!(
            "  {boost:>5.2} | {:>2} {:>2} | {:>2} {:>2}",
            right.0, right.1, wrong.0, wrong.1
        );
    }

    // The same plan lives in the desktop and in the memory repository: the copy of the repository
    // being worked in should win, whichever it is.
    eprintln!(
        "  twin documents (hybrid-search.md in two repositories): the copy of the working repository first, of 6"
    );

    for boost in [1.0, 1.02, 1.1, 1.25, 1.5] {
        let mut own = 0;

        for text in [
            "reciprocal rank fusion and spike thresholds for hybrid search",
            "hybrid search goals interfaces and additive extensions",
            "english notes and additive migration",
        ] {
            for (context, folder) in [(DESKTOP, "desktop"), (MEMORY, "memory")] {
                let found = candidates(lab, text, &from(context));
                let ranked = lab.documents(&boosted(found, &[], boost, boost));

                own += usize::from(
                    ranked
                        .iter()
                        .find(|name| name.ends_with("plans/hybrid-search.md"))
                        .is_some_and(|name| name.contains(&format!("/{folder}/"))),
                );
            }
        }

        eprintln!("  {boost:>5.2} | {own}");
    }

    for (id, _) in documents {
        lab.store.set_links(*id, &[]).expect("clear the links");
    }
}

/// Questions the hooks and the routines document both answer.
const AMBIGUOUS: [&str; 6] = [
    "how are approvals decided before it runs",
    "team sharing and who can change it",
    "validity summary",
    "project identity and repository key",
    "agent rules for the MCP server",
    "where are the files stored and the folders",
];

fn globs(lab: &mut Lab, documents: &[(i64, String)]) {
    let globs_of = |name: &str| -> Option<Vec<&'static str>> {
        let globs: &[&'static str] = match name.rsplit('/').next()? {
            "hooks-format.md" => &["src-tauri/src/hooks/**"],
            "routines-format.md" => &["src-tauri/src/routines/**"],
            "architecture.md" => &["src-tauri/src/**", "src/**"],
            "security-privacy.md" => &["src-tauri/**"],
            "share-setup.md" => &["src-tauri/src/share/**"],
            "adding-a-harness.md" => &["src-tauri/src/adapters/**"],
            _ => &[],
        };

        name.contains("/desktop/").then(|| globs.to_vec())
    };

    for (id, name) in documents {
        if let Some(globs) = globs_of(name) {
            lab.store.set_links(*id, &link(DESKTOP, &globs)).expect("link");
        }
    }

    let scope = from(DESKTOP);
    let hooks = vec!["src-tauri/src/hooks/mod.rs".to_owned()];
    let routines = vec!["src-tauri/src/routines/mod.rs".to_owned()];

    eprintln!("\nglob boost: the desktop documents linked with globs, working in the hooks or the routines code");
    eprintln!(
        "  linked glob | ambiguous top-1 right (of 12) | all questions from the hooks code: found@3 first (of {})",
        CASES.len()
    );

    for (linked, glob_boost) in [
        (1.0, 1.0),
        (1.02, 1.3),
        (1.1, 1.1),
        (1.1, 1.25),
        (1.1, 1.3),
        (1.1, 1.4),
        (1.1, 1.5),
        (1.1, 1.75),
        (1.25, 1.5),
    ] {
        let mut right = 0;

        for text in AMBIGUOUS {
            for (paths, expected) in [(&hooks, "hooks-format.md"), (&routines, "routines-format.md")] {
                let found = candidates(lab, text, &scope);
                let ranked = lab.documents(&boosted(found, paths, linked, glob_boost));
                right += usize::from(ranked.first().is_some_and(|name| name.ends_with(expected)));
            }
        }

        let mut found_three = 0;
        let mut first = 0;

        for case in CASES {
            let found = candidates(lab, case.query, &scope);
            let ranked = lab.documents(&boosted(found, &hooks, linked, glob_boost));
            let at = position(&ranked, case);

            found_three += usize::from(at.is_some_and(|at| at <= 3));
            first += usize::from(at == Some(1));
        }

        eprintln!("  {linked:>4.2} {glob_boost:>4.2} | {right:>2} | {found_three:>2} {first:>2}");
    }
}
