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

/// Most chunks one page of pending chunks, or one write of embeddings, carries.
pub const EMBED_BATCH_MAX: usize = 256;

/// Most chunk hashes one lookup of the documents that share them takes (a document has at most
/// [`CHUNKS_PER_DOC_MAX`] chunks).
pub const SHARED_SHAS_MAX: usize = CHUNKS_PER_DOC_MAX;

/// Most document ids one `docs delete` takes.
pub const DELETE_IDS_MAX: usize = 1000;

/// Most words of a search query that reach the keyword index.
pub const QUERY_TOKENS_MAX: usize = 32;

/// Most candidates one keyword or vector search returns (the search asks for 50).
pub const CANDIDATES_MAX: usize = 200;

/// Most bytes of the JSON a command reads from stdin (`docs links set`).
pub const STDIN_MAX: u64 = 256 * 1024;

/// Most bytes of a file an import reads (a PDF may be [`PDF_BYTES_MAX`]).
pub const FILE_BYTES_MAX: u64 = 32 * 1024 * 1024;

/// Most bytes of a PDF an import reads.
pub const PDF_BYTES_MAX: u64 = 64 * 1024 * 1024;

/// Most bytes of the text extracted from one document.
pub const TEXT_BYTES_MAX: usize = 8 * 1024 * 1024;

/// Most files one import takes (the files of its folders count).
pub const FILES_PER_IMPORT_MAX: usize = 5000;

/// How deep an import walks into a folder (the folder itself is depth 0).
pub const WALK_DEPTH_MAX: usize = 16;

/// Most bytes of the manifest `docs import --ndjson` reads from stdin.
pub const MANIFEST_MAX: u64 = 4 * 1024 * 1024;

/// Most seconds the PDF worker may take for one file before it is killed.
pub const PDF_TIMEOUT_SECS: u64 = 60;
