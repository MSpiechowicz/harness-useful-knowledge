//! The socket, the single-instance lock and the client, over a real local socket.

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::Ordering;
use std::time::Duration;

use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::traits::ListenerExt as _;
use tempfile::TempDir;

use super::support::{World, fake_vector};
use crate::embed::client::{ClientError, EmbedderClient};
use crate::embed::engine::Kind;
#[cfg(unix)]
use crate::embed::server;
use crate::embed::server::{Endpoint, Lock, LockError, Server};

/// A world with a ready model, and a server on its socket.
pub(super) struct Running {
    pub world: World,
    server: Option<Server>,
    pub client: EmbedderClient,
}

pub(super) fn running(workers: usize) -> Running {
    let world = World::ready(workers);
    let endpoint = Endpoint::of(&world.root, &std::env::temp_dir());
    let server = Server::start(endpoint.clone(), world.service.clone()).expect("start the server");

    Running {
        world,
        server: Some(server),
        client: EmbedderClient::from_endpoint(endpoint),
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown();
        }
    }
}

impl Running {
    fn client_stream(&self) -> interprocess::local_socket::Stream {
        self.server
            .as_ref()
            .expect("server")
            .endpoint()
            .connect(Duration::from_secs(5))
            .expect("connect")
    }
}

#[test]
fn ping_status_and_texts_are_answered_over_the_socket() {
    let running = running(2);

    let pong = running.client.ping().expect("ping");
    assert_eq!(pong.model.as_deref(), Some("fake@1"));
    assert!(pong.ready);

    let status = running.client.status().expect("status");
    assert!(status.ready && status.loaded);
    assert_eq!((status.workers, status.dim), (2, 8));

    assert_eq!(
        running
            .client
            .embed_query("answered over a local socket")
            .expect("query"),
        fake_vector(Kind::Query, "answered over a local socket")
    );

    let texts: Vec<String> = (0..150).map(|number| format!("passage {number}")).collect();
    let vectors = running.client.embed_passages(&texts).expect("passages");

    assert_eq!(vectors.len(), 150, "three requests of at most 64 texts, in order");

    for (text, vector) in texts.iter().zip(&vectors) {
        assert_eq!(vector, &fake_vector(Kind::Passage, text));
    }

    assert_eq!(
        running.client.embed_passages(&[]).expect("nothing to embed"),
        Vec::<Vec<f32>>::new()
    );
}

#[test]
fn many_clients_at_once_are_all_answered() {
    let running = running(4);

    let threads: Vec<_> = (0..16)
        .map(|number| {
            let client = running.client.clone();

            std::thread::spawn(move || {
                let text = format!("client {number}");
                let query = client.embed_query(&text).expect("query");
                let passages = client.embed_passages(&[text.clone(), text.clone()]).expect("passages");

                (text, query, passages)
            })
        })
        .collect();

    for thread in threads {
        let (text, query, passages) = thread.join().expect("thread");

        assert_eq!(query, fake_vector(Kind::Query, &text));
        assert_eq!(passages, vec![fake_vector(Kind::Passage, &text); 2]);
    }
}

#[test]
fn a_query_is_answered_while_an_import_keeps_every_worker_busy() {
    let running = running(2);
    let control = running.world.fake.control.clone();

    control.delay(Duration::from_millis(150));

    let import = {
        let client = running.client.clone();
        let texts: Vec<String> = (0..30).map(|number| format!("chunk {number}")).collect();

        std::thread::spawn(move || client.embed_passages(&texts))
    };

    control.wait_for(|control| control.inside() == 2);

    let started = std::time::Instant::now();
    running.client.embed_query("urgent question").expect("query");

    // Behind the 28 queued passages it would wait over two seconds: it waits for one text.
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(import.join().expect("import").expect("passages").len(), 30);

    let order: Vec<String> = control.taken().into_iter().map(|(_, text)| text).collect();
    let position = order.iter().position(|text| text == "urgent question").expect("taken");
    assert!(position < 8, "the query was taken at {position} of {}", order.len());
}

