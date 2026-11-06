## [2.2.1](https://github.com/MSpiechowicz/harness-useful-knowledge/compare/v2.2.0...v2.2.1) (2026-11-06)


### Bug Fixes

* **project:** do not take a project's key from planted or changed git config ([f2c7857](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/f2c785782be29121e52acbc0703bc73a10e4abf8)), closes [MSpiechowicz/harness-useful-desktop#55](https://github.com/MSpiechowicz/harness-useful-desktop/issues/55)

# [2.2.0](https://github.com/MSpiechowicz/harness-useful-knowledge/compare/v2.1.4...v2.2.0) (2026-11-06)


### Features

* **docs:** filter and sort the documents by who added them ([a837071](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/a837071a821d05494293b930b78b82a99f900298))

## [2.1.4](https://github.com/MSpiechowicz/harness-useful-knowledge/compare/v2.1.3...v2.1.4) (2026-11-06)


### Bug Fixes

* **mcp:** tell the agent to call a tool again after refused arguments ([e853e49](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/e853e499c7ca5d63dc4a252687d1b07caa672070))

## [2.1.3](https://github.com/MSpiechowicz/harness-useful-knowledge/compare/v2.1.2...v2.1.3) (2026-11-05)


### Bug Fixes

* **recall:** keep a place for a matching global document and make the link boost a tie-break ([3c7b68c](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/3c7b68c8616ba9eaef0a011431022904e88c2215))

## [2.1.2](https://github.com/MSpiechowicz/harness-useful-knowledge/compare/v2.1.1...v2.1.2) (2026-11-05)


### Bug Fixes

* **mongo:** count an any-database update privilege as writable ([435321f](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/435321f4653b05ed316ae232c608909118a7e41a))

## [2.1.1](https://github.com/MSpiechowicz/harness-useful-knowledge/compare/v2.1.0...v2.1.1) (2026-11-05)


### Bug Fixes

* **routing:** give a slow remote database longer before calling it offline ([6c33908](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/6c33908afa77db275edf7018b609bbb153ab3931))

# [2.1.0](https://github.com/MSpiechowicz/harness-useful-knowledge/compare/v2.0.0...v2.1.0) (2026-11-05)


### Features

* **docs:** sort the document list by how many paths it is narrowed to ([ae2e257](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/ae2e257e0251b09655e1b71d706d4bfd44dfd246))

# [2.0.0](https://github.com/MSpiechowicz/harness-useful-knowledge/compare/v1.1.0...v2.0.0) (2026-11-04)


* feat(docs)!: drafts, team approval and review of knowledge documents ([9fe2d71](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/9fe2d71522ae9a1c994dc0e3176f3fa2fc0c124b))


### Bug Fixes

* **postgres:** two clients that create pgvector at the same moment both go on ([328dbbd](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/328dbbdc608df821a72ad3b54adf83c940051aeb))


### BREAKING CHANGES

* the database schema moves to version 2 on every backend
(review state, events, comments, stored originals). Older harness-knowledge
builds refuse a version 2 database; run `harness-knowledge upgrade` (the
desktop runs it) to move a team database.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>

# [1.1.0](https://github.com/MSpiechowicz/harness-useful-knowledge/compare/v1.0.1...v1.1.0) (2026-11-02)


### Features

* **docs:** usefulness, authors and similar documents ([cdb483e](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/cdb483efadb301ac262ecbce08444f94de075e6d))

## [1.0.1](https://github.com/MSpiechowicz/harness-useful-knowledge/compare/v1.0.0...v1.0.1) (2026-11-01)


### Bug Fixes

* **libsql:** repeat a read once when the server closed the connection cleanly before replying ([4c37ff3](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/4c37ff346418b6901bc11cf8992f2b018cd39ae9))

# 1.0.0 (2026-11-01)


### Bug Fixes

* **ci:** cross-platform held claims and tests ([06c5e29](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/06c5e2974b37a8a1f0aae12e8574d8cd7e613468))
* **ci:** platform-gated imports for macOS and Windows clippy ([0bc56a0](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/0bc56a0a9033b0950826eb3407cf4d9dc45955d0))
* held claim stamped before the move, longer claim timeout, text fallback for look-alike transcripts, lone angle brackets, secret folders refused ([abdddf6](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/abdddf63b13f4d5d2a719ce5985f29d6347376ab))
* **held:** a claim that finds no file after its rename lost the race (Windows) ([76f2943](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/76f2943547a1d80696e7d9056c89d4c8a7e81fa1))
* **held:** an exclusive claim marker per proposal, atomic on every platform ([68fcc90](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/68fcc9015bd23e4c41778aa954ad8a646afd3755))
* lost TLS handshakes are unreachable, finite idle limit in migrations, truthful late-upgrade messages, approvals check the replaced document's identity ([85a83a1](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/85a83a14ed34c67931e3d0e65bd4a9c0330771d9))
* **mcp:** secret folders refused whatever their case ([feaac97](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/feaac976623785bcbeec6ff8d91c54569a83b7b8))
* review and security findings, subtitle transcripts ([a36116e](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/a36116e30f5a0b66a2a6c8297d5ddfdd75aef69c))
* routing and offline review findings ([71440b3](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/71440b307c8312213a693daf6023e407a66cfcac))
* second routing review repairs ([3fad84a](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/3fad84a1c4ed128e51cb779d0e0546cbf24d0f1d))


### Features

* crate skeleton, config, CI, release and installers ([2f71b3d](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/2f71b3d81967cb61110849ea2a4d3ae40acdfc8b)), closes [MSpiechowicz/harness-useful-desktop#45](https://github.com/MSpiechowicz/harness-useful-desktop/issues/45)
* **docs:** sort the list by format, chunks and repositories ([4f2a103](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/4f2a103d18222f06cf9ca6eaa7172c03d5e22c60))
* e5 embedder, ingestion pipeline and PostgreSQL, MongoDB, libSQL stores ([5a00704](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/5a00704c6ccc12c8f00ee67d3b2a08d24c09ef81))
* hybrid search, MCP server, review queue, recall hook and integration fixes ([f0ba153](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/f0ba153441d82aac1a38edcd248c05e140434dbb))
* offline fallback for remote+local and a place filter for the document list ([3747c3a](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/3747c3a46e5dcf09b88084b2cdc648718bf3646c))
* Remote / This computer / Remote + this computer routing, per-document matches, phrase list filter ([5ae0b08](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/5ae0b08456d2fca26b9d90749a9f2598932bb13e))
* **stats:** count waiting review proposals in stats ([42bddae](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/42bddae42e3359d9b211f471c5d8e727bef86ccb))
* **store:** knowledge store trait, SQLite backend and docs CLI ([e54eea8](https://github.com/MSpiechowicz/harness-useful-knowledge/commit/e54eea88c79f086b13b725642f8409d6b8176a66))
