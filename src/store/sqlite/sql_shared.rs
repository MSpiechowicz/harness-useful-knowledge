//! The SQL text the local SQLite file and the libSQL server share. libSQL is SQLite, so both
//! stores keep the same tables, the same FTS5 keyword index (with its triggers) and run the same
//! keyword, list and scope statements. They differ in the vectors only: a `BLOB` searched by brute
//! force here, an `F32_BLOB(384)` with an ANN index there.
//!
//! The DDL is a macro so each store can `concat!` it into its migration (a `const`). The
//! statements number their parameters (`?1`), so both runners bind them the same way.

use super::sql_quality::document_columns;
use crate::limits::{LIST_PAGE_MAX, QUERY_TOKENS_MAX};
use crate::store::author_list::groups_sql_sqlite;
use crate::store::{AuthorFilter, Columns, ListQuery, RepoFilter, Sort, StatusFilter, StoreError, UsefulFilter};

/// The meta keys the store keeps itself.
const RESERVED_META: [&str; 3] = ["generation", "schema_version", "dims"];

/// Fails for a meta key a caller may not set: one the store keeps itself, or an unreasonable one.
pub(crate) fn check_settable(key: &str) -> Result<(), StoreError> {
    if RESERVED_META.contains(&key) {
        return Err(StoreError::Invalid(format!("the meta key {key} belongs to the store")));
    }

    if key.is_empty() || key.len() > 64 {
        return Err(StoreError::Invalid("a meta key is 1-64 characters".to_owned()));
    }

    Ok(())
}

/// The meta, documents and links tables of version 1 (see [`super::schema`] for what they hold). A
/// document's number is never given again (`AUTOINCREMENT`), so a number that a proposal or a link
/// names cannot come to mean another document.
macro_rules! tables {
    () => {
        "
CREATE TABLE knowledge_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE knowledge_documents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    source_name TEXT NOT NULL,
    source_key TEXT NOT NULL,
    format TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    file_sha256 TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    chunk_count INTEGER NOT NULL,
    origin TEXT NOT NULL CHECK (origin IN ('user', 'agentApproved')),
    warnings TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX knowledge_documents_source ON knowledge_documents (source_key);
CREATE INDEX knowledge_documents_content ON knowledge_documents (content_sha256);

CREATE TABLE knowledge_doc_links (
    doc_id INTEGER NOT NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    repo_key TEXT NOT NULL,
    globs TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (doc_id, repo_key)
) WITHOUT ROWID;
CREATE INDEX knowledge_doc_links_repo ON knowledge_doc_links (repo_key, doc_id);
"
    };
}

