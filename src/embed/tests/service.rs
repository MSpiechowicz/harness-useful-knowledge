//! The service: the model's download and load, and the answer to each request.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::support::{Fixture, World, fake_manifest, fake_vector};
use crate::embed::Event;
use crate::embed::engine::Kind;
use crate::embed::protocol::{PASSAGES_MAX, Request};

fn vector_of(reply: &Value) -> Vec<f32> {
    serde_json::from_value(reply["vector"].clone()).expect("a vector")
}

fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let started = Instant::now();

    while !condition() {
        assert!(started.elapsed() < Duration::from_secs(10), "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn preparing_downloads_checks_loads_and_reports_ready() {
    let fixture = Fixture::serving(&fake_manifest("fake@1"));
    let base = fixture.url();
    let world = World::empty(1, |options| options.base_url = Some(base));

    assert!(!world.service.is_ready());
    world.service.prepare().expect("prepare");
    assert!(world.service.is_ready());

    let names = world.events.names();
    assert_eq!(names.first().map(String::as_str), Some("download"));
    assert_eq!(names.iter().filter(|name| *name == "verifying").count(), 2);
    assert_eq!(names[names.len() - 2..], ["loading", "ready"]);

    let ready = world.events.all().into_iter().last().expect("an event");
    assert_eq!(
        ready,
        Event::Ready {
            model: "fake@1".to_owned(),
            dim: 8
        }
    );

    // Again with everything in place: nothing is fetched.
    let before = fixture.requested().len();
    world.service.prepare().expect("prepare again");
    assert_eq!(fixture.requested().len(), before);
}

#[test]
fn a_failed_download_is_reported_and_tried_again() {
    let fixture = Fixture::serving(&fake_manifest("fake@1"));
    fixture.fail.store(true, Ordering::SeqCst);
    let base = fixture.url();

    let world = World::empty(1, |options| {
        options.base_url = Some(base);
        options.retry = Duration::from_millis(50);
    });

    let running = {
        let service = world.service.clone();
        std::thread::spawn(move || service.lifecycle())
    };

    wait_until("no error was reported", || !world.events.errors().is_empty());
    assert!(!world.service.is_ready());
    assert!(world.events.errors()[0].contains("500"), "{:?}", world.events.errors());

    fixture.fail.store(false, Ordering::SeqCst);
    wait_until("the model never became ready", || world.service.is_ready());

    world.service.shutdown();
    running.join().expect("lifecycle");
}

#[test]
fn texts_are_refused_until_the_model_is_ready() {
    let world = World::with(1, |_| {});

    for request in [
        Request::EmbedQuery {
            text: "hello".to_owned(),
        },
        Request::EmbedPassages {
            texts: vec!["hello".to_owned()],
        },
    ] {
        let reply = world.service.handle(request);

        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "the model is not ready");
    }

    let pong = world.service.handle(Request::Ping);
    assert_eq!(
        (pong["ok"].clone(), pong["ready"].clone(), pong["model"].clone()),
        (true.into(), false.into(), Value::Null)
    );
}

#[test]
fn a_query_and_passages_are_embedded_with_their_prefix() {
    let world = World::ready(2);

    let reply = world.service.handle(Request::EmbedQuery {
        text: "rotate the API keys".to_owned(),
    });
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["model"], "fake@1");
    assert_eq!(vector_of(&reply), fake_vector(Kind::Query, "rotate the API keys"));

    let texts = vec!["first passage".to_owned(), "second passage".to_owned()];
    let reply = world.service.handle(Request::EmbedPassages { texts: texts.clone() });
    let vectors: Vec<Vec<f32>> = serde_json::from_value(reply["vectors"].clone()).expect("vectors");

    assert_eq!(
        vectors,
        vec![
            fake_vector(Kind::Passage, &texts[0]),
            fake_vector(Kind::Passage, &texts[1])
        ]
    );

    let taken = world.fake.control.taken();
    assert!(taken.contains(&(Kind::Query, "rotate the API keys".to_owned())));
    assert!(taken.contains(&(Kind::Passage, "first passage".to_owned())));
}

#[test]
fn a_text_is_cut_before_it_reaches_the_engine() {
    let world = World::ready(1);
    let long = "w".repeat(crate::embed::engine::TEXT_MAX + 500);

    let reply = world.service.handle(Request::EmbedPassages { texts: vec![long] });
    assert_eq!(reply["ok"], true);

    let taken = world.fake.control.taken();
    assert_eq!(taken[0].1.chars().count(), crate::embed::engine::TEXT_MAX);
}

