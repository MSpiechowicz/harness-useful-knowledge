//! The chunks that wait for a vector and the vectors made for them, in the places that hold them.

use super::{RoutedStore, ids};
use crate::config::Place;
use crate::store::{PendingChunk, StoreError};

/// The chunks without a vector, the remote ones first and then the local ones: the order of the
/// tagged ids. A remote that cannot be reached has none to offer.
pub(super) fn pending(
    routed: &mut RoutedStore,
    model_id: &str,
    after: i64,
    limit: usize,
) -> Result<Vec<PendingChunk>, StoreError> {
    routed.ready()?;

    let (after_place, inner) = ids::split(after);
    let mut page: Vec<PendingChunk> = Vec::new();

    for place in [Place::Remote, Place::Local] {
        let start = match (after_place, place) {
            (Some(Place::Local), Place::Remote) => continue,
            (Some(given), place) if given == place => inner,
            (None, Place::Remote) => inner,
            _ => 0,
        };
        let room = limit.saturating_sub(page.len());

        if room == 0 {
            break;
        }

        let Some(store) = routed.store(place) else { continue };

        for mut chunk in store.pending_chunks(model_id, start, room)? {
            chunk.chunk_id = ids::tag(place, chunk.chunk_id);
            chunk.doc_id = ids::tag(place, chunk.doc_id);
            page.push(chunk);
        }
    }

    Ok(page)
}

/// Stores each vector in the place its chunk id names. A vector for the remote is refused while
/// it cannot be reached.
pub(super) fn set(
    routed: &mut RoutedStore,
    model_id: &str,
    embeddings: &[(i64, Vec<f32>)],
) -> Result<usize, StoreError> {
    let mut done = 0;

    for place in [Place::Remote, Place::Local] {
        let own: Vec<(i64, Vec<f32>)> = embeddings
            .iter()
            .filter(|(id, _)| ids::place_of(*id) == place)
            .map(|(id, vector)| (ids::split(*id).1, vector.clone()))
            .collect();

        if own.is_empty() {
            continue;
        }

        routed.writable(place)?;

        if let Some(store) = routed.store(place) {
            done += store.set_chunk_embeddings(model_id, &own)?;
        }
    }

    Ok(done)
}
