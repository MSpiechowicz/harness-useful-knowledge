//! A small blocking client for the libSQL server's HTTP pipeline (`POST /v2/pipeline`, Hrana over
//! HTTP).
//!
//! Each request carries a list of stream requests. A [`Stream`] that keeps its baton stays on one
//! server connection across requests, which is what an interactive transaction needs; a one-shot
//! stream closes in the same request, so every statement runs on its own. Error messages carry the
//! server's error code (only when it looks like one, `[A-Z_]{1,40}`) or the HTTP status: never the
//! URL, the token, a statement, stored text or a message the server wrote. The one exception is
//! [`wire::Failure::problem`](super::wire::Failure::problem), which reads the server's message to tell a missing capability from any
//! other failure and then reports a fixed text.
//!
//! Proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`) are ignored: requests always go
//! straight to the server, so the token never passes through a proxy, in clear for `http://`.
//!
//! The HTTP agent keeps its connections alive and opens a new one when a kept one went stale. A
//! one-shot stream (reads only) also repeats a request once when the connection broke before any
//! reply, so a restarted server does not fail the first read after it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use ureq::Agent;
use ureq::Timeout;
use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};

use super::endpoint;
use super::sql::{Executed, Row, Sql, Value};
use super::wire::{Cell, Outcome, Reply, Response, StatementResult, backend, execute, execute_request, unexpected};
use crate::store::StoreError;

/// How long one HTTP request may take, waiting for the write lock included (the server queues a
/// writer for about 5 s before it takes the lock from an idle transaction).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long connecting may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest reply read; every query is bounded far below it.
const REPLY_MAX: u64 = 16 * 1024 * 1024;

/// Statements sent in one request by [`Sql::execute_many`].
const MANY_PER_REQUEST: usize = 64;

/// The HTTP side: where requests go and the credentials they carry.
pub(super) struct Client {
    agent: Agent,
    base: String,
    authorization: Option<String>,
}

impl Client {
    /// A client for the server at `base` ([`endpoint::parse`]d). TLS uses the ring provider and
    /// the platform's trusted roots; redirects are not followed and proxy variables are ignored,
    /// so the token goes to the configured host only.
    pub(super) fn new(base: String, token: Option<&str>) -> Result<Self, StoreError> {
        Self::within(base, token, CONNECT_TIMEOUT)
    }

    /// [`Client::new`] giving up on a connection after `connect_timeout`.
    pub(super) fn within(base: String, token: Option<&str>, connect_timeout: Duration) -> Result<Self, StoreError> {
        Self::with_timeouts(base, token, connect_timeout, REQUEST_TIMEOUT)
    }

    /// [`Client::within`] with `request_timeout` as the most a whole request may take.
    pub(super) fn with_timeouts(
        base: String,
        token: Option<&str>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, StoreError> {
        let authorization = match token.map(str::trim).filter(|token| !token.is_empty()) {
            Some(token) if token.bytes().all(|byte| byte.is_ascii_graphic()) => Some(format!("Bearer {token}")),
            Some(_) => {
                return Err(StoreError::Invalid(
                    "the libSQL token must be printable ASCII".to_owned(),
                ));
            }
            None => None,
        };

        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .root_certs(platform_roots())
            .build();

        let agent = Agent::config_builder()
            .tls_config(tls)
            .timeout_global(Some(request_timeout))
            .timeout_connect(Some(connect_timeout))
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .user_agent(concat!("harness-knowledge/", env!("HARNESS_KNOWLEDGE_VERSION")))
            .build()
            .new_agent();

        Ok(Self {
            agent,
            base,
            authorization,
        })
    }

    /// A stream where every statement runs on its own: for reads.
    pub(super) fn single(&self) -> Stream<'_> {
        Stream {
            client: self,
            keep: false,
            baton: None,
            base: None,
            closed: false,
        }
    }

