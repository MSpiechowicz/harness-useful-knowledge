//! Finds the stored document a file or a proposal is probably a version of, so a person can replace
//! it instead of keeping two. Used by `docs similar` and by `knowledge_add`.
//!
//! The reasons, strongest first:
//!
//! 1. [`Reason::SourceKey`]: the same source key, which is what an import updates in place.
//! 2. [`Reason::SameContent`]: the same title and text under another key.
//! 3. [`Reason::SharedChunks`]: more than half of the distinct chunks of the text, and at least 2,
//!    are in the document (an edited copy). The document with the most, then the lowest id. A store
//!    takes at most [`SHARED_SHAS_MAX`] hashes at once (as many as a document has chunks), so of a
//!    longer text the first ones are asked about and "more than half" counts against those: a text
//!    never matches by a share that is measured against chunks the store was not asked about.
//! 4. [`Reason::Title`]: the same title once case and white space are ignored, in the same
//!    repository or global scope only. The lowest id.

use std::collections::{HashMap, HashSet};

use super::prepare::{self, prepare};
use super::walk::Candidate;
use crate::chunk::{self, Chunked};
use crate::extract;
use crate::extract::pdf::PdfPages;
use crate::limits::SHARED_SHAS_MAX;
use crate::store::{Detail, Document, KnowledgeStore, StoreError, TitleScope};
use crate::tokens::TokenCounter;

/// Fewest chunks an edited copy shares with its original.
const SHARED_MIN: usize = 2;

/// What a text is compared by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub title: String,
    /// The hash an import stores as `content_sha256`.
    pub content_sha: String,
    /// The distinct chunk hashes of the text, in order.
    pub chunk_shas: Vec<String>,
    pub source_key: String,
    /// Where a title may match: the repository the text links to, else the global scope.
    pub scope: TitleScope,
}

impl Probe {
    /// The probe of a text chunked the way an import chunks it (see [`crate::chunk`]) under `title`
    /// and `source_key`.
    pub fn new(title: &str, chunked: &Chunked, source_key: &str, scope: TitleScope) -> Self {
        let mut seen = HashSet::new();
        let chunk_shas = chunked
            .pieces
            .iter()
            .filter(|piece| seen.insert(piece.chunk_sha.as_str()))
            .map(|piece| piece.chunk_sha.clone())
            .collect();

        Self {
            title: title.to_owned(),
            content_sha: prepare::content_sha(title, &chunked.text),
            chunk_shas,
            source_key: source_key.to_owned(),
            scope,
        }
    }
}

/// The probe of the file `candidate` as an import would read it (the same extraction, title, chunks
/// and source key), or why it cannot be read.
pub fn probe(
    candidate: &Candidate,
    pdf: &dyn PdfPages,
    counter: &dyn TokenCounter,
    scope: TitleScope,
) -> Result<Probe, String> {
    let read = prepare(candidate, pdf, counter, None, true).map_err(prepare::Stop::reason)?;

    Ok(Probe::new(&read.title, &read.chunked, &read.source_key, scope))
}

/// The probe of a proposed text as its approval would import it: `content` is read under `name`
/// (its extension decides the format) and `title` replaces the text's own, as an approval does (see
/// [`crate::held`]). `source_key` is the key the approval will give it.
pub fn probe_text(
    name: &str,
    content: &str,
    title: &str,
    source_key: &str,
    counter: &dyn TokenCounter,
    scope: TitleScope,
) -> Result<Probe, String> {
    let bytes = content.as_bytes();
    let format = extract::detect(name, &bytes[..bytes.len().min(prepare::HEAD_BYTES as usize)])
        .ok_or_else(|| "the text is not a document".to_owned())?;
    let extracted = extract::extract(format, name, bytes).map_err(|refusal| prepare::Stop::from(refusal).reason())?;
    let chunked = chunk::chunk_document(title, &extracted.sections, counter);

    if chunked.pieces.is_empty() {
        return Err("the text has no content".to_owned());
    }

    Ok(Probe::new(title, &chunked, source_key, scope))
}

/// Why a document is a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    SourceKey,
    SameContent,
    SharedChunks,
    Title,
}

impl Reason {
    /// The word `docs similar` prints.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourceKey => "sourceKey",
            Self::SameContent => "sameContent",
            Self::SharedChunks => "sharedChunks",
            Self::Title => "title",
        }
    }
}

