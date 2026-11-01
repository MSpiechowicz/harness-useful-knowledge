use super::*;

fn pick(url: &str) -> Result<Backend, ConfigError> {
    backend_for(Some(url.to_owned()), None)
}

#[test]
fn the_scheme_picks_the_backend() {
    assert_eq!(backend_for(None, None).ok(), Some(Backend::Sqlite));
    assert_eq!(pick("   ").ok(), Some(Backend::Sqlite));
    assert!(matches!(pick("postgres://db/m"), Ok(Backend::Postgres { .. })));
    assert!(matches!(pick("PostgreSQL://db/m"), Ok(Backend::Postgres { .. })));
    assert!(matches!(pick("libsql://team.turso.io"), Ok(Backend::Libsql { .. })));
    assert!(matches!(pick("https://sqld.example.com"), Ok(Backend::Libsql { .. })));
    assert!(matches!(pick("mongodb://db:27017/m"), Ok(Backend::Mongodb { .. })));
    assert!(matches!(
        pick("mongodb+srv://cluster.example.net/m"),
        Ok(Backend::Mongodb { .. })
    ));
    assert!(matches!(pick("mysql://db/m"), Err(ConfigError::Scheme(_))));
    assert!(matches!(pick("host=db user=u password=p"), Err(ConfigError::Scheme(_))));
}

#[test]
fn an_uppercase_scheme_is_passed_on_in_lowercase() {
    assert!(matches!(pick("POSTGRES://db/M"), Ok(Backend::Postgres { url }) if url == "postgres://db/M"));
    assert!(matches!(pick("HTTP://localhost:8080"), Ok(Backend::Libsql { url, .. }) if url == "http://localhost:8080"));
}

#[test]
fn documentdb_endpoints_are_refused_with_a_clear_message() {
    for url in [
        "mongodb://u:p@docs.cluster-abc.eu-central-1.docdb.amazonaws.com:27017/?tls=true&replicaSet=rs0",
        "mongodb+srv://u:p@docs.cluster-abc.eu-central-1.docdb.amazonaws.com/m",
        "mongodb://a.example.com,b.cluster-x.us-east-1.docdb.amazonaws.com:27017/m",
        "mongodb://u:p@x.docdb-elastic.amazonaws.com.cn/m",
    ] {
        let refused = pick(url).expect_err(url);

        assert!(matches!(refused, ConfigError::DocumentDb), "{url}");
        assert!(refused.to_string().contains("DocumentDB is not supported"), "{refused}");
    }

    assert!(matches!(
        pick("mongodb://db.example.com/docdb.amazonaws.com"),
        Ok(Backend::Mongodb { .. })
    ));
}

#[test]
fn plain_http_is_for_this_machine_only() {
    for local in ["http://localhost:8080", "http://127.0.0.1:8080/", "http://[::1]:8080"] {
        assert!(matches!(pick(local), Ok(Backend::Libsql { .. })), "{local}");
    }

    for remote in [
        "http://sqld.example.com",
        "http://127.0.0.1.example.com",
        "http://10.0.0.5:8080",
    ] {
        assert!(matches!(pick(remote), Err(ConfigError::PlainHttp)), "{remote}");
    }
}

#[test]
fn libsql_urls_carry_no_credentials() {
    for url in [
        "libsql://team.turso.io?authToken=abc",
        "https://u:p@sqld.example.com",
        "http://localhost@evil.example.com",
        "libsql://team.turso.io#token",
    ] {
        assert!(matches!(pick(url), Err(ConfigError::LibsqlUrlSecrets)), "{url}");
    }

    assert!(!ConfigError::LibsqlUrlSecrets.to_string().contains("abc"));
}

#[test]
fn the_token_goes_with_libsql() {
    let backend = backend_for(Some("libsql://db".to_owned()), Some(" tok ".to_owned())).expect("libsql");

    assert_eq!(
        backend,
        Backend::Libsql {
            url: "libsql://db".to_owned(),
            token: Some("tok".to_owned())
        }
    );
}

#[test]
fn a_backend_never_shows_its_secrets() {
    let shown = format!("{:?}", pick("mongodb://u:hunter2@db/m").expect("mongo"));
    assert!(!shown.contains("hunter2"), "{shown}");

    let libsql = Backend::Libsql {
        url: "libsql://db".to_owned(),
        token: Some("tok-123".to_owned()),
    };
    let scrubbed = libsql.secrets().scrub("rejected tok-123 for libsql://db");
    assert!(!scrubbed.contains("tok-123"), "{scrubbed}");
}

#[test]
fn the_environment_wins_over_the_config_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[database]\nurl = \"postgres://file/m\"\ntoken = \"file-token\"\n",
    )
    .expect("write");

    let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());
    assert_eq!(
        select(&env).expect("file").backend,
        Backend::Postgres {
            url: "postgres://file/m".to_owned()
        }
    );

    let env = env
        .with_var(URL_VAR, "libsql://env.example.com")
        .with_var(TOKEN_VAR, "");
    assert_eq!(
        select(&env).expect("env").backend,
        Backend::Libsql {
            url: "libsql://env.example.com".to_owned(),
            token: Some("file-token".to_owned())
        }
    );

    let env = env.with_var(TOKEN_VAR, "env-token");
    assert_eq!(
        select(&env).expect("env token").backend,
        Backend::Libsql {
            url: "libsql://env.example.com".to_owned(),
            token: Some("env-token".to_owned())
        }
    );
}

