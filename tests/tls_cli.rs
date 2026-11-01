//! A team database whose certificate the TLS check refuses is a setting to fix, not a database that
//! is unreachable: with both places in use the command fails and says so, instead of answering from
//! this computer and starting the cooldown. The server is a local TLS listener with a self-signed
//! certificate (`openssl s_server`), so the test skips when `openssl` cannot be run.

// The two tests that need `openssl s_server` run on Unix only, so its helpers are unused elsewhere.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BINARY: &str = env!("CARGO_BIN_EXE_harness-knowledge");

/// An `openssl s_server` with a certificate that nothing trusts, stopped when dropped.
struct TlsServer {
    child: Child,
    port: u16,
}

impl TlsServer {
    fn start(folder: &Path) -> Option<Self> {
        let certificate = folder.join("certificate.pem");
        let key = folder.join("key.pem");

        let made = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
            ])
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&certificate)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;

        if !made.success() {
            return None;
        }

        let port = TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok()?.port();
        let child = Command::new("openssl")
            .args(["s_server", "-quiet", "-accept"])
            .arg(port.to_string())
            .arg("-cert")
            .arg(&certificate)
            .arg("-key")
            .arg(&key)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let server = Self { child, port };
        let started = Instant::now();

        while started.elapsed() < Duration::from_secs(10) {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Some(server);
            }

            std::thread::sleep(Duration::from_millis(50));
        }

        None
    }
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        let _killed = self.child.kill();
        let _waited = self.child.wait();
    }
}

fn run(root: &Path, url: &str, layout: &str, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(BINARY)
        .args(args)
        .env_clear()
        .env("HOME", root)
        .env("HARNESS_KNOWLEDGE_DIR", root.join("knowledge"))
        .env("HARNESS_KNOWLEDGE_DATABASE_URL", url)
        .env("HARNESS_KNOWLEDGE_REPOSITORY_DOCUMENTS", layout)
        .env("HARNESS_KNOWLEDGE_GLOBAL_DOCUMENTS", layout)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .output()
        .expect("runs");

    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

// The stand-in TLS server (`openssl s_server`) does not listen on Windows runners.
#[cfg(unix)]
#[test]
fn a_mongodb_whose_certificate_is_refused_fails_with_a_tls_message_and_is_never_offline() {
    let root = tempfile::tempdir().expect("temp dir");
    let Some(server) = TlsServer::start(root.path()) else {
        eprintln!("skipped: no `openssl` to make a self-signed server");
        return;
    };
    let url = format!(
        "mongodb://localhost:{}/?directConnection=true&tls=true&serverSelectionTimeoutMS=800",
        server.port
    );

    for layout in ["remote+local", "remote"] {
        let (code, stdout, stderr) = run(root.path(), &url, layout, &["docs", "list", "--json"]);

        assert_eq!(code, Some(1), "{layout}: {stdout} {stderr}");
        assert!(
            stderr.contains("TLS with the MongoDB server failed"),
            "{layout}: {stderr}"
        );
        assert!(!stdout.contains("offline"), "{layout}: {stdout}");
        assert!(
            !root.path().join("knowledge").join("remote-unreachable.json").exists(),
            "{layout}: a certificate that is refused starts no cooldown"
        );
    }

    let (code, _, stderr) = run(
        root.path(),
        &url,
        "remote+local",
        &["search", "--json", "--query", "gateway"],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("TLS with the MongoDB server failed"), "{stderr}");
}

/// A server that agrees to TLS (`S` to the SSL request) and then talks nonsense, as a port that does
/// not speak PostgreSQL does. The client's TLS check refuses it.
fn nonsense_after_tls_request() -> u16 {
    use std::io::{Read, Write};

    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut request = [0_u8; 8];
            let _ = stream.read_exact(&mut request);
            let _ = stream.write_all(b"S");
            let _ = stream.write_all(&[0x16; 200]);
        }
    });

    port
}

/// A server that agrees to TLS (`S` to the SSL request) and closes the connection, as one that is
/// restarting or reset does. The handshake is lost, the certificate was never refused.
fn closing_after_tls_request() -> u16 {
    use std::io::{Read, Write};

    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut request = [0_u8; 8];
            let _ = stream.read_exact(&mut request);
            let _ = stream.write_all(b"S");
        }
    });

    port
}

// The stand-in TLS server (`openssl s_server`) does not listen on Windows runners.
#[cfg(unix)]
#[test]
fn a_postgres_server_whose_tls_is_refused_is_a_setting_to_fix_and_check_starts_no_cooldown() {
    let root = tempfile::tempdir().expect("temp dir");
    let port = nonsense_after_tls_request();
    let url = format!("postgres://someone@127.0.0.1:{port}/knowledge?sslmode=require&connect_timeout=3");
    let cooldown = root.path().join("knowledge").join("remote-unreachable.json");

    let (code, stdout, stderr) = run(root.path(), &url, "remote+local", &["check", "--json"]);
    let checked: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|_| panic!("{stdout} {stderr}"));

    assert_eq!(code, Some(3), "{stdout} {stderr}");
    assert_eq!(checked["places"]["remote"]["problem"], "unreachable", "{stdout}");
    assert!(!cooldown.exists(), "a refused TLS check is not an outage: {stdout}");

    let (code, stdout, stderr) = run(root.path(), &url, "remote+local", &["docs", "list", "--json"]);
    assert_eq!(code, Some(1), "{stdout} {stderr}");
    assert!(!stdout.contains("offline"), "{stdout}");
    assert!(!cooldown.exists(), "{stderr}");

    // A port nobody listens at is an outage, and the same check records it.
    let closed = TcpListener::bind("127.0.0.1:0")
        .expect("listener")
        .local_addr()
        .expect("address")
        .port();
    let refused = format!("postgres://someone@127.0.0.1:{closed}/knowledge?sslmode=disable&connect_timeout=3");
    let (code, stdout, _) = run(root.path(), &refused, "remote+local", &["check", "--json"]);

    assert_eq!(code, Some(3), "{stdout}");
    assert!(cooldown.exists(), "a connection that does not work is recorded");
}

#[test]
fn a_postgres_handshake_that_is_lost_is_unreachable_and_check_starts_the_cooldown() {
    let root = tempfile::tempdir().expect("temp dir");
    let port = closing_after_tls_request();
    let url = format!("postgres://someone@127.0.0.1:{port}/knowledge?sslmode=require&connect_timeout=3");
    let cooldown = root.path().join("knowledge").join("remote-unreachable.json");

    let (code, stdout, stderr) = run(root.path(), &url, "remote+local", &["check", "--json"]);
    let checked: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|_| panic!("{stdout} {stderr}"));

    assert_eq!(code, Some(3), "{stdout} {stderr}");
    assert_eq!(checked["places"]["remote"]["problem"], "unreachable", "{stdout}");
    assert!(
        cooldown.exists(),
        "a handshake that was cut off is an outage, not a refused certificate: {stdout}"
    );
}
