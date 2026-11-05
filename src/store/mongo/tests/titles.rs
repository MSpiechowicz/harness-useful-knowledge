//! The list sorted by title in MongoDB.

use super::store;
use crate::store::testing::{path_order_contract, title_order_contract};

#[test]
fn team_mongo_titles_sort_case_insensitively_for_ascii_and_by_bytes_for_the_rest() {
    let Some((_db, mut store)) = store() else { return };

    title_order_contract(&mut store);
}

#[test]
fn team_mongo_paths_sort_by_the_globs_of_all_the_links_together() {
    let Some((_db, mut store)) = store() else { return };

    path_order_contract(&mut store);
}