#[test]
fn config_file_problems_are_errors_without_secrets() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());

    assert!(matches!(select(&env), Err(ConfigError::ConfigMissing(_))));

    std::fs::write(&path, "[database]\nulr = \"postgres://u:hunter2@db/m\"\n").expect("write");
    let typo = select(&env).expect_err("unknown key").to_string();
    assert!(typo.contains("ulr") && typo.contains("line 2"), "{typo}");
    assert!(!typo.contains("hunter2"), "{typo}");

    std::fs::write(&path, "[databse]\nurl = \"postgres://u:hunter2@db/m\"\n").expect("write");
    let table = select(&env).expect_err("unknown table").to_string();
    assert!(table.contains("databse") && !table.contains("hunter2"), "{table}");

    std::fs::write(&path, "[database]\nurl = \"postgres://u:hunter2@db/m\n").expect("write");
    let broken = select(&env).expect_err("unterminated string").to_string();
    assert!(!broken.contains("hunter2"), "{broken}");

    let relative = Env::for_tests("/home/u").with_var(CONFIG_VAR, "config.toml");
    assert!(matches!(select(&relative), Err(ConfigError::ConfigRelative)));
}

#[test]
fn the_default_config_file_is_optional() {
    let dir = tempfile::tempdir().expect("temp dir");
    let env = Env::for_tests(dir.path())
        .with_os(OsKind::Linux)
        .with_var("XDG_CONFIG_HOME", dir.path().join("config").to_string_lossy());

    let selection = select(&env).expect("no file");

    assert_eq!(selection.backend, Backend::Sqlite);
    assert!(selection.warnings.is_empty());
}

#[test]
fn config_folders_per_platform() {
    let linux = Env::for_tests("/home/u").with_os(OsKind::Linux);
    assert_eq!(
        config_path(&linux).expect("path"),
        Some((PathBuf::from("/home/u/.config/harness-knowledge/config.toml"), false))
    );

    let xdg = linux.clone().with_var("XDG_CONFIG_HOME", "relative");
    assert_eq!(config_dir(&xdg), Some(PathBuf::from("/home/u/.config")));

    let mac = Env::for_tests("/Users/u").with_os(OsKind::MacOs);
    assert_eq!(
        config_dir(&mac),
        Some(PathBuf::from("/Users/u").join("Library").join("Application Support"))
    );

    let windows = Env::for_tests("C:/Users/u").with_os(OsKind::Windows);
    assert_eq!(config_dir(&windows), None);

    let roaming = std::env::temp_dir().join("AppData").join("Roaming");
    let windows = windows.with_var("APPDATA", roaming.to_string_lossy());
    assert_eq!(config_dir(&windows), Some(roaming));
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_readable_config_file_gets_a_warning() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[database]\nurl = \"postgres://db/m\"\n").expect("write");
        let env = Env::for_tests("/home/u").with_var(CONFIG_VAR, path.to_string_lossy());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let warnings = select(&env).expect("select").warnings;
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("chmod 600"), "{warnings:?}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(select(&env).expect("select").warnings.is_empty());
    }

    #[test]
    fn a_config_file_others_can_change_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let named = dir.path().join("named.toml");
        let default = dir.path().join("config").join("harness-knowledge").join(CONFIG_FILE);
        std::fs::create_dir_all(default.parent().expect("parent")).expect("mkdir");

        let by_variable = Env::for_tests(dir.path()).with_var(CONFIG_VAR, named.to_string_lossy());
        let by_default = Env::for_tests(dir.path())
            .with_os(OsKind::Linux)
            .with_var("XDG_CONFIG_HOME", dir.path().join("config").to_string_lossy());

        for (path, env) in [(&named, &by_variable), (&default, &by_default)] {
            std::fs::write(path, "[database]\nurl = \"postgres://u:hunter2@db/m\"\n").expect("write");

            for mode in [0o620, 0o602, 0o666] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
                let refused = select(env).expect_err("writable by others");
                assert!(
                    matches!(refused, ConfigError::ConfigWritable(_)),
                    "{mode:o}: {refused:?}"
                );

                let message = refused.to_string();
                assert!(
                    message.contains("chmod 600") && !message.contains("hunter2"),
                    "{message}"
                );
            }

            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
            assert!(matches!(
                select(env).expect("private and owned").backend,
                Backend::Postgres { .. }
            ));
        }
    }

    #[test]
    fn group_or_world_readable_modes_are_reported() {
        let readable = |mode: u32| {
            let dir = tempfile::tempdir().expect("temp dir");
            let path = dir.path().join("config.toml");
            std::fs::write(&path, "").expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
            permission_warning(&path).is_some()
        };

        assert!(!readable(0o600));
        assert!(!readable(0o700));
        assert!(readable(0o640));
        assert!(readable(0o604));
    }
}
