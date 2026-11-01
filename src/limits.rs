//! Per-call limits shared by the commands.

/// Rows of one page of a list (`docs list`, `held list`) when none is asked for.
pub const LIST_PAGE_DEFAULT: usize = 50;

/// Most rows of one page of a list.
pub const LIST_PAGE_MAX: usize = 200;

/// Most characters of a document title.
pub const TITLE_MAX: usize = 300;

/// Most repositories one document links to.
pub const LINKS_MAX: usize = 32;

/// Most path globs of one repository link.
pub const GLOBS_MAX: usize = 32;

/// Most characters of one path glob.
pub const GLOB_MAX: usize = 256;

/// Most characters of a repository key (`git:host/owner/repo` or `path:<root>`).
pub const REPO_KEY_MAX: usize = 1024;

/// Most chunks of one document.
pub const CHUNKS_PER_DOC_MAX: usize = 4000;

/// Most document ids one `docs delete` takes.
pub const DELETE_IDS_MAX: usize = 1000;

/// Most words of a search query that reach the keyword index.
pub const QUERY_TOKENS_MAX: usize = 32;

/// Most candidates one keyword or vector search returns (the search asks for 50).
pub const CANDIDATES_MAX: usize = 200;

/// Most bytes of the JSON a command reads from stdin (`docs links set`).
pub const STDIN_MAX: u64 = 256 * 1024;
