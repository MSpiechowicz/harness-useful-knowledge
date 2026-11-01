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
- **Formats:** Markdown, text, HTML/Confluence export, OpenAPI JSON/YAML, AsciiDoc, RST, PDF (pdf-extract in an isolated worker process; scanned PDFs refused). Files only.
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
DocumentDB, live connectors, code auto-links (#49), OCR, attachments, an agent delete tool, a shared team review queue, and memory's notes, entities, scripts, trust, hooks, routines, push, offline and usefulness features.

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
  - `documents` (source_key, content_sha256, format, origin, …)
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
