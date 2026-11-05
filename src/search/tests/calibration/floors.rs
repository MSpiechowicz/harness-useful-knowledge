//! Part two: the floors and the margin. Reciprocal rank fusion only knows ranks, so the floors
//! before the fusion (a cosine for the vector list, a relevance for the keyword list) are what can
//! tell a query with no answer from one with an answer. The grid prints, for each combination, how
//! many answerable queries still have their document in the first three results and how many
//! queries with no answer still return something.

use std::collections::HashMap;

use super::Lab;
use super::recall::{Raw, capture, fused_with_share, position};
use crate::recall;
use crate::search::{self, Query, QueryEmbedder, Tuning};
use crate::store::Scope;
use crate::store::fuse::Scored;
use crate::store::rank;

/// `fused` after the floor and the margin, each document at most twice, as `rank` does without
/// boosts.
pub(super) fn finish(fused: Vec<Scored>, floor: f64, margin: f64) -> Vec<Scored> {
    let best = fused.first().map_or(0.0, |scored| scored.score);
    let mut per_document: HashMap<i64, usize> = HashMap::new();

    fused
        .into_iter()
        .filter(|scored| scored.score >= floor && scored.score >= best * margin)
        .filter(|scored| {
            let kept = per_document.entry(scored.hit.doc_id).or_default();
            *kept += 1;

            *kept <= 2
        })
        .collect()
}

/// What one setting did to the whole query set.
#[derive(Default, Clone, Copy)]
struct Outcome {
    /// Answerable queries whose document is in the first three results.
    found: usize,
    /// Answerable queries that returned something.
    answered: usize,
    /// Answerable queries whose first result is an expected document.
    first_right: usize,
    answerable: usize,
    /// Queries with no answer that returned something.
    noise: usize,
    unanswerable: usize,
    /// Answerable queries of the English half that returned nothing.
    english_empty: usize,
}

/// The lists a setting reads.
#[derive(Clone, Copy)]
struct Setting {
    cosine: f64,
    keyword: f64,
    share: f64,
    use_keyword: bool,
    use_vector: bool,
    floor: f64,
    margin: f64,
}

fn outcome(lab: &mut Lab, raws: &[Raw], setting: Setting) -> Outcome {
    let mut outcome = Outcome::default();

    for raw in raws {
        let candidates = fused_with_share(
            raw,
            setting.cosine,
            setting.keyword,
            setting.share,
            setting.use_keyword,
            setting.use_vector,
        );
        let ranked = finish(candidates, setting.floor, setting.margin);
        let documents = lab.documents(&ranked);

        match raw.case {
            Some(case) => {
                outcome.answerable += 1;
                outcome.answered += usize::from(!ranked.is_empty());
                outcome.found += usize::from(position(&documents, case).is_some_and(|at| at <= 3));
                outcome.first_right += usize::from(position(&documents, case) == Some(1));
                outcome.english_empty += usize::from(ranked.is_empty() && case.lang == "en");
            }
            None => {
                outcome.unanswerable += 1;
                outcome.noise += usize::from(!ranked.is_empty());
            }
        }
    }

    outcome
}

/// How far the best keyword candidate stands above the tenth: the one number of the keyword list
/// that does not depend on the backend's scale.
fn standout(raw: &Raw) -> f64 {
    let mut scores: Vec<f64> = raw.keyword.iter().map(|hit| hit.score).collect();
    scores.sort_by(|a, b| b.total_cmp(a));

    match (scores.first(), scores.get(9)) {
        (Some(best), Some(tenth)) if *tenth > 0.0 => best / tenth,
        _ => 0.0,
    }
}

