use super::*;

fn parse(url: &str) -> ClientOptions {
    ClientOptions::parse(url)
        .run()
        .unwrap_or_else(|error| panic!("{url}: {error:?}"))
}

#[test]
fn loopback_hosts_and_sockets_are_local() {
    for host in [
        "localhost",
        "localhost:27017",
        "127.0.0.1:1",
        "[::1]:27017",
        "%2Ftmp%2Fmongodb-27017.sock",
    ] {
        assert!(is_local_host(host), "{host}");
    }

    for host in [
        "db.example.com",
        "10.0.0.5:27017",
        "[2001:db8::1]:27017",
        "localhost.example.com",
    ] {
        assert!(!is_local_host(host), "{host}");
    }
}

#[test]
fn remote_servers_need_tls_with_certificate_checks() {
    let refused = [
        "mongodb://db.example.com/?tls=false",
        "mongodb://db.example.com/?SSL=FALSE",
        "mongodb+srv://cluster.example.com/?tls=false",
        "mongodb://localhost,db.example.com/?tls=false",
        "mongodb://localhost,db.example.com/?directConnection=true&tls=false",
        "mongodb://db.example.com/?tlsInsecure=true",
        "mongodb://db.example.com/?tlsAllowInvalidCertificates=true",
    ];
    for url in refused {
        assert!(matches!(check(url), Err(StoreError::Invalid(_))), "{url}");
    }

    assert!(!check("mongodb://db.example.com/x?tls=true").expect("remote").local);
    assert!(matches!(check("postgres://localhost"), Err(StoreError::Invalid(_))));
}

#[test]
fn loopback_seeds_are_local_only_when_the_driver_stays_on_them() {
    // Without a direct or load-balanced connection the driver also connects to the replica-set
    // members the server lists, which may be remote: TLS rules apply.
    let discovering = [
        "mongodb://localhost/?tls=false&replicaSet=rs0",
        "mongodb://localhost:27017/?tls=false",
        "mongodb://127.0.0.1,[::1]/?tls=false",
        "mongodb://127.0.0.1/?directConnection=false&tls=false",
        "mongodb://localhost/?tlsAllowInvalidCertificates=true",
    ];
    for url in discovering {
        assert!(matches!(check(url), Err(StoreError::Invalid(_))), "{url}");
    }

    let local = [
        "mongodb://127.0.0.1:27017/?directConnection=true&tls=false",
        "mongodb://localhost:57017/x?replicaSet=rs0&DIRECTCONNECTION=TRUE",
        "mongodb://localhost/?loadBalanced=true&tls=false",
        "mongodb://%2Ftmp%2Fmongodb-27017.sock/?tls=false",
    ];
    for url in local {
        assert!(check(url).expect("local").local, "{url}");
    }

    assert!(
        !check("mongodb://localhost/?replicaSet=rs0")
            .expect("TLS turned on")
            .local
    );
}

#[test]
fn parsed_options_decide_when_the_url_looked_local() {
    let local = Checked { local: true };

    let mut direct = parse("mongodb://127.0.0.1:27017/?directConnection=true&tls=false");
    assert!(enforce_tls(&mut direct, &local).is_ok());

    let mut discovering = parse("mongodb://127.0.0.1:27017/?tls=false");
    assert!(matches!(
        enforce_tls(&mut discovering, &local),
        Err(StoreError::Invalid(_))
    ));

    let mut unset = parse("mongodb://127.0.0.1:27017/");
    enforce_tls(&mut unset, &local).expect("TLS turned on");
    assert!(matches!(unset.tls, Some(Tls::Enabled(_))));
}

#[test]
fn relative_tls_files_are_refused_for_every_server() {
    // A relative path is read from the working directory, the project: these files exist there.
    let urls = [
        "mongodb://localhost/?directConnection=true&tls=true&tlsCAFile=assets%2Fca.pem",
        "mongodb://localhost/?directConnection=true&tls=true&tlsCAFile=.%2FCargo.toml",
        "mongodb://localhost/?directConnection=true&tls=true&tlsCertificateKeyFile=Cargo.toml",
        "mongodb://db.example.com/?tlsCAFile=ca.pem",
        "mongodb+srv://cluster.example.com/?TLSCERTIFICATEKEYFILE=certs%2Fme.pem",
    ];

    for url in urls {
        let error = check(url).err().unwrap_or_else(|| panic!("{url}: accepted"));
        assert!(matches!(error, StoreError::Invalid(_)), "{url}: {error:?}");
        assert!(error.to_string().contains("absolute path"), "{error}");
        assert!(!error.to_string().contains(';'), "{error}");
    }

    // The parsed options are checked too, whatever `check` let through.
    for url in &urls[..3] {
        let error = tls_files(&parse(url)).expect_err("refused");
        assert!(matches!(error, StoreError::Invalid(_)), "{url}: {error:?}");
    }
}

#[test]
fn an_existing_absolute_ca_file_is_used_and_a_missing_one_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let own = dir.path().join("ca.pem");
    std::fs::write(&own, "own bundle").expect("write");
    let encoded = own.to_string_lossy().replace('/', "%2F");

    let options = parse(&format!("mongodb://db.example.com/?tls=true&tlsCAFile={encoded}"));
    tls_files(&options).expect("own file");

    // Absolute on every platform (`/nowhere` is not on Windows, which needs a drive).
    let gone = dir
        .path()
        .join("nowhere")
        .join("ca.pem")
        .to_string_lossy()
        .replace('/', "%2F");
    let missing = parse(&format!("mongodb://db.example.com/?tlsCAFile={gone}"));
    let error = tls_files(&missing).expect_err("missing");
    assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");
    assert!(error.to_string().contains("does not exist"), "{error}");

    let plain = parse("mongodb://localhost:27017/?directConnection=true&tls=false");
    tls_files(&plain).expect("no TLS, nothing to check");
}

#[test]
fn server_code_names_are_shown_only_when_they_are_plain_words() {
    assert_eq!(
        coded("write failed", Some("DuplicateKey"), 11000),
        "write failed: DuplicateKey (11000)"
    );
    assert_eq!(coded("write failed", None, 11000), "write failed (11000)");

    for name in [
        "",
        "Duplicate Key",
        "Bad: see https://evil.example",
        "Über",
        &"A".repeat(41),
    ] {
        assert_eq!(coded("write failed", Some(name), 2), "write failed (2)", "{name}");
    }
}
