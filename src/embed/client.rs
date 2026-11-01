//! What `harness-knowledge mcp`, `recall` and the importer call to ask the embedder, which may not
//! run: every call fails fast (the socket is missing or refuses) or within its timeout, and the
//! caller falls back to keyword results or reports that embedding is unavailable.
//!
//! The timeout of a call covers all of it (connect, request, reply) and is kept by waiting for
//! the exchange, which runs on a thread of its own, for that long: the operating system's socket
//! timeouts alone would not do (Windows named pipes have none). A thread that outlives its call
//! is abandoned, and ends when the embedder answers or closes the connection (on Unix at the
//! socket timeout). At most [`ABANDONED_MAX`] abandoned calls run at once, then calls fail fast.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use super::engine::TEXT_MAX;
use super::protocol::{MAX_REQUEST, PASSAGES_MAX, Pong, Reply, Request, Status};
use super::server::Endpoint;

/// How long a query may take: room for the model to load when a query is the first call after the
/// embedder unloaded it (about two seconds), though [`EmbedderClient::warm`] normally has it
/// loaded already.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(4);

/// How long a ping or a status may take.
pub const PING_TIMEOUT: Duration = Duration::from_millis(500);

/// How long a warm request may take: the embedder answers it before it loads anything.
pub const WARM_TIMEOUT: Duration = Duration::from_millis(250);

/// How long one request of up to [`PASSAGES_MAX`] passages may take: 64 texts of 512 tokens on
/// one worker take about 40 seconds, and a busy embedder serves queries first.
pub const PASSAGES_TIMEOUT: Duration = Duration::from_secs(120);

/// The longest reply read: 64 vectors of 384 numbers at about 15 bytes each, with room to spare.
const MAX_REPLY: u64 = 1024 * 1024;

/// The most bytes of texts one passages request carries, so its line stays under [`MAX_REQUEST`]
/// even with the escapes.
const PASSAGES_BYTES: usize = MAX_REQUEST / 2;

/// Why a call to the embedder failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The embedder is not running or did not answer in time.
    #[error("the embedder is not reachable ({0})")]
    Unreachable(String),
    /// The embedder answered with an error.
    #[error("the embedder refused: {0}")]
    Refused(String),
}

/// A client of the embedder of one data folder.
#[derive(Debug, Clone)]
pub struct EmbedderClient {
    endpoint: Endpoint,
}

impl EmbedderClient {
    /// A client for the embedder of the data folder `root` (`<data>/harness-knowledge`); `temp` is
    /// the temporary folder a long socket path falls back to.
    pub fn new(root: &Path, temp: &Path) -> Self {
        Self {
            endpoint: Endpoint::of(root, temp),
        }
    }

    pub fn from_endpoint(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    /// Whether the embedder answers texts, and with which model.
    pub fn ping(&self) -> Result<Pong, ClientError> {
        let reply = self.call(&Request::Ping, PING_TIMEOUT)?;

        Ok(Pong {
            model: reply.model,
            ready: reply.ready,
        })
    }

    /// What the embedder reports about itself.
    pub fn status(&self) -> Result<Status, ClientError> {
        self.call(&Request::Status, PING_TIMEOUT).map(|reply| reply.status())
    }

    /// Asks the embedder to load its model now if it unloaded it, best effort: the answer comes
    /// at once and the load goes on in the embedder.
    pub fn warm(&self) -> Result<(), ClientError> {
        self.call(&Request::Warm, WARM_TIMEOUT).map(drop)
    }

    /// The vector of a search query. It is answered before any passage the embedder has queued.
    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>, ClientError> {
        let request = Request::EmbedQuery { text: clip(text) };
        let reply = self.call(&request, QUERY_TIMEOUT)?;

        if reply.vector.is_empty() {
            return Err(ClientError::Unreachable("an unreadable reply".to_owned()));
        }

        Ok(reply.vector)
    }

    /// The vectors of passages, in order. They are sent in as few requests as fit.
    pub fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ClientError> {
        let mut vectors = Vec::with_capacity(texts.len());

        for batch in batches(texts) {
            let request = Request::EmbedPassages { texts: batch.clone() };
            let reply = self.call(&request, PASSAGES_TIMEOUT)?;

            let fine = reply.vectors.len() == batch.len() && reply.vectors.iter().all(|vector| !vector.is_empty());

            if !fine {
                return Err(ClientError::Unreachable("an unreadable reply".to_owned()));
            }

            vectors.extend(reply.vectors);
        }

        Ok(vectors)
    }

    fn call(&self, request: &Request, timeout: Duration) -> Result<Reply, ClientError> {
        let line = request.to_line();

        if line.len() >= MAX_REQUEST {
            return Err(ClientError::Refused("the request is too large".to_owned()));
        }

        let endpoint = self.endpoint.clone();

        let answer = within(timeout, move || exchange(&endpoint, &line, timeout))
            .ok_or_else(|| ClientError::Unreachable(std::io::ErrorKind::TimedOut.to_string()))?
            .map_err(|error| ClientError::Unreachable(error.kind().to_string()))?;

        let reply: Reply = serde_json::from_str(answer.trim_end())
            .map_err(|_| ClientError::Unreachable("an unreadable reply".to_owned()))?;

        if !reply.ok {
            return Err(ClientError::Refused(reply.error.unwrap_or_default()));
        }

        Ok(reply)
    }
}

/// `text` cut to what the embedder tokenizes.
fn clip(text: &str) -> String {
    text.chars().take(TEXT_MAX).collect()
}

/// The texts in requests of at most [`PASSAGES_MAX`] texts and about [`PASSAGES_BYTES`] bytes,
/// each text cut to what the embedder tokenizes.
fn batches(texts: &[String]) -> Vec<Vec<String>> {
    let mut batches: Vec<Vec<String>> = Vec::new();
    let mut bytes = 0;

    for text in texts {
        let text = clip(text);
        let size = serde_json::to_string(&text).map_or(text.len(), |json| json.len());

        let full = batches
            .last()
            .is_none_or(|batch| batch.len() >= PASSAGES_MAX || bytes + size > PASSAGES_BYTES);

        if full {
            batches.push(Vec::new());
            bytes = 0;
        }

        bytes += size;

        if let Some(batch) = batches.last_mut() {
            batch.push(text);
        }
    }

    batches
}

/// Sends `line` to the embedder and reads its one line of reply.
fn exchange(endpoint: &Endpoint, line: &str, timeout: Duration) -> std::io::Result<String> {
    let mut stream = endpoint.connect(timeout)?;

    writeln!(stream, "{line}")?;
    stream.flush()?;

    let mut answer = String::new();
    BufReader::new(&stream).take(MAX_REPLY).read_line(&mut answer)?;

    Ok(answer)
}

/// How many calls given up on may still be running in this process. Above it a new call fails at
/// once, so an embedder that holds connections without answering cannot make the process grow
/// a thread for every call.
const ABANDONED_MAX: usize = 8;

/// The calls given up on whose thread has not ended yet.
static ABANDONED: AtomicUsize = AtomicUsize::new(0);

/// The result of `work`, run on a thread of its own; `None` when it did not finish within
/// `timeout` (the thread is left to end by itself), when [`ABANDONED_MAX`] earlier calls are still
/// running, or when it could not be started.
fn within<T: Send + 'static>(timeout: Duration, work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    within_limited(&ABANDONED, ABANDONED_MAX, timeout, work)
}

