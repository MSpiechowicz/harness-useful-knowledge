//! The documents a recall points to count as shown, after its output is out.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use super::global_tests::hostile_seed;
use super::tests::{PROMPT, answering, guide, repo_world, world};
use super::*;
use crate::location::FILE_NAME;
use crate::store::testing::{Seed, axis};
use crate::store::{Detail, SqliteStore};

/// A budget no busy test machine runs out of, for the tests that check where the counts land (the real
/// [`COUNT_BUDGET`] is checked by the test of a locked database).
const LANDS: Duration = Duration::from_secs(10);

/// `(id, shown)` of every document in the database under `dir`, in id order.
fn shown(dir: &std::path::Path) -> Vec<(i64, i64)> {
    let path = dir.join("knowledge").join(FILE_NAME);
    let mut store = SqliteStore::open_read_only(&path).expect("opens").expect("exists");
    let mut counts = Vec::new();

    for id in 1.. {
        let Some(detail) = store.get(id, &Detail::default()).expect("reads") else {
            break;
        };

        counts.push((id, detail.usage.expect("counters").shown));
    }

    counts
}

fn six() -> Vec<Seed> {
    (0..6)
        .map(|number| guide(&format!("Gateway guide {number}"), 5.0 + number as f32))
        .collect()
}

#[test]
fn the_documents_a_recall_points_to_count_as_shown_and_the_others_do_not() {
    let (dir, setup) = world(&six(), answering(axis(0)));

    let recalled = recalled_pointers(PROMPT, setup).expect("a block");
    assert_eq!(
        shown(dir.path()).iter().map(|(_, count)| count).sum::<i64>(),
        0,
        "not before the output"
    );

    let mut out: Vec<u8> = Vec::new();
    print_and_count(&mut out, recalled, LANDS);

    let pointed = String::from_utf8(out).expect("text").matches("<knowledge-doc ").count();
    let counts = shown(dir.path());

    assert_eq!(pointed, POINTERS_MAX);
    assert_eq!(
        counts.iter().filter(|(_, count)| *count == 1).count(),
        pointed,
        "{counts:?}"
    );
    assert_eq!(counts.iter().filter(|(_, count)| *count == 0).count(), 6 - pointed);
}

/// A writer that looks at the counters whenever it is flushed.
struct Looking {
    dir: std::path::PathBuf,
    seen: Rc<RefCell<Vec<i64>>>,
}

impl std::io::Write for Looking {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let total = shown(&self.dir).iter().map(|(_, count)| count).sum();
        self.seen.borrow_mut().push(total);

        Ok(())
    }
}

#[test]
fn counting_starts_only_after_the_output_is_flushed() {
    let (dir, setup) = world(&six(), answering(axis(0)));
    let recalled = recalled_pointers(PROMPT, setup).expect("a block");
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut out = Looking {
        dir: dir.path().to_owned(),
        seen: Rc::clone(&seen),
    };

    print_and_count(&mut out, recalled, LANDS);

    assert_eq!(*seen.borrow(), vec![0], "nothing counted when the output was flushed");
    assert_eq!(
        shown(dir.path()).iter().map(|(_, count)| count).sum::<i64>(),
        POINTERS_MAX as i64
    );
}

#[test]
fn a_locked_database_drops_the_count_and_the_output_still_comes() {
    let (dir, setup) = world(&six(), answering(axis(0)));
    let recalled = recalled_pointers(PROMPT, setup).expect("a block");
    let lock = rusqlite::Connection::open(dir.path().join("knowledge").join(FILE_NAME)).expect("second connection");
    lock.execute_batch("BEGIN IMMEDIATE").expect("takes the write lock");

    let mut out: Vec<u8> = Vec::new();
    let started = Instant::now();
    print_and_count(&mut out, recalled, COUNT_BUDGET);

    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert!(
        String::from_utf8(out)
            .expect("text")
            .contains("<harness-knowledge-recall>")
    );

    lock.execute_batch("ROLLBACK").expect("releases it");
    drop(lock);
    assert_eq!(shown(dir.path()).iter().map(|(_, count)| count).sum::<i64>(), 0);
}

#[test]
fn an_unreachable_remote_does_not_stop_the_documents_of_this_computer_counting() {
    let (dir, mut setup) = world(&[guide("Gateway guide", 5.0)], answering(axis(0)));
    setup.backend =
        crate::config::backend_for(Some("postgres://nobody@127.0.0.1:1/none".to_owned()), None).expect("backend");
    setup.routing = Routing::new(crate::config::Where::RemoteLocal, crate::config::Where::RemoteLocal);

    let recalled = recalled_pointers(PROMPT, setup).expect("a block");
    print_and_count(&mut Vec::new(), recalled, LANDS);

    assert_eq!(shown(dir.path()), vec![(1, 1)]);
}

#[test]
fn the_global_document_in_the_last_place_counts_and_the_one_it_displaced_does_not() {
    // Three linked documents are closer than the global one, which takes the third place.
    let (dir, setup) = repo_world(
        |repo| {
            vec![
                guide("Linked one", 2.0).link(repo, &[]),
                guide("Linked two", 3.0).link(repo, &[]),
                guide("Linked three", 4.0).link(repo, &[]),
                guide("Global guide", 15.0),
            ]
        },
        answering(axis(0)),
    );

    let recalled = recalled_pointers(PROMPT, setup).expect("a block");
    let mut out: Vec<u8> = Vec::new();
    print_and_count(&mut out, recalled, LANDS);

    let block = String::from_utf8(out).expect("text");
    assert!(
        block.contains("Global guide") && !block.contains("Linked three"),
        "{block}"
    );
    assert_eq!(shown(dir.path()), vec![(1, 1), (2, 1), (3, 0), (4, 1)]);
}

#[test]
fn a_global_document_that_does_not_fit_beside_the_best_pointer_is_not_counted() {
    let (dir, setup) = repo_world(
        |repo| {
            vec![
                hostile_seed(1, 2.0).link(repo, &[]),
                hostile_seed(2, 3.0).link(repo, &[]),
                hostile_seed(3, 15.0),
            ]
        },
        answering(axis(0)),
    );

    let recalled = recalled_pointers(PROMPT, setup).expect("a block");
    print_and_count(&mut Vec::new(), recalled, LANDS);

    assert_eq!(shown(dir.path()), vec![(1, 1), (2, 0), (3, 0)]);
}
