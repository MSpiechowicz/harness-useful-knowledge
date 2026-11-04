# Knowledge drafts and team approval

Status: Implemented 2026-11-04, not released.

Approved 2026-11-04 (option A, recommended answers). Repos: harness-knowledge (→ 2.0.0) and harness-useful-desktop on `feat/knowledge-drafts`; harness-useful-skills only if agent-facing text changes. Mockup reference: session scratchpad `drafts-mockups/option-a.html` (History H1).

## Decisions after the mockups (override the sections below where they differ)

- E1 States are `live`, `draft`, `waiting` and `changesRequested`. Request changes needs a comment and moves waiting → changesRequested. Ask (again) moves draft or changesRequested → waiting. `send-back` is renamed `request-changes`, with refusal `commentRequired`.
- E2 Reviews are PR-style. `docs review --json ID --expect REV --verdict comment|approve|requestChanges [--body TEXT] [--comments FILE]` submits one review, which holds an overall body plus line comments.
  - A line comment is `{section, side: old|new, from, to, body}`, where `from`/`to` are line numbers inside the section's diff.
  - `approve` = promote (D8). `comment` changes no state.
  - The author cannot submit approve or requestChanges, but may comment and reply.
  - Comments are stored in `knowledge_doc_comments (id, doc_id, review_id, section, side, line_from, line_to, body, by_name, at, parent_id NULL, resolved_at NULL, resolved_by NULL, revision)`. Mongo keeps them in a `knowledge_comments` collection.
  - Replies: `docs comment reply ID COMMENT_ID --body`. Resolve: `docs comment resolve|unresolve ID COMMENT_ID`.
  - A line comment is "outdated" in the JSON when its section is gone or the lines it was written on (its `quote`) differ now. A comment without a quote falls back to its revision against the last content change. Overall comments and replies never are.
- E3 `docs compare --json ID [--since REV]` returns `FileDiff`-shaped sections, matching the desktop binding `ui/src/lib/ipc/bindings/FileDiff.ts`: `{path: heading, oldPath, status, hunks, additions, deletions, truncated, isBinary:false, staged:false}`.
  - Lines are the section text split at line breaks, with sentences inside long paragraphs kept as they are.
  - A new document compares against nothing, so every line is added.
  - `--since REV` compares with the snapshot taken when REV was asked. Every `asked` event stores its section texts (capped at 2 MiB, else `sinceUnavailable`).
- E4 Originals are stored for Download. `knowledge_doc_files (doc_id PK, name, bytes BLOB, sha256)` holds the imported file's bytes up to `FILE_BYTES_MAX`/`PDF_BYTES_MAX`. Mongo uses chunked binary documents in a `knowledge_files` collection, 8 MiB per part.
  - `docs download --json ID --to PATH [--live]` writes the original. Without a stored original it writes the stored text as `.md`/`.txt` and answers `"original": false`.
  - A pending row has its own file.
- E5 Replace and ask again is one step: `docs import --replacing … --ask [--note TEXT]` replaces the file and asks in one go, and refuses when approval is off.
- E6 Agent proposals:
  - `held request-changes --json ID --expect REV --comment TEXT` marks the held item `changesRequested` with the comment. The desktop sends the comment as a follow-up message to the chat whose session the item records, and disables the action when no live desktop chat has that session.
  - A later `knowledge_add` from the same session with the same title, or naming the item, replaces the held item.
  - `held reject … [--comment TEXT]`. The desktop model reads the existing held `addedBy`, which it drops today.
