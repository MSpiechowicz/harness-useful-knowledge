//! The commands while the remote database cannot be reached: with both places in use they answer
//! from this computer and say so, a document or a write of the remote is refused, and with the
//! remote database alone nothing changes. The remote in these tests is a port nobody listens on,
//! so the connection is refused at once.

use std::time::{Duration, Instant};

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::import::import_with;
use super::*;
use crate::config::Where;
use crate::extract::pdf::PdfPages;
use crate::import;
use crate::location::{DIR_VAR, FILE_NAME};
use crate::store::testing::{Seed, insert};
use crate::store::{Reach, SqliteStore};

struct NoPdf;

impl PdfPages for NoPdf {
    fn pages(&self, _path: &std::path::Path) -> Result<Vec<String>, String> {
        Err("no PDF reader in this test".to_owned())
    }
}

/// A computer whose remote database is at `url`, with one document of its own.
struct Computer {
    root: tempfile::TempDir,
    env: Env,
    backend: Backend,
}

const BOTH: Routing = Routing {
    repository: Where::RemoteLocal,
    global: Where::RemoteLocal,
};

impl Computer {
    /// Nobody listens on port 1: a refused connection.
    fn new() -> Self {
        Self::at("postgres://nobody@127.0.0.1:1/none")
    }

    fn at(url: &str) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let folder = root.path().join("knowledge");
        let env = Env::for_tests("/home/u").with_var(DIR_VAR, folder.to_string_lossy());
        let backend = crate::config::backend_for(Some(url.to_owned()), None).expect("backend");

        let mut local = SqliteStore::open(&folder.join(FILE_NAME)).expect("local database");
        insert(&mut local, &Seed::new("Mine").chunk("Intro", "gateway notes of mine"));

        Self { root, env, backend }
    }

    fn context(&self, routing: Routing) -> Context<'_> {
        Context::new(&self.env, &self.backend, routing)
    }

    fn run(&self, routing: Routing, args: &[&str]) -> Reply {
        let args: Vec<Option<&str>> = args.iter().map(|arg| Some(*arg)).collect();

        docs_reply(&self.context(routing), &args, &mut std::io::empty())
    }

    fn json(&self, routing: Routing, args: &[&str]) -> Value {
        self.run(routing, args).expect("answers").json
    }

    fn cooldown_file(&self) -> std::path::PathBuf {
        self.root.path().join("knowledge").join("remote-unreachable.json")
    }

    fn reach(&self) -> Reach {
        backend::reach_of(&self.env, &self.backend)
    }
}

#[test]
fn a_list_comes_from_this_computer_and_says_offline_while_the_remote_is_unreachable() {
    let computer = Computer::new();

    let listed = computer.json(BOTH, &["list", "--json"]);

    assert_eq!(listed["total"], 1);
    assert_eq!(listed["items"][0]["title"], "Mine");
    assert_eq!(listed["items"][0]["id"], "local:1");
    assert_eq!(listed["offline"], json!(true));

    // The silent servers of other tests in this process can hold every slot for opening the remote
    // for a while. Then nothing was tried and nothing is recorded (`OpenError::Busy`), so try again.
    let started = Instant::now();

    while !computer.cooldown_file().is_file() && started.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(100));
        computer.json(BOTH, &["list", "--json"]);
    }

    assert!(
        computer.cooldown_file().is_file(),
        "the failure is shared with other processes"
    );

    let repos = computer.json(BOTH, &["list", "--json"]);
    assert_eq!(repos["offline"], json!(true), "the cooldown answers the same");
}

#[test]
fn the_offline_flag_is_absent_when_the_remote_answers_or_is_not_in_use() {
    let computer = Computer::new();

    let local_only = computer.json(Routing::local(), &["list", "--json"]);
    assert_eq!(local_only["total"], 1);
    assert!(local_only.get("offline").is_none(), "{local_only}");

    let repos = overview::repos(&computer.context(Routing::local()), &[Some("--json")]).expect("repos");
    assert!(repos.json.get("offline").is_none());
    assert!(!computer.cooldown_file().exists(), "nothing was tried");
}

