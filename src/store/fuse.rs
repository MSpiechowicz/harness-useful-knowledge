//! Hybrid search: the keyword list and the vector list of one query, fused into one.
//!
//! Reciprocal rank fusion: a chunk scores `1 / (K + rank)` in each list it is in (ranks count from
//! 1), so a chunk both lists find outranks a chunk one list finds, and neither list's own scale
//! matters. The keyword list is ordered by the keyword relevance alone. The repository and glob
//! boosts are not applied here, they apply to the fused score ([`super::rank`]). The score is
//! scaled so a chunk first in both lists scores 1000.

use std::collections::HashMap;

use super::ChunkHit;

/// The rank constant of the fusion: how fast a chunk's share falls with its rank.
const K: f64 = 60.0;

/// A chunk with its fused score.
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub hit: ChunkHit,
    /// 1000 for a chunk first in both lists, then boosted by [`super::rank`].
    pub score: f64,
}

/// What a chunk is worth in the lists it is in.
struct Entry {
    hit: ChunkHit,
    keyword: Option<usize>,
    vector: Option<usize>,
}

/// The best `limit` chunks of `keyword` (in the order of their relevance) and `vector` (the
/// closest first, only chunks the query may see), each with its fused score. A chunk in both
/// lists is kept as the keyword search found it.
pub fn fuse(keyword: Vec<ChunkHit>, vector: Vec<ChunkHit>, limit: usize) -> Vec<Scored> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut at: HashMap<i64, usize> = HashMap::new();

    for (rank, hit) in by_relevance(keyword).into_iter().enumerate() {
        at.insert(hit.chunk_id, entries.len());
        entries.push(Entry {
            hit,
            keyword: Some(rank + 1),
            vector: None,
        });
    }

    for (rank, hit) in vector.into_iter().enumerate() {
        match at.get(&hit.chunk_id) {
            Some(index) => entries[*index].vector = Some(rank + 1),
            None => {
                at.insert(hit.chunk_id, entries.len());
                entries.push(Entry {
                    hit,
                    keyword: None,
                    vector: Some(rank + 1),
                });
            }
        }
    }

    let mut fused: Vec<(Scored, bool)> = entries
        .into_iter()
        .map(|entry| {
            let share = |rank: Option<usize>| rank.map_or(0.0, |rank| 1.0 / (K + rank as f64));
            let score = (share(entry.keyword) + share(entry.vector)) * (K + 1.0) / 2.0 * 1000.0;

            (Scored { hit: entry.hit, score }, entry.keyword.is_some())
        })
        .collect();

    fused.sort_by(|(a, a_keyword), (b, b_keyword)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b_keyword.cmp(a_keyword))
            .then_with(|| b.hit.updated_at.cmp(&a.hit.updated_at))
            .then_with(|| b.hit.chunk_id.cmp(&a.hit.chunk_id))
    });
    fused.truncate(limit);

    fused.into_iter().map(|(scored, _)| scored).collect()
}

/// `hits` ordered by their keyword relevance; hits of equal relevance keep their order.
fn by_relevance(mut hits: Vec<ChunkHit>) -> Vec<ChunkHit> {
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A keyword hit of relevance `relevance`.
    fn keyword(id: i64, relevance: f64) -> ChunkHit {
        ChunkHit {
            chunk_id: id,
            doc_id: id,
            title: format!("document {id}"),
            ordinal: 0,
            heading_path: String::new(),
            text: "around the match".to_owned(),
            score: relevance,
            updated_at: 0,
            repo_globs: None,
            status: crate::store::Status::Live,
        }
    }

    fn vector(id: i64) -> ChunkHit {
        keyword(id, 0.9)
    }

    fn ids(hits: &[Scored]) -> Vec<i64> {
        hits.iter().map(|scored| scored.hit.chunk_id).collect()
    }

    #[test]
    fn a_chunk_in_both_lists_outranks_chunks_in_one() {
        let fused = fuse(vec![keyword(1, 9.0), keyword(2, 8.0)], vec![vector(3), vector(2)], 10);

        assert_eq!(ids(&fused), [2, 1, 3]);
        assert!(fused[0].score > fused[1].score);
    }

    #[test]
    fn a_chunk_only_the_vectors_find_surfaces() {
        let fused = fuse(Vec::new(), vec![vector(7)], 10);

        assert_eq!(ids(&fused), [7]);
        assert!((fused[0].score - 500.0).abs() < 1e-9);
    }

    #[test]
    fn a_chunk_first_in_both_lists_scores_a_thousand() {
        let fused = fuse(vec![keyword(1, 7.0)], vec![vector(1)], 10);

        assert!((fused[0].score - 1000.0).abs() < 1e-9);
        assert_eq!(fused[0].hit.score, 7.0, "the keyword hit is kept");
    }

    #[test]
    fn the_keyword_list_is_ordered_by_relevance_whatever_order_it_came_in() {
        let fused = fuse(vec![keyword(1, 3.0), keyword(2, 9.0)], Vec::new(), 10);

        assert_eq!(ids(&fused), [2, 1]);

        let first: f64 = (1.0 / 61.0) * 61.0 / 2.0 * 1000.0;
        let second: f64 = (1.0 / 62.0) * 61.0 / 2.0 * 1000.0;
        assert!((fused[0].score - first).abs() < 1e-9);
        assert!((fused[1].score - second).abs() < 1e-9);
    }

    #[test]
    fn the_limit_keeps_the_best() {
        let hits: Vec<ChunkHit> = (1..=5).map(vector).collect();

        assert_eq!(ids(&fuse(Vec::new(), hits, 2)), [1, 2]);
    }

    #[test]
    fn equal_scores_prefer_the_keyword_chunk() {
        let fused = fuse(vec![keyword(1, 5.0)], vec![vector(2)], 10);

        assert_eq!(ids(&fused), [1, 2]);
    }

    #[test]
    fn equal_scores_in_both_lists_prefer_the_newer_chunk() {
        // Chunk 1 is first by keyword and second by vector, chunk 2 the other way round.
        let older = ChunkHit {
            updated_at: 5,
            ..keyword(1, 9.0)
        };
        let newer = ChunkHit {
            updated_at: 10,
            ..keyword(2, 8.0)
        };
        let fused = fuse(
            vec![older.clone(), newer.clone()],
            vec![
                ChunkHit {
                    updated_at: 10,
                    ..vector(2)
                },
                ChunkHit {
                    updated_at: 5,
                    ..vector(1)
                },
            ],
            10,
        );

        assert!((fused[0].score - fused[1].score).abs() < 1e-9);
        assert_eq!(ids(&fused), [2, 1]);
    }
}
