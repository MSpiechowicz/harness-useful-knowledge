//! What needs no server: the database name from the URL and the rules the URL must meet.

use crate::store::StoreError;
use crate::store::mongo::{DEFAULT_DATABASE, connect_client};

#[test]
fn the_database_is_the_url_path_and_harness_knowledge_without_one() {
    let (_client, named) = connect_client("mongodb://localhost:27017/team_docs?directConnection=true").expect("client");
    assert_eq!(named.name(), "team_docs");

    let (_client, unnamed) = connect_client("mongodb://localhost:27017/?directConnection=true").expect("client");
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
        match connect_client(url) {
            Err(StoreError::Invalid(_)) => {}
            Err(other) => panic!("{url}: {other:?}"),
            Ok(_) => panic!("{url}: accepted"),
        }
    }
}