/// The FTS5 index of title, heading path and text over `knowledge_chunks` (external content), the
/// triggers that keep it current, and the trigger that copies a retitle to the chunks.
macro_rules! keyword_index {
    () => {
        "
CREATE VIRTUAL TABLE knowledge_chunks_fts USING fts5(
    title, heading_path, text,
    content = 'knowledge_chunks', content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER knowledge_chunks_fts_insert AFTER INSERT ON knowledge_chunks BEGIN
    INSERT INTO knowledge_chunks_fts (rowid, title, heading_path, text)
        VALUES (new.id, new.title, new.heading_path, new.text);
END;
CREATE TRIGGER knowledge_chunks_fts_delete AFTER DELETE ON knowledge_chunks BEGIN
    INSERT INTO knowledge_chunks_fts (knowledge_chunks_fts, rowid, title, heading_path, text)
        VALUES ('delete', old.id, old.title, old.heading_path, old.text);
END;
CREATE TRIGGER knowledge_chunks_fts_update AFTER UPDATE OF title, heading_path, text ON knowledge_chunks BEGIN
    INSERT INTO knowledge_chunks_fts (knowledge_chunks_fts, rowid, title, heading_path, text)
        VALUES ('delete', old.id, old.title, old.heading_path, old.text);
    INSERT INTO knowledge_chunks_fts (rowid, title, heading_path, text)
        VALUES (new.id, new.title, new.heading_path, new.text);
END;
CREATE TRIGGER knowledge_documents_retitle AFTER UPDATE OF title ON knowledge_documents BEGIN
    UPDATE knowledge_chunks SET title = new.title WHERE doc_id = new.id;
END;
"
    };
}

/// Version 2: the review of drafts (see [`super::schema`] for what the tables hold). The columns are
/// added to the documents table, so every existing document is live at revision 0.
///
/// - `status`: `live`, `draft`, `waiting` or `changesRequested`.
/// - `pending_of`: the live document a row is the pending version of. A live row has none, and a
///   document has at most one pending version (the partial unique index).
/// - `knowledge_doc_events`: the history. `knowledge_doc_comments`: the comments of reviews, a reply
///   pointing at its parent (`quote`: the lines a line comment was written on, a JSON array of
///   `{line, kind, text}`, see [`crate::store::QuoteLine`], null for any other comment).
///   `knowledge_doc_files`: the original file, its bytes kept in the parts of
///   `knowledge_doc_file_parts` ([`super::sql_review::FILE_PART_BYTES`] each, because a libSQL server
///   refuses a single argument of more than a few MiB). `knowledge_doc_snapshots`:
///   the section texts as of an ask, a JSON array of `{heading, text}` at the revision it made.
///
/// Everything but the documents table is removed with its document.
macro_rules! review {
    () => {
        "
ALTER TABLE knowledge_documents ADD COLUMN status TEXT NOT NULL DEFAULT 'live'
    CHECK (status IN ('live', 'draft', 'waiting', 'changesRequested'));
ALTER TABLE knowledge_documents ADD COLUMN review_rev INTEGER NOT NULL DEFAULT 0;
ALTER TABLE knowledge_documents ADD COLUMN pending_of INTEGER
    REFERENCES knowledge_documents(id) ON DELETE CASCADE
    CHECK (pending_of IS NULL OR status <> 'live');
CREATE UNIQUE INDEX knowledge_documents_pending ON knowledge_documents (pending_of) WHERE pending_of IS NOT NULL;
CREATE INDEX knowledge_documents_status ON knowledge_documents (status);

CREATE TABLE knowledge_doc_events (
    id INTEGER PRIMARY KEY,
    doc_id INTEGER NOT NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    by_name TEXT,
    comment TEXT,
    at INTEGER NOT NULL
);
CREATE INDEX knowledge_doc_events_doc ON knowledge_doc_events (doc_id, id);

CREATE TABLE knowledge_doc_comments (
    id INTEGER PRIMARY KEY,
    doc_id INTEGER NOT NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    review_id INTEGER,
    section TEXT,
    occurrence INTEGER,
    side TEXT CHECK (side IS NULL OR side IN ('old', 'new')),
    line_from INTEGER,
    line_to INTEGER,
    body TEXT NOT NULL,
    by_name TEXT,
    at INTEGER NOT NULL,
    parent_id INTEGER REFERENCES knowledge_doc_comments(id) ON DELETE CASCADE,
    resolved_at INTEGER,
    resolved_by TEXT,
    revision INTEGER NOT NULL,
    quote TEXT
);
CREATE INDEX knowledge_doc_comments_doc ON knowledge_doc_comments (doc_id, id);

CREATE TABLE knowledge_doc_files (
    doc_id INTEGER PRIMARY KEY REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL
);

CREATE TABLE knowledge_doc_file_parts (
    doc_id INTEGER NOT NULL REFERENCES knowledge_doc_files(doc_id) ON DELETE CASCADE ON UPDATE CASCADE,
    part INTEGER NOT NULL,
    bytes BLOB NOT NULL,
    PRIMARY KEY (doc_id, part)
) WITHOUT ROWID;

CREATE TABLE knowledge_doc_snapshots (
    doc_id INTEGER NOT NULL REFERENCES knowledge_documents(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    sections TEXT NOT NULL,
    PRIMARY KEY (doc_id, revision)
) WITHOUT ROWID;
"
    };
}

pub(crate) use {keyword_index, review, tables};

/// A document linked to some repository (aliased `d`).
pub(crate) const LINKED: &str = "EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id)";

/// A document linked to no repository (aliased `d`).
pub(crate) const GLOBAL: &str = "NOT EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id)";

/// True for the documents of the scope: parameter `?{all}` is 1 for every document, else the
/// global documents (no link) and the ones linked to the repository in `?{repo}` (NULL for none).
/// Only live documents, unless the scope sees `drafts`, which sees every row but a pending version.
pub(crate) fn visible(all: usize, repo: usize, drafts: bool) -> String {
    let state = if drafts {
        "d.pending_of IS NULL"
    } else {
        "d.status = 'live'"
    };

    format!(
        "({state} AND (?{all} = 1
          OR NOT EXISTS (SELECT 1 FROM knowledge_doc_links v WHERE v.doc_id = d.id)
          OR EXISTS (SELECT 1 FROM knowledge_doc_links v WHERE v.doc_id = d.id AND v.repo_key = ?{repo})))"
    )
}