- E7 Any teammate other than the author can approve, not only the requester.
- E8 Schema version re-check per read (R1 b): every open store re-reads the version cheaply before each agent read. That is `PRAGMA user_version` (sqlite), meta (libsql/postgres) or a cached meta with a 5 s TTL (mongo). A newer version fails closed with `NewerSchema`. The desktop shows a one-time "restart your harnesses after updating Harness Knowledge" notice.
- E9 Version 2.0.0. The `upgrade` command (D11) is kept.
- E10 Desktop UI follows option A:
  - Review section, Changes summary (FileList colours) with one Review changes button, Sections that expand, Actions [Download][Replace file…][Delete], History H1 (neutral, merged events);
  - Status column and Status filter (All / Live / Drafts / Waiting for approval / Changes requested), badges size lg;
  - every button group one row, no wrapping;
  - no blue info text, toasts bottom right;
  - locked Save as draft with its reason and Change in Settings;
  - Update it / Add as new as a DropdownButton;
  - Review tab Added by column;
  - proposals get Approve / Request changes / Reject.
- E11 The full-width Review changes view reuses the Changes view:
  - `DiffView` (and its rows, thread and composer) as is;
  - the presentational parts of `features/changes/FileList.svelte` and `ReviewDialog`/`SendReviewDialog` are extracted into shared components, which git and knowledge both feed (jscpd threshold 0);
  - in split mode `DiffView` puts a thread under its row in the half of the commented side. This fixes code review too.

## Decisions

- D1 States `live`, `draft`, `waiting`. A pending version is a separate row with `pending_of = <live id>` and status `draft|waiting`, never `live`. At most one per live document.
- D2 Agents never see a non-live row: `knowledge_search`, `knowledge_get`, `knowledge_list`, `recall --hook`, the duplicate refusal (`mcp/duplicate.rs`), `knowledge_add` `replaces` (`mcp/add.rs:158`), `knowledge_feedback`, `record_shown`/`record_useful`. Fail closed by type: `Scope` constructors, `Detail::default()` and every `find_*` are live-only. User paths opt in explicitly.
- D3 User paths see drafts with their status: `docs list|get|similar`, `search --json`, import matching. Pending rows are not list/search rows of their own. The live row carries `pending`, and the pending text is reached through `docs get|compare ID`.
- D4 Team approval is the remote meta key `team_approval` (`on`, else off). It is read from the remote store explicitly, never through `RoutedStore::meta` (which falls back to local, `routed/mod.rs:118-139`). Remote unreadable → go-live, approve, send-back and approval set refuse with `unreachable`. The local SQLite store is always off.
- D5 Approval on, remote destination: every import is a draft. `held approve` of a new proposal gives a draft. A replace (`replaces`, `replacing`, "Update it", held `replaces`) of a live document writes or updates its pending version. Approval off: a replace is direct, unless `--draft`, which writes a pending version.
- D6 Changing a draft or waiting row (Replace file, held approve onto it) keeps it a draft and records `edited`. Waiting goes back to draft, so an approval always covers exactly what was asked.
- D7 The approver needs a git `user.name` that is none of the authors of the version (trimmed, case-insensitive). The authors are everyone who changed its content since it was last live: the names on the `drafted`, `edited` (a rename too) and `updateDrafted` events of the reviewed row since its latest `wentLive` or `approved` (a pending row has only its own events), plus the row's `updatedBy.name` and the latest `asked` event's name. One rule serves `selfApproval`, take-back (any author may take the ask back) and the waiting-for-you count. A rename records its `edited` event and leaves `updatedBy` and its `via` as they were. An author without a name blocks nobody. With approval on, asking needs a name. Self-declared identity stops honest mistakes, not attacks (said in Settings).
- D8 Promote (approve, or go-live of a pending row) runs in one transaction:
  - delete the live chunks and move the pending chunks with their embeddings to the live id;
  - copy title, source name, format, hashes, bytes, warnings, origin, `updated_at`, `updated_by`;
  - keep id, links, `created_at`, `added_by` and usage counters;
  - move the events to the live id and delete the pending row.

  Mongo also resyncs chunk `title`, `repo_keys`, `global`, `updated_at`, `review`.
