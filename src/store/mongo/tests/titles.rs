//! The list sorted by title in MongoDB.

use super::store;
use crate::store::testing::title_order_contract;

#[test]
fn team_mongo_titles_sort_case_insensitively_for_ascii_and_by_bytes_for_the_rest() {
    let Some((_db, mut store)) = store() else { return };

    title_order_contract(&mut store);
}