/// The globs of the document's link to the repository in `?{repo}`: NULL when it is not linked.
pub(crate) fn linked_globs(repo: usize) -> String {
    format!("(SELECT g.globs FROM knowledge_doc_links g WHERE g.doc_id = d.id AND g.repo_key = ?{repo})")
}

/// The words of `text` as an FTS5 query: `"a" OR "b"`. `None` when there is no word. The words
/// are lowercase letters and digits, so nothing in the query is FTS5 syntax.
pub(crate) fn fts_query(text: &str) -> Option<String> {
    let mut words: Vec<String> = Vec::new();

    for word in text.split(|character: char| !character.is_alphanumeric()) {
        if word.is_empty() || words.len() >= QUERY_TOKENS_MAX {
            continue;
        }

        let word = word.to_lowercase();

        if !words.contains(&word) {
            words.push(word);
        }
    }

    if words.is_empty() {
        return None;
    }

    let quoted: Vec<String> = words.iter().map(|word| format!("\"{word}\"")).collect();
    Some(quoted.join(" OR "))
}

/// The keyword candidates of the FTS5 query `?1` for the scope `?2` (all) and `?3` (repository),
/// at most `?4`: a word in the title counts four times, in the heading path twice, in the text once.
/// Columns: the shared hit columns (see `ChunkHit`), the score being the negated BM25, the last the
/// state of the document.
pub(crate) fn keyword_sql(drafts: bool) -> String {
    format!(
        "SELECT c.id, c.doc_id, d.title, c.ordinal, c.heading_path, c.text,
                -bm25(knowledge_chunks_fts, 4, 2, 1) AS relevance, d.updated_at, {globs}, d.status
         FROM knowledge_chunks_fts
         JOIN knowledge_chunks c ON c.id = knowledge_chunks_fts.rowid
         JOIN knowledge_documents d ON d.id = c.doc_id
         WHERE knowledge_chunks_fts MATCH ?1 AND {visible}
         ORDER BY relevance DESC, c.id
         LIMIT ?4",
        globs = linked_globs(3),
        visible = visible(2, 3, drafts),
    )
}

/// The condition (on a document aliased `d`) that keeps the rows `status` lists, as a live
/// document is kept by the state of its pending version.
pub(crate) fn status_condition(status: StatusFilter) -> String {
    let pending =
        |extra: &str| format!("EXISTS (SELECT 1 FROM knowledge_documents p WHERE p.pending_of = d.id{extra})");

    match status {
        StatusFilter::All => "d.pending_of IS NULL".to_owned(),
        StatusFilter::Live => "d.status = 'live'".to_owned(),
        StatusFilter::Pending => format!("(d.status = 'live' AND {})", pending("")),
        state => {
            let wanted = state.state().map_or("live", |state| state.as_str());

            format!(
                "((d.pending_of IS NULL AND d.status = '{wanted}') OR (d.status = 'live' AND {}))",
                pending(&format!(" AND p.status = '{wanted}'"))
            )
        }
    }
}