- D9 Deleting a live document deletes its pending version. Deleting a pending row discards the update. Rename and links on live documents stay direct with approval on. Links on a pending row are refused (the live links win).
- D10 Schema bump in all four backends: sqlite `MIGRATIONS[1]`, libsql `MIGRATIONS[1]`, postgres migration 2 with `SCHEMA_VERSION = 2` and `READABLE_FROM = 2`, mongo `UPGRADES[0]` with `READABLE_FROM = 2`. Older servers get `NewerSchema` on open. This is an explicit exception to knowledge-quality.md's "no schema bump". docs/plans/knowledge.md:49 drops "a shared team review queue" from the non-goals.
- D11 `harness-knowledge upgrade --json` opens each place with `Access::Existing` + `Upgrade::Run`. The desktop runs it after installing or updating the sidecar and when `stats` answers "not upgraded yet". Read-only roles cannot upgrade; the desktop then says it waits for a teammate's update.
- D12 Lists stay server-side filtered and paged (`--status`, like `--useful`). Every state change bumps the generation, so the pulse keeps the view live. No Refresh button.
- D13 Feature tokens:
  - `docs-drafts`: states, `--draft`, list/get fields, history, compare, ask/go-live, stats counts, upgrade.
  - `docs-approval`: `approval get|set`, approve, send-back.

  Without `docs-drafts` the desktop hides all draft UI.

## Contracts

### Rows (`docs list`, `docs get`), behind `docs-drafts`

- `"status": "live"|"draft"|"waiting"`.
- `"revision": <int>`: the stored `review_rev`, raised by every content change and transition.
- Live rows: `"pending": null | {"id", "status", "revision", "updatedBy", "updatedAt"}`.
- `docs get` of a pending row: `"pendingOf": <live id>`.
- `docs get` of a non-live row: `"lastSentBack": null | {"by", "at", "comment"}`.

### `docs list --status live|draft|waiting|pending`

Default: every non-pending row.

- `live`: status live, with or without a pending version.
- `draft`: draft rows, plus live rows whose pending version is a draft.
- `waiting`: waiting rows, plus live rows whose pending version waits. This is the desktop's "Waiting for approval".
- `pending`: live rows with a pending version.

`StatusFilter` lives in `store/mod.rs` next to `UsefulFilter` (`store/quality.rs`). It is parsed in `cli_docs/listing.rs` beside `--useful` and applied in:
- `sql_shared::list_conditions`
- `postgres/listing.rs`
- `mongo/listing.rs:104`
- the routed merge

### Transitions

`docs ask|send-back|approve|go-live --json ID --expect REV [--comment TEXT]`

- `--comment` is for send-back only: at most 500 characters, cleaned like titles.
- The reply is `{"id", "status", "revision", "promoted": bool}`. `id` is the live id after a promote.
- Refusals print `{"error": code, "message"}` and exit 1. Codes:
  - `notFound`
  - `changed` (`--expect` differs)
  - `wrongState`
  - `approvalOn`
  - `approvalOff`
  - `selfApproval`
  - `noName`
  - `readOnly` (`remote_writable() == Some(false)`)
  - `refused`
  - `unreachable` (remote down, never a local fallback)

| Action | Approval on | Approval off or local |
|---|---|---|
| ask | draft → waiting | `approvalOff` |
| send-back | waiting → draft | `approvalOff` |
| approve | waiting → live or promote | `approvalOff` |
| go-live | `approvalOn` | draft or waiting → live or promote |

Each transition is one compare-and-set per backend: `UPDATE … WHERE id = ? AND review_rev = ?`, or Mongo `findOneAndUpdate` in the existing transaction (`store/mongo/write.rs`). The policy checks live in `src/review.rs`.

### `docs history --json ID`

- Reply: `{"events": [{"kind", "by": name|null, "at": RFC 3339, "comment": string|null}], "total"}`, newest first, at most 200.
- Kinds: `drafted`, `edited`, `updateDrafted`, `asked`, `sentBack`, `approved`, `wentLive`.
- A live document's history includes the rounds of its promoted pending versions.

### `docs compare --json ID`

ID is the pending row, or a live row that has one.

Reply: `{"live", "pending", "sections": [{"heading", "status": "added"|"removed"|"changed"|"same", "live": text|null, "pending": text|null}], "truncated"}`.

