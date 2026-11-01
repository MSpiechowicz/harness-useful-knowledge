# harness-knowledge

Project knowledge MCP server for any coding agent harness: people import documents (Markdown, text,
HTML, OpenAPI, PDF and more) and agents search them with hybrid BM25 and vector ranking. Part of the
Harness family, next to `harness-memory`.

**Status: in development.** The crate skeleton exists (`--version`, `features`, the configuration and
the repository identity shared with `harness-memory`). Import, search and the MCP tools are not built
yet, so this README stays short until they are.

## Build

```sh
cargo build --release
./target/release/harness-knowledge --version
```

## Configuration

The local database is `<data dir>/harness-knowledge/knowledge.sqlite3`. A team database is set in
`<config dir>/harness-knowledge/config.toml` or with environment variables.

```toml
[database]
url = "postgres://user:password@db.example.com/knowledge"
token = "only for libsql"
```

| Variable | Meaning |
| --- | --- |
| `HARNESS_KNOWLEDGE_DIR` | Folder of the local database (absolute) |
| `HARNESS_KNOWLEDGE_CONFIG` | Config file to use instead of the default one |
| `HARNESS_KNOWLEDGE_DATABASE_URL` | Team database URL (overrides the config file) |
| `HARNESS_KNOWLEDGE_DATABASE_TOKEN` | libSQL auth token |

Supported schemes: `postgres://`, `mongodb://`, `mongodb+srv://`, `libsql://` and `https://`. Amazon
DocumentDB is not supported.

## License

GPL-3.0-or-later, see `LICENSE`.