    /// A stream that stays on one server connection until [`Stream::commit`].
    pub(super) fn stream(&self) -> Stream<'_> {
        Stream {
            keep: true,
            ..self.single()
        }
    }

    /// The base URL a stream's later requests use when the server names one (`base_url`): only
    /// where [`endpoint::may_follow`] allows, so the token goes nowhere else.
    fn follow(&self, url: &str) -> Result<String, Failed> {
        let moved = || {
            Failed::local(backend(
                "the libSQL server sent the stream to another address".to_owned(),
            ))
        };
        let base = endpoint::parse(url).map_err(|_| moved())?;

        if !endpoint::may_follow(&self.base, &base) {
            return Err(moved());
        }

        Ok(base)
    }

    fn post(&self, base: &str, baton: Option<&str>, requests: Vec<serde_json::Value>) -> Result<Reply, Failed> {
        let body = json!({ "baton": baton, "requests": requests }).to_string();
        let mut request = self
            .agent
            .post(format!("{base}/v2/pipeline"))
            .header("content-type", "application/json");

        if let Some(authorization) = &self.authorization {
            request = request.header("authorization", authorization);
        }

        let mut response = request.send(body).map_err(Failed::unanswered)?;
        let status = response.status().as_u16();

        let (kind, refused) = match status {
            200 => (Kind::Other, None),
            401 | 403 => (
                Kind::Auth,
                Some(format!("the libSQL server refused the token (HTTP {status})")),
            ),
            404 => (
                Kind::Other,
                Some("no libSQL pipeline at this URL (HTTP 404)".to_owned()),
            ),
            _ => (Kind::Other, Some(format!("the libSQL server answered HTTP {status}"))),
        };

        if let Some(message) = refused {
            // A gateway can answer 5xx after the server ran the request.
            return Err(Failed {
                error: backend(message),
                maybe_ran: status >= 500,
                kind,
            });
        }

        let bytes = response
            .body_mut()
            .with_config()
            .limit(REPLY_MAX)
            .read_to_vec()
            .map_err(Failed::unanswered)?;

        serde_json::from_slice(&bytes).map_err(|_| Failed {
            error: unexpected(),
            maybe_ran: true,
            kind: Kind::Other,
        })
    }
}

/// What kind of trouble a request ran into, for the capability check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// HTTP 401 or 403: the token is wrong or missing.
    Auth,
    /// No reply: the host, the port, the network or TLS. `stale` when a kept connection broke
    /// before any reply, so one more attempt may succeed.
    Unreachable {
        stale: bool,
    },
    Other,
}

/// A request that got no usable reply.
pub(super) struct Failed {
    pub(super) error: StoreError,
    /// The server may have run the request: it may have arrived, but no reply was read.
    maybe_ran: bool,
    pub(super) kind: Kind,
}

impl Failed {
    /// A failure decided before the request could run.
    fn local(error: StoreError) -> Self {
        Self {
            error,
            maybe_ran: false,
            kind: Kind::Other,
        }
    }

    /// A transport failure: one before the connection opened cannot have run the request.
    fn unanswered(error: ureq::Error) -> Self {
        let before_sending = matches!(
            error,
            ureq::Error::HostNotFound
                | ureq::Error::ConnectionFailed
                | ureq::Error::Timeout(Timeout::Resolve | Timeout::Connect)
        );
        let stale = matches!(&error, ureq::Error::ConnectionFailed)
            || matches!(&error, ureq::Error::Io(io) if io.get_ref().is_none());

        Self {
            error: transport(error),
            maybe_ran: !before_sending,
            kind: Kind::Unreachable { stale },
        }
    }
}

/// One Hrana stream. See the module docs.
pub(super) struct Stream<'c> {
    client: &'c Client,
    /// Keep the stream open between requests.
    keep: bool,
    baton: Option<String>,
    /// The URL the server asked this stream's later requests to use.
    base: Option<String>,
    /// The server ended a kept stream; its transaction is gone.
    closed: bool,
}

