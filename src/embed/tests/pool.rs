//! The worker threads: one shared model, queries before passages, and the idle unload.

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::support::{Fake, PoolRig, fake_vector};
use crate::embed::engine::Kind;
use crate::embed::pool::{THREADS_VAR, worker_count};

fn texts(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

const WAIT: Duration = Duration::from_secs(30);

#[test]
fn the_default_worker_count_is_a_quarter_of_the_cpus_between_one_and_four() {
    let counts: Vec<usize> = [1, 3, 4, 8, 12, 16, 64]
        .iter()
        .map(|cpus| worker_count(*cpus, None))
        .collect();

    assert_eq!(counts, vec![1, 1, 1, 2, 3, 4, 4]);
}

#[test]
fn the_override_must_be_a_number_from_one_to_eight() {
    assert_eq!(THREADS_VAR, "HARNESS_KNOWLEDGE_EMBED_THREADS");
    assert_eq!(worker_count(16, Some("1")), 1);
    assert_eq!(worker_count(2, Some(" 8 ")), 8);
    assert_eq!(worker_count(16, Some("6")), 6);

    for refused in ["0", "9", "-1", "many", "", "2.5"] {
        assert_eq!(
            worker_count(16, Some(refused)),
            4,
            "{refused:?} falls back to the default"
        );
    }
}

#[test]
fn passages_come_back_in_order_whichever_worker_ran_them() {
    let rig = PoolRig::new(4, Fake::new());
    let names: Vec<String> = (0..40).map(|number| format!("passage number {number}")).collect();

    let vectors = rig.pool.passages(&names, WAIT).expect("passages");

    assert_eq!(vectors.len(), 40);

    for (name, vector) in names.iter().zip(&vectors) {
        assert_eq!(vector, &fake_vector(Kind::Passage, name), "{name}");
    }

    let query = rig.pool.query("passage number 3").expect("query");
    assert_eq!(query, fake_vector(Kind::Query, "passage number 3"));
    assert_ne!(query, vectors[3], "a query is not embedded as a passage");
}

#[test]
fn workers_run_texts_at_once_and_share_one_loaded_model() {
    let fake = Fake::new().loading_for(Duration::from_millis(150));
    fake.control.meet(4);
    let rig = PoolRig::new(4, fake);

    let names: Vec<String> = (0..8).map(|number| format!("text {number}")).collect();
    rig.pool.passages(&names, WAIT).expect("passages");

    assert_eq!(
        rig.fake.control.peak.load(Ordering::SeqCst),
        4,
        "four texts were inside the engine at once"
    );
    assert_eq!(rig.fake.loads.load(Ordering::SeqCst), 1, "one load for four workers");
    assert_eq!(rig.events.names(), vec!["loading"]);
}

#[test]
fn a_query_goes_before_the_passages_that_are_queued() {
    let fake = Fake::new();
    let control = fake.control.clone();
    control.close();
    let rig = PoolRig::new(1, fake);

    // The only worker takes the first passage and stops inside the engine.
    let first = {
        let pool = rig.pool.clone();
        std::thread::spawn(move || pool.passages(&texts(&["held"]), WAIT))
    };
    control.wait_for(|control| control.inside() == 1);

    let backlog = {
        let pool = rig.pool.clone();
        std::thread::spawn(move || pool.passages(&texts(&["one", "two", "three", "four", "five"]), WAIT))
    };
    let mut waited = 0;
    while rig.pool.queued().1 < 5 {
        waited += 1;
        assert!(waited < 1000, "the backlog never queued");
        std::thread::sleep(Duration::from_millis(5));
    }

    let query = {
        let pool = rig.pool.clone();
        std::thread::spawn(move || pool.query("urgent"))
    };
    let mut waited = 0;
    while rig.pool.queued().0 < 1 {
        waited += 1;
        assert!(waited < 1000, "the query never queued");
        std::thread::sleep(Duration::from_millis(5));
    }

    control.open();

    assert_eq!(
        query.join().expect("query thread").expect("query"),
        fake_vector(Kind::Query, "urgent")
    );
    first.join().expect("first thread").expect("first");
    backlog.join().expect("backlog thread").expect("backlog");

    let order: Vec<String> = control.taken().into_iter().map(|(_, text)| text).collect();
    assert_eq!(order, vec!["held", "urgent", "one", "two", "three", "four", "five"]);
}

#[test]
fn a_text_the_model_refuses_fails_the_request_and_not_the_pool() {
    let rig = PoolRig::new(2, Fake::new());

    let error = rig
        .pool
        .passages(&texts(&["fine", "explode please", "fine too"]), WAIT)
        .expect_err("one text fails");
    assert!(error.contains("refuses"), "{error}");

    assert!(rig.pool.query("still working").is_ok());
}

#[test]
fn a_request_nobody_waits_for_is_not_run() {
    let fake = Fake::new();
    let control = fake.control.clone();
    control.close();
    let rig = PoolRig::new(1, fake);

    let error = rig
        .pool
        .passages(&texts(&["a", "b", "c", "d"]), Duration::from_millis(150))
        .expect_err("the engine is held");
    assert!(error.contains("too long"), "{error}");

    control.open();

    // The text already inside the engine finishes, the three queued ones are dropped.
    assert!(rig.pool.query("after").is_ok());
    assert_eq!(
        control.embeds.load(Ordering::SeqCst),
        2,
        "one abandoned text and the query"
    );
}

#[test]
fn the_model_is_dropped_when_idle_but_never_under_a_running_text() {
    let fake = Fake::new();
    let control = fake.control.clone();
    let rig = PoolRig::new(1, fake);
    let holder = rig.pool.holder().clone();

    assert!(!holder.unload_if_idle(Duration::ZERO), "nothing is loaded yet");
    rig.pool.query("load it").expect("query");
    assert!(holder.is_loaded());
    assert!(
        !holder.unload_if_idle(Duration::from_secs(3600)),
        "not idle long enough"
    );

    control.close();
    let running = {
        let pool = rig.pool.clone();
        std::thread::spawn(move || pool.query("busy"))
    };
    control.wait_for(|control| control.inside() == 1);

    assert!(!holder.unload_if_idle(Duration::ZERO), "a text is inside the model");

    control.open();
    running.join().expect("thread").expect("query");

    assert!(holder.unload_if_idle(Duration::ZERO));
    assert!(!holder.is_loaded());

    rig.pool.query("again").expect("loads again");
    assert_eq!(rig.fake.loads.load(Ordering::SeqCst), 2);
    assert_eq!(rig.events.names(), vec!["loading", "loading"]);
}

#[test]
fn a_model_that_fails_to_load_fails_the_texts_and_loads_later() {
    let fake = Fake::new();
    fake.fail_loads.store(true, Ordering::SeqCst);
    let rig = PoolRig::new(2, fake);

    let error = rig.pool.query("hello").expect_err("no model");
    assert!(error.contains("cannot load"), "{error}");

    rig.fake.fail_loads.store(false, Ordering::SeqCst);
    assert!(rig.pool.query("hello").is_ok());
}

#[test]
fn a_stopped_pool_refuses_and_a_stop_with_texts_waiting_does_not_hang() {
    let fake = Fake::new();
    let control = fake.control.clone();
    control.close();
    let rig = PoolRig::new(1, fake);

    let waiting = {
        let pool = rig.pool.clone();
        std::thread::spawn(move || pool.passages(&texts(&["x", "y"]), WAIT))
    };
    control.wait_for(|control| control.inside() == 1);

    let stopping = {
        let pool = rig.pool.clone();
        std::thread::spawn(move || pool.stop())
    };
    std::thread::sleep(Duration::from_millis(50));
    control.open();

    stopping.join().expect("stop thread");
    assert!(waiting.join().expect("waiting thread").is_err());
    assert!(rig.pool.query("late").expect_err("stopped").contains("stopping"));
}
