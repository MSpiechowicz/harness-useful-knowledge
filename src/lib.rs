//! `harness-knowledge`: a project knowledge MCP server for any coding agent harness.
//!
//! People import documents (Markdown, text, HTML, OpenAPI, PDF and more) and agents search them
//! with hybrid keyword and vector ranking. A document is global or linked to one or more
//! repositories (identified by their `origin` remote, see [`project`], with the same keys
//! `harness-memory` uses).
//!
//! - [`backend`]: opens the store the settings select.
//! - [`chunk`]: cuts the sections of a document into the chunks that are embedded.
//! - [`cli`]: what the commands share: option parsing, the repository options, paging and the
//!   failure report.
//! - [`cli_docs`]: the `docs`, `repos`, `stats`, `project-key` and `check` commands.
//! - [`cli_embedder`]: the `embedder` command.
//! - [`config`]: which database to use (the local file, or a team database from the environment or
//!   the config file).
//! - [`digest`]: SHA-256 as hex.
//! - [`embed`]: the embedder: the e5 model on tract, its worker threads, its local socket and client.
//! - [`extract`]: reads Markdown, text, HTML, OpenAPI, AsciiDoc, RST and PDF into sections.
//! - [`import`]: `docs import` and `docs reindex`: the manifest, the folder walk, dedupe, events.
//! - [`limits`]: per-call limits.
//! - [`location`]: where the local database lives.
//! - [`project`]: which repository a call is about.
//! - [`store`]: what a backend stores and answers, the local SQLite file, the search fusion and ranking.
//! - [`tokens`]: token counts for the chunker (the model's tokenizer, or an estimate).
//! - [`support`]: the environment, git runner, secret masking and text checks the rest uses.
//!
//! The local database lives in the per-machine data folder ([`location::database_path`]), never in
//! a repository, and a read never creates it.

pub mod backend;
pub mod chunk;
pub mod cli;
pub mod cli_docs;
pub mod cli_embedder;
pub mod config;
pub mod digest;
pub mod embed;
pub mod extract;
pub mod import;
pub mod limits;
pub mod location;
pub mod project;
pub mod store;
pub mod support;
pub mod tokens;

pub use location::database_path;
