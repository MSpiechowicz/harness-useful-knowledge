//! The worker threads that run the model, and the one model they share.
//!
//! [`Holder`] loads the engine when it is first needed and drops it after [`super::engine::IDLE`]
//! without a call. [`Pool`] runs `workers` threads over it: every call takes one text through the
//! engine (the engine is `Sync`, so the threads share one loaded plan and one tokenizer, and
//! the memory of the model is paid once). Queries are queued apart from passages and a free worker
//! always takes a query first, so a search never waits behind an import's queue, only for the
//! inference a worker already started.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::engine::{Embed, Kind, Loader};
use super::manifest::Spec;
use super::{Event, Events, locked};

/// Names the number of worker threads (1 to 8).
pub const THREADS_VAR: &str = "HARNESS_KNOWLEDGE_EMBED_THREADS";

/// The most workers the default picks, and the most an override may ask for.
const WORKERS_DEFAULT_MAX: usize = 4;
const WORKERS_MAX: usize = 8;

/// How many texts may wait for a worker: more is refused as busy, so abandoned requests cannot
/// pile up without end.
const QUERIES_QUEUED_MAX: usize = 64;
const PASSAGES_QUEUED_MAX: usize = 512;

/// How long a query may take, the load of the model included: far more than a client waits, so a
/// stuck model frees the connection thread that asked.
const QUERY_WAIT: Duration = Duration::from_secs(30);

/// The stack of a worker thread. The default 2 MiB is thin for a graph walk over 512 tokens.
const WORKER_STACK: usize = 8 * 1024 * 1024;

/// The number of workers: `override_` (a number from 1 to 8) when it is one, else a quarter of the
/// `cpus` (at least 1, at most 4), because one inference already runs several of tract's kernels
/// and the memory per worker is its activations only.
pub fn worker_count(cpus: usize, override_: Option<&str>) -> usize {
    let asked = override_
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|count| (1..=WORKERS_MAX).contains(count));

    asked.unwrap_or_else(|| (cpus / 4).clamp(1, WORKERS_DEFAULT_MAX))
}

/// [`worker_count`] for this computer and environment.
pub fn workers_here() -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);

    worker_count(cpus, std::env::var(THREADS_VAR).ok().as_deref())
}

struct Slot {
    engine: Option<Arc<dyn Embed>>,
    used: Instant,
}

/// The engine of the model folder, loaded when needed and dropped when idle.
pub struct Holder {
    loader: Arc<dyn Loader>,
    dir: PathBuf,
    spec: Spec,
    events: Arc<dyn Events>,
    slot: Mutex<Slot>,
    /// Held while the engine loads, so threads that need it meanwhile wait for the one load.
    loading: Mutex<()>,
}