#[test]
fn repos_and_stats_report_this_computer_and_the_remote_as_offline() {
    let computer = Computer::new();

    let repos = overview::repos(&computer.context(BOTH), &[Some("--json")]).expect("repos");
    assert_eq!(repos.json["global"], 1);
    assert_eq!(repos.json["offline"], json!(true));

    let stats = overview::stats(&computer.context(BOTH), &[Some("--json")]).expect("stats");

    assert_eq!(stats.code, 0);
    assert_eq!(stats.json["offline"], json!(true));
    assert_eq!(stats.json["documents"], 1, "the counts at the top are this computer's");
    assert_eq!(stats.json["places"]["remote"], json!({ "offline": true }));
    assert_eq!(stats.json["places"]["local"]["documents"], 1);

    // The generation of the place that is counted, and none for the one that is not.
    assert_eq!(
        stats.json["generations"],
        json!({ "local": stats.json["places"]["local"]["generation"] })
    );
    assert!(
        stats.json["generations"]["local"]
            .as_i64()
            .is_some_and(|generation| generation > 0)
    );
}

#[test]
fn a_document_of_the_remote_is_refused_with_exit_1_and_one_of_this_computer_is_read() {
    let computer = Computer::new();

    for args in [
        &["get", "--json", "5"][..],
        &["matches", "--json", "5", "--query", "gateway"],
    ] {
        let refused = computer.run(BOTH, args);
        assert_eq!(
            refused,
            Err(Failure::Unreachable(
                "remote knowledge is unreachable right now".to_owned()
            )),
            "{args:?}"
        );
    }

    let local = computer.json(BOTH, &["get", "--json", "local:1"]);
    assert_eq!(local["document"]["title"], "Mine");

    let matches = computer.json(BOTH, &["matches", "--json", "local:1", "--query", "gateway"]);
    assert_eq!(matches["total"], 1);
}

#[test]
fn a_change_meant_for_the_remote_is_refused_and_a_change_to_this_computer_goes_on() {
    let computer = Computer::new();
    let down = |reply: Reply| match reply {
        Err(Failure::Unreachable(message)) => {
            assert!(message.starts_with("remote knowledge is unreachable"), "{message}")
        }
        other => panic!("not refused: {other:?}"),
    };

    down(computer.run(BOTH, &["rename", "--json", "5", "--title", "New"]));
    down(computer.run(BOTH, &["delete", "--json", "5"]));

    let renamed = computer.json(BOTH, &["rename", "--json", "local:1", "--title", "Renamed"]);
    assert_eq!(renamed["title"], "Renamed");

    // An import to the remote, which is the default place, is refused before anything is read.
    let file = computer.root.path().join("guide.md");
    std::fs::write(&file, "# Guide\n\nHow the gateway routes requests.").expect("write");
    let manifest = import::Manifest {
        paths: vec![file.to_string_lossy().into_owned()],
        ..import::Manifest::default()
    };
    let mut out: Vec<u8> = Vec::new();

    let result = import_with(&computer.context(BOTH), &manifest, &NoPdf, None, &|| false, &mut out);

    assert_eq!(
        result.err(),
        Some(Failure::Unreachable(crate::store::routed::REMOTE_DOWN_WRITE.to_owned()))
    );
    assert!(out.is_empty(), "no event: nothing was imported");

    let listed = computer.json(BOTH, &["list", "--json"]);
    assert_eq!(listed["total"], 1, "nothing landed on this computer instead");
}

#[test]
fn an_import_that_names_this_computer_does_not_need_the_remote() {
    let computer = Computer::new();
    let file = computer.root.path().join("guide.md");
    std::fs::write(&file, "# Guide\n\nHow the gateway routes requests.").expect("write");
    let manifest = import::Manifest {
        paths: vec![file.to_string_lossy().into_owned()],
        place: Some(Place::Local),
        ..import::Manifest::default()
    };
    let mut out: Vec<u8> = Vec::new();

    import_with(&computer.context(BOTH), &manifest, &NoPdf, None, &|| false, &mut out).expect("imports");

    let listed = computer.json(BOTH, &["list", "--json"]);
    assert_eq!(listed["total"], 2);
}