pub(super) fn run(lab: &mut Lab) {
    let raws = capture(lab);

    eprintln!("\nkeyword standout (best / tenth), answerable then no answer");

    for raw in &raws {
        eprintln!(
            "  {} {:>5.2} cand {:>2}  {}",
            if raw.case.is_some() { "yes" } else { "NO " },
            standout(raw),
            raw.keyword.len(),
            raw.text
        );
    }

    eprintln!(
        "\nsearch (both lists): cosine kw floor margin | found@3 answered first right | noise (of 40 answerable, 6 not)"
    );

    for cosine in [0.78, 0.80, 0.82] {
        for keyword in [2.0, 3.0, 5.0, 8.0] {
            for (floor, margin) in [(250.0, 0.4), (400.0, 0.4), (400.0, 0.6)] {
                let setting = Setting {
                    cosine,
                    keyword,
                    share: 0.0,
                    use_keyword: true,
                    use_vector: true,
                    floor,
                    margin,
                };
                let o = outcome(lab, &raws, setting);

                eprintln!(
                    "  cos {cosine:.2} kw {keyword:>4.1} floor {floor:>3.0} margin {margin:.1} | {:>2} {:>2} {:>2} | noise {}",
                    o.found, o.answered, o.first_right, o.noise
                );
            }
        }
    }

    eprintln!(
        "\nsearch with a relative keyword floor (share of the best keyword relevance): cosine share | found@3 answered first right | noise"
    );

    for share in [0.0, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7] {
        let setting = Setting {
            cosine: 0.80,
            keyword: 0.0,
            share,
            use_keyword: true,
            use_vector: true,
            floor: 400.0,
            margin: 0.4,
        };
        let o = outcome(lab, &raws, setting);

        eprintln!(
            "  cos 0.80 share {share:.1} | {:>2} {:>2} {:>2} | noise {}",
            o.found, o.answered, o.first_right, o.noise
        );
    }

    eprintln!("\nrecall with the embedder (vector list alone): cosine | found@3 answered first right | noise");

    for cosine in [0.80, 0.81, 0.82, 0.83, 0.84, 0.85] {
        let setting = Setting {
            cosine,
            keyword: 0.0,
            share: 0.0,
            use_keyword: false,
            use_vector: true,
            floor: 0.0,
            margin: 0.4,
        };
        let o = outcome(lab, &raws, setting);

        eprintln!(
            "  cos {cosine:.2} | {:>2} {:>2} {:>2} | noise {}",
            o.found, o.answered, o.first_right, o.noise
        );
    }

    eprintln!(
        "\nrecall pointers kept within a band of the best cosine: band | found@3 answered first right | pointers per answer"
    );

    for band in [0.01, 0.02, 0.03, 0.04, 0.06, 1.0] {
        let (mut found, mut answered, mut first, mut pointers) = (0, 0, 0, 0);

        for raw in raws.iter().filter(|raw| raw.case.is_some()) {
            let case = raw.case.expect("answerable");
            let hits: Vec<Scored> = fused_with_share(raw, 0.82, 0.0, 0.0, false, true);
            let top = hits.first().map_or(0.0, |scored| scored.hit.score);
            let kept: Vec<Scored> = hits
                .into_iter()
                .filter(|scored| scored.hit.score >= top - band)
                .collect();
            let documents = lab.documents(&kept);
            let documents: Vec<String> = documents.into_iter().take(3).collect();

            answered += usize::from(!documents.is_empty());
            pointers += documents.len();
            found += usize::from(position(&documents, case).is_some());
            first += usize::from(position(&documents, case) == Some(1));
        }

        eprintln!(
            "  band {band:.2} | {found:>2} {answered:>2} {first:>2} | {:.2}",
            pointers as f64 / answered.max(1) as f64
        );
    }

    eprintln!("\nrecall without it (keyword list alone): kw | found@3 answered first right | noise");

    for keyword in [8.0, 10.0, 12.0, 14.0, 16.0, 20.0] {
        let setting = Setting {
            cosine: 0.0,
            keyword,
            share: 0.0,
            use_keyword: true,
            use_vector: false,
            floor: 0.0,
            margin: 0.4,
        };
        let o = outcome(lab, &raws, setting);

        eprintln!(
            "  kw {keyword:>4.1} | {:>2} {:>2} {:>2} | noise {}",
            o.found, o.answered, o.first_right, o.noise
        );
    }

    let shipped = |lab: &mut Lab, setting: Setting| outcome(lab, &raws, setting);

    let search = shipped(
        lab,
        Setting {
            cosine: rank::COSINE_FLOOR,
            keyword: rank::KEYWORD_FLOOR,
            share: 0.0,
            use_keyword: true,
            use_vector: true,
            floor: rank::FLOOR,
            margin: rank::MARGIN,
        },
    );
    eprintln!(
        "\nSEARCH settings: found@3 {}/{} answered {} first right {} noise {}/{}",
        search.found, search.answerable, search.answered, search.first_right, search.noise, search.unanswerable
    );
    assert!(search.found >= 37, "search recall@3 over 40 queries: {}", search.found);

    let by_vector = shipped(
        lab,
        Setting {
            cosine: rank::RECALL_COSINE_FLOOR,
            keyword: 0.0,
            share: 0.0,
            use_keyword: false,
            use_vector: true,
            floor: rank::RECALL_FLOOR,
            margin: rank::MARGIN,
        },
    );
    eprintln!(
        "RECALL with the embedder: found@3 {}/{} answered {} first right {} noise {}/{}",
        by_vector.found,
        by_vector.answerable,
        by_vector.answered,
        by_vector.first_right,
        by_vector.noise,
        by_vector.unanswerable
    );
    assert_eq!(
        by_vector.noise, 0,
        "the recall hook adds nothing to a prompt that has no answer"
    );
    assert!(by_vector.found >= 34, "recall hook found@3: {}", by_vector.found);

    let by_keyword = shipped(
        lab,
        Setting {
            cosine: 0.0,
            keyword: rank::RECALL_KEYWORD_FLOOR,
            share: 0.0,
            use_keyword: true,
            use_vector: false,
            floor: rank::RECALL_FLOOR,
            margin: rank::MARGIN,
        },
    );
    eprintln!(
        "RECALL without it: found@3 {}/{} answered {} first right {} noise {}/{}",
        by_keyword.found,
        by_keyword.answerable,
        by_keyword.answered,
        by_keyword.first_right,
        by_keyword.noise,
        by_keyword.unanswerable
    );
    assert_eq!(
        by_keyword.noise, 0,
        "the keyword-only recall adds nothing to a prompt that has no answer"
    );

    through_the_code(lab, &raws);
}

