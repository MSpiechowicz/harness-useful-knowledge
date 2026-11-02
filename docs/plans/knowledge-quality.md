# Knowledge usefulness, update, duplicates and authors (desktop issue #52)

Approved 2026-11-01. Repos: harness-knowledge (`feat/knowledge-quality`), harness-memory (`feat/author`),
harness-useful-skills and harness-useful-desktop (`feat/knowledge-quality`).

## Decisions

- Author = git `user.name` only, never the email. Knowledge documents, memory notes and memory scripts.
- Title matches count only within the same repository or global scope.
- The up to 3 pointers the desktop chat recall injects count as shown (memory's recall does not count).
- `knowledge_add` with a similar document and neither `replaces` nor `distinct: true` is refused, nothing is queued.
- No schema version bump in any backend: columns are added in place (memory's `ensure_extensions` pattern),
  so older servers keep reading the database and read-only roles read without the new fields.
- Usefulness stays out of ranking.

## Contracts

### Authors (knowledge and memory)

- JSON: `"addedBy": {"name": "Ada", "via": "claude-code" | null} | null`, same for `"updatedBy"`.
  `null` when no git name was resolved. `via` is the MCP client name for agent writes, `null` for user
  writes (CLI without `--client`, desktop imports).
- Storage: nullable `added_by_name`, `added_by_via`, `updated_by_name`, `updated_by_via` (Mongo: fields).
  In memory's Postgres this is not `written_by` (that is the database role).
- A create sets both. An update or replace keeps `added*` and sets `updated*`.
- Name: `git config --get user.name` through the hardened runner (`src/support/git.rs`) in the writer's
  repository root, else in the current folder (global config still applies). Cached once per process.
  Cleaned like memory's `check_author`: trimmed, control characters refused, at most 80 characters.
- Held proposals capture the author when they are held, approval writes it. The held file gets
  `"addedBy": "Ada"` (name only, the harness is its `client`), left out when no name was known, and it is
  not part of the file's `revision`, so files with and without it load in every version. An approval of
  a file without it reads the name then, as before.
- Memory keeps its existing `author {source, root}` object unchanged.

### Knowledge counters

- `shown`, `useful`, `lastUsefulAt` on every document row in `docs list` and `docs get`, absent when the
  backend has no counter columns (a read-only role on a database no writer extended yet).
- Counted per document, once per MCP session. The desktop client (`harness-desktop`) does not count shown.
- Recall (`recall --hook`) counts the documents it points to as shown, written after its output is
  flushed, a single attempt.
- Writes: a separate handle used only for counters (`Access::Counters`), a single attempt that never waits
  on a lock (SQLite busy timeout 0 from the open on), errors dropped. It never migrates or upgrades, never
  adds columns and never runs DDL (a database that needs either, or that lacks the columns, drops the
  count: only write paths that may wait extend a database). Each flush has a hard time budget, and after
  a failed open or flush the handle is not tried again for a minute. The search handle stays read-only.
  Counters land in the place the id carries. Counters saturate at the largest integer and a stored value
  that is not an integer reads as 0.

### `docs list`

- `--useful any|ever|never`, `--sort ...|useful|lastUseful` (same words as memory's `notes list`),
  filtered and sorted in each backend and in the routed merge. `ever` = useful at least once, `never` =
  shown at least 3 times and not once useful (memory's rule: a document nobody has seen says nothing, a null
  counter counts as 0). A document nobody rated sorts as 0, ties by id in the sort's direction. Without the
  counter columns `ever` and `never` list nothing, `any` all, and the two sorts are the default order
  (`updated`).
- Rows of `docs list` and `docs get` carry `shown`, `useful`, `lastUsefulAt` (RFC 3339 or `null`), the keys
  are absent without the counter columns.

### `knowledge_feedback` (MCP)

- `{ids: [integer|string] (at most 10), reason?: string (at most 200), project_path?, scope?: "repo"|"all"}`.
  Ids as `knowledge_get` takes them (`12`, `"local:12"`) or chunk refs as a search shows them (`12:340`),
  reduced to the document. `scope` matches the search's, so documents of other repositories a `scope: "all"`
  search showed can be rated.
- Reply: `marked N documents useful: <ids>`, then `not found: <ids>` / `not checked: <ids>` lines.
- Instructions and the `knowledge_search` description tell agents to call it with the ids that helped
  (or an empty list).

### `docs similar --json [--repo KEY | --global] PATH...`

- Read access. Each file is extracted and chunked by the import rules (source key, title, content hash,
  chunk hashes) and looked up in every place in use. Folders expand to their files.
- Reply: `{"similar": [{"path", "match": null | {"id", "title", "place", "reason", "sharedChunks",
  "chunks", "addedBy", "updatedBy", "updatedAt"}}]}` (`updatedAt` RFC 3339 like every CLI time), plus `"offline": true` while the remote is down.
- `reason`, strongest wins: `sourceKey` > `sameContent` > `sharedChunks` (more than half of the file's
  distinct chunk hashes and at least 2) > `title` (case and whitespace insensitive, same repository or
  global scope only).
- Shared logic in `src/import/similar.rs`, also used by `knowledge_add`.

### Import manifest

- New `"replacing": {"/abs/path.md": "local:12" | 7}`. Every key must be one of `paths`, ids are unique,
  each id's place decides that file's place. Not combined with `replaces` (kept as it is).

### `knowledge_add`

- New optional `distinct: boolean`. Without `replaces` or `distinct`, a similar document refuses the call
  with the match (id, title, reason) and the hint to pass `replaces: <id>` or `distinct: true`. The text is
  read as its approval imports it (`import::similar::probe_text`). Its source key is `agent/proposal/<name>`
  (the approval's key holds the held id, which does not exist yet), so `sourceKey` matches only a stored
  document with exactly that key. The title scope is the repository the proposal links to (the project's
  when among the links, else the first link), else global. A store that cannot be opened or read never
  blocks the proposal.

### Remote write access

- `stats --json` (and the status the desktop reads) gains `"remoteWritable": true | false | null`.
  `null` = unknown, the desktop then keeps actions enabled and shows the import's error. Top level only,
  the remote place's answer: PostgreSQL and MongoDB report the user's UPDATE privilege, libSQL and a
  local file cannot tell on a read handle (`null`), and no remote or a remote that is down is `null`.

### Feature tokens

- knowledge: `docs-useful`, `docs-similar` (`docs similar`, `replacing`, the add check), `docs-authors`.
- memory: `notes-authors` (notes and scripts).

## Packages

| Id | Repo | Content | After |
|---|---|---|---|
| K1 | knowledge | added columns per backend, `Document`/`DocumentRow` fields, store methods (`record_shown`, `record_useful`, `find_by_title`, `docs_sharing_chunks`, `remote_writable`), contract tests | - |
| M1 | memory | authors on notes and scripts: capture, held, columns, split/push, JSON, token | - |
| K2 | knowledge | `docs similar`, `replacing`, import authors, author resolver | K1 |
| K3 | knowledge | `knowledge_feedback`, shown counting (MCP and recall), `knowledge_add` check | K1 |
| K4 | knowledge | list/get JSON, `--useful` and sorts, tokens, `remoteWritable` | K2 |
| S1 | skills | feedback and similar-document guidance | K3 |
| D0 | desktop | mockups: Replace + confirm, import match row, columns, chat cards | contracts |
| D1 | desktop | business/Tauri plumbing, gated on tokens | #48 on main |
| D2-D4 | desktop | Knowledge view, chat cards (shared with memory), memory Added by | D0, D1 |

## Release order

knowledge 1.1.0 and memory 3.1.0, then skills, then desktop. Each step only with the user's approval.
