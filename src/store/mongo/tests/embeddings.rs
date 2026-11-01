//! Embeddings stored after the fact, the way a reindex does.

use super::{eventually, insert, store};
use crate::store::mongo::MongoStore;
use crate::store::testing::{MODEL, Seed, axis, pending_chunks_contract};
use crate::store::{KnowledgeStore, Scope};

#[test]
fn team_mongo_pending_chunks_and_embeddings_follow_the_contract() {
    let Some((_db, mut store)) = store() else { return };

    pending_chunks_contract(&mut store);
}

#[test]
fn team_mongo_a_stored_vector_is_found_by_the_vector_search_and_the_keyword_index_is_left_alone() {
    let Some((_db, mut store)) = store() else { return };
    insert(&mut store, &Seed::new("Guide").chunk("Intro", "needle words"));

    let keyword = |store: &mut MongoStore| {
        store
            .keyword_top_k("needle", &Scope::everything(), 10)
            .map(|hits| hits.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>())
    };

    // The indexes catch up with a write about a second later.
    let before = eventually("the word is indexed", || keyword(&mut store), |ids| ids.len() == 1);

    let pending = store.pending_chunks(MODEL, 0, 10).expect("pending");
    store
        .set_chunk_embeddings(MODEL, &[(pending[0].chunk_id, axis(0))])
        .expect("embed");

    let hits = eventually(
        "the vector is indexed",
        || store.vector_top_k(&axis(0), &Scope::everything(), 10),
        |hits| hits.len() == 1,
    );
    assert_eq!(hits.iter().map(|hit| hit.chunk_id).collect::<Vec<_>>(), before);
    assert_eq!(keyword(&mut store).expect("keyword"), before);
}