#[test]
fn check_keeps_its_answer_and_tells_the_cooldown_what_it_found() {
    let computer = Computer::new();
    let done = check::check(&computer.context(BOTH), &[Some("--json")]).expect("check answers");

    assert_eq!(done.code, 3);
    assert_eq!(done.json["problem"], "unreachable");
    assert_eq!(done.json["places"]["remote"]["problem"], "unreachable");
    assert_eq!(done.json["places"]["local"]["ok"], true);
    assert!(
        computer.reach().cooling().is_some(),
        "an unreachable remote starts the cooldown"
    );

    computer.reach().reached();
    assert!(computer.reach().cooling().is_none());
}

#[test]
fn with_the_remote_alone_an_unreachable_database_fails_as_before_and_leaves_no_cooldown() {
    let computer = Computer::new();
    let remote = Routing::new(Where::Remote, Where::Remote);

    match computer.run(remote, &["list", "--json"]) {
        Err(Failure::Unreachable(message)) => {
            assert!(message.contains("cannot reach the PostgreSQL database"), "{message}");
            assert!(!message.contains("remote knowledge is unreachable"), "{message}");
        }
        other => panic!("{other:?}"),
    }

    assert!(!computer.cooldown_file().exists());
    assert_eq!(
        computer
            .run(remote, &["get", "--json", "3"])
            .err()
            .map(|failure| matches!(failure, Failure::Unreachable(_))),
        Some(true)
    );
}

#[test]
fn a_recorded_failure_skips_the_remote_without_dialling_it() {
    // 192.0.2.1 is reserved for documentation: a connection to it never completes.
    let computer = Computer::at("postgres://nobody@192.0.2.1:5432/none?sslmode=require");
    computer.reach().failed("cannot connect");

    let started = Instant::now();
    let listed = computer.json(BOTH, &["list", "--json"]);

    assert_eq!(listed["offline"], json!(true));
    assert_eq!(listed["total"], 1);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_list_narrowed_to_a_place_answers_for_that_place_and_a_bad_place_is_a_usage_error() {
    let computer = Computer::new();

    let local = computer.json(BOTH, &["list", "--json", "--place", "local"]);
    assert_eq!(
        (local["total"].clone(), local["items"][0]["id"].clone()),
        (json!(1), json!("local:1"))
    );
    assert_eq!(local["offline"], json!(true), "the remote is still down");

    assert_eq!(
        computer.run(BOTH, &["list", "--json", "--place", "remote"]),
        Err(Failure::Unreachable(
            "remote knowledge is unreachable right now".to_owned()
        ))
    );
    assert_eq!(
        computer.run(BOTH, &["list", "--json", "--place", "elsewhere"]),
        Err(Failure::Usage)
    );

    // With one place in use, the other lists nothing and its own lists everything.
    let none = computer.json(Routing::local(), &["list", "--json", "--place", "remote"]);
    assert_eq!((none["total"].clone(), none["items"].clone()), (json!(0), json!([])));

    let here = computer.json(Routing::local(), &["list", "--json", "--place", "local"]);
    assert_eq!(here["total"], 1);

    let remote_alone = Routing::new(Where::Remote, Where::Remote);
    let none = computer.json(remote_alone, &["list", "--json", "--place", "local"]);
    assert_eq!(none["total"], 0);
}

#[test]
fn a_remote_whose_tls_is_refused_is_reported_but_starts_no_cooldown() {
    // A server that answers the TLS request of the client with "no TLS here".
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut request = [0_u8; 8];
            let _read = std::io::Read::read(&mut stream, &mut request);
            let _answered = std::io::Write::write_all(&mut stream, b"N");
        }
    });

    let computer = Computer::at(&format!("postgres://nobody@127.0.0.1:{port}/none?sslmode=require"));
    let done = check::check(&computer.context(BOTH), &[Some("--json")]).expect("check answers");

    assert_eq!(done.json["places"]["remote"]["problem"], "unreachable");
    assert!(
        done.json["places"]["remote"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("TLS")),
        "{}",
        done.json
    );
    assert!(computer.reach().cooling().is_none(), "no cooldown for a refused TLS");
    assert!(!computer.cooldown_file().exists());
}