#[test]
fn a_request_over_the_cap_or_in_another_version_is_refused() {
    let running = running(1);

    let exchange = |line: &[u8]| -> String {
        let mut stream = running.client_stream();
        stream.write_all(line).expect("write");
        stream.write_all(b"\n").expect("newline");
        let mut answer = String::new();
        BufReader::new(stream).read_line(&mut answer).expect("read");
        answer
    };

    let big = format!(
        "{{\"v\":1,\"op\":\"embed_query\",\"text\":\"{}\"}}",
        "x".repeat(crate::embed::protocol::MAX_REQUEST + 10)
    );
    assert!(exchange(big.as_bytes()).contains("too large"));
    assert!(exchange(b"{\"v\":2,\"op\":\"ping\"}").contains("unsupported protocol version"));
    assert!(exchange(b"not json").contains("\"ok\":false"));
    assert!(exchange(b"{\"v\":1,\"op\":\"search\"}").contains("unknown or malformed request"));
    assert!(exchange(b"{\"v\":1,\"op\":\"ping\"}").contains("\"ok\":true"));
}

#[test]
fn a_client_without_an_embedder_fails_at_once() {
    let dir = TempDir::new().expect("temp dir");
    let client = EmbedderClient::new(dir.path(), dir.path());

    let started = std::time::Instant::now();
    let error = client.embed_query("anything").expect_err("nobody listens");

    assert!(matches!(error, ClientError::Unreachable(_)), "{error}");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(client.embed_passages(&["x".to_owned()]).is_err());
    assert!(client.ping().is_err() && client.status().is_err() && client.warm().is_err());
}

#[test]
fn a_model_that_is_not_ready_answers_an_error_and_ping_says_so() {
    let world = World::with(1, |_| {});
    let endpoint = Endpoint::of(&world.root, &std::env::temp_dir());
    let server = Server::start(endpoint.clone(), world.service.clone()).expect("start");
    let client = EmbedderClient::from_endpoint(endpoint);

    let pong = client.ping().expect("ping");
    assert!(!pong.ready);
    assert_eq!(pong.model, None);
    assert!(matches!(client.embed_query("x"), Err(ClientError::Refused(_))));
    assert!(matches!(
        client.embed_passages(&["x".to_owned()]),
        Err(ClientError::Refused(_))
    ));

    server.shutdown();
}

#[test]
fn warming_over_the_socket_loads_a_dropped_model() {
    let running = running(1);
    let loads = || running.world.fake.loads.load(Ordering::SeqCst);

    assert!(running.world.service.pool().holder().unload_if_idle(Duration::ZERO));
    running.client.warm().expect("warm");

    let started = std::time::Instant::now();

    while loads() < 2 {
        assert!(started.elapsed() < Duration::from_secs(5), "the model was never loaded");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_second_embedder_in_the_same_folder_finds_the_lock_taken() {
    let dir = TempDir::new().expect("temp dir");

    assert!(!Lock::is_held(dir.path()));
    let first = Lock::take(dir.path()).expect("first lock");
    assert!(Lock::is_held(dir.path()));
    assert!(matches!(Lock::take(dir.path()), Err(LockError::Busy)));

    drop(first);
    assert!(!Lock::is_held(dir.path()));
    assert!(Lock::take(dir.path()).is_ok(), "released with the first");
}

#[test]
fn a_lock_file_left_by_a_killed_embedder_is_taken_over() {
    let dir = TempDir::new().expect("temp dir");

    // The file stays after the process is gone: what holds the lock is the process, not the file.
    drop(Lock::take(dir.path()).expect("first lock"));
    assert!(dir.path().join(crate::embed::LOCK_FILE).exists());
    assert!(!Lock::is_held(dir.path()));

    let second = Lock::take(dir.path()).expect("the stale lock file does not block");
    assert!(Lock::is_held(dir.path()));
    drop(second);
}

#[cfg(unix)]
#[test]
fn the_lock_file_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("temp dir");
    let _lock = Lock::take(dir.path()).expect("lock");
    let mode = std::fs::metadata(dir.path().join(crate::embed::LOCK_FILE))
        .expect("lock file")
        .permissions()
        .mode();

    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        std::fs::metadata(dir.path()).expect("folder").permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn a_status_probe_holding_the_lock_for_an_instant_does_not_make_an_embedder_exit_as_busy() {
    let dir = TempDir::new().expect("temp dir");
    drop(Lock::take(dir.path()).expect("creates the lock file"));

    // What `Lock::is_held` does, held for longer than a real probe.
    let file = std::fs::File::open(dir.path().join(crate::embed::LOCK_FILE)).expect("lock file");
    file.lock_shared().expect("shared lock");

    let releasing = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(file);
    });

    assert!(Lock::take(dir.path()).is_ok(), "waited for the probe to let go");
    releasing.join().expect("probe");
}

