//! A team database whose certificate the TLS check refuses is a setting to fix, not a database that
//! is unreachable: with both places in use the command fails and says so, instead of answering from
//! this computer and starting the cooldown. The server is a local TLS listener with a self-signed
//! certificate (`openssl s_server`), so the test skips when `openssl` cannot be run.

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