/// The shipped settings through the code the commands run (`search::run`, `recall::recalled`), not
/// through this file's replicas of it: the same outcomes.
fn through_the_code(lab: &mut Lab, raws: &[Raw]) {
    let scope = Scope::everything();
    let mut searched = (0, 0);
    let mut recalled_by_vector = (0, 0);
    let mut recalled_by_keyword = (0, 0);

    for raw in raws {
        let query = Query {
            text: raw.text,
            scope: &scope,
            paths: &[],
            limit: 10,
        };

        let found = search::run(&mut lab.store, Some(&mut lab.real), &query, Tuning::SEARCH).expect("search");
        let by_vector = recall::recalled(&mut lab.store, &mut lab.real, &query)
            .expect("recall")
            .0;
        let by_keyword = recall::recalled(&mut lab.store, &mut Down, &query)
            .expect("recall without the embedder")
            .0;

        let tally = |lab: &mut Lab, hits: &[Scored], tally: &mut (usize, usize)| match raw.case {
            Some(case) => {
                let documents = lab.documents(hits);
                tally.0 += usize::from(position(&documents, case).is_some_and(|at| at <= 3));
            }
            None => tally.1 += usize::from(!hits.is_empty()),
        };

        tally(lab, &found.hits, &mut searched);
        tally(lab, &by_vector, &mut recalled_by_vector);
        tally(lab, &by_keyword, &mut recalled_by_keyword);
    }

    eprintln!(
        "\nthrough the code (found@3 of 40, off-topic answered of 6): search {searched:?}, recall {recalled_by_vector:?}, \
         recall without the embedder {recalled_by_keyword:?}"
    );

    assert!(searched.0 >= 37, "search recall@3: {}", searched.0);
    assert!(
        recalled_by_vector.0 >= 34,
        "recall hook found@3: {}",
        recalled_by_vector.0
    );
    assert_eq!(
        recalled_by_vector.1, 0,
        "the recall hook says nothing to an off-topic prompt"
    );
    assert!(
        recalled_by_keyword.0 >= 20,
        "recall without the embedder found@3: {}",
        recalled_by_keyword.0
    );
    assert_eq!(recalled_by_keyword.1, 0, "nor without the embedder");
}

/// An embedder that is not there.
struct Down;

impl QueryEmbedder for Down {
    fn embed_query(&mut self, _text: &str) -> Result<Vec<f32>, String> {
        Err("the embedder is not reachable (test)".to_owned())
    }
}
