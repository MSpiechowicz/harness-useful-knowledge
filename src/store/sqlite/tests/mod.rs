//! Behaviour of the local store through the [`KnowledgeStore`](crate::store::KnowledgeStore)
//! trait, on in-memory databases (and on files where the file itself is the point).

mod counters;
mod documents;
mod embeddings;
mod files;
mod listing;
mod quality;
mod search;
mod titles;