- Sections are chunks grouped by `heading_path` in ordinal order. A repeated heading pairs by occurrence.
- Sections follow the pending order. Removed sections sit at their live position.
- `same` carries no texts.
- Texts are capped at 1 MiB in total.
- Read through a new store method `section_texts(id)`.

### `approval get|set`, behind `docs-approval`

- `approval get --json` → `{"approval": bool, "place": "remote"|"local", "writable": bool|null}`.
- `approval set --json on|off`:
  - remote writer only: `readOnly`, or `refused` with no remote;
  - writes `set_meta("team_approval", …)`, which bumps the generation;
  - turning it on changes no stored document;
  - turning it off leaves waiting rows waiting, and their authors can go live.

### Import

- New flag `docs import --draft` and manifest field `"draft": true`.
- Event status gains `pending`. `added`/`updated` events gain `"draft": bool`.
- When a pending version is requested and the content hash matches the live document → `unchanged`.
- A match on the pending row → `unchanged` with `docId` = the pending row.
- `held approve` may answer `pending`, or `added` with `"draft": true` when approval is on.

### Stats

- `stats --json`, per place: `drafts`, `waiting`, `pending`.
- `documents` counts non-pending rows, drafts included.
- Top level:
  - `waiting`: every place;
  - `"approval": bool|null`: `null` with no remote, remote down, or a server without `docs-approval`.

## Storage per backend (K1)

- **SQL** (sqlite and libsql through a `review!()` macro in `sql_shared.rs`; postgres has its own):
  - `knowledge_documents` gains:
    - `status TEXT NOT NULL DEFAULT 'live' CHECK (status IN ('live','draft','waiting'))`
    - `pending_of INTEGER NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE`
    - `review_rev INTEGER NOT NULL DEFAULT 0`
    - `CHECK (pending_of IS NULL OR status <> 'live')`
  - Unique index `ON (pending_of) WHERE pending_of IS NOT NULL`, and an index `ON (status)`.
  - New table `knowledge_doc_events (id PK, doc_id FK CASCADE, kind, by_name NULL, comment NULL, at)`, with an index on `(doc_id, id)`.
- **Mongo** `UPGRADES[0]`:
  - documents get `status: "live"`, `review_rev: 0`, `review: []` (embedded events, `$slice` 200);
  - chunks get `review: "live"` (values `live`, `draft` or `pending`; waiting maps to draft, so draft ↔ waiting never rewrites chunks);
  - a partial unique index on `pending_of`, and an index on `status`;
  - new search indexes `knowledge_text_2` and `knowledge_vectors_2` with `review` as token/filter. `ensure_search_indexes` creates them by name. The old indexes are dropped once the new ones are Ready.
- **Store API**:
  - `Document` gains `status`, `pending_of`, `review_rev`.
  - `Scope` gains `drafts: bool`, false from `from_repo`/`everything`, true through `with_drafts()`.
  - `ListQuery` gains `status: StatusFilter` with no default.
  - `Detail` gains `drafts: bool`, default false.
  - `ChunkHit` gains `status`.
  - `find_*` take `Visibility::{Live, WithDrafts}` and always skip pending rows.
  - New methods: `pending_of`, `set_review`, `promote`, `history`, `section_texts`.
  - Counters count live rows only.
- **Agent SQL filter**: `d.status = 'live'` in `visible()` (`sql_shared.rs:110`) and in postgres `visible` (`postgres/search.rs:32`). User scope: `d.pending_of IS NULL`.
- **Mongo search**: `keyword_scope`/`vector_filter` always add `review = live` for agents.
- **libSQL ANN**: the existing `WINDOWS` widening loop absorbs the drafts.

## Packages

Revised after the mockups:
- K1 adds the `changesRequested` state, the comments table/collection, the files table/collection, the asked snapshots and the per-read version check.
- K3 adds the review submit, the comments, `--ask`, the stored originals on import and held request-changes.
- K4 adds `docs review|comment|download|compare --since`.
- D1 adds the review, comments, download (native save dialog), held `addedBy` and request-changes, plus the follow-up to the originating chat.
- A new package D1b (frontend, before D2) extracts the FileList and review-dialog presentational parts and fixes the `DiffView` split thread side.

