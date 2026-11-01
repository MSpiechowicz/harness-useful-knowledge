//! The libSQL store through the [`KnowledgeStore`](crate::store::KnowledgeStore) trait, on a real
//! server, plus the cases that need no server.
//!
//! The `team_*` cases run when `HARNESS_KNOWLEDGE_TEST_LIBSQL_URL` names a libSQL server (for
//! example `http://127.0.0.1:58080`) and skip with a message when it is unset, like the other team
//! backends (see the comment in `.github/workflows/ci.yml`). The server holds one database, so each
//! case takes a lock file first, emptied after the previous case, and runs one at a time, also
//! across test binaries. Point the variable at a throwaway server: its knowledge tables are
//! dropped. `HARNESS_KNOWLEDGE_TEST_LIBSQL_TOKEN` is sent when set.

mod access;
mod documents;
mod embeddings;
mod listing;
mod search;
mod support;
mod transport;
