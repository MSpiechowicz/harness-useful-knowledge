//! What of the fused candidates is an answer, and in which order.
//!
//! 1. A chunk of a document linked to the repository the search is for scores
//!    [`LINKED_BOOST`] times its fused score, and [`GLOB_BOOST`] times when one of the link's globs
//!    matches a path the caller gave (the files the agent is working on).
//! 2. Chunks that score less than [`FLOOR`], or less than [`MARGIN`] of the best chunk, are
//!    dropped, so a long tail of weak matches does not follow one strong one.
//! 3. A document keeps at most [`PER_DOCUMENT_MAX`] chunks, its best.
//! 4. The best `limit` chunks are the answer.
//!
//! The scores are the fused scores of [`super::fuse`] (1000 for a chunk first in both lists).

use std::collections::HashMap;

use super::fuse::Scored;
use super::glob;

/// Multiplies the score of a chunk of a document linked to the repository of the search (a global
/// document is 1.0): an equally relevant linked document wins, a clearly more relevant global one
/// still beats a weak linked one. Provisional, calibrated in X6.
pub const LINKED_BOOST: f64 = 1.25;

/// The whole boost of a chunk whose document's link has a glob that matches a given path (it
/// replaces [`LINKED_BOOST`], it does not multiply it). Provisional, calibrated in X6.
pub const GLOB_BOOST: f64 = 1.5;

/// The least a chunk may score after the boosts. Provisional, calibrated in X6.
pub const FLOOR: f64 = 250.0;

/// The share of the best chunk's score a chunk must reach. Provisional, calibrated in X6.
pub const MARGIN: f64 = 0.4;

/// Most chunks of one document in an answer.
pub const PER_DOCUMENT_MAX: usize = 2;

/// The answer to a search: `fused` boosted, filtered and collapsed as the module docs say, best
/// first, at most `limit` chunks. `paths` are the repository-relative files the caller is working
/// on (may be empty).
pub fn rank(fused: Vec<Scored>, paths: &[String], limit: usize) -> Vec<Scored> {
    let mut boosted: Vec<Scored> = fused
        .into_iter()
        .map(|scored| Scored {
            score: scored.score * boost(&scored, paths),
            ..scored
        })
        .collect();

    boosted.sort_by(|a, b| b.score.total_cmp(&a.score));

    let best = boosted.first().map_or(0.0, |scored| scored.score);
    boosted.retain(|scored| scored.score >= FLOOR && scored.score >= best * MARGIN);

    let mut per_document: HashMap<i64, usize> = HashMap::new();
    boosted.retain(|scored| {
        let kept = per_document.entry(scored.hit.doc_id).or_default();
        *kept += 1;

        *kept <= PER_DOCUMENT_MAX
    });

    boosted.truncate(limit);
    boosted
}

/// What the link of the chunk's document to the search's repository is worth.
fn boost(scored: &Scored, paths: &[String]) -> f64 {
    match &scored.hit.repo_globs {
        None => 1.0,
        Some(globs) if glob::any_match(globs, paths) => GLOB_BOOST,
        Some(_) => LINKED_BOOST,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ChunkHit;

    fn scored(chunk_id: i64, doc_id: i64, score: f64, globs: Option<&[&str]>) -> Scored {
        Scored {
            hit: ChunkHit {
                chunk_id,
                doc_id,
                title: format!("document {doc_id}"),
                ordinal: 0,
                heading_path: String::new(),
                text: String::new(),
                score: 0.0,
                updated_at: 0,
                repo_globs: globs.map(|globs| globs.iter().map(|glob| (*glob).to_owned()).collect()),
            },
            score,
        }
    }

    fn chunk_ids(ranked: &[Scored]) -> Vec<i64> {
        ranked.iter().map(|scored| scored.hit.chunk_id).collect()
    }

    fn paths(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn a_linked_document_beats_an_equally_relevant_global_one() {
        let ranked = rank(vec![scored(1, 1, 800.0, None), scored(2, 2, 800.0, Some(&[]))], &[], 10);

        assert_eq!(chunk_ids(&ranked), [2, 1]);
        assert!((ranked[0].score - 800.0 * LINKED_BOOST).abs() < 1e-9);
    }

    #[test]
    fn a_clearly_more_relevant_global_document_still_wins() {
        let ranked = rank(
            vec![scored(1, 1, 1000.0, None), scored(2, 2, 700.0, Some(&[]))],
            &[],
            10,
        );

        assert_eq!(chunk_ids(&ranked), [1, 2]);
    }

    #[test]
    fn a_glob_that_matches_a_given_path_boosts_further() {
        let fused = || {
            vec![
                scored(1, 1, 800.0, Some(&["src/api/**"])),
                scored(2, 2, 800.0, Some(&["docs/**"])),
            ]
        };

        let ranked = rank(fused(), &paths(&["src/api/users.rs"]), 10);
        assert_eq!(chunk_ids(&ranked), [1, 2]);
        assert!(
            (ranked[0].score - 800.0 * GLOB_BOOST).abs() < 1e-9,
            "the glob boost is the whole boost"
        );
        assert!((ranked[1].score - 800.0 * LINKED_BOOST).abs() < 1e-9);

        // Without a path the globs are no signal.
        let ranked = rank(fused(), &[], 10);
        assert!((ranked[0].score - 800.0 * LINKED_BOOST).abs() < 1e-9);
    }

    #[test]
    fn weak_chunks_fall_below_the_floor_and_the_margin() {
        let ranked = rank(
            vec![
                scored(1, 1, 1000.0, None),
                scored(2, 2, 450.0, None),
                scored(3, 3, 390.0, None),
            ],
            &[],
            10,
        );
        assert_eq!(chunk_ids(&ranked), [1, 2], "390 is under 40% of the best");

        // Everything under the floor is nothing, even the best chunk.
        assert!(rank(vec![scored(1, 1, FLOOR - 1.0, None)], &[], 10).is_empty());
        assert!(rank(Vec::new(), &[], 10).is_empty());
    }

    #[test]
    fn a_document_keeps_its_two_best_chunks() {
        let ranked = rank(
            vec![
                scored(1, 1, 900.0, None),
                scored(2, 1, 850.0, None),
                scored(3, 1, 800.0, None),
                scored(4, 2, 700.0, None),
            ],
            &[],
            10,
        );

        assert_eq!(chunk_ids(&ranked), [1, 2, 4]);
    }

    #[test]
    fn the_limit_keeps_the_best() {
        let fused: Vec<Scored> = (1..=5).map(|id| scored(id, id, 900.0 - id as f64, None)).collect();

        assert_eq!(chunk_ids(&rank(fused, &[], 3)), [1, 2, 3]);
    }
}