#[test]
fn a_silent_connection_does_not_stop_others_from_being_served() {
    let running = running(1);
    let _silent = running.client_stream();

    assert!(running.client.ping().is_ok());
}

/// The calls to an embedder that accepts and never answers (a stuck one) give up at their own
/// timeouts on every platform: the streams' timeouts are not relied on (named pipes have none).
#[test]
fn a_stuck_embedder_costs_each_call_no_more_than_its_timeout() {
    const CALLS: usize = 4;

    let dir = TempDir::new().expect("temp dir");
    let endpoint = Endpoint::of(dir.path(), &std::env::temp_dir());
    endpoint.publish().expect("publish");
    let listener = ListenerOptions::new()
        .name(endpoint.name().expect("name"))
        .create_sync()
        .expect("listen");

    // Accepts the calls and keeps their connections open without a word.
    let stuck = std::thread::spawn(move || listener.incoming().take(CALLS).flatten().collect::<Vec<_>>());

    let client = EmbedderClient::from_endpoint(endpoint);
    let timed = |limit: Duration, call: &dyn Fn() -> Result<(), ClientError>| {
        let started = std::time::Instant::now();
        let result = call();

        assert!(matches!(result, Err(ClientError::Unreachable(_))), "{result:?}");
        assert!(started.elapsed() < limit, "{:?}", started.elapsed());
    };

    timed(Duration::from_secs(1), &|| client.ping().map(drop));
    timed(Duration::from_secs(1), &|| client.status().map(drop));
    timed(Duration::from_secs(1), &|| client.warm());
    // A query gives up after four seconds, a hundred and twenty seconds for passages is not waited out here.
    timed(Duration::from_secs(6), &|| client.embed_query("stuck").map(drop));

    drop(stuck.join().expect("the stuck embedder"));
}

#[cfg(unix)]
#[test]
fn a_socket_left_by_a_killed_embedder_is_replaced_and_a_file_is_not() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let world = World::ready(1);
    let root = world.root.clone();
    let endpoint = Endpoint::of(&root, &std::env::temp_dir());

    // A dead process's socket: bound, then left behind.
    drop(std::os::unix::net::UnixListener::bind(endpoint.path()).expect("bind the stale socket"));
    assert!(
        std::fs::symlink_metadata(endpoint.path())
            .expect("stale")
            .file_type()
            .is_socket()
    );

    let server = Server::start(endpoint.clone(), world.service.clone()).expect("replaces the stale socket");
    let mode = std::fs::metadata(endpoint.path()).expect("socket").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(EmbedderClient::from_endpoint(endpoint.clone()).ping().is_ok());
    server.shutdown();
    assert!(!endpoint.path().exists(), "the socket goes with the server");

    // Anything else at that path is not ours to delete.
    std::fs::write(endpoint.path(), b"precious").expect("a regular file");
    assert!(Server::start(endpoint.clone(), world.service.clone()).is_err());
    assert_eq!(std::fs::read(endpoint.path()).expect("still there"), b"precious");
}

