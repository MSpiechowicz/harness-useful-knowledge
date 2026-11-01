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
/// still beats a weak linked one. X6 (every family document linked to its own repository, 40
/// queries): 1.1 puts the right answer first 36 times instead of 31 and the copy of the working
/// repository first for 6 of 6 twin documents (3 of 6 without a boost), and costs a question
/// whose answer is in another repository one place in the first three (38 to 37). 1.25 costs 8
/// first places, 1.5 costs 9 and loses 3 of the first three.
pub const LINKED_BOOST: f64 = 1.1;

/// The whole boost of a chunk whose document's link has a glob that matches a given path (it
/// replaces [`LINKED_BOOST`], it does not multiply it). X6 (the hooks and the routines document
/// answering the same 6 questions, working in the code of one or the other): 1.1 resolves 4 of 12,
/// 1.25 and everything above resolves 8 of 12, and the cost to unrelated questions grows with the
/// boost (the right answer first 28 times at 1.3 and 25 at 1.75, of 31 without a boost).
pub const GLOB_BOOST: f64 = 1.3;

/// The least a chunk may score after the boosts. A chunk first in one list scores 500 and one in
/// the first 16 of one list at least 400, so this trims the tail of weak single-list matches. X6:
/// 250 and 400 give the same answers once the cosine floor below is applied.
/// Above 500 only chunks both lists found survive, which loses 14 of 40 answers.
pub const FLOOR: f64 = 400.0;

/// The share of the best chunk's score a chunk must reach. X6: 0.4 and 0.6 give the same answers.
pub const MARGIN: f64 = 0.4;

/// The least cosine a vector candidate needs to reach the fusion. The e5 cosines of a query and
/// its answer are compressed: the best chunk of 40 answerable queries reached 0.813 to 0.931 (all
/// the English ones 0.848 and up), and the best chunk of 6 queries with no answer 0.723 to 0.816.
/// X6: 0.80 leaves every one of the 40 queries a candidate and recall@3 at 38 of 40, 0.82 loses 2 more.
pub const COSINE_FLOOR: f64 = 0.80;

/// The least keyword relevance a candidate needs to reach the fusion: none. The relevance is the
/// backend's own score (the negated BM25 of FTS5, `ts_rank_cd` of PostgreSQL, the `searchScore` of
/// Atlas), so no number means the same on all of them, and it grows with the size of the corpus
/// (the inverse document frequency), so a floor that fits 1144 chunks silences the keyword list of
/// a corpus of 20. X6 (1144 chunks on SQLite): a floor of 8 puts the right answer first 34 times
/// of 40 instead of 31 and returns nothing for 4 of 6 off-topic queries instead of for none, with
/// the same recall@3 of 38. That does not justify a number only SQLite understands, so the vectors
/// (whose cosines are comparable everywhere) do the rejecting, and only the recall hook has a
/// keyword floor ([`RECALL_KEYWORD_FLOOR`]).
pub const KEYWORD_FLOOR: f64 = 0.0;

/// The cosine floor of the recall hook, which adds text to a prompt unasked, so it is stricter:
/// 0.82 is above every off-topic query of X6 (0.816 at most) and keeps 37 of 40 answers in the
/// first three documents (0.83 keeps 32).
pub const RECALL_COSINE_FLOOR: f64 = 0.82;

/// How far under the best chunk's cosine a chunk may be to be recalled with it: a prompt gets
/// the pointers that are as good as the best one, not three because three fit. X6: 0.02 keeps
/// recall@3 at 37 of 40 and cuts the pointers per answer from 2.8 to 2.2, 0.01 loses one answer.
pub const RECALL_COSINE_BAND: f64 = 0.02;

/// The relevance floor of the recall hook when the embedder is not available and only the keyword
/// list is read, in the scale of SQLite and libSQL (FTS5 BM25, see [`KEYWORD_FLOOR`]): 10 is above
/// every off-topic query of X6 (8.5 at most) and keeps 23 of 40 answers (the English half, a
/// Polish prompt over English documentation has no words in common). A corpus much smaller than
/// X6's scores lower and a team database scores in its own scale, so there the hook says less
/// without the embedder, never more.
pub const RECALL_KEYWORD_FLOOR: f64 = 10.0;

/// The least a chunk may score after the boosts to be recalled into a prompt. The recall hook
/// reads one list at a time, so a chunk scores 30500 / (60 + rank): 450 keeps the first 7.
pub const RECALL_FLOOR: f64 = 450.0;

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
