//! Part one: how often each list finds the answer in the first three documents, and the raw
//! cosines and relevances the floors have to separate.

use std::collections::HashMap;

use super::Lab;
use crate::search::tests::queries::{CASES, Case, NO_ANSWER};
use crate::search::{CANDIDATES, QueryEmbedder};
use crate::store::fuse::{Scored, fuse};
use crate::store::{ChunkHit, KnowledgeStore, Scope};

/// The two candidate lists of one query as the store returned them.
pub(super) struct Raw {
    pub text: &'static str,
    /// `None` for a query that has no answer.
    pub case: Option<&'static Case>,
    pub keyword: Vec<ChunkHit>,
    pub vector: Vec<ChunkHit>,
}

/// The candidate lists of every calibration query, answerable ones first.
pub(super) fn capture(lab: &mut Lab) -> Vec<Raw> {
    let scope = Scope::everything();
    let mut raws = Vec::new();

    let queries = CASES
        .iter()
        .map(|case| (case.query, Some(case)))
        .chain(NO_ANSWER.iter().map(|query| (*query, None)));

    for (text, case) in queries {
        let embedding = lab.real.embed_query(text).expect("query vector");

        raws.push(Raw {
            text,
            case,
            keyword: lab.store.keyword_top_k(text, &scope, CANDIDATES).expect("keyword"),
            vector: lab.store.vector_top_k(&embedding, &scope, CANDIDATES).expect("vector"),
        });
    }

    raws
}

/// The fusion of `raw` with the floors applied before it, as the search does.
pub(super) fn fused(raw: &Raw, cosine: f64, keyword: f64, use_keyword: bool, use_vector: bool) -> Vec<Scored> {
    fused_with_share(raw, cosine, keyword, 0.0, use_keyword, use_vector)
}

/// [`fused`] that also drops keyword candidates under `share` of the best keyword relevance.
pub(super) fn fused_with_share(
    raw: &Raw,
    cosine: f64,
    keyword: f64,
    share: f64,
    use_keyword: bool,
    use_vector: bool,
) -> Vec<Scored> {
    let best = raw.keyword.iter().map(|hit| hit.score).fold(0.0, f64::max);
    let keyword_hits: Vec<ChunkHit> = raw
        .keyword
        .iter()
        .filter(|hit| use_keyword && hit.score >= keyword && hit.score >= best * share)
        .cloned()
        .collect();
    let vector_hits: Vec<ChunkHit> = raw
        .vector
        .iter()
        .filter(|hit| use_vector && hit.score >= cosine)
        .cloned()
        .collect();

    fuse(keyword_hits, vector_hits, CANDIDATES * 2)
}

/// Where the first expected document is in `found` (1 is first), or `None`.
pub(super) fn position(found: &[String], case: &Case) -> Option<usize> {
    found
        .iter()
        .position(|name| case.expect.iter().any(|expected| name.ends_with(expected)))
        .map(|at| at + 1)
}

pub(super) fn run(lab: &mut Lab) {
    let raws = capture(lab);
    let mut totals: HashMap<(&str, &str), (usize, usize)> = HashMap::new();

    eprintln!("\n{:<4} {:>4} {:>4} {:>5}  query", "lang", "kw", "vec", "fused");

    for raw in raws.iter().filter(|raw| raw.case.is_some()) {
        let case = raw.case.expect("answerable");
        let mut at = Vec::new();

        for (keyword, vector) in [(true, false), (false, true), (true, true)] {
            let found = lab.documents(&fused(raw, -1.0, f64::MIN, keyword, vector));
            at.push(position(&found, case));
        }

        for (mode, position) in ["keyword", "vector", "fused"].into_iter().zip(&at) {
            let entry = totals.entry((case.lang, mode)).or_default();
            entry.1 += 1;
            entry.0 += usize::from(position.is_some_and(|at| at <= 3));
        }

        let show = |position: Option<usize>| position.map_or("-".to_owned(), |at| at.to_string());
        eprintln!(
            "{:<4} {:>4} {:>4} {:>5}  {}",
            case.lang,
            show(at[0]),
            show(at[1]),
            show(at[2]),
            case.query
        );
    }

    eprintln!("\nrecall@3 (documents, no floors)");

    for mode in ["keyword", "vector", "fused"] {
        let part = |lang: &str| totals.get(&(lang, mode)).copied().unwrap_or_default();
        let (en, pl) = (part("en"), part("pl"));

        eprintln!(
            "  {mode:<8} en {}/{}  pl {}/{}  all {}/{}",
            en.0,
            en.1,
            pl.0,
            pl.1,
            en.0 + pl.0,
            en.1 + pl.1
        );
    }

    // The fused list finds what the vectors find, and English needs no help from the vectors.
    let all = |mode: &str| totals[&("en", mode)].0 + totals[&("pl", mode)].0;
    assert!(all("fused") >= 36, "fused recall@3 over 40 queries: {}", all("fused"));
    assert!(all("fused") >= all("keyword") && all("fused") >= all("vector").saturating_sub(1));

    signals(lab, &raws);
}

/// The best cosine and relevance of each query.
fn signals(lab: &mut Lab, raws: &[Raw]) {
    eprintln!("\nbest cosine (any / expected doc), 50th cosine, best relevance (any / expected doc)");

    let mut answerable = (f64::MAX, f64::MAX, f64::MAX);
    let mut unanswerable = (0.0_f64, 0.0_f64);

    for raw in raws {
        let expected = |lab: &mut Lab, hits: &[ChunkHit]| {
            let Some(case) = raw.case else {
                return 0.0;
            };

            hits.iter()
                .filter(|hit| {
                    let name = lab.name(hit.doc_id);
                    case.expect.iter().any(|expected| name.ends_with(expected))
                })
                .map(|hit| hit.score)
                .fold(0.0, f64::max)
        };

        let cosine = raw.vector.first().map_or(0.0, |hit| hit.score);
        let background = raw.vector.last().map_or(0.0, |hit| hit.score);
        let relevance = raw.keyword.iter().map(|hit| hit.score).fold(0.0, f64::max);
        let cosine_expected = expected(lab, &raw.vector);
        let relevance_expected = expected(lab, &raw.keyword);

        eprintln!(
            "{} cos {cosine:.3}/{cosine_expected:.3} bg {background:.3} kw {relevance:>6.2}/{relevance_expected:>6.2}  {}",
            if raw.case.is_some() { "yes" } else { "NO " },
            raw.text
        );

        if raw.case.is_some() {
            answerable = (
                answerable.0.min(cosine),
                answerable.1.min(cosine_expected),
                answerable.2.min(relevance),
            );
        } else {
            unanswerable = (unanswerable.0.max(cosine), unanswerable.1.max(relevance));
        }
    }

    eprintln!(
        "\nbest cosine: answerable min {:.3} (expected doc min {:.3}), no-answer max {:.3}",
        answerable.0, answerable.1, unanswerable.0
    );
    eprintln!(
        "best relevance: answerable min {:.2}, no-answer max {:.2}",
        answerable.2, unanswerable.1
    );
}
