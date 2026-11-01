//! The list sorted by title in PostgreSQL.

use super::support::database;
use crate::store::testing::title_order_contract;

#[test]
fn team_titles_sort_case_insensitively_for_ascii_and_by_bytes_for_the_rest() {
    let Some(db) = database() else { return };

    title_order_contract(&mut db.open());
}