impl Stream<'_> {
    /// Sends `requests` (plus a close for a one-shot stream); one outcome per request.
    pub(super) fn send(&mut self, requests: Vec<serde_json::Value>) -> Result<Vec<Outcome>, StoreError> {
        self.exchange(requests).map_err(|failed| failed.error)
    }

    /// [`Self::send`], telling what kind of trouble a failure was.
    pub(super) fn exchange(&mut self, requests: Vec<serde_json::Value>) -> Result<Vec<Outcome>, Failed> {
        // A kept stream is a transaction: it is never repeated.
        if self.keep {
            return self.attempt(requests);
        }

        // A one-shot stream only reads, so repeating it once is safe.
        match self.attempt(requests.clone()) {
            Err(Failed {
                kind: Kind::Unreachable { stale: true },
                ..
            }) => self.attempt(requests),
            other => other,
        }
    }

    fn attempt(&mut self, mut requests: Vec<serde_json::Value>) -> Result<Vec<Outcome>, Failed> {
        if self.closed {
            return Err(Failed::local(ended()));
        }

        let count = requests.len();
        if !self.keep {
            requests.push(json!({ "type": "close" }));
        }

        let base = self.base.as_deref().unwrap_or(&self.client.base);
        let reply = match self.client.post(base, self.baton.as_deref(), requests) {
            Ok(reply) => reply,
            Err(error) => {
                self.closed = self.keep;
                return Err(error);
            }
        };

        // Only a kept stream sends more requests.
        if let Some(url) = reply.base_url.filter(|_| self.keep) {
            self.base = Some(self.client.follow(&url)?);
        }

        self.closed = self.keep && reply.baton.is_none();
        self.baton = reply.baton.filter(|_| self.keep);

        let mut results = reply.results;
        if results.len() < count {
            return Err(Failed {
                error: unexpected(),
                maybe_ran: true,
                kind: Kind::Other,
            });
        }

        results.truncate(count);
        Ok(results)
    }

    /// Runs one statement and returns its result.
    fn statement(&mut self, sql: &str, args: &[Value], want_rows: bool) -> Result<StatementResult, StoreError> {
        match self
            .send(vec![execute_request(sql, args, want_rows)])?
            .remove(0)
            .into_result()?
        {
            Response::Execute { result } => Ok(result),
            Response::Other => Err(unexpected()),
        }
    }

    /// Commits a kept stream's transaction and closes the stream in the same request. When the
    /// request may have reached the server but no answer came back, the commit may have run: the
    /// error says so, and nothing sends the write again.
    pub(super) fn commit(self) -> Result<(), StoreError> {
        match self.finish("COMMIT") {
            Ok(()) => Ok(()),
            Err(Failed {
                error: StoreError::Backend(reason),
                maybe_ran: true,
                ..
            }) => Err(backend(format!(
                "the libSQL server did not confirm the commit ({reason}), so the write may have been \
                 applied: read it back before repeating it"
            ))),
            Err(failed) => Err(failed.error),
        }
    }

    /// Rolls back a kept stream's transaction and closes the stream in the same request.
    pub(super) fn rollback(self) -> Result<(), StoreError> {
        self.finish("ROLLBACK").map_err(|failed| failed.error)
    }

    /// Ends a kept stream with `sql` and closes it in the same request.
    fn finish(mut self, sql: &str) -> Result<(), Failed> {
        if self.baton.is_none() {
            return Err(Failed::local(ended()));
        }

        self.keep = false;
        let outcome = self.attempt(vec![execute(sql)])?.remove(0);
        outcome.into_result().map(drop).map_err(Failed::local)
    }

    /// Closes a kept stream (the server rolls back an open transaction); errors are ignored, as
    /// the server also drops a stream nobody uses.
    pub(super) fn close(mut self) {
        if self.baton.is_some() {
            self.keep = false;
            self.send(Vec::new()).ok();
        }
    }
}

