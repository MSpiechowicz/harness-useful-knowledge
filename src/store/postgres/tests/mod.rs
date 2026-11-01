//! Behaviour of the PostgreSQL store through the [`KnowledgeStore`](crate::store::KnowledgeStore)
//! trait. The `team_*` tests need a server with pgvector (and `capability` also one without) and
//! skip themselves when `HARNESS_KNOWLEDGE_TEST_POSTGRES_URL` (and
//! `HARNESS_KNOWLEDGE_TEST_POSTGRES_PLAIN_URL`) is unset, see [`support`].

mod capability;
mod documents;
mod embeddings;
mod files;
mod listing;
mod phrase;
mod routed;
mod search;
mod support;
mod titles;