| Id | Repo | Content | After | Class |
|---|---|---|---|---|
| K1 | knowledge | Schema v2 in 4 backends, `upgrade` command, store types + new trait methods (each backend, routed, `routed/tests/flaky.rs`), shared contracts `store/testing/drafts.rs` wired per backend, version asserts 1→2 | - | backend |
| K2 | knowledge | Read paths: search SQL/Mongo filters, list/get/`StatusFilter`, `find_*` visibility, MCP (`scope`, `list`, `get`, `feedback`, `add`, `duplicate`, `counting`), `recall.rs`, `import/similar.rs`, user `cli_search.rs` `with_drafts`, a leak test per path | K1 | backend |
| K3 | knowledge | Writes: Importer `draft` + approval-forced draft, pending-version path, held approve under approval, `src/review.rs` policy + transitions, delete cascade, links refusal on pending | K1 | backend |
| K4 | knowledge | CLI: `docs ask/send-back/approve/go-live/history/compare`, `--status`, `--draft`, `approval get/set`, `upgrade`, JSON fields, stats counts, refusal codes, FEATURES tokens | K2, K3 | backend |
| K5 | knowledge | README, knowledge.md:49, knowledge-quality.md exception note, MCP snapshot check | K4 | general-purpose |
| D0 | desktop | Mockups: import switch, panel review, table status, Settings row, rail badge | - | parent |
| D1 | desktop | Business + Tauri plumbing: models, list `--status`, `knowledge_review.rs`, approval get/set, import `draft`, `KnowledgeFileStatus::Pending`, pulse `waiting`, upgrade call, commands (lib.rs, build.rs, permissions, capabilities), `commands-knowledge.ts`, mocks, bindings, stand-in + `knowledge_review.rs` tests | K4 contracts | backend |
| D2-D4 | desktop | Import switch, DocumentPanel review/history/compare, table status column + filter, view-state, `knowledge` locales ×6 | D0 pick, D1 | frontend |
| D5 | desktop | Settings team-approval row (applied at once, identity note, read-only disabled), rail badge, `settings`/`shell` locales | D0 pick, D1 | frontend |

Order:
- K1 runs first.
- K2 and K3 run in parallel, then K4.
- D2-D4 are serial in one worker. D5 runs in parallel with them.

## Verification

- **Leak contract, all backends.** A fixture with live, draft, waiting and pending rows sharing words, links, titles, content and chunk hashes. Every agent path returns only live ids:
  - keyword and vector top-k in repo, global and everything scopes;
  - `get` of a draft or pending row → None;
  - `list`, and `find_*` with `Live`;
  - counters on drafts are no-ops;
  - the same assertions through MCP calls and `recall --hook`.
- **Migration.**
  - v1 → v2: every row is live and the events table exists.
  - A v1 reader gets "not upgraded yet" until `upgrade` runs.
  - A v3 database gives `NewerSchema`.
  - Postgres and Mongo use the env-gated upgrade tests. Mongo also checks the `_2` indexes.
- **Promote.**
  - Id, links, `created_at`, `added_by` and counters survive. Chunks and embeddings move. No pending row remains.
  - Search finds the new text only after the promote.
  - Mongo resyncs the chunks.
  - The unique pending index refuses a second pending row.
- **CLI.** Each command and each refusal code:
  - `changed`;
  - self-approval of " ada " against "Ada";
  - `noName`;
  - local → `approvalOff`;
  - remote down → `unreachable` with no local fallback (Flaky wrapper);
  - read-only → `readOnly`;
  - the generation grows on each transition.