impl Sql for Stream<'_> {
    fn query(&mut self, sql: &str, args: &[Value]) -> Result<Vec<Row>, StoreError> {
        self.statement(sql, args, true)?
            .rows
            .into_iter()
            .map(|cells| {
                cells
                    .into_iter()
                    .map(Cell::into_value)
                    .collect::<Result<_, _>>()
                    .map(Row::new)
            })
            .collect()
    }

    fn execute(&mut self, sql: &str, args: &[Value]) -> Result<Executed, StoreError> {
        let result = self.statement(sql, args, false)?;

        Ok(Executed {
            changes: usize::try_from(result.affected_row_count).unwrap_or(usize::MAX),
            last_insert_rowid: result.last_insert_rowid.and_then(|id| id.parse().ok()).unwrap_or(0),
        })
    }

    fn execute_many(&mut self, sql: &str, rows: &[Vec<Value>]) -> Result<(), StoreError> {
        for group in rows.chunks(MANY_PER_REQUEST) {
            let requests = group.iter().map(|args| execute_request(sql, args, false)).collect();

            // The server runs every request of the group even after one failed, so all the
            // outcomes are read and the first failure is the one reported.
            let outcomes = self.send(requests)?;
            outcomes
                .into_iter()
                .try_for_each(|outcome| outcome.into_result().map(drop))?;
        }

        Ok(())
    }

    fn batch(&mut self, sql: &str) -> Result<(), StoreError> {
        let outcome = self.send(vec![json!({ "type": "sequence", "sql": sql })])?.remove(0);
        outcome.into_result().map(drop)
    }
}

/// The platform's trusted roots (an empty set when none load: every TLS connection then fails
/// with a certificate error instead of trusting anything).
fn platform_roots() -> RootCerts {
    let loaded = rustls_native_certs::load_native_certs();
    let certs: Vec<Certificate<'static>> = loaded
        .certs
        .iter()
        .map(|cert| Certificate::from_der(cert.as_ref()).to_owned())
        .collect();

    RootCerts::new_with_certs(&certs)
}

/// A transport failure, described without the URL: [`StoreError::Unreachable`] when the server
/// could not be reached (a connection that took too long, a host that does not resolve, a refused or
/// lost connection), [`StoreError::Backend`] for anything the TLS check refused or the reply broke.
/// A request that the server took too long to answer is a failure of that request, not an
/// unreachable server: a slow statement must not send the routed store to this computer.
fn transport(error: ureq::Error) -> StoreError {
    let (reason, reachable) = match error {
        ureq::Error::Timeout(Timeout::Resolve | Timeout::Connect) => {
            ("the libSQL server did not answer in time".to_owned(), false)
        }
        ureq::Error::Timeout(_) => ("the libSQL server took too long to answer a request".to_owned(), true),
        ureq::Error::HostNotFound => ("the libSQL host was not found".to_owned(), false),
        ureq::Error::ConnectionFailed => ("cannot connect to the libSQL server".to_owned(), false),
        ureq::Error::Io(error) => match error.get_ref().and_then(|inner| inner.downcast_ref::<rustls::Error>()) {
            Some(tls) => (format!("TLS with the libSQL server failed ({tls})"), true),
            None => (format!("cannot reach the libSQL server ({})", error.kind()), false),
        },
        ureq::Error::Rustls(error) => (format!("TLS with the libSQL server failed ({error})"), true),
        ureq::Error::Tls(reason) => (format!("TLS with the libSQL server failed ({reason})"), true),
        ureq::Error::BodyExceedsLimit(_) => ("the libSQL server's reply is too large".to_owned(), true),
        _ => ("the request to the libSQL server failed".to_owned(), true),
    };

    if reachable {
        backend(reason)
    } else {
        StoreError::Unreachable(reason)
    }
}

fn ended() -> StoreError {
    backend("the libSQL server ended the transaction".to_owned())
}
