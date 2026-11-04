# Plan: Harness Knowledge (#45)

Repos:
- **A** = `harness-useful-knowledge` (new crate and binary `harness-knowledge`)
- **B** = `harness-useful-desktop`
- **C** = `harness-useful-skills`

Reference: `harness-useful-memory` v2.11.0. Follow-ups: #48 (`harness-code`), #49 (auto-link documents to code).

## Settled decisions
- **Backends:**
  - Local SQLite in its own file, `<data>/harness-knowledge/knowledge.sqlite3`. FTS5 bm25 plus f32 BLOB vectors, brute-force cosine in Rust (`unsafe_code = forbid`, so no sqlite-vec).
  - MongoDB: Atlas or 8.2+ with mongot, using `$search` and `$vectorSearch`.
  - Postgres: pgvector HNSW plus `ts_rank_cd` with the `simple` config.
  - libSQL: `F32_BLOB`, `vector_top_k` and FTS5.
  - DocumentDB is later.
  - Vectors and the keyword index live in the database. `check` reports missing capabilities.
- **Formats:** Markdown, text, HTML/Confluence export, OpenAPI JSON/YAML, AsciiDoc, RST, WebVTT/SRT transcripts (`transcript`, one section per 2–3 minutes headed by its time span, speakers kept), PDF (pdf-extract in a worker process; scanned PDFs refused). Files only.
- **Repo links:** a document links to 0..n repos plus path globs. Unlinked means global. Docs linked to the current repo get a boost, a glob match gets a further boost, and the view filters by repo.
- **Agent writes:** `knowledge_add` sends a proposal to a local review queue (per computer). Approve or reject in the Knowledge view.
- **Recall:** at most 3 pointers (title, section, snippet, id), ≤1 KiB.
  - Desktop chats: the app itself runs `harness-knowledge recall` for an approved family hook, prompt on stdin, 2.5 s timeout. The prompt never reaches shell or team hooks.
  - Plain harness sessions: native recall through C's hooks, at parity with memory.
- **Model:** multilingual-e5-small (MIT), int8 ONNX, own release asset `model-e5-small-int8-1` and own embedder process and socket.
- **Desktop:**
  - Knowledge sits right after Memory in the rail (Hooks moves to position 10 and loses its digit).
  - Own on/off switch.
  - Full Harness family integration (Ecosystem, onboarding for Claude, Codex, OpenCode, Pi and OMP, MCP registration, inline Install/Update), gated on `harness-knowledge features`.
- **Version:** `harness-knowledge` 1.0.0.
- **No connection pool:** the long-lived mcp and embedder processes keep one lazily opened connection with reconnect.

## Acceptance
1. Installs with install.sh/ps1 on 3 OS. `--version` and `features` work.
2. File and folder import with per-file status (added, updated, unchanged, skipped, failed plus a reason). Re-importing unchanged content does no re-embedding.
3. Hybrid search on all 4 backends. An empty database returns `[]` with exit 0, and read commands never create the local database.
4. `check` problem codes: no-mongot, no-pgvector, pgvector-no-privilege, pgvector-too-old, libsql-no-vector, no-fts5, index-building, unreachable, auth.
5. Repo scope as in memory, plus link and glob boosts.
6. MCP: `knowledge_search`, `knowledge_get`, `knowledge_list` and `knowledge_add`. `knowledge_add` writes only to the review queue.
7. Recall: ≤3 pointers, nothing below the floor or when Knowledge is unavailable, within the deadline.
8. Knowledge view:
   - server-paged table (1000+ docs), filters by repo, format and text, live data with no Refresh button;
   - multi-file and folder pickers plus drag-and-drop, animated per-file progress and cancel;
   - document slide-over: links, globs, sections, delete;
   - review queue with a badge.
9. Settings: a Knowledge team-database panel (SQLite, Postgres, MongoDB, libSQL) with a capability-aware test, plus embedder status.
10. Onboarding, Ecosystem, family hooks and inline Install/Update. 6 locales, no semicolons, "Harness family" wording.

