use super::*;
use crate::location::FILE_NAME;

fn env_in(dir: &std::path::Path) -> Env {
    Env::for_tests("/home/u").with_var(DIR_VAR, dir.to_string_lossy())
}

#[test]
fn reading_and_changing_never_create_the_local_database_and_creating_does() {
    let dir = tempfile::tempdir().expect("temp dir");
    let folder = dir.path().join("knowledge");
    let env = env_in(&folder);

    assert!(
        open(&env, &Backend::Sqlite, Routing::local(), Access::Read)
            .expect("read")
            .is_none()
    );
    assert!(
        open(&env, &Backend::Sqlite, Routing::local(), Access::Existing)
            .expect("existing")
            .is_none()
    );
    assert!(!folder.exists());

    assert!(
        open(&env, &Backend::Sqlite, Routing::local(), Access::Create)
            .expect("create")
            .is_some()
    );
    assert!(folder.join(FILE_NAME).is_file());

    assert!(
        open(&env, &Backend::Sqlite, Routing::local(), Access::Read)
            .expect("read")
            .is_some()
    );
}

#[test]
fn a_relative_folder_override_is_refused() {
    let env = Env::for_tests("/home/u").with_var(DIR_VAR, "relative");

    assert!(matches!(
        open(&env, &Backend::Sqlite, Routing::local(), Access::Read),
        Err(OpenError::Failed(_))
    ));
}

/// A team database nobody listens at: a connection to it is refused at once.
fn refusing() -> Backend {
    crate::config::backend_for(Some("postgres://nobody@127.0.0.1:1/none".to_owned()), None).expect("backend")
}

/// Fills `slots` with threads that never end until the returned sender is dropped.
fn fill(slots: &'static deadline::Slots, count: usize) -> std::sync::mpsc::Sender<()> {
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = Arc::new(std::sync::Mutex::new(released));

    for _ in 0..count {
        let released = Arc::clone(&released);
        let stuck = deadline::within_slots(slots, "stuck", Duration::from_millis(20), move || {
            let _ = released.lock().expect("lock").recv();
        });

        assert_eq!(stuck, Err(deadline::Missed::Late));
    }

    release
}

#[test]
fn a_remote_that_refuses_is_recorded_for_every_process() {
    static OPENS: deadline::Slots = deadline::Slots::new(2);

    let dir = tempfile::tempdir().expect("temp dir");
    let env = env_in(&dir.path().join("knowledge"));
    let backend = refusing();

    let attempt = attempt_in(&OPENS, &env, &backend, Access::Read, true).expect("attempt");
    assert!(matches!(attempt, Attempt::Offline(_)));
    assert!(
        reach_of(&env, &backend).cooling().is_some(),
        "a refused connection is a fact"
    );
}

#[test]
fn a_full_set_of_silent_opens_is_this_process_alone_and_records_nothing_for_the_others() {
    static OPENS: deadline::Slots = deadline::Slots::new(2);

    let dir = tempfile::tempdir().expect("temp dir");
    let folder = dir.path().join("knowledge");
    let env = env_in(&folder);
    let backend = refusing();
    let release = fill(&OPENS, 2);

    for cooldown in [true, false] {
        let attempt = attempt_in(&OPENS, &env, &backend, Access::Read, cooldown).expect("attempt");

        assert!(
            matches!(attempt, Attempt::Offline(_)),
            "the remote counts as unreachable for this call"
        );
    }

    assert!(
        !folder.join("remote-unreachable.json").exists(),
        "nothing is known about the remote, so nothing is shared"
    );
    assert_eq!(reach_of(&env, &backend).cooling(), None);

    drop(release);
}

/// What an opener that needs an upgrade does: defers the first time, and upgrades for `upgrade`.
fn needing_upgrade(
    upgrade: Duration,
    runs: &Arc<std::sync::atomic::AtomicUsize>,
) -> impl Fn(Upgrade) -> Result<Opened<Box<dyn KnowledgeStore>>, OpenError> + Send + Sync + 'static {
    let runs = Arc::clone(runs);

    move |mode| match mode {
        Upgrade::Defer => Ok(Opened::NeedsUpgrade),
        Upgrade::Run => {
            std::thread::sleep(upgrade);
            runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

            Ok(Opened::Ready(None))
        }
    }
}