/// Dropped when the thread of a call ends (also when it panics): if the caller gave up on it
/// before, it is no longer running.
struct Ending {
    settled: Arc<AtomicBool>,
    abandoned: &'static AtomicUsize,
}

impl Drop for Ending {
    fn drop(&mut self) {
        if self.settled.swap(true, Ordering::SeqCst) {
            self.abandoned.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn within_limited<T: Send + 'static>(
    abandoned: &'static AtomicUsize,
    limit: usize,
    timeout: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    if abandoned.load(Ordering::SeqCst) >= limit {
        return None;
    }

    let (sender, receiver) = mpsc::channel();

    // Whichever of the caller (giving up) and the thread (ending) comes second settles the count.
    let settled = Arc::new(AtomicBool::new(false));
    let ending = Ending {
        settled: Arc::clone(&settled),
        abandoned,
    };

    std::thread::Builder::new()
        .name("embedder-call".to_owned())
        .spawn(move || {
            let _ending = ending;
            let _ = sender.send(work());
        })
        .ok()?;

    let result = receiver.recv_timeout(timeout).ok();

    if result.is_none() {
        // Counted before the hand-over, so the thread's `fetch_sub` never runs ahead of it and
        // the count never wraps; undone when the thread had already ended.
        abandoned.fetch_add(1, Ordering::SeqCst);

        if settled.swap(true, Ordering::SeqCst) {
            abandoned.fetch_sub(1, Ordering::SeqCst);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_that_outlasts_its_timeout_is_given_up_on_at_the_timeout() {
        let started = std::time::Instant::now();
        let finished = within(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(3));
            1
        });

        assert_eq!(finished, None);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        assert_eq!(within(Duration::from_secs(5), || 7), Some(7));
    }

    /// A hung embedder costs a process a bounded number of threads: past the limit calls fail
    /// at once, and the count comes back down as the abandoned threads end.
    #[test]
    fn calls_given_up_on_are_capped_and_the_cap_lifts_when_they_end() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let hang = || std::thread::sleep(Duration::from_millis(600));

        for _ in 0..3 {
            assert_eq!(within_limited(&COUNT, 3, Duration::from_millis(20), hang), None);
        }

        let started = std::time::Instant::now();
        let refused = within_limited(&COUNT, 3, Duration::from_secs(5), || 1);

        assert_eq!(refused, None);
        // "at once" against the 5 s timeout: Windows CI runners are slow enough to miss a tight bound
        let at_once = if cfg!(windows) {
            Duration::from_secs(2)
        } else {
            Duration::from_millis(300)
        };
        assert!(started.elapsed() < at_once, "{:?}", started.elapsed());

        // the abandoned 600 ms calls have ended by then, with room for a slow runner
        std::thread::sleep(if cfg!(windows) {
            Duration::from_secs(3)
        } else {
            Duration::from_millis(900)
        });

        assert_eq!(within_limited(&COUNT, 3, Duration::from_secs(5), || 2), Some(2));
        assert_eq!(COUNT.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn passages_go_in_requests_of_at_most_64_texts_and_a_bounded_size() {
        let texts: Vec<String> = (0..150).map(|number| format!("passage {number}")).collect();
        let sent = batches(&texts);

        assert_eq!(sent.iter().map(Vec::len).collect::<Vec<_>>(), vec![64, 64, 22]);
        assert_eq!(sent.concat(), texts);

        let long: Vec<String> = (0..64).map(|_| "ż".repeat(TEXT_MAX * 2)).collect();
        let sent = batches(&long);

        assert!(sent.len() > 1, "64 long texts do not fit one request");

        for batch in &sent {
            let line = Request::EmbedPassages { texts: batch.clone() }.to_line();

            assert!(line.len() < MAX_REQUEST, "{}", line.len());
            assert!(batch.iter().all(|text| text.chars().count() == TEXT_MAX));
        }

        assert!(batches(&[]).is_empty());
    }
}
