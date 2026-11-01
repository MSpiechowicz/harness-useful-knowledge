//! The list sorted by title in the local file.

use crate::store::testing::{store, title_order_contract};

#[test]
fn titles_sort_case_insensitively_for_ascii_and_by_bytes_for_the_rest() {
    title_order_contract(&mut store());
}