## Non-goals
DocumentDB, live connectors, code auto-links (#49), OCR, attachments, an agent delete tool, a shared team review queue (now [knowledge-drafts.md](knowledge-drafts.md)), and memory's notes, entities, scripts, trust, hooks, routines, push, offline and usefulness features.

## Interfaces (A)
- **Config:** `<config>/harness-knowledge/config.toml` with `[database] url`, `token`. Env: `HARNESS_KNOWLEDGE_{DIR,CONFIG,DATABASE_URL,DATABASE_TOKEN,MODEL_BASE_URL,SESSION}`. Tables and collections are prefixed `knowledge_`, and the default Mongo database is `harness_knowledge`.
- **CLI** (`--json`; exit 0 ok, 1 fail, 2 unsupported, 3 missing capability):
  - `features`, `mcp`, `check`
  - `docs list|get|import --ndjson|links set|rename|delete|reindex`
  - `repos`, `project-key`, `stats`, `search`, `recall --hook`
  - `held list|get|approve|reject`
  - `embedder run|status|stop`
- **Data:**
  - `meta`
  - `documents` (source_key, content_sha256, format, origin, …). `source_key` is `<scope>/<folder>/<path>` with the scope the first linked repository key (`path-<hash>` for a `path:` key, so no absolute path), else `global`. A Confluence page is `confluence:<id>` and an approved agent proposal `agent/<heldId>/<name>`. `content_sha256` covers the title and the text.
  - `doc_links` (doc, repo_key, globs)
  - `chunks` (heading_path, text, chunk_sha, embedding, model_id)
  - Mongo chunks also carry `repo_keys[]` and `global` for prefiltering.
- **Chunking:** heading-aware, ≤510 e5 tokens including the `passage:` and title prefix, target about 350, about 15% overlap only when a section is split, tiny sibling sections merged. OpenAPI: one section per operation and per schema.
- **Search:**
  1. Embed `query:` + q.
  2. Keyword top-50 and vector top-50.
  3. RRF (memory's `fuse.rs` verbatim).
  4. Repo and glob boosts.
  5. Floor and margin measured in X6.
  6. At most 2 chunks per doc.
  7. Return pointers or chunks, wrapped as untrusted documentation.
- **Limits:** files 32 MiB (PDF 64), text 8 MiB per doc, 4000 chunks per doc, 5000 files per import, folder walk depth 16 with no symlinks and `.git`/`node_modules`/`target`/hidden entries skipped.

## Experiments first (A spike, results go into `docs/plans/knowledge.md`)
- **X1:** e5 ONNX in tract (input count, cosine parity ≥0.99 against the HF reference).
- **X2:** XLM-R tokenizer parity.
- **X3:** chunks/s at batch 1/8/16 and resident memory.
- **X4:** pdf-extract quality, panics and cargo-deny result.
- **X5:** CI images: atlas-local READY latency, pgvector/pg17, libsql-server vectors, and negative images (postgres:17, mongo:8 community).
- **X6:** calibration corpus (family docs plus about 40 English and Polish queries) to set floor, margin and boosts.
- **X7:** YAML crate check against cargo-deny.
- **X8:** local brute-force latency at 10k, 50k and 100k chunks.

## Work packages
| ID | Type | Repo | Content | After |
|---|---|---|---|---|
| X1–8 | backend | A | experiments above | none |
| K1a | backend | A | crate skeleton; cli/location/project/support copied and adapted | none |
| K1b | general | A | CI (3 OS plus backend services), release, deny, install scripts | none |
| K2 | backend | A | embedder (e5 prefixes, input count from the graph, own socket) | X1–3, K1a |
| MODEL | main session | A | `gh release model-e5-small-int8-1` (onnx, tokenizer, LICENSE, NOTICE, SHA256SUMS) | K2 |
| K3 | backend | A | store trait, SQLite, fuse/rank, backend.rs, `docs` CLI, local `check` | K1a |
| K4 | backend | A | extractors, PDF worker, chunker, NDJSON import, dedupe, cancel | K2, K3 |
| K5 | backend | A | search, MCP tools, held queue, recall | K4 |
| K6a/b/c | backend | A | Postgres / MongoDB / libSQL backends with capability checks (in parallel) | K3 |
| K7 | general | A | README, `docs/plans/knowledge.md` | K5, K6 |
| S1 | general | C | `us-use-harness-knowledge` skill, renders, native recall hooks per harness, tests | K5 |
| D0 | backend | B | refactor memory plumbing (binary, cli, features, embedder, remote config, register, generic remote and embedder UI) into reusable sidecar modules, no behaviour change | none |
| D1 | backend | B | knowledge_* ecosystem modules, commands, multi-file and folder pickers, drop intake, streamed import with cancel, enum extensions, registration for every harness, stand-in tests | D0 |
| D2 | backend | B | `KnowledgeFirst` and `KnowledgeRecall` family hooks, app-run recall at compose time, docs | D1 |
| MOCKUP | parent + you | none | 2–4 clickable HTML mockups of the Knowledge view, you pick one | D1 |
| F0 | frontend | B | logo vendoring, family.ts, IPC, locales, nav/views | D1 |
| D3 | frontend | B | Knowledge view, stores, upload, review queue | F0, MOCKUP |
| D4 | frontend | B | settings section, onboarding, Ecosystem, FamilyDialog | F0 |

## Verification
- **A:**
  - CI green on 3 OS, cargo-deny passes, installer smoke test into a temp prefix.
  - Unit tests: embed prefixes and truncation, migrations from empty, fresh dir creates nothing.
  - Paging at 1500 docs. One fixture per format (scanned and malformed PDFs included), the dedupe matrix, cancel atomicity.
  - MCP `tools/list` snapshot; held approve/reject, claim race and sweep; repo-scope matrix.
  - Recall deadline with the embedder down.
  - Shared contract tests on each backend's service container, plus negative capability tests.
- **C:** full test suite green, re-rendering produces no diff.
- **B:**
  - D0: 4 test binaries, `bun run guard`, `bun run dup`, Vitest and svelte-check green with no snapshot changes; memory view, settings and embedder smoke-tested on the real `harness-memory`.
  - D1/D2: stand-in tests as modules in the existing binaries.
  - D3/D4: Vitest stores, i18n parity, WebKitGTK pixel check, 60 fps.
- **Real-binary desktop check:**
  1. Install a local build, then run the dev app on its own port (never touching :1420).
  2. Onboarding install, empty state.
  3. Folder upload of about 200 files with progress, then cancel. Drag-and-drop.
  4. Link editor, filter, delete.
  5. An agent proposal goes to review, is approved, then is found by search.
  6. Recall chip appears in a chat.
  7. Each container shows its capability message in settings.
- **Reviews:** a fresh correctness review and a security review per repo change set (max 3 each).

## Risks
- D0 refactor regressions in memory plumbing that has already shipped.
- e5 in tract (X1).
- Two embedder processes at about 120 MB each.
- Ingest of 30k chunks takes about 10–20 minutes (needs batching, ETA, cancel).
- pdf-extract panics or hangs (handled by the worker).
- A PDF that inflates to gigabytes (decompression bomb). The worker runs with an empty environment and, on Unix, `RLIMIT_AS` 1.5 GiB and `RLIMIT_CPU` 60 s, and a refused allocation or the CPU limit fails that file as "the PDF is too large to read". Windows has no limit here (`unsafe_code` is forbidden, so no job object, and a FlateDecode stream does not declare its inflated size, so a bomb cannot be refused cheaply). There the 60 s deadline is the only bound. macOS sets the limit but may not enforce `RLIMIT_AS`.
- mongot index builds are asynchronous and can make CI flaky (poll READY, Linux only).
- ANN post-filter recall on libSQL.
- `simple` config means no stemming in Postgres.
- Prompt injection from documents (mitigated by wrapping).
- An agent with a shell can call the CLI and skip review. This is the same accepted risk as memory and will be documented.

## Delivery order
1. A: experiments, then K1–K7.
2. A: model asset release.
3. A: release 1.0.0.
4. C: release.
5. B: release.

Feature branches in each repo, commits per milestone, `cargo clean` before commits. No push, PR or release without your explicit go.

## Experiment results (2026-11-01)
- **X1/X2 model:** `Xenova/multilingual-e5-small` at HF commit `761b726dd34fb83930e26aab4e9ac3899aa1fa78`.
  - Use `onnx/model_quantized.onnx` (118308185 B, sha256 `f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193`) and `tokenizer.json` (17082730 B, sha256 `0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39`).
  - The graph takes 3 i64 inputs (`input_ids`, `attention_mask`, `token_type_ids`) and outputs the last hidden state `[b,s,384]`.
  - Cosine against fp32: mean 0.9965, min 0.9958 (512-token text 0.992). Token ids match HF exactly.
  - Batching changes vectors slightly (dynamic quantisation, min 0.9939), so embed one text per call.
  - The Xenova repo has no licence file. Base model intfloat is MIT. The release needs a NOTICE.
- **X3 throughput** (tract, 350-token chunks):
  - 1 thread: 1.6/s at b1, 2.8/s at b16. Rayon matmul: up to 4.9/s.
  - 4 processes: 10.4/s total.
  - 128 tokens: 4.1/s at b1, 8.8/s at b16.
  - RSS: 457 MB loaded, about 670–860 MB while running.
  - onnxruntime (reference): about 10x faster.
- **X4 PDF:** pdf-extract 0.12.1.
  - Quality is good for single-column documents. NFKC-normalise the output (ligatures).
  - Panics on 8 of 48 fuzzed mutants, so it needs the worker process.
  - Text-less rule: average non-whitespace chars per page < 100, or ≥ half of pages under 25 chars.
  - cargo-deny: add an ignore for RUSTSEC-2026-0192 (ttf-parser unmaintained, via lopdf).
- **X7 YAML:** `saphyr` 0.3.3 (no advisories, alias-bomb safe, parses the 10.7 MB GitHub spec in 0.19 s). serde_yaml_ng fails deny, and yaml-rust2 aborts on an alias bomb.
- **X5 backends:**
  - **Atlas local** (mongod 8.3.13): search and vectorSearch indexes are READY in about 0.9 s at 1k docs, and new docs are searchable after about 1 s (eventually consistent, so tests poll).
  - **pgvector** 0.8.7: HNSW, `ts_rank_cd` and `websearch_to_tsquery('simple')` work, and `hnsw.iterative_scan` is accepted.
  - **libSQL:** memory's pinned digest already supports `F32_BLOB`, `libsql_vector_idx`, `vector_top_k` and FTS5. ANN filtering is post-filter only, so over-fetch. `ł` is not folded by `remove_diacritics 2`.
- **Negative probes:**
  - postgres:17 → SQLSTATE 0A000 `extension "vector" is not available`.
  - mongo:8 community → code 31082 `SearchNotEnabled`.
  - libSQL missing index → `vector index(search): failed to parse vector index parameters`.
- **X8 brute force** (384-d, top-10): 100k vectors take 3.7 ms in RAM on 1 thread (146 MB), or 54–87 ms when scanned from SQLite. Use a multi-accumulator dot product (a naive one is 6x slower).

## Calibration (X6) and embedder numbers
- **Embedder pool** (tract, one shared plan): 4 threads give about 7 passages/s (350 tokens) at about 580 MB RSS, and 8 threads give about 12/s. Idle unload drops to 130 MB, and the reload takes about 0.9 s. A query jumps ahead of an import (about 0.15 s).
- **Real reindex:** 759 chunks in 62 s.
- **Corpus:** the docs of desktop, memory and skills: 20 docs, 1144 chunks. 40 queries (25 English, 15 Polish) plus 6 off-topic.
  - recall@3 fused: 38/40.
  - keyword: 31/40 (Polish only 6/15).
  - vector: 38/40.
- **Constants** (`src/store/rank.rs`):
  - `COSINE_FLOOR` 0.80, `FLOOR` 400, `MARGIN` 0.4, `LINKED_BOOST` 1.1, `GLOB_BOOST` 1.3, `KEYWORD_FLOOR` 0. Keyword scores are not comparable across backends, so cosine is the portable signal.
  - Recall: `RECALL_COSINE_FLOOR` 0.82 with a 0.02 band (0/6 off-topic answered, found@3 37/40), and `RECALL_KEYWORD_FLOOR` 10 when the embedder is down (SQLite/libSQL scale, uncalibrated on Postgres and Atlas).
- **Reproduce:** `cargo test --release --lib search::tests::calibration -- --nocapture` with `HARNESS_KNOWLEDGE_TEST_MODEL_DIR` and `HARNESS_KNOWLEDGE_TEST_CORPUS_ROOT`.

## Routing: Remote / This computer / Remote + this computer (approved 2026-11-01)
At parity with Harness Memory's "Kept in".
- **Config:** `[routing] repository_documents` and `global_documents`, each `remote`, `local` or `remote+local`. Remote is the default when a database URL is set.
- **Routed store:** this computer's SQLite plus the remote database.
  - Repo-linked documents follow `repository_documents`, and global documents follow `global_documents`.
  - The list merges both places with paging.
  - Search runs the hybrid pipeline in each place and fuses the result lists with RRF.
- **Ids:** documents carry their place. Ids from this computer are written `local:<id>`, and the CLI and MCP tools accept both forms.
- **Writes:** `docs import` and `held approve` take `--place remote|local`, defaulting to remote in `remote+local`. `check` and `stats` report both places.
- **Desktop:** Kept in rows in Settings → Remote knowledge (memory's 3-way component) with a routing-change warning, a "Save to" choice in Add documents when a row is `remote+local`, and a Kept in filter and place chip in the view.
- **Not included:** copying local documents to remote, and hide on this computer (follow-up issue).

## Offline fallback (approved 2026-11-01)
At parity with Harness Memory.
- In `remote+local`, an unreachable remote (timeout, refused connection, DNS failure) makes the routed store answer from this computer only. `docs list`, `search`, `stats` and the MCP replies then carry `offline: true`, and MCP and recall add one notice line.
- A shared cooldown file stops every call from waiting out the remote timeout again (memory's `store/reach.rs` pattern).
- Remote-only routing still fails clearly.
- Writes meant for remote are refused while it is unreachable, never redirected.
- Desktop: an offline banner in the Knowledge view, like Memory's.

### Offline fallback: what was built
- **Unreachable** is a connect timeout, a refused connection, a host that does not resolve or a lost connection (`StoreError::Unreachable`, `OpenError::Unreachable`). A certificate the TLS check refuses, a login that fails and a missing capability stay failures. PostgreSQL also counts SQLSTATE class 08 and 57P01 to 57P03, libSQL every transport failure except TLS, MongoDB server selection, network and DNS errors.
- **Probe:** with both places in use the remote gets 3 s to connect (1 s for `recall --hook`), and 6 s (2 s) in all for a server that accepts the connection and never speaks. A URL that sets its own timeout keeps it. Remote-only keeps the drivers' 10 s.
- **Cooldown:** `<data>/harness-knowledge/remote-unreachable.json` (next to the local database, private, names the remote by a hash without credentials), 60 s, shared by every process. An unreachable result writes it, an answer from the remote (or a `check` that reaches it) removes it. Reads skip the remote while it runs, `docs import` and `docs reindex` to the remote always try.
- **JSON:** `offline: true` (absent otherwise) on `docs list --json`, `repos --json` and `stats --json`. In `stats`, `places.remote` is `{"offline": true}` and the top-level counts are this computer's. `search --json` keeps its array and writes `harness-knowledge: Remote knowledge is unreachable. Showing documents on this computer only.` to stderr. `check --json` is unchanged.
- **Refusals:** `docs get`, `docs matches` and the changes of a remote id exit 1 with `remote knowledge is unreachable right now`. Imports and reindexes to the remote exit 1 with `remote knowledge is unreachable right now, so nothing was changed. Nothing is saved to this computer instead`.
- **MCP:** `knowledge_search`, `knowledge_list` and `knowledge_get` start the first offline reply with the notice line, and again after the remote was reachable in between. `recall --hook` uses this computer's documents and adds nothing to its block.
- **Feature token:** `offline-fallback`.
- **`docs list --place remote|local`:** narrows the list to one place, merged and paged in the routed store so `total` and the pages are of that set (`KnowledgeStore::list_in`). With one place in use the other lists nothing, a bad value exits 2, and `--place remote` while the remote is unreachable exits 1 with `remote knowledge is unreachable right now`.

## Review repairs to routing and offline (2026-11-01)
- **Probe records:** a probe that only ran out of time (the 1 s of `recall --hook`, the 3 s of a read) is recorded in the cooldown with how long it waited (`waited` in `remote-unreachable.json`), and only a reader that waits no longer honours it. A refused connection, a failed lookup or a lost connection is a fact for every reader. `reached()` clears only its own remote's record, and a lost connection's reason is scrubbed first.
- **Writes try the remote:** every write that reaches a routed store (rename, delete, links set, held approve, embeddings, metadata) tries the remote with the driver's full timeout, cooldown or not, like an import. It is refused only when the remote cannot be reached now.
- **Errors:** a TLS failure (a certificate the client refuses, a server that does not speak TLS) is a failure in every backend, never unreachable and never offline. A libSQL request that is too slow and PostgreSQL SQLSTATE 08P01 are failures too. `check` reports a TLS refusal as `unreachable` but starts no cooldown.
- **Exit and JSON:** a command that fails because the remote cannot be reached prints `{"error": "unreachable", "message": ...}` on stdout (and the line on stderr), exit 1: `docs get`/`matches` of a remote id, `docs list --place remote`, a change of a remote id, an import or reindex to the remote, `held approve` to it, `search` with the remote alone.
- **`stats --json`:** each `places.<place>` carries its own `generation`, and `generations` is `{"remote": N, "local": M}` of the places that were counted.
- **Replaces:** `replaces` is an id as the tools show it (`12` or `local:12`) in the manifest and in `knowledge_add`, kept with its place. An import or an approval saves where that document is, refuses a `place` that disagrees, and refuses an id that names a place (`local:N`) while only one place is in use. A plain id with both places in use is the remote database's.
- **Title order:** every store, and the merge of two places, sorts titles with ASCII letters folded to lower case and the rest by bytes (SQLite `NOCASE`, PostgreSQL `translate`, MongoDB `title_key`).
- **Phrase filter:** PostgreSQL looks a phrase up in a `tsvector` of the heading path and one of the text (`tsv_heading`, `tsv_text`), so it never spans the title, the heading and the text. MongoDB matches a multi-word phrase on the server against `phrase_text` (no cap on candidates, totals and pages are exact). Both are part of schema version 1: the knowledge is unreleased, so there is no version 2 and no upgrade of an existing database (see "Second review repairs").
- **Bounds:** at most 4 remote opens may stay running past their deadline (a server that takes the connection and never speaks), more count as unreachable at once. `knowledge_list` caps the page number it accepts.

## Second review repairs (2026-11-01)
- **Schema:** the phrase columns (PostgreSQL) and fields (MongoDB) are folded into version 1, `SCHEMA_VERSION` is 1 in both, and the version 1 to 2 migrations are gone. A database made by the review build has to be recreated.
- **Upgrades of the future:** a migration runs in its own open, never inside the probe or silent-open deadline of an ordinary open (`backend::open_in_phases`). The first open only looks (`Upgrade::Defer`, answer `Opened::NeedsUpgrade`), the second runs it (`Upgrade::Run`) under `UPGRADE_BOUND` (10 minutes) on its own thread slot. A database still upgrading after that is a failure that starts with `upgrading the knowledge database`, never unreachable and never a cooldown. PostgreSQL runs the migration with `SET LOCAL statement_timeout = 0`, a finite 60 s idle-in-transaction limit (see "Third review repairs") and a 5 minute `lock_timeout`. MongoDB upgrades are idempotent entries of `UPGRADES`, record each version they reach, and fill fields in pages of 500 through `migrate::update_in_pages` (an `update` command per page, so it also works before MongoDB 8). libSQL has only version 1 and keeps its migrations as they are.
- **Reads of an older database:** `READABLE_FROM` of each backend is the oldest version a read copes with. A change that only adds leaves older databases readable, a change that does not raises it, and a read of anything older says it has not been upgraded yet. The rule is written in `store/postgres/schema.rs` and `store/mongo/schema.rs`, and decided in one place (`store/upgrade.rs`).
- **Silent opens:** a full set of silent-open slots is local to the process (`OpenError::Busy`): the call counts the remote as unreachable, and nothing is written to `remote-unreachable.json`.
- **Held replaces:** a proposal is given its place when it is held (`import::place::hold_replaces`): `12` becomes this computer's, the remote's or the only place's, tagged even with one place in use. An approval saves where that document is, accepts a tag that names the only place in use, and refuses one whose place is no longer in use or that disagrees with `place`. A legacy plain id is the remote's while both places are in use and the only place otherwise.
- **Held approve:** an approval to the remote goes through `attempt_remote` like `docs import` (bounded, cooldown recorded, refused while the remote cannot be reached).
- **TLS:** a MongoDB handshake that ends in end-of-file (connection limit, shutdown, balancer without a backend) is unreachable, only rustls' own words (certificate, alert, incompatible peer, corrupt message) are a TLS refusal. `check` decides whether to start a cooldown from the typed result of each probe (`Problem::Refused`, `mongo::probe::Problem::refused`, `libsql::Trouble::Refused`), not from words in the message.

## Third review repairs (2026-11-01)
- **TLS refusal (PostgreSQL):** the driver files every handshake failure under its TLS kind, a lost connection included. `check` counts it as a TLS refusal (no cooldown) only when rustls' error is among the causes or the cause is not an I/O error at all (a server that answers the TLS request with "no TLS here"). An I/O error without rustls inside (`tls handshake eof`, a reset) is unreachable, like `error::unreachable`, and starts the cooldown.
- **Migration limits:** the PostgreSQL migration lifts only `statement_timeout`. A client that stalls mid-migration still hits `idle_in_transaction_session_timeout` (60 s) and its locks are released.
- **A late upgrade says what becomes of it:** the message after `UPGRADE_BOUND` is per backend (`backend::late_upgrade`). PostgreSQL upgrades in one transaction on a thread of the process, so a command that ends closes the connection and the server rolls it back: the message says so and points to a long-running session (the MCP server, whose thread goes on and commits). MongoDB stamps its progress per version, so its message says the next run keeps going. The bound is a constant, not a setting. The alternative, joining the upgrade thread in the CLI before it exits, was not taken: it would make every command wait as long as the upgrade, which the bound exists to prevent.
- **Held `replaces` and reused numbers:** `knowledge_add` reads the document it replaces and keeps its `content_sha256` and `source_key` in the held item (`replaced`, left out of the file when there is none). The approval refuses, with `refused` ("the document this proposal replaces changed or was deleted"), unless that document is still there with both values, so a document deleted and a new one that took its number is never overwritten. A `replaces` naming a document that does not exist is refused when proposed. New local and libSQL databases count document numbers with `AUTOINCREMENT`, so a number is not given again (the knowledge is unreleased, so schema 1 changes in place).

## Known follow-ups
- **Mongo upgrades and older writers:** a future MongoDB upgrade can race with a writer of an older version, which keeps writing documents without the new field while the upgrade fills it in pages. Before the first entry of `UPGRADES` is added, re-scan after the version bump (fill what was written in between).
- **Desktop, remote-only outage:** the desktop briefly shows "0 documents" while the remote database is unreachable and nothing else is in use. The upgrading state also takes up to 15 s to clear after the upgrade ends.
- **`manifest::parse` and a plain `replaces` under local-only:** a plain id is read as the remote's there and fails safe, but the message names the remote database, which is misleading when only this computer is in use.
- **Approval check and write:** the identity check of a replacement and its write are two steps, so a change in between (another writer on a team database) is not caught. The window is a few milliseconds.