/// The `WHERE` clause of `query`'s filters and its numbered parameters.
pub(crate) fn list_conditions<V: From<String>>(columns: Columns, query: &ListQuery) -> (String, Vec<V>) {
    let mut clauses: Vec<String> = vec!["1 = 1".to_owned()];
    let mut params: Vec<V> = Vec::new();

    match &query.filter {
        RepoFilter::All => {}
        RepoFilter::Global => clauses.push(GLOBAL.to_owned()),
        RepoFilter::Linked => clauses.push(LINKED.to_owned()),
        RepoFilter::Repo(key) => {
            params.push(key.clone().into());
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM knowledge_doc_links l WHERE l.doc_id = d.id AND l.repo_key = ?{})",
                params.len()
            ));
        }
    }

    clauses.push(status_condition(query.status));

    if let Some(format) = &query.format {
        params.push(format.clone().into());
        clauses.push(format!("d.format = ?{}", params.len()));
    }

    if let Some(useful) = UsefulFilter::condition(query.useful, ("d.shown", "d.useful"), columns.usage) {
        clauses.push(useful);
    }

    let text = query.text.trim();

    if !text.is_empty() {
        params.push(format!("%{}%", escape_like(text)).into());
        let mut matching = format!("d.title LIKE ?{} ESCAPE '\\'", params.len());

        if let Some(fts) = prefix_query(text) {
            params.push(fts.into());
            matching.push_str(&format!(
                " OR d.id IN (SELECT c.doc_id FROM knowledge_chunks_fts
                              JOIN knowledge_chunks c ON c.id = knowledge_chunks_fts.rowid
                              WHERE knowledge_chunks_fts MATCH ?{})",
                params.len()
            ));
        }

        clauses.push(format!("({matching})"));
    }

    (clauses.join(" AND "), params)
}

/// `filter` with the condition of `query`'s author filter added, and its name (if any) pushed to
/// `params`. Kept apart from [`list_conditions`]: the facet of the authors honours every filter but
/// this one, and a statement that does not use a parameter is refused.
pub(crate) fn with_author<V: From<String>>(
    columns: Columns,
    query: &ListQuery,
    filter: &str,
    params: &mut Vec<V>,
) -> String {
    let condition = AuthorFilter::condition(query.author.as_ref(), AUTHOR_COLUMN, columns.authors, |name| {
        params.push(name.to_owned().into());
        format!("?{}", params.len())
    });

    match condition {
        Some(condition) => format!("{filter} AND {condition}"),
        None => filter.to_owned(),
    }
}

/// The name of the person who added a document (aliased `d`).
pub(crate) const AUTHOR_COLUMN: &str = "d.added_by_name";

/// The groups of the authors facet under `filter` (see [`AuthorFilter`]): the name as the hex of its
/// bytes, `None` for no known author, and the count.
pub(crate) fn author_groups_sql(columns: Columns, filter: &str) -> String {
    groups_sql_sqlite(AUTHOR_COLUMN, columns.authors, filter)
}