/// The document a probe matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The document, its id tagged with its place when a routed store found it.
    pub doc: Document,
    pub reason: Reason,
    /// How many distinct chunks of the probe the document has (whatever the reason).
    pub shared: usize,
    /// How many distinct chunks of the probe the store was asked about (all of them up to
    /// [`SHARED_SHAS_MAX`]).
    pub chunks: usize,
}

/// The strongest match for `probe` in `store`, `None` when no document is similar.
pub fn find(store: &mut dyn KnowledgeStore, probe: &Probe) -> Result<Option<Match>, StoreError> {
    let asked = &probe.chunk_shas[..probe.chunk_shas.len().min(SHARED_SHAS_MAX)];
    let chunks = asked.len();
    let sharing: HashMap<i64, usize> = if asked.is_empty() {
        HashMap::new()
    } else {
        store.docs_sharing_chunks(asked)?.into_iter().collect()
    };

    let matched = |doc: Document, reason: Reason| {
        let shared = sharing.get(&doc.id).copied().unwrap_or(0);

        Match {
            doc,
            reason,
            shared,
            chunks,
        }
    };

    if let Some(doc) = store.find_by_source_key(&probe.source_key)? {
        return Ok(Some(matched(doc, Reason::SourceKey)));
    }

    if let Some(doc) = store.find_by_content_sha(&probe.content_sha)? {
        return Ok(Some(matched(doc, Reason::SameContent)));
    }

    // The most shared first, then the lowest id.
    let mut sharing_most: Vec<(usize, i64)> = sharing
        .iter()
        .filter(|(_, shared)| **shared >= SHARED_MIN && **shared * 2 > chunks)
        .map(|(id, shared)| (*shared, *id))
        .collect();
    sharing_most.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    for (_, id) in sharing_most {
        // A document deleted since the lookup is passed over.
        if let Some(found) = store.get(id, &Detail::default())? {
            return Ok(Some(matched(found.doc, Reason::SharedChunks)));
        }
    }

    let titled = store.find_by_title(&probe.title, &probe.scope)?;

    Ok(titled
        .into_iter()
        .min_by_key(|doc| doc.id)
        .map(|doc| matched(doc, Reason::Title)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testing::{Seed, insert, store};

    /// A probe whose chunk hashes are those of `Seed` chunks with the texts `t0`, `t1`, ...
    fn probe_of(count: usize) -> Probe {
        Probe {
            title: "Unrelated".to_owned(),
            content_sha: "none".to_owned(),
            chunk_shas: (0..count).map(|number| format!("sha-t{number}")).collect(),
            source_key: "/none".to_owned(),
            scope: TitleScope::Global,
        }
    }

    #[test]
    fn a_longer_text_is_compared_by_the_chunks_the_store_was_asked_about() {
        let mut store = store();
        let mut seed = Seed::new("Big guide").source("/big");

        for number in 0..SHARED_SHAS_MAX * 5 / 8 {
            seed = seed.chunk("Part", &format!("t{number}"));
        }

        let id = insert(&mut store, &seed);

        // Within the limit: 2500 of 4000 chunks are shared, more than half.
        let found = find(&mut store, &probe_of(SHARED_SHAS_MAX))
            .expect("find")
            .expect("a match");
        assert_eq!((found.doc.id, found.reason), (id, Reason::SharedChunks));
        assert_eq!((found.shared, found.chunks), (SHARED_SHAS_MAX * 5 / 8, SHARED_SHAS_MAX));

        // Beyond it only the first 4000 are asked about, and the same 2500 are still more than half
        // of those, not weighed against the 7000 the text has.
        let found = find(&mut store, &probe_of(SHARED_SHAS_MAX * 7 / 4))
            .expect("find")
            .expect("a match");
        assert_eq!((found.doc.id, found.reason), (id, Reason::SharedChunks));
        assert_eq!((found.shared, found.chunks), (SHARED_SHAS_MAX * 5 / 8, SHARED_SHAS_MAX));

        // Fewer than half of the 4000 asked about are shared: no match, however long the text.
        let few = probe_of(SHARED_SHAS_MAX)
            .chunk_shas
            .into_iter()
            .skip(SHARED_SHAS_MAX / 2);
        let mut elsewhere = probe_of(SHARED_SHAS_MAX * 7 / 4);
        elsewhere.chunk_shas = few
            .chain((0..SHARED_SHAS_MAX * 2).map(|n| format!("sha-other{n}")))
            .collect();
        assert_eq!(find(&mut store, &elsewhere).expect("find"), None);
    }
}
