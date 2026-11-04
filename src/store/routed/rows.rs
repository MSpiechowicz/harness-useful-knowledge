//! Giving the rows of a place its tag (see [`super::ids`]), and the rank-interleaving of hits.

use super::ids;
use crate::config::Place;
use crate::store::{ChunkHit, Document, DocumentDetail, DocumentRow, PendingVersion, ReviewReply};

pub(super) fn tag_document(place: Place, doc: &mut Document) {
    doc.id = ids::tag(place, doc.id);
    doc.pending_of = doc.pending_of.map(|live| ids::tag(place, live));
}

fn tag_pending(place: Place, pending: &mut Option<PendingVersion>) {
    if let Some(pending) = pending {
        pending.id = ids::tag(place, pending.id);
    }
}

pub(super) fn tag_row(place: Place, row: &mut DocumentRow) {
    tag_document(place, &mut row.doc);
    tag_pending(place, &mut row.pending);
}

/// The reply of a transition with the id of the document tagged.
pub(super) fn tag_reply(place: Place, reply: ReviewReply) -> ReviewReply {
    match reply {
        ReviewReply::Done(mut done) => {
            done.id = ids::tag(place, done.id);
            ReviewReply::Done(done)
        }
        other => other,
    }
}

fn tag_hit(place: Place, hit: &mut ChunkHit) {
    hit.chunk_id = ids::tag(place, hit.chunk_id);
    hit.doc_id = ids::tag(place, hit.doc_id);
}

pub(super) fn tag_detail(place: Place, detail: &mut DocumentDetail) {
    tag_document(place, &mut detail.doc);
    tag_pending(place, &mut detail.pending);

    for section in detail.sections.iter_mut().flatten() {
        section.chunk_id = ids::tag(place, section.chunk_id);
    }

    if let Some(chunk) = &mut detail.chunk {
        chunk.id = ids::tag(place, chunk.id);
        chunk.doc_id = ids::tag(place, chunk.doc_id);
    }
}

pub(super) fn tagged_hits(place: Place, mut hits: Vec<ChunkHit>) -> Vec<ChunkHit> {
    for hit in &mut hits {
        tag_hit(place, hit);
    }

    hits
}

/// The first of each list, then the second of each, and so on, at most `k`.
pub(super) fn interleave(lists: Vec<Vec<ChunkHit>>, k: usize) -> Vec<ChunkHit> {
    let mut iterators: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
    let mut out = Vec::new();

    while out.len() < k {
        let before = out.len();

        for list in &mut iterators {
            if let Some(hit) = list.next()
                && out.len() < k
            {
                out.push(hit);
            }
        }

        if out.len() == before {
            break;
        }
    }

    out
}
