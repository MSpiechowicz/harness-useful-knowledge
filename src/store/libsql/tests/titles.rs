//! The list sorted by title on a libSQL server.

use super::support::server;
use crate::store::testing::title_order_contract;

#[test]
fn team_titles_sort_case_insensitively_for_ascii_and_by_bytes_for_the_rest() {
    let Some(server) = server() else { return };

    title_order_contract(&mut server.store());
}
