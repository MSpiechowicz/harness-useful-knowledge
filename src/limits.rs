//! Per-call limits shared by the commands.

/// Rows of one page of a list (`docs list`, `held list`) when none is asked for.
pub const LIST_PAGE_DEFAULT: usize = 50;

/// Most rows of one page of a list.
pub const LIST_PAGE_MAX: usize = 200;
