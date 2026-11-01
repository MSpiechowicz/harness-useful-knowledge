//! The fakes and fixtures the embedder tests share.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::embed::engine::{Embed, Kind, Loader};
use crate::embed::manifest::{Manifest, ModelFile, ONNX_FILE, Pooling, Spec, TOKENIZER_FILE};
use crate::embed::pool::{Holder, Pool};
use crate::embed::{Event, Events, Options, Service, locked, model};

pub const ONNX_BYTES: &[u8] = b"fake onnx graph bytes";
pub const TOKENIZER_BYTES: &[u8] = b"{\"fake\":\"tokenizer\"}";

/// The size of a fake vector.
pub const DIMS: usize = 8;

/// A manifest of two tiny files; every id has the same bytes.
pub fn fake_manifest(id: &str) -> Manifest {
    let file = |name: &'static str, bytes: &[u8]| ModelFile {
        name,
        url: Box::leak(format!("https://invalid.example/{name}").into_boxed_str()),
        sha256: Box::leak(hex(bytes).into_boxed_str()),
        size: bytes.len() as u64,
    };

    let files: &'static [ModelFile] = Box::leak(Box::new([
        file(ONNX_FILE, ONNX_BYTES),
        file(TOKENIZER_FILE, TOKENIZER_BYTES),
    ]));

    Manifest {
        id: Box::leak(id.to_owned().into_boxed_str()),
        files,
        dims: DIMS,
        max_tokens: 512,
        pooling: Pooling::Mean,
        normalize: true,
    }
}

pub fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Puts the files of `manifest` in place the way a download does.
pub fn install(root: &Path, manifest: &Manifest) {
    let base = Fixture::serving(manifest);
    let events = Collect::default();

    model::ensure(root, manifest, Some(&base.url()), &events, &|| false).expect("install the fake model");
}

/// Events kept in a list.
#[derive(Default)]
pub struct Collect(Mutex<Vec<Event>>);

impl Events for Collect {
    fn emit(&self, event: Event) {
        locked(&self.0).push(event);
    }
}

impl Collect {
    pub fn all(&self) -> Vec<Event> {
        locked(&self.0).clone()
    }