impl Holder {
    pub fn new(loader: Arc<dyn Loader>, dir: PathBuf, spec: Spec, events: Arc<dyn Events>) -> Self {
        Self {
            loader,
            dir,
            spec,
            events,
            slot: Mutex::new(Slot {
                engine: None,
                used: Instant::now(),
            }),
            loading: Mutex::new(()),
        }
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    fn current(&self) -> Option<Arc<dyn Embed>> {
        let mut slot = locked(&self.slot);
        let engine = slot.engine.clone()?;
        slot.used = Instant::now();

        Some(engine)
    }

    /// The engine, loaded first (emitting `loading`) when it is not.
    pub fn ensure(&self) -> Result<Arc<dyn Embed>, String> {
        if let Some(engine) = self.current() {
            return Ok(engine);
        }

        let _loading = locked(&self.loading);

        // Another thread may have loaded it while this one waited.
        if let Some(engine) = self.current() {
            return Ok(engine);
        }

        self.events.emit(Event::Loading);
        let engine = self.loader.load(&self.dir, &self.spec)?;

        let mut slot = locked(&self.slot);
        slot.engine = Some(Arc::clone(&engine));
        slot.used = Instant::now();

        Ok(engine)
    }

    /// Starts the idle time again.
    pub fn touch(&self) {
        locked(&self.slot).used = Instant::now();
    }

    /// Drops the engine when it was last used `idle` ago or earlier and no thread is inside it;
    /// whether it did.
    pub fn unload_if_idle(&self, idle: Duration) -> bool {
        let mut slot = locked(&self.slot);

        let unused = slot
            .engine
            .as_ref()
            .is_some_and(|engine| Arc::strong_count(engine) == 1);

        if !unused || slot.used.elapsed() < idle {
            return false;
        }

        slot.engine = None;
        true
    }

    pub fn is_loaded(&self) -> bool {
        locked(&self.slot).engine.is_some()
    }
}

type Answer = (usize, Result<Vec<f32>, String>);

struct Job {
    kind: Kind,
    text: String,
    index: usize,
    reply: mpsc::Sender<Answer>,
    /// Set when nobody waits for the answer any more.
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct Queues {
    queries: VecDeque<Job>,
    passages: VecDeque<Job>,
    closed: bool,
}

struct Shared {
    holder: Arc<Holder>,
    queues: Mutex<Queues>,
    wake: Condvar,
}

/// Cancels the jobs of a request that has not been answered when the caller stops waiting.
struct Waiting {
    cancelled: Arc<AtomicBool>,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// The worker threads.
pub struct Pool {
    shared: Arc<Shared>,
    workers: usize,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Pool {
    /// Starts `workers` threads (at least 1). The model is loaded by the first call, not here.
    pub fn new(workers: usize, holder: Arc<Holder>) -> Self {
        let workers = workers.max(1);

        let shared = Arc::new(Shared {
            holder,
            queues: Mutex::new(Queues::default()),
            wake: Condvar::new(),
        });

        let threads = (0..workers)
            .filter_map(|number| {
                let shared = Arc::clone(&shared);

                std::thread::Builder::new()
                    .name(format!("embed-worker-{number}"))
                    .stack_size(WORKER_STACK)
                    .spawn(move || work(&shared))
                    .ok()
            })
            .collect();

        Self {
            shared,
            workers,
            threads: Mutex::new(threads),
        }
    }

    pub fn workers(&self) -> usize {
        self.workers
    }

    pub fn holder(&self) -> &Arc<Holder> {
        &self.shared.holder
    }

    /// The texts waiting for a worker: queries, passages.
    pub fn queued(&self) -> (usize, usize) {
        let queues = locked(&self.shared.queues);

        (queues.queries.len(), queues.passages.len())
    }

    /// The vector of a query. It goes before every queued passage.
    pub fn query(&self, text: &str) -> Result<Vec<f32>, String> {
        let mut vectors = self.run(Kind::Query, std::slice::from_ref(&text.to_owned()), Some(QUERY_WAIT))?;

        vectors.pop().ok_or_else(|| "no vector came back".to_owned())
    }

    /// The vectors of passages, in order, computed by all workers at once. Gives up after `wait`.
    pub fn passages(&self, texts: &[String], wait: Duration) -> Result<Vec<Vec<f32>>, String> {
        self.run(Kind::Passage, texts, Some(wait))
    }

    fn run(&self, kind: Kind, texts: &[String], wait: Option<Duration>) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let (sender, receiver) = mpsc::channel();
        let waiting = Waiting {
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        {
            let mut queues = locked(&self.shared.queues);

            if queues.closed {
                return Err("the embedder is stopping".to_owned());
            }

            let (queue, limit) = match kind {
                Kind::Query => (&mut queues.queries, QUERIES_QUEUED_MAX),
                Kind::Passage => (&mut queues.passages, PASSAGES_QUEUED_MAX),
            };

            if queue.len() + texts.len() > limit {
                return Err("the embedder is busy".to_owned());
            }

            for (index, text) in texts.iter().enumerate() {
                queue.push_back(Job {
                    kind,
                    text: text.clone(),
                    index,
                    reply: sender.clone(),
                    cancelled: Arc::clone(&waiting.cancelled),
                });
            }
        }

        drop(sender);
        self.shared.wake.notify_all();

        collect(&receiver, texts.len(), wait)
    }

    /// Stops the workers after the texts they are running, and drops what waits.
    pub fn stop(&self) {
        {
            let mut queues = locked(&self.shared.queues);
            queues.closed = true;
            queues.queries.clear();
            queues.passages.clear();
        }

        self.shared.wake.notify_all();

        for thread in std::mem::take(&mut *locked(&self.threads)) {
            let _ = thread.join();
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Waits for `count` answers and puts them in order. The first failure is the result.
fn collect(receiver: &mpsc::Receiver<Answer>, count: usize, wait: Option<Duration>) -> Result<Vec<Vec<f32>>, String> {
    let deadline = wait.map(|wait| Instant::now() + wait);
    let mut vectors: Vec<Option<Vec<f32>>> = vec![None; count];

    for _ in 0..count {
        let answer = match deadline {
            Some(deadline) => receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|error| match error {
                    mpsc::RecvTimeoutError::Timeout => "the embedder took too long".to_owned(),
                    mpsc::RecvTimeoutError::Disconnected => "the embedder is stopping".to_owned(),
                })?,
            None => receiver.recv().map_err(|_| "the embedder is stopping".to_owned())?,
        };

        let (index, result) = answer;
        vectors[index] = Some(result?);
    }

    vectors
        .into_iter()
        .map(|vector| vector.ok_or_else(|| "no vector came back".to_owned()))
        .collect()
}

/// One worker: takes a query when there is one, else a passage, until the pool closes.
fn work(shared: &Shared) {
    loop {
        let job = {
            let mut queues = locked(&shared.queues);

            loop {
                if queues.closed {
                    return;
                }

                if let Some(job) = queues.queries.pop_front().or_else(|| queues.passages.pop_front()) {
                    break job;
                }

                queues = shared.wake.wait(queues).unwrap_or_else(PoisonError::into_inner);
            }
        };

        if job.cancelled.load(Ordering::SeqCst) {
            continue;
        }

        // A panic inside the model fails this text, not the embedder.
        let result = catch_unwind(AssertUnwindSafe(|| {
            shared
                .holder
                .ensure()
                .and_then(|engine| engine.embed(job.kind, &job.text))
        }))
        .unwrap_or_else(|_| Err("the model stopped unexpectedly".to_owned()));

        shared.holder.touch();
        let _ = job.reply.send((job.index, result));
    }
}