#[test]
fn empty_texts_and_oversized_batches_are_refused() {
    let world = World::ready(1);
    let error = |request: Request| {
        world.service.handle(request)["error"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };

    assert_eq!(
        error(Request::EmbedQuery {
            text: "  \n".to_owned()
        }),
        "the text is empty"
    );
    assert_eq!(
        error(Request::EmbedPassages { texts: Vec::new() }),
        "send between 1 and 64 texts"
    );
    assert_eq!(
        error(Request::EmbedPassages {
            texts: vec!["a".to_owned(); PASSAGES_MAX + 1]
        }),
        "send between 1 and 64 texts"
    );
    assert_eq!(
        error(Request::EmbedPassages {
            texts: vec!["a".to_owned(), String::new()]
        }),
        "text 1 is empty"
    );

    let full = world.service.handle(Request::EmbedPassages {
        texts: vec!["a".to_owned(); PASSAGES_MAX],
    });
    assert_eq!(full["ok"], true, "exactly 64 are fine");
    assert_eq!(full["vectors"].as_array().map(Vec::len), Some(PASSAGES_MAX));
}

#[test]
fn status_reports_the_workers_and_whether_the_model_is_in_memory() {
    let world = World::ready(3);

    let status = world.service.handle(Request::Status);
    assert_eq!(status["ok"], true);
    assert_eq!(status["ready"], true);
    assert_eq!(status["loaded"], true);
    assert_eq!(status["model"], "fake@1");
    assert_eq!(status["dim"], 8);
    assert_eq!(status["workers"], 3);
    assert_eq!(status["queued_queries"], 0);
    assert_eq!(status["queued_passages"], 0);

    assert!(world.service.pool().holder().unload_if_idle(Duration::ZERO));
    let status = world.service.handle(Request::Status);
    assert_eq!(
        (status["ready"].clone(), status["loaded"].clone()),
        (true.into(), false.into())
    );
}

#[test]
fn warming_loads_a_model_the_idle_unload_dropped() {
    let world = World::ready(1);
    let loads = || world.fake.loads.load(Ordering::SeqCst);

    assert_eq!(loads(), 1);
    assert!(world.service.pool().holder().unload_if_idle(Duration::ZERO));

    assert_eq!(world.service.handle(Request::Warm)["ok"], true);
    assert_eq!(loads(), 1, "the reply comes before the load");

    world.service.warm();
    assert_eq!(loads(), 2);

    world.service.warm();
    assert_eq!(loads(), 2, "nothing is loaded twice");
}

#[test]
fn an_idle_model_is_dropped_and_the_next_text_loads_it_again() {
    let world = World::with(1, |options| options.idle = Duration::from_millis(100));
    let running = {
        let service = world.service.clone();
        std::thread::spawn(move || service.lifecycle())
    };

    wait_until("the model never became ready", || world.service.is_ready());
    wait_until("the idle model was never dropped", || {
        world.events.names().contains(&"idle".to_owned())
    });
    assert!(!world.service.pool().holder().is_loaded());

    let reply = world.service.handle(Request::EmbedQuery {
        text: "back again".to_owned(),
    });
    assert_eq!(reply["ok"], true);
    assert_eq!(world.fake.loads.load(Ordering::SeqCst), 2);
    assert_eq!(world.events.names()[..4], ["loading", "ready", "idle", "loading"]);

    world.service.shutdown();
    running.join().expect("lifecycle");
}

#[test]
fn the_enabled_flag_is_off_until_set_and_kept_private() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let root = dir.path().join("harness-knowledge");

    assert!(!crate::embed::enabled(&root), "no folder, no file: off");

    crate::embed::set_enabled(&root, true).expect("enable");
    assert!(crate::embed::enabled(&root));

    crate::embed::set_enabled(&root, false).expect("disable");
    assert!(!crate::embed::enabled(&root));

    std::fs::write(root.join(crate::embed::CONFIG_FILE), "enabled = \"yes\"\n").expect("a wrong value");
    assert!(!crate::embed::enabled(&root), "anything but a boolean is off");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        crate::embed::set_enabled(&root, true).expect("enable");
        let mode = std::fs::metadata(root.join(crate::embed::CONFIG_FILE))
            .expect("file")
            .permissions()
            .mode();

        assert_eq!(mode & 0o777, 0o600);
    }
}
