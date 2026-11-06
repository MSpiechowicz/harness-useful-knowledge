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
something works. It is the written side of the Harness family. [`harness-memory`](https://github.com/MSpiechowicz/harness-useful-memory) keeps what agents
learn that is written down nowhere, `harness-knowledge` keeps what is written down already, and
[`harness-code`](https://github.com/MSpiechowicz/harness-useful-code) knows the code. It works on its own, with neither of the
others installed.

Documents are split into heading-aware chunks and searched with hybrid ranking: keyword search (BM25)
plus vector search with the multilingual
[e5-small](https://huggingface.co/intfloat/multilingual-e5-small) model, merged with reciprocal rank
fusion. The index lives in a local SQLite file, or in a team database (MongoDB, PostgreSQL or libSQL)
that holds both the keyword and the vector index. Document text is treated as untrusted data, never
as instructions.

Agents do not write knowledge on their own. `knowledge_add` sends a proposal to a review queue, and
only a person approving it in Harness Desktop (or with `held approve`) makes it searchable. With
team approval on, the approved proposal is a draft first and a teammate must approve it again, see
[Drafts and team approval](#drafts-and-team-approval).

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
| `knowledge_add` | Sends a document to review, never writes the database. `revises` names the held proposal (`held:7`) a person asked to change, and replaces it |

The reading tools see live documents only, never a draft or a pending update. They take `project_path`, so results follow the repository the agent works in. Pass
`scope: "all"` to read documents linked only to other repositories.

## Drafts and team approval

A document is in one state:

| State | Meaning |
| --- | --- |
| `live` | Searchable by agents |
| `draft` | Written, not asked for approval yet |
| `waiting` | Asked for approval, a teammate has to answer |
| `changesRequested` | A reviewer sent it back with a comment |
| pending update | A separate draft or waiting version of a live document. It carries the live document's id as `pendingOf`, and the live text stays in force until the update goes live |

Agents never see a document that is not live, in search, `knowledge_get`, `knowledge_list`, recall
or the duplicate check. People see every state with `docs list|get|similar` and `search --json`.

Team approval is one switch stored in the remote database, so it holds for everyone on it:

```sh
harness-knowledge approval get --json   # {"approval": true, "place": "remote", "writable": true, "changedBy": "Ada", "changedAt": "2026-11-04T10:00:00Z"}
harness-knowledge approval set --json on    # needs a git user.name: it records who switched it and when
```

The local file is always off. With approval on, every import to the remote database is a draft, and a
replace of a live document writes its pending update. A draft goes live when someone other than its
authors approves it. An author is everyone who drafted or edited that version (a rename counts) since
it was last live, and whoever asked for its approval. Only the authors can take the ask back. A change
to a draft or waiting document sets it back to draft, so an approval always covers exactly what was
asked. The identity is the git `user.name` (trimmed, compared without case). It stops honest mistakes,
not impersonation, because anyone can set a name. Asking needs a name. Rename and links of live documents stay direct. Deleting a live document deletes its pending
update.

Every transition takes the revision you saw (`--expect REV`) and refuses with `{"error": CODE,
"message"}` and exit 1 when it changed. Codes: `notFound`, `changed`, `wrongState`, `approvalOn`,
`approvalOff`, `selfApproval`, `notAuthor`, `noName`, `readOnly`, `commentRequired`, `sinceUnavailable`, `refused`,
`localPath`, `unreachable`.

```sh
harness-knowledge docs ask --json 12 --expect 3                       # draft or changesRequested -> waiting
harness-knowledge docs request-changes --json 12 --expect 4 --comment "Add the retry limits"
harness-knowledge docs approve --json 12 --expect 4                   # waiting -> live (a pending update replaces the live text)
harness-knowledge docs go-live --json 12 --expect 3                   # approval off: draft or waiting -> live
harness-knowledge docs take-back --json 12 --expect 4                  # waiting -> draft, by its author or asker
harness-knowledge docs review --json 12 --expect 4 --verdict requestChanges --body "See comments" --comments comments.json
harness-knowledge docs history --json 12                              # who did what and when, newest first
harness-knowledge docs comments --json 12
harness-knowledge docs comment reply --json 12 COMMENT_ID --body "Done"
harness-knowledge docs comment resolve --json 12 COMMENT_ID          # or unresolve
harness-knowledge docs comment delete --json 12 COMMENT_ID           # your own comment, a thread with its replies
harness-knowledge docs compare --json 12 [--since 3]                  # sections of a draft against the live text
harness-knowledge docs download --json 12 --to ./original.md [--live]
```

`docs review` submits one review: an overall body plus line comments from a JSON file,
`[{"section", "side": "old"|"new", "from", "to", "body"}]`. The verdict `approve` is the same as
`docs approve`, `requestChanges` needs a comment, `comment` changes no state, and the author cannot
approve or request changes. `docs comment delete` prints `{"deleted": [ID, ...]}` (the comment and, for a
thread, its replies). Only the one who wrote a comment can delete it (`notAuthor` otherwise), in any state
of the document. `docs download` writes the imported file, or the stored text when no
original was kept (`"original": false`).

Other commands:

```sh
harness-knowledge docs import --ndjson --draft < manifest.json            # store as a draft (or a pending update)
harness-knowledge docs import --ndjson --ask --note "ready" < manifest.json  # write and ask in one step, needs approval on
harness-knowledge docs list --json --status waiting                       # all|live|draft|waiting|changesRequested|pending
harness-knowledge held request-changes --json ID --expect REV --comment "Shorter please"
harness-knowledge held reject --json ID --comment "Duplicate of 12"
harness-knowledge upgrade --json                                          # {"upgraded": bool, "waiting": bool}
```

`--draft`, `--ask` and `--note` are also the manifest fields `draft`, `ask` and `note`. A manifest with `replacing` or `replaces` plus `--ask` replaces a file and asks again in one step. `docs list --status
draft|waiting|changesRequested` also keeps live documents whose pending update is in that state.
`stats --json` counts `drafts`, `waiting` and `pending` per place, and reports `approval`. A held
proposal that was sent back is replaced when the agent calls `knowledge_add` again in the same
session with the same title or with `revises`.

Team databases never store a local folder path. A project without a git remote is keyed `path:/folder`;
linking a team document to it (`docs links set`, `docs import`, `held approve` into the team database) is
refused as `localPath`, and only a `git:` project key leaves this computer. This computer's own database
keeps such links. A writer that opens a team database removes `path:` links an earlier version left there (a document left with no link becomes a draft, never a global one). Searches and lists scoped to such a project never send the folder to the team database either.

### Upgrading to 2.0.0

Drafts need schema version 2 in every backend. An older `harness-knowledge` refuses a database that
was upgraded, so everyone on a team database updates together and restarts their harnesses afterwards.
Run `harness-knowledge upgrade` or let Harness Desktop do it. A role that cannot write cannot upgrade
and waits for a teammate. Known limits:

- libSQL cannot tell a read-only token, so the actions stay enabled and fail with the server's error.
- The MongoDB upgrade briefly needs 4 search indexes, and a cluster capped at 3 (for example Atlas
  M0) may fail.
- PostgreSQL does one extra version lookup per read.

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
harness-knowledge docs list --json --author "Ada" --sort addedBy   # or --author-unknown
harness-knowledge search --json --query "how are refunds retried" --project .
harness-knowledge held list --json [--sort created|addedBy]
harness-knowledge held approve --json <id>
harness-knowledge docs list --json --status draft
echo "how do we deploy?" | harness-knowledge recall --hook --project .
```

`harness-knowledge --help` lists every command. Commands print JSON and exit 0 on success, 1 on a
failure, 2 for a usage error or an unknown command, and 3 for a missing database capability.
`recall --hook` always exits 0 and prints at most three pointers, or nothing.

`docs list` keeps the documents one person added with `--author NAME` (the git `user.name` they were
written under, matched exactly, upper and lower case apart) or the ones nobody is known for with
`--author-unknown`, and `--sort addedBy` orders them by that name with the unknown ones last either way.
Its answer also carries `authors`: `{"people": [{"name", "count"}], "unknown": N}`, how many documents each
person added under every filter but the author one, at most 200 people with the one asked for always among them.

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
