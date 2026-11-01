//! The embedder without its socket: the model's download and load, the worker pool, and the
//! answer to each request. Tests drive it with a fake engine.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use super::engine::{Loader, TEXT_MAX};
use super::pool::{Holder, Pool};
use super::protocol::{PASSAGES_MAX, Request, error_reply};
use super::{Event, Events, Options, Stop, model};

/// How long a request for passages waits for its vectors, a little under the client's timeout so
/// the answer is an error rather than silence.
const PASSAGES_WAIT: Duration = Duration::from_secs(115);

/// The embedder without its socket.
pub struct Service {
    opts: Options,
    events: Arc<dyn Events>,
    pool: Pool,
    stop: Stop,
    /// The model is downloaded and has loaded once.
    ready: AtomicBool,
}

impl Service {
    /// Starts the worker threads. The model is neither downloaded nor loaded yet: [`Service::prepare`]
    /// does that.
    pub fn new(opts: Options, loader: Arc<dyn Loader>, events: Arc<dyn Events>) -> Self {
        let holder = Arc::new(Holder::new(
            loader,
            model::model_dir(&opts.root),
            opts.manifest.spec(),
            Arc::clone(&events),
        ));

        Self {
            pool: Pool::new(opts.workers, holder),
            opts,
            events,
            stop: Stop::default(),
            ready: AtomicBool::new(false),
        }
    }

    /// The stop flag every loop of the service watches.
    pub fn stop(&self) -> &Stop {
        &self.stop
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// The model that answers, once it is ready.
    pub fn active_model(&self) -> Option<String> {
        self.is_ready().then(|| self.opts.manifest.id.to_owned())
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// Downloads the model when it is missing, loads it and emits `ready`.
    pub fn prepare(&self) -> Result<(), String> {
        let stopped = || self.stop.is_stopped();

        model::ensure(
            &self.opts.root,
            &self.opts.manifest,
            self.opts.base_url.as_deref(),
            self.events.as_ref(),
            &stopped,
        )
        .map_err(|error| error.to_string())?;

        self.pool.holder().ensure()?;
        self.ready.store(true, Ordering::SeqCst);

        self.events.emit(Event::Ready {
            model: self.opts.manifest.id.to_owned(),
            dim: self.opts.manifest.dims,
        });

        Ok(())
    }

    /// Answers one request. The reply is the JSON line to send.
    pub fn handle(&self, request: Request) -> Value {
        match request {
            Request::Ping => {
                let model = self.active_model();
                json!({ "ok": true, "ready": model.is_some(), "model": model })
            }
            // The load itself follows the reply (see [`Service::warm`]).
            Request::Warm => json!({ "ok": true }),
            Request::EmbedQuery { text } => self.embed_query(&text),
            Request::EmbedPassages { texts } => self.embed_passages(&texts),
            Request::Status => self.status(),
        }
    }

    /// Loads the model when it was dropped for being idle, and starts its idle time again; waits
    /// for the load, so the caller of a `warm` request runs it after its reply. A query that comes
    /// meanwhile waits for the model like it would have loaded it.
    pub fn warm(&self) {
        if self.is_ready() {
            let _ = self.pool.holder().ensure();
        }
    }

    fn status(&self) -> Value {
        let (queries, passages) = self.pool.queued();

        json!({
            "ok": true,
            "ready": self.is_ready(),
            "loaded": self.pool.holder().is_loaded(),
            "model": self.active_model(),
            "dim": self.opts.manifest.dims,
            "workers": self.pool.workers(),
            "queued_queries": queries,
            "queued_passages": passages,
        })
    }

    fn embed_query(&self, text: &str) -> Value {
        if !self.is_ready() {
            return error_reply("the model is not ready");
        }

        if text.trim().is_empty() {
            return error_reply("the text is empty");
        }

        match self.pool.query(text) {
            Ok(vector) => json!({ "ok": true, "model": self.opts.manifest.id, "vector": vector }),
            Err(error) => error_reply(&error),
        }
    }

    fn embed_passages(&self, texts: &[String]) -> Value {
        if !self.is_ready() {
            return error_reply("the model is not ready");
        }

        if texts.is_empty() || texts.len() > PASSAGES_MAX {
            return error_reply(&format!("send between 1 and {PASSAGES_MAX} texts"));
        }

        if let Some(position) = texts.iter().position(|text| text.trim().is_empty()) {
            return error_reply(&format!("text {position} is empty"));
        }

        let clipped: Vec<String> = texts.iter().map(|text| text.chars().take(TEXT_MAX).collect()).collect();

        match self.pool.passages(&clipped, PASSAGES_WAIT) {
            Ok(vectors) => json!({ "ok": true, "model": self.opts.manifest.id, "vectors": vectors }),
            Err(error) => error_reply(&error),
        }
    }

    /// Drops the engine after the idle time; emits `idle` when it did.
    fn idle_loop(&self) {
        let pause = (self.opts.idle / 2).clamp(Duration::from_millis(100), Duration::from_secs(30));

        while !self.stop.wait(pause) {
            if self.pool.holder().unload_if_idle(self.opts.idle) {
                self.events.emit(Event::Idle);
                nudge_allocator();
            }
        }
    }

    /// The life of the embedder after its socket is up: get the model ready (trying again every
    /// retry interval after a failure), then unload it when idle. Returns when stopped.
    pub fn lifecycle(&self) {
        loop {
            match self.prepare() {
                Ok(()) => break,
                Err(message) => {
                    if self.stop.is_stopped() {
                        return;
                    }

                    self.events.emit(Event::Error { message });

                    if self.stop.wait(self.opts.retry) {
                        return;
                    }
                }
            }
        }

        self.idle_loop();
    }

    /// Stops the worker threads (after the texts they are running).
    pub fn shutdown(&self) {
        self.stop.stop();
        self.pool.stop();
    }
}

/// Gives the allocator the activity it needs to hand freed memory back. mimalloc purges what was
/// freed after a short delay (10 ms) but only when the process allocates again, and an idle
/// embedder does not: so after the model is dropped, a few large allocations spread over a
/// second let the purge run.
fn nudge_allocator() {
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(100));
        std::hint::black_box(vec![0_u8; 1 << 20]);
    }
}