#[test]
fn an_upgrade_runs_in_its_own_open_that_the_probe_deadline_does_not_bound() {
    static OPENS: deadline::Slots = deadline::Slots::new(2);
    static UPGRADES: deadline::Slots = deadline::Slots::new(1);

    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe = Duration::from_millis(40);

    // The upgrade takes five probe deadlines, and still completes.
    let opened = open_in_phases(
        &OPENS,
        &UPGRADES,
        probe,
        (Duration::from_secs(10), String::new()),
        needing_upgrade(probe * 10, &runs),
    );

    assert!(matches!(opened, Ok(None)), "the upgrade completed");
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn a_database_that_is_still_being_upgraded_is_not_unreachable() {
    static OPENS: deadline::Slots = deadline::Slots::new(2);
    static UPGRADES: deadline::Slots = deadline::Slots::new(1);

    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe = Duration::from_millis(40);

    let Err(OpenError::Failed(message)) = open_in_phases(
        &OPENS,
        &UPGRADES,
        probe,
        (Duration::from_millis(100), late_upgrade(&refusing(), UPGRADE_BOUND)),
        needing_upgrade(Duration::from_millis(600), &runs),
    ) else {
        panic!("a failure that says it is upgrading");
    };
    assert!(message.contains("upgrading the knowledge database"), "{message}");

    // The upgrade is still running in this process: another try does not start a second one.
    let Err(OpenError::Failed(again)) = open_in_phases(
        &OPENS,
        &UPGRADES,
        probe,
        (Duration::from_millis(100), late_upgrade(&refusing(), UPGRADE_BOUND)),
        needing_upgrade(Duration::from_millis(600), &runs),
    ) else {
        panic!("still upgrading");
    };
    assert!(again.contains("upgrading the knowledge database"), "{again}");
    assert_eq!(
        runs.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "none has finished yet"
    );
}

#[test]
fn an_upgrade_that_passes_its_bound_says_what_becomes_of_it_for_each_backend() {
    let postgres = late_upgrade(&refusing(), UPGRADE_BOUND);
    assert!(postgres.contains("longer than 10 minutes"), "{postgres}");
    assert!(
        postgres.contains("rolls it back when this process ends") && postgres.contains("long-running session"),
        "{postgres}"
    );
    assert!(!postgres.contains("keeps going"), "{postgres}");

    let mongodb = crate::config::backend_for(Some("mongodb://127.0.0.1:1/none".to_owned()), None).expect("backend");
    let mongodb = late_upgrade(&mongodb, UPGRADE_BOUND);
    assert!(mongodb.contains("keeps going on the next run"), "{mongodb}");
    assert!(!mongodb.contains("rolls it back"), "{mongodb}");
}

#[test]
fn a_database_that_needs_no_upgrade_opens_in_the_first_phase_and_a_silent_one_is_unreachable() {
    static OPENS: deadline::Slots = deadline::Slots::new(2);
    static UPGRADES: deadline::Slots = deadline::Slots::new(1);

    let probe = Duration::from_millis(40);

    let ready = open_in_phases(
        &OPENS,
        &UPGRADES,
        probe,
        (Duration::from_secs(1), String::new()),
        |mode| {
            assert_eq!(mode, Upgrade::Defer, "no upgrade is needed, so none runs");

            Ok(Opened::Ready(None))
        },
    );
    assert!(matches!(ready, Ok(None)));

    let silent = open_in_phases(
        &OPENS,
        &UPGRADES,
        probe,
        (Duration::from_secs(1), String::new()),
        |_| {
            std::thread::sleep(Duration::from_millis(400));

            Ok(Opened::Ready(None))
        },
    );
    assert!(matches!(silent, Err(OpenError::Unreachable(_))));
}