/// The page of the documents `filter` keeps, in `query`'s order. The limit and the offset are the
/// last two of the `params` numbered parameters.
pub(crate) fn page_sql(columns: Columns, filter: &str, query: &ListQuery, params: usize) -> String {
    // Without the counters the sorts by usefulness are the default order, without the author columns
    // so is the sort by who added a document.
    let sort = query.sort.for_columns(columns);
    let direction = if query.descending { "DESC" } else { "ASC" };
    let order = match sort {
        Sort::Title => "d.title COLLATE NOCASE",
        Sort::Updated => "d.updated_at",
        Sort::Size => "d.bytes",
        Sort::Format => "d.format",
        Sort::Chunks => "d.chunk_count",
        // What cannot be read as globs reads as none (see `link`), and must not fail the page.
        Sort::Paths => {
            "coalesce((SELECT sum(CASE WHEN json_valid(l.globs) THEN json_array_length(l.globs) ELSE 0 END) FROM knowledge_doc_links l WHERE l.doc_id = d.id), 0)"
        }
        Sort::Repos => "coalesce((SELECT min(l.repo_key) FROM knowledge_doc_links l WHERE l.doc_id = d.id), '')",
        Sort::Useful => "coalesce(d.useful, 0)",
        Sort::LastUseful => "coalesce(d.last_useful_at, 0)",
        // No known author comes last in both directions. `lower` folds ASCII letters only.
        Sort::AddedBy => "(nullif(d.added_by_name, '') IS NULL) ASC, lower(nullif(d.added_by_name, ''))",
    };

    format!(
        "SELECT {} FROM knowledge_documents d WHERE {filter}
         ORDER BY {order} {direction}, d.id {direction} LIMIT ?{} OFFSET ?{}",
        document_columns(columns),
        params - 1,
        params
    )
}

/// The rows of a page the store may return: `limit` capped at [`LIST_PAGE_MAX`].
pub(crate) fn page_limit(query: &ListQuery) -> i64 {
    i64::try_from(query.limit.min(LIST_PAGE_MAX)).unwrap_or(0)
}

/// `text` with the `LIKE` wildcards written out, for `ESCAPE '\'`.
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// The words of `text` as an FTS5 phrase query: the words next to each other in that order, the
/// last as a prefix, `"api gat"*`. `None` when there is no word.
pub(crate) fn prefix_query(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(QUERY_TOKENS_MAX)
        .map(str::to_lowercase)
        .collect();

    (!words.is_empty()).then(|| format!("\"{}\"*", words.join(" ")))
}

/// The FTS5 query of `text` over the heading path and the text of the chunks only (the title is in
/// every chunk of the document): the phrase of [`prefix_query`] in those columns.
pub(crate) fn matches_query(text: &str) -> Option<String> {
    prefix_query(text).map(|phrase| format!("{{heading_path text}} : {phrase}"))
}

/// The chunks of document `?2` that match the FTS5 query `?1`, by position, at most `?3`:
/// id, ordinal, heading path, text.
pub(crate) const MATCHES_SQL: &str = "SELECT c.id, c.ordinal, c.heading_path, c.text
     FROM knowledge_chunks_fts JOIN knowledge_chunks c ON c.id = knowledge_chunks_fts.rowid
     WHERE knowledge_chunks_fts MATCH ?1 AND c.doc_id = ?2 ORDER BY c.ordinal LIMIT ?3";

/// How many chunks of document `?2` match the FTS5 query `?1`.
pub(crate) const MATCHES_COUNT_SQL: &str = "SELECT count(*)
     FROM knowledge_chunks_fts JOIN knowledge_chunks c ON c.id = knowledge_chunks_fts.rowid
     WHERE knowledge_chunks_fts MATCH ?1 AND c.doc_id = ?2";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_words_are_quoted_and_or_ed() {
        assert_eq!(fts_query("Git CLI").as_deref(), Some("\"git\" OR \"cli\""));
        assert_eq!(fts_query("git git").as_deref(), Some("\"git\""));
        assert_eq!(fts_query("  ?! "), None);
        assert_eq!(fts_query(""), None);
    }

    #[test]
    fn query_syntax_is_dropped_into_plain_words() {
        let query = fts_query("title:\"x\" NEAR(a b) OR NOT * -secret ^start");

        assert_eq!(
            query.as_deref(),
            Some("\"title\" OR \"x\" OR \"near\" OR \"a\" OR \"b\" OR \"or\" OR \"not\" OR \"secret\" OR \"start\"")
        );
    }

    #[test]
    fn a_long_query_keeps_its_first_words() {
        let text: Vec<String> = (0..100).map(|n| format!("w{n}")).collect();
        let query = fts_query(&text.join(" ")).expect("query");

        assert_eq!(query.matches(" OR ").count(), QUERY_TOKENS_MAX - 1);
    }
}
