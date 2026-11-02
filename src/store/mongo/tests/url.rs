//! What needs no server: the database name from the URL and the rules the URL must meet.

use crate::store::StoreError;
use std::time::Duration;

use crate::store::mongo::open::{COUNT_CONNECT_TIMEOUT, COUNT_HEARTBEAT, client_options};
use crate::store::mongo::{DEFAULT_DATABASE, Mode, SERVER_SELECTION_TIMEOUT, connect_client};

#[test]
fn the_database_is_the_url_path_and_harness_knowledge_without_one() {
    let (_client, named) = connect_client(
        "mongodb://localhost:27017/team_docs?directConnection=true",
        SERVER_SELECTION_TIMEOUT,
    )
    .expect("client");
    assert_eq!(named.name(), "team_docs");

    let (_client, unnamed) = connect_client(
        "mongodb://localhost:27017/?directConnection=true",
        SERVER_SELECTION_TIMEOUT,
    )
    .expect("client");
    assert_eq!(unnamed.name(), DEFAULT_DATABASE);
    assert_eq!(DEFAULT_DATABASE, "harness_knowledge");
}

#[test]
fn a_url_that_is_not_mongodb_or_breaks_the_tls_rules_is_refused_without_a_lookup() {
    for url in [
        "postgres://localhost/knowledge",
        "mongodb://db.example.com/?tls=false",
        "mongodb://db.example.com/?tlsAllowInvalidCertificates=true",
        "mongodb://db.example.com/?tlsCAFile=ca.pem",
    ] {
        match connect_client(url, SERVER_SELECTION_TIMEOUT) {
            Err(StoreError::Invalid(_)) => {}
            Err(other) => panic!("{url}: {other:?}"),
            Ok(_) => panic!("{url}: accepted"),
        }
    }
}

#[test]
fn only_a_counting_client_gives_up_on_a_server_that_stops_answering_within_seconds() {
    let url = "mongodb://localhost:27017/?directConnection=true";

    for mode in [Mode::ReadOnly, Mode::Existing, Mode::Create] {
        let (options, _) = client_options(url, SERVER_SELECTION_TIMEOUT, mode).expect("options");
        assert_eq!(
            (options.connect_timeout, options.heartbeat_freq),
            (None, None),
            "{mode:?}"
        );
    }

    let (options, _) = client_options(url, SERVER_SELECTION_TIMEOUT, Mode::Counters).expect("options");
    assert_eq!(options.connect_timeout, Some(COUNT_CONNECT_TIMEOUT));
    assert_eq!(options.heartbeat_freq, Some(COUNT_HEARTBEAT));
    assert!(COUNT_CONNECT_TIMEOUT + COUNT_HEARTBEAT * 2 <= Duration::from_secs(2));

    // A shorter bound the URL names itself is kept, a longer one is cut down.
    let named = "mongodb://localhost:27017/?directConnection=true&connectTimeoutMS=300&heartbeatFrequencyMS=60000";
    let (options, _) = client_options(named, SERVER_SELECTION_TIMEOUT, Mode::Counters).expect("options");
    assert_eq!(options.connect_timeout, Some(Duration::from_millis(300)));
    assert_eq!(options.heartbeat_freq, Some(COUNT_HEARTBEAT));

    // Zero means "no limit" to the driver, so it does not lift the bound.
    let unbounded = "mongodb://localhost:27017/?directConnection=true&connectTimeoutMS=0";
    let (options, _) = client_options(unbounded, SERVER_SELECTION_TIMEOUT, Mode::Counters).expect("options");
    assert_eq!(options.connect_timeout, Some(COUNT_CONNECT_TIMEOUT));
}