    /// The names of the events (`ready`, `loading`, ...), in order.
    pub fn names(&self) -> Vec<String> {
        self.all()
            .iter()
            .map(|event| event.to_json()["event"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    pub fn errors(&self) -> Vec<String> {
        self.all()
            .into_iter()
            .filter_map(|event| match event {
                Event::Error { message } => Some(message),
                _ => None,
            })
            .collect()
    }
}

/// What the fake engine does and has done, shared by all its users.
#[derive(Default)]
pub struct Control {
    pub embeds: AtomicUsize,
    /// The texts in the order the engine took them (after the gate).
    pub order: Mutex<Vec<(Kind, String)>>,
    closed: Mutex<bool>,
    gate: Condvar,
    delay_ms: AtomicU64,
    /// When above 1, calls wait until that many are inside the engine at once (once).
    meet: AtomicUsize,
    inside: AtomicUsize,
    pub peak: AtomicUsize,
}

impl Control {
    /// Calls block inside the engine until [`Control::open`].
    pub fn close(&self) {
        *locked(&self.closed) = true;
    }

    pub fn open(&self) {
        *locked(&self.closed) = false;
        self.gate.notify_all();
    }

    pub fn delay(&self, time: Duration) {
        self.delay_ms.store(time.as_millis() as u64, Ordering::SeqCst);
    }

    pub fn meet(&self, calls: usize) {
        self.meet.store(calls, Ordering::SeqCst);
    }

    pub fn taken(&self) -> Vec<(Kind, String)> {
        locked(&self.order).clone()
    }

    /// Waits until `condition` holds (up to five seconds).
    pub fn wait_for(&self, condition: impl Fn(&Self) -> bool) {
        let started = Instant::now();

        while !condition(self) {
            assert!(started.elapsed() < Duration::from_secs(5), "the condition never held");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn inside(&self) -> usize {
        self.inside.load(Ordering::SeqCst)
    }
}

/// A hashed bag of words, scaled to length 1: a text always gets the same vector, and the kind's
/// prefix is part of the text (a query and a passage with the same words differ).
pub fn fake_vector(kind: Kind, text: &str) -> Vec<f32> {
    let mut vector = [0_f32; DIMS];

    for word in format!("{}{text}", kind.prefix()).split_whitespace() {
        let digest = Sha256::digest(word.to_lowercase().as_bytes());
        vector[usize::from(digest[0]) % DIMS] += 1.0;
    }

    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt().max(1e-12);
    vector.iter().map(|value| value / norm).collect()
}

struct FakeEngine {
    control: Arc<Control>,
}

impl Embed for FakeEngine {
    fn embed(&self, kind: Kind, text: &str) -> Result<Vec<f32>, String> {
        let control = &self.control;
        let now = control.inside.fetch_add(1, Ordering::SeqCst) + 1;
        control.peak.fetch_max(now, Ordering::SeqCst);

        // The first group that meets lets everyone through, and the rest of the run goes unhindered.
        let started = Instant::now();

        loop {
            let meet = control.meet.load(Ordering::SeqCst);

            if meet <= 1 || started.elapsed() > Duration::from_secs(5) {
                break;
            }

            if control.inside.load(Ordering::SeqCst) >= meet {
                control.meet.store(0, Ordering::SeqCst);
                break;
            }

            std::thread::sleep(Duration::from_millis(2));
        }

        {
            let mut closed = locked(&control.closed);

            while *closed {
                closed = control
                    .gate
                    .wait(closed)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        locked(&control.order).push((kind, text.to_owned()));
        control.embeds.fetch_add(1, Ordering::SeqCst);

        std::thread::sleep(Duration::from_millis(control.delay_ms.load(Ordering::SeqCst)));
        control.inside.fetch_sub(1, Ordering::SeqCst);

        if text.contains("explode") {
            return Err("the fake engine refuses this text".to_owned());
        }

        Ok(fake_vector(kind, text))
    }
}

/// The fake loader: counts its loads, and can take a while to load. A clone shares everything.
#[derive(Clone)]
pub struct Fake {
    pub loads: Arc<AtomicUsize>,
    pub control: Arc<Control>,
    pub load_time: Duration,
    pub fail_loads: Arc<AtomicBool>,
}

impl Fake {
    pub fn new() -> Self {
        Self {
            loads: Arc::new(AtomicUsize::new(0)),
            control: Arc::new(Control::default()),
            load_time: Duration::ZERO,
            fail_loads: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn loading_for(mut self, time: Duration) -> Self {
        self.load_time = time;
        self
    }
}

impl Loader for Fake {
    fn load(&self, _dir: &Path, _spec: &Spec) -> Result<Arc<dyn Embed>, String> {
        std::thread::sleep(self.load_time);

        if self.fail_loads.load(Ordering::SeqCst) {
            return Err("cannot load the model".to_owned());
        }

        self.loads.fetch_add(1, Ordering::SeqCst);

        Ok(Arc::new(FakeEngine {
            control: Arc::clone(&self.control),
        }))
    }
}

/// A pool of `workers` over the fake engine (nothing downloaded: the fake loads from nowhere).
pub struct PoolRig {
    pub pool: Arc<Pool>,
    pub fake: Fake,
    pub events: Arc<Collect>,
}

impl PoolRig {
    pub fn new(workers: usize, fake: Fake) -> Self {
        let events = Arc::new(Collect::default());

        let holder = Arc::new(Holder::new(
            Arc::new(fake.clone()),
            std::path::PathBuf::new(),
            fake_manifest("fake@1").spec(),
            events.clone(),
        ));

        Self {
            pool: Arc::new(Pool::new(workers, holder)),
            fake,
            events,
        }
    }
}

/// A service over the fake engine with the fake model downloaded, and its events.
pub struct World {
    pub service: Arc<Service>,
    pub fake: Fake,
    pub events: Arc<Collect>,
    pub root: std::path::PathBuf,
    _dir: TempDir,
}

impl World {
    /// Options with the model installed; `tune` changes them before the service starts.
    pub fn with(workers: usize, tune: impl FnOnce(&mut Options)) -> Self {
        Self::build(workers, true, tune)
    }

    /// Options with no model in the data folder: `prepare` has to download it.
    pub fn empty(workers: usize, tune: impl FnOnce(&mut Options)) -> Self {
        Self::build(workers, false, tune)
    }

    fn build(workers: usize, installed: bool, tune: impl FnOnce(&mut Options)) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("data");
        std::fs::create_dir_all(&root).expect("data folder");

        let manifest = fake_manifest("fake@1");

        if installed {
            install(&root, &manifest);
        }

        let mut options = Options::new(root.clone(), manifest);
        options.workers = workers;
        tune(&mut options);

        let fake = Fake::new();
        let events = Arc::new(Collect::default());

        Self {
            service: Arc::new(Service::new(options, Arc::new(fake.clone()), events.clone())),
            fake,
            events,
            root,
            _dir: dir,
        }
    }

    /// A service that has prepared its model.
    pub fn ready(workers: usize) -> Self {
        let world = Self::with(workers, |_| {});
        world.service.prepare().expect("prepare");
        world
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.service.shutdown();
    }
}

/// A local HTTP server for the model files.
pub struct Fixture {
    port: u16,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub fail: Arc<AtomicBool>,
    pub files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl Fixture {
    pub fn serving(manifest: &Manifest) -> Self {
        let bytes = |name: &str| if name == ONNX_FILE { ONNX_BYTES } else { TOKENIZER_BYTES }.to_vec();
        let files = manifest
            .files
            .iter()
            .map(|file| (format!("/{}", file.name), bytes(file.name)))
            .collect();

        Self::with_files(files)
    }

    pub fn with_files(files: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fixture");
        let port = listener.local_addr().expect("address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(AtomicBool::new(false));
        let files = Arc::new(Mutex::new(files));

        let (seen, failing, served) = (Arc::clone(&requests), Arc::clone(&fail), Arc::clone(&files));

        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (seen, failing, served) = (Arc::clone(&seen), Arc::clone(&failing), Arc::clone(&served));
                std::thread::spawn(move || answer(stream, &seen, &failing, &served));
            }
        });

        Self {
            port,
            requests,
            fail,
            files,
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requested(&self) -> Vec<String> {
        locked(&self.requests).clone()
    }
}

fn answer(stream: TcpStream, seen: &Mutex<Vec<String>>, failing: &AtomicBool, files: &Mutex<HashMap<String, Vec<u8>>>) {
    let mut reader = BufReader::new(stream);
    let mut request = String::new();

    if reader.read_line(&mut request).is_err() {
        return;
    }

    let mut header = String::new();

    while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
        header.clear();
    }

    let path = request.split_whitespace().nth(1).unwrap_or_default().to_owned();
    locked(seen).push(path.clone());

    let body = locked(files).get(&path).cloned();
    let mut stream = reader.into_inner();

    let reply = match body {
        Some(body) if !failing.load(Ordering::SeqCst) => {
            let mut reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            reply.extend(body);
            reply
        }
        Some(_) => b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    };

    let _ = stream.write_all(&reply);
    let _ = stream.flush();
}
