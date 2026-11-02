//! The documents a recall points to count as shown, after its output is out.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use super::tests::{PROMPT, answering, guide, world};
use super::*;
use crate::location::FILE_NAME;
use crate::store::testing::{Seed, axis};
use crate::store::{Detail, SqliteStore};

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
    print_and_count(&mut out, recalled, COUNT_BUDGET);

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

    print_and_count(&mut out, recalled, COUNT_BUDGET);

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
    print_and_count(&mut Vec::new(), recalled, COUNT_BUDGET);

    assert_eq!(shown(dir.path()), vec![(1, 1)]);
}