- **Team tests.** `HARNESS_KNOWLEDGE_TEST_{POSTGRES,LIBSQL,MONGO}_URL`, reported as not run when no database is available.
- **Desktop.**
  - `cargo nextest`, vitest, svelte-check, i18n parity, guard checks by exit code.
  - WebKitGTK mock check of every state.
  - Real sidecar on local SQLite: draft → go live, agent search before and after through `harness-knowledge mcp`.
  - Two identities through repo-local `git config user.name`.
- **Review and security scope.**
  - The trust boundary is "agents never see non-live rows": every `Scope`, `Detail` and `find_*` caller.
  - A spoofed name allows self-approval. This is accepted and documented.
  - Comments are shown in the UI only.

## Risks

- R1 (High) Processes started before the upgrade keep reading v2 and would show drafts as live:
  - Mongo `reader()` checks only the flag set at connect;
  - SQLite checks `user_version` only at open;
  - Postgres re-reads the version only in write transactions.

  Options:
  - (a) A release note and a desktop notice: restart harnesses after updating.
  - (b) v2+ readers re-check the schema version per read.
  - (c) A separate draft chunk table.

  Recommended: (a)+(b), and don't claim old clients are protected.
- R2 The schema bump forces every teammate to update together, which points to a major sidecar version (2.0.0).
- R3 The Mongo `_2` index build delays searches until the indexes are Ready. Not measured on Atlas. DocumentDB has no mongot, so check how its keyword path filters.
- R4 libSQL `remote_writable` cannot detect read-only tokens: actions stay enabled and fail with the server error.
- R5 Without the D11 upgrade trigger, the first agent search after the update fails on a v1 database.
- R6 About 230 `Scope`/`ListQuery` literals in 49 files (mostly tests) change mechanically in K1.

## Release order

knowledge (K1-K5) → skills (S1 only if needed) → desktop (D1-D5, gated on `docs-drafts`/`docs-approval`). Each release only with the user's approval. The notes say everyone on a team database updates and restarts their harnesses.

## Release notes

### harness-knowledge 2.0.0

Breaking:
- Schema v2 in all four backends. An older server refuses the upgraded database, so everyone on a team database updates together.
- Restart harnesses after updating, so no old process keeps reading.
- Run `harness-knowledge upgrade` or let Harness Desktop do it. Read-only roles cannot upgrade and wait for a teammate.

New:
- States `live`, `draft`, `waiting`, `changesRequested` and pending updates. Agents only ever see live documents.
- Team approval in the remote database (`approval get|set`). One approval by a teammate other than the author, identity = git `user.name`.
- `docs ask|request-changes|approve|go-live|take-back|review|history|comments|comment|compare|download`, `docs list --status`, `docs import --draft|--ask|--note`, `upgrade`.
- `held request-changes`, `held reject --comment`, `knowledge_add` `revises`.
- Line comments keep the lines they were written on (`quote`: line, kind added|removed|same, text, cut?; at most 20 lines and 4000 bytes) and `outdated` follows them. The column is part of schema v2 (MongoDB stores the field).
- Going live ends a review: approving a waiting document, `go-live` of a draft and promoting a pending version delete the version's comments (replies included) in the same transaction. The history keeps its events. A `quote` line over the byte cap is kept cut (`"cut": true`) and compared by its start.
- Team databases never store a local folder: a `path:` project key (a folder with no git remote) is refused for the remote place as `localPath` (`docs links set`, `docs import`, `held approve`, and by the PostgreSQL, libSQL and MongoDB stores themselves), and a writer that opens a team database removes `path:` links an earlier version left. This computer's database keeps them.
- `docs comment delete`: a review comment can be deleted by the one who wrote it, GitHub-style. A thread takes its replies with it, a reply goes alone, and the history keeps its events.
- `stats` counts `drafts`, `waiting`, `pending` and reports `approval`.

Known limits:
- libSQL cannot detect read-only tokens, so actions stay enabled and fail with the server's error.
- The Mongo upgrade briefly needs 4 search indexes. Clusters capped at 3 (for example Atlas M0) may fail.
- Postgres does one extra version lookup per read.
- Identity is self-declared: it stops mistakes, not impersonation.