#[cfg(unix)]
#[test]
fn a_long_data_folder_gets_its_socket_in_a_folder_of_this_user() {
    let temp = TempDir::new().expect("temp dir");
    let runtime = TempDir::new().expect("runtime dir");
    let short = std::path::Path::new("/data/harness-knowledge");
    let long = std::path::PathBuf::from(format!("/home/someone/{}/harness-knowledge", "deep/".repeat(30)));

    let near = Endpoint::placed(short, temp.path(), Some(runtime.path()));
    assert_eq!(near.path(), short.join("embedder.sock"));

    let run = Endpoint::placed(&long, temp.path(), Some(runtime.path()));
    assert_eq!(run.path().parent(), Some(runtime.path()));
    assert!(run.path().as_os_str().len() <= 100);

    let far = Endpoint::placed(&long, temp.path(), None);
    let uid = nix::unistd::getuid().as_raw();
    assert_eq!(
        far.path().parent(),
        Some(temp.path().join(format!("harness-knowledge-{uid}")).as_path())
    );
    assert_ne!(
        far.path(),
        Endpoint::placed(&long.join("other"), temp.path(), None).path(),
        "one per data folder"
    );

    // A runtime folder that does not fit the socket path is passed over.
    let deep = temp.path().join("d".repeat(90));
    std::fs::create_dir(&deep).expect("deep runtime dir");
    assert_eq!(
        Endpoint::placed(&long, temp.path(), Some(&deep)).path().parent(),
        far.path().parent()
    );
}

#[cfg(unix)]
#[test]
fn a_link_in_place_of_the_socket_folder_is_refused_and_its_target_left_alone() {
    use std::os::unix::fs::PermissionsExt;

    let world = World::ready(1);

    let temp = TempDir::new().expect("temp");
    let long = std::path::PathBuf::from(format!("/home/someone/{}/harness-knowledge", "deep/".repeat(30)));
    let endpoint = Endpoint::placed(&long, temp.path(), None);
    let folder = endpoint.path().parent().expect("folder").to_path_buf();

    let target = temp.path().join("target");
    std::fs::create_dir(&target).expect("target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("mode");
    std::os::unix::fs::symlink(&target, &folder).expect("link");

    assert!(Server::start(endpoint, world.service.clone()).is_err());
    assert_eq!(
        std::fs::metadata(&target).expect("target").permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(std::fs::read_dir(&target).expect("listing").count(), 0);
}

#[cfg(unix)]
#[test]
fn a_runtime_folder_is_used_only_when_it_is_private_and_this_users() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("temp dir");
    let folder = dir.path().join("run");
    std::fs::create_dir(&folder).expect("folder");

    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).expect("private");
    assert_eq!(server::usable_runtime(folder.clone()), Some(folder.clone()));

    for mode in [0o750, 0o705, 0o755] {
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(mode)).expect("mode");
        assert_eq!(server::usable_runtime(folder.clone()), None, "{mode:o}");
    }

    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).expect("private");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&folder, &link).expect("link");

    assert_eq!(server::usable_runtime(link), None, "not through a link");
    assert_eq!(server::usable_runtime("run".into()), None, "not a relative path");
    assert_eq!(server::usable_runtime(dir.path().join("missing")), None);
}

#[cfg(unix)]
#[test]
fn the_folder_of_a_long_data_folders_socket_is_made_private_and_a_strangers_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let world = World::ready(1);
    let temp = TempDir::new().expect("temp folder");
    let long = world.root.join("deep/".repeat(30));

    let endpoint = Endpoint::placed(&long, temp.path(), None);
    let folder = endpoint.path().parent().expect("folder").to_path_buf();

    // Taken by a folder other users may write to (a stranger's would fail the same check).
    std::fs::create_dir(&folder).expect("squatted folder");
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o777)).expect("open it up");

    let refused = Server::start(endpoint.clone(), world.service.clone())
        .err()
        .expect("refused");
    assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(refused.to_string().contains("write"), "{refused}");
    assert!(!endpoint.path().exists());
    assert_eq!(
        endpoint
            .connect(Duration::from_millis(200))
            .err()
            .map(|error| error.kind()),
        Some(std::io::ErrorKind::PermissionDenied)
    );

    // A link in its place is refused too.
    std::fs::remove_dir(&folder).expect("remove");
    std::os::unix::fs::symlink(temp.path(), &folder).expect("link");
    assert!(Server::start(endpoint.clone(), world.service.clone()).is_err());
    std::fs::remove_file(&folder).expect("remove the link");

    // Free, it is made `0700` and serves.
    let server = Server::start(endpoint.clone(), world.service.clone()).expect("start");
    let mode = std::fs::metadata(&folder).expect("folder").permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert!(EmbedderClient::from_endpoint(endpoint.clone()).ping().is_ok());
    server.shutdown();
}
