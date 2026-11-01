//! What the places hold, added up: the repositories with their documents, and the counts.

use std::collections::HashMap;

use super::RoutedStore;
use crate::config::Place;
use crate::store::{RepoCounts, Stats, StoreError};

/// The repositories of the places with their documents, the kinds each place shows only. A remote
/// that cannot be reached adds nothing.
pub(super) fn repos(routed: &mut RoutedStore) -> Result<RepoCounts, StoreError> {
    routed.ready()?;

    let mut merged = RepoCounts::default();
    let mut by_key: HashMap<String, usize> = HashMap::new();

    for place in [Place::Remote, Place::Local] {
        let kinds = routed.kinds(place);
        let Some(store) = routed.store(place) else { continue };

        let counts = store.repos();
        let Some(counts) = routed.read(place, counts)? else {
            continue;
        };

        if kinds.global {
            merged.global += counts.global;
        }

        if kinds.repository {
            for (key, docs) in counts.repos {
                *by_key.entry(key).or_default() += docs;
            }
        }
    }

    let mut repos: Vec<(String, usize)> = by_key.into_iter().collect();
    repos.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    merged.repos = repos;

    Ok(merged)
}

/// The sums of the places' counts, over everything each holds whatever its routing shows. The
/// model and the schema are the remote database's (this computer's when there is none, or when the
/// remote cannot be reached, which then adds nothing).
pub(super) fn stats(routed: &mut RoutedStore) -> Result<Stats, StoreError> {
    routed.ready()?;

    let mut sum: Option<Stats> = None;

    for place in [Place::Remote, Place::Local] {
        let Some(store) = routed.store(place) else { continue };

        let stats = store.stats();
        let Some(stats) = routed.read(place, stats)? else {
            continue;
        };

        sum = Some(match sum {
            None => stats,
            Some(first) => Stats {
                documents: first.documents + stats.documents,
                chunks: first.chunks + stats.chunks,
                embedded_chunks: first.embedded_chunks + stats.embedded_chunks,
                bytes: first.bytes + stats.bytes,
                global_documents: first.global_documents + stats.global_documents,
                // A repository can have documents in both places: the larger count is a lower bound.
                repositories: first.repositories.max(stats.repositories),
                generation: first.generation + stats.generation,
                schema_version: first.schema_version,
                model_id: first.model_id.or(stats.model_id),
            },
        });
    }

    Ok(sum.unwrap_or_default())
}
