<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/lockup-dark.svg">
    <img src="docs/brand/lockup-light.svg" alt="harness knowledge" width="430">
  </picture>
</p>

# Harness Knowledge

**Project documentation for your AI coding agents, in one local binary.** `harness-knowledge` is a
stdio [MCP](https://modelcontextprotocol.io) server that Claude Code, Codex, OMP, PI, OpenCode and any
other MCP client can start. People import documents (guides, READMEs, ADRs, OpenAPI specs,
Confluence exports, PDFs, meeting transcripts) and agents search them when they need to know how
something works. It is the written side of the Harness family. `harness-memory` keeps what agents
learn that is written down nowhere, and `harness-knowledge` keeps what is written down already.

Documents are split into heading-aware chunks and searched with hybrid ranking: keyword search (BM25)
plus vector search with the multilingual
[e5-small](https://huggingface.co/intfloat/multilingual-e5-small) model, merged with reciprocal rank
fusion. The index lives in a local SQLite file, or in a team database (MongoDB, PostgreSQL or libSQL)
that holds both the keyword and the vector index. Document text is treated as untrusted data, never
as instructions.

Agents do not write knowledge on their own. `knowledge_add` sends a proposal to a review queue, and
only a person approving it in Harness Desktop (or with `held approve`) makes it searchable.

## Install

**macOS and Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/MSpiechowicz/harness-useful-knowledge/main/install.sh | sh
```

This installs `~/.local/bin/harness-knowledge`. Linux is x86_64 only, macOS is Apple Silicon or Intel.

**Windows (PowerShell)**

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/MSpiechowicz/harness-useful-knowledge/main/install.ps1 | iex"
```

This installs `%LOCALAPPDATA%\Programs\harness-knowledge\harness-knowledge.exe`. Both installers check
the SHA-256 against the release's `checksums.txt` before installing anything.

## Register with your harness

Harness Desktop installs it, registers the MCP server for every harness and adds a Knowledge view
(import, review queue, repository links). Without the desktop:

- **Claude Code:** `claude mcp add --scope user harness-knowledge -- harness-knowledge mcp`
- **Codex:** `codex mcp add harness-knowledge -- harness-knowledge mcp`
- **OMP, PI, OpenCode** and other MCP clients: add this entry to their MCP config:

```json
{ "mcpServers": { "harness-knowledge": { "command": "harness-knowledge", "args": ["mcp"] } } }
```

## Documents

| Format | Files | Sections |
| --- | --- | --- |
| Markdown | `.md`, `.markdown` | headings |
| Plain text | `.txt` | paragraphs |
| HTML, Confluence exports | `.html`, `.htm` | headings, Confluence breadcrumbs |
| OpenAPI / Swagger | `.json`, `.yaml`, `.yml` | one per operation and per schema |
| AsciiDoc | `.adoc`, `.asciidoc` | `=` headings |
| reStructuredText | `.rst` | adornment levels |
| PDF with a text layer | `.pdf` | pages |
| Transcripts | `.vtt`, `.srt` | time ranges (`00:12:30–00:15:10`) |

A document can link to one or more repositories, each narrowed to path globs (`services/billing/**`).
A search from inside a repository sees global documents and the ones linked to it, and ranks linked
ones higher (higher still when a glob matches the files at hand). An unlinked document is global.

Re-importing unchanged content changes nothing. Changed content updates the document in place and
reuses the vectors of unchanged chunks. Limits: 32 MiB per file (64 MiB for a PDF), 8 MiB of text and
4000 chunks per document, 5000 files per import. Scanned PDFs without a text layer are refused. The
PDF reader runs in a child process with an empty environment, a timeout and, on Linux and macOS,
memory and CPU limits.

## MCP tools

| Tool | What it does |
| --- | --- |
| `knowledge_search` | Hybrid search, as pointers (title, section, snippet) or whole chunks |
| `knowledge_get` | The text of a document or chunk, with neighbouring chunks |
| `knowledge_list` | The documents of this repository and the global ones |
| `knowledge_add` | Sends a document to review, never writes the database |

The reading tools take `project_path`, so results follow the repository the agent works in. Pass
`scope: "all"` to read documents linked only to other repositories.

## Vector search

`harness-knowledge embedder` runs one embedder per computer. It downloads the model once from this
repository's GitHub release (pinned by SHA-256 and size, about 135 MB), runs it on
[tract](https://github.com/sonos/tract) (pure Rust, no native runtime) and answers every
`harness-knowledge` process over a local socket. Imports embed with up to 4 threads sharing one model
(`HARNESS_KNOWLEDGE_EMBED_THREADS`, 1 to 8), searches jump ahead of imports, and the model unloads
after 10 idle minutes. Without the embedder, search falls back to keywords.

`HARNESS_KNOWLEDGE_MODEL_BASE_URL` points the download at a mirror (https, or http on loopback).

## Team database

Set the database in `<config dir>/harness-knowledge/config.toml`:

```toml
[database]
url = "mongodb+srv://user:password@cluster.example.net/knowledge"
token = "only for libsql"
```

| Database | Needs | Keyword | Vectors |
| --- | --- | --- | --- |
| MongoDB | Atlas, or 8.2+ with `mongot` | Atlas Search (BM25) | `$vectorSearch` |
| PostgreSQL | the `pgvector` extension, 0.5 or newer | full-text search (`ts_rank_cd`) | HNSW |
| libSQL, Turso | a server with vector support | FTS5 (BM25) | DiskANN |

`harness-knowledge check --json` tells what a server lacks (`no-mongot`, `no-pgvector`,
`pgvector-no-privilege`, `libsql-no-vector` and more). Any command that needs a missing capability
exits with code 3. Tables and collections are prefixed `knowledge_`, so the database can be shared
with `harness-memory`. TLS is required for every server that is not on this computer. Amazon
DocumentDB is not supported yet.

### Where documents are kept

With a team database set, `[routing]` says where each kind of document lives: `remote` (the
default), `local` (this computer's file) or `remote+local` (both are read, new documents go
remote unless `--place local` says otherwise).

```toml
[routing]
repository_documents = "remote"      # documents linked to a repository
global_documents = "remote+local"    # documents linked to none
```

When both places are in use, ids from this computer are written `local:12` (`local:12:340` for a
chunk) and remote ids stay plain numbers. Every command and MCP tool takes both forms, and a plain
number is the remote database's. JSON documents then carry `"place": "remote" | "local"`.
`docs import` takes `"place"` in the manifest, `held approve` takes `--place remote|local`, and a
place the routing does not keep that kind of document in is refused. A list merges both places, a
search runs in each place and fuses the two result lists by rank, and `check` and `stats` report
each place under `places`.

| Variable | Meaning |
| --- | --- |
| `HARNESS_KNOWLEDGE_REPOSITORY_DOCUMENTS` | Routing of documents linked to a repository (overrides the file) |
| `HARNESS_KNOWLEDGE_GLOBAL_DOCUMENTS` | Routing of documents linked to no repository (overrides the file) |
| `HARNESS_KNOWLEDGE_DIR` | Folder of the local database and the model (absolute) |
| `HARNESS_KNOWLEDGE_CONFIG` | Config file to use instead of the default one |
| `HARNESS_KNOWLEDGE_DATABASE_URL` | Team database URL (overrides the config file) |
| `HARNESS_KNOWLEDGE_DATABASE_TOKEN` | libSQL auth token |

## Command line

```sh
harness-knowledge docs import --ndjson < manifest.json   # {"paths":[],"folders":[],"links":[]}
harness-knowledge docs list --json --query billing
harness-knowledge search --json --query "how are refunds retried" --project .
harness-knowledge held list --json
harness-knowledge held approve --json <id>
echo "how do we deploy?" | harness-knowledge recall --hook --project .
```

`harness-knowledge --help` lists every command. Commands print JSON and exit 0 on success, 1 on a
failure, 2 for a usage error or an unknown command, and 3 for a missing database capability.
`recall --hook` always exits 0 and prints at most three pointers, or nothing.

## Build

```sh
cargo build --release
cargo test
```

Team database tests run when `HARNESS_KNOWLEDGE_TEST_POSTGRES_URL`, `HARNESS_KNOWLEDGE_TEST_MONGO_URL`
or `HARNESS_KNOWLEDGE_TEST_LIBSQL_URL` is set (see `.github/workflows/ci.yml` for the images). Design
notes and measurements are in [`docs/plans/knowledge.md`](docs/plans/knowledge.md).

## License

GPL-3.0-or-later, see `LICENSE`. The e5-small model is MIT licensed by its authors.
