use std::time::Duration;

use super::store::path_of;
use super::*;
use crate::store::Link;

fn proposal(title: &str, content: &str) -> HeldItem {
    HeldItem::new(
        Source {
            client: "tests".to_owned(),
            session: Some("session-1".to_owned()),
            project_path: Some("/work/repo".to_owned()),
            repo_key: Some("git:h/o/r".to_owned()),
        },
        Proposal {
            title: title.to_owned(),
            format: "markdown".to_owned(),
            name: "notes.md".to_owned(),
            source_path: None,
            content: content.to_owned(),
            links: vec![Link {
                repo_key: "git:h/o/r".to_owned(),
                globs: Vec::new(),
            }],
            replaces: None,
        },
    )
}

fn store() -> (tempfile::TempDir, HeldStore) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = HeldStore::in_folder(dir.path().to_path_buf());

    (dir, store)
}

#[test]
fn a_held_proposal_is_one_private_file_that_lists_and_reads_back() {
    let (_dir, store) = store();
    let id = store
        .hold(proposal("Install notes", "# Install\n\nRun it."))
        .expect("held");

    assert!(is_held_id(&id), "{id}");

    let path = path_of(&store, &id);
    assert!(path.is_file());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = |path: &std::path::Path| std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(store.folder()), 0o700);
    }

    let listed = store.list().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "Install notes");
    assert_eq!(listed[0].client, "tests");
    assert_eq!(listed[0].session.as_deref(), Some("session-1"));
    assert_eq!(listed[0].repo_key.as_deref(), Some("git:h/o/r"));
    assert_eq!(listed[0].content, "# Install\n\nRun it.");
    assert_eq!(store.get(&id).expect("get"), Some(listed[0].clone()));
}

#[test]
fn a_read_never_creates_the_folder() {
    let (dir, store) = store();

    assert_eq!(store.list(), Ok(Vec::new()));
    assert_eq!(store.get(&random_id()), Ok(None));
    assert!(store.claim(&random_id()).expect("claim").is_none());
    assert!(!dir.path().join(HELD_FOLDER).exists());
}

#[test]
fn proposing_the_same_text_again_returns_the_waiting_proposal() {
    let (_dir, store) = store();

    let first = store.hold(proposal("Guide", "text one")).expect("held");
    let again = store.hold(proposal("Guide", "text one")).expect("held");
    let other = store.hold(proposal("Guide", "text two")).expect("held");

    assert_eq!(first, again);
    assert_ne!(first, other);
    assert_eq!(store.list().expect("list").len(), 2);
}

#[test]
fn a_file_that_is_not_a_proposal_is_not_listed() {
    let (_dir, store) = store();
    let id = store.hold(proposal("Guide", "text")).expect("held");
    let folder = store.folder().to_path_buf();

    // Not named by an id, with a name that is not its id, and with a revision that does not match.
    std::fs::write(folder.join("notes.json"), "{}").expect("write");
    std::fs::copy(path_of(&store, &id), folder.join(format!("{}.json", random_id()))).expect("copy");

    let edited = std::fs::read_to_string(path_of(&store, &id))
        .expect("read")
        .replace("text", "other text");
    let forged = random_id();
    std::fs::write(folder.join(format!("{forged}.json")), edited).expect("write");

    let ids: Vec<String> = store
        .list()
        .expect("list")
        .into_iter()
        .map(|item| item.held_id)
        .collect();
    assert_eq!(ids, [id]);
    assert_eq!(store.get(&forged), Ok(None));
}

#[test]
fn a_proposal_is_claimed_once_and_a_restore_puts_it_back() {
    let (_dir, store) = store();
    let id = store.hold(proposal("Guide", "text")).expect("held");

    let claim = store.claim(&id).expect("claim").expect("waiting");
    assert_eq!(claim.item.held_id, id);
    assert!(
        store.claim(&id).expect("claim again").is_none(),
        "a claimed proposal cannot be claimed"
    );
    assert_eq!(store.list(), Ok(Vec::new()), "nor is it listed");

    claim.restore().expect("restore");
    assert_eq!(store.list().expect("list").len(), 1);

    store
        .claim(&id)
        .expect("claim")
        .expect("waiting")
        .finish()
        .expect("finish");
    assert_eq!(store.list(), Ok(Vec::new()));
}

#[test]
fn of_many_claims_at_once_exactly_one_wins() {
    let (_dir, store) = store();
    let id = store.hold(proposal("Guide", "text")).expect("held");

    let winners: usize = std::thread::scope(|scope| {
        let claims: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| store.claim(&id).expect("claim").is_some()))
            .collect();

        claims
            .into_iter()
            .map(|claim| usize::from(claim.join().expect("thread")))
            .sum()
    });

    assert_eq!(winners, 1);
}

#[test]
fn stale_proposals_and_temporary_files_are_swept() {
    let (_dir, store) = store();
    let id = store.hold(proposal("Guide", "text")).expect("held");
    let temporary = store.folder().join(".held-x.tmp");
    std::fs::write(&temporary, "x").expect("write");

    let old = std::time::SystemTime::now() - Duration::from_secs(31 * 24 * 60 * 60);

    for path in [path_of(&store, &id), temporary.clone()] {
        let file = std::fs::File::options().write(true).open(&path).expect("open");
        file.set_modified(old).expect("mtime");
    }

    store.sweep();

    assert_eq!(store.list(), Ok(Vec::new()));
    assert!(!temporary.exists());
}

#[test]
fn the_queue_holds_at_most_a_hundred_proposals_and_never_drops_one() {
    let (_dir, store) = store();

    for number in 0..HELD_MAX_ITEMS {
        store
            .hold(proposal(&format!("Guide {number}"), &format!("text {number}")))
            .expect("held");
    }

    assert_eq!(store.hold(proposal("One more", "text more")), Err(HoldError::Limit));
    assert_eq!(store.list().expect("list").len(), HELD_MAX_ITEMS);

    // A retry of a waiting one is not one more.
    assert!(store.hold(proposal("Guide 3", "text 3")).is_ok());
}

#[test]
fn a_proposal_over_the_file_limit_is_refused() {
    let (_dir, store) = store();
    let big = "x".repeat(HELD_FILE_MAX);

    assert_eq!(store.hold(proposal("Big", &big)), Err(HoldError::Limit));
    assert_eq!(store.list(), Ok(Vec::new()));
}

#[test]
fn a_claim_left_behind_by_a_dead_process_is_put_back_after_ten_minutes() {
    let (_dir, store) = store();
    let id = store.hold(proposal("Guide", "text")).expect("held");

    // The claimer dies: the claim is neither finished nor restored.
    let claim = store.claim(&id).expect("claim").expect("waiting");
    drop(claim);
    assert_eq!(store.list(), Ok(Vec::new()), "a fresh claim hides the proposal");

    let aside: Vec<_> = std::fs::read_dir(store.folder())
        .expect("folder")
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".held-"))
        .collect();
    assert_eq!(aside.len(), 1);

    let old = std::time::SystemTime::now() - Duration::from_secs(3 * 60 * 60);
    std::fs::File::options()
        .write(true)
        .open(aside[0].path())
        .expect("open")
        .set_modified(old)
        .expect("mtime");

    let items = store.list().expect("list");

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].held_id, id);
    assert!(path_of(&store, &id).is_file());
    assert!(store.claim(&id).expect("claim").is_some(), "it can be decided again");
}
