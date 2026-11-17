// Author: Nicola Antonello

//! A fake merchant that receives the events of `payments-core` (ADR-0054, ADR-0056).
//!
//! It shares no code with the server, only the wire format: a thin event (ADR-0049),
//! signed as Standard Webhooks `v1` under its endpoint's secret (ADR-0051). A
//! misunderstanding of that format then fails a test, instead of hiding in code that both
//! sides share. [`signature`] checks a delivery as a merchant's endpoint does, and
//! [`event`] reads its body.
//!
//! A test starts one fake of its own on a free local port, hands it the secret that
//! registering its endpoint answered, and scripts its answers through [`MockMerchant`]: a
//! standing answer, and answers aimed at one payment's events, which win over it. The
//! fake takes each event once, as an idempotent consumer does, and its handle returns
//! what arrived.

pub mod event;
mod server;
pub mod signature;

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use event::Event;
use signature::{Refusal, Secret, SecretError};

/// What a fake starts with.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where it listens. Port 0 picks a free one.
    pub address: SocketAddr,
    /// The secrets that it trusts from the start.
    pub secrets: Vec<Secret>,
}

impl Config {
    /// A fake for one test: on a free local port, trusting no secret until the test hands
    /// it one.
    #[must_use]
    pub fn for_tests() -> Self {
        Self {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            secrets: Vec::new(),
        }
    }
}

/// How the fake answers a delivery of a new event (ADR-0056).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// This status at once. A `2xx` takes the event.
    Status(StatusCode),
    /// This status at once, with `Retry-After` in seconds. A `2xx` takes the event.
    RetryAfter { status: StatusCode, seconds: u32 },
    /// A `200` after this long, the event taken when it arrived: an answer that comes after
    /// the relay's timeout, from a merchant that acted on the event.
    Delay(Duration),
}

impl Answer {
    /// Whether this answer takes the event.
    fn takes(self) -> bool {
        match self {
            Self::Status(status) | Self::RetryAfter { status, .. } => status.is_success(),
            Self::Delay(_) => true,
        }
    }
}

impl Default for Answer {
    /// The standing answer at the start.
    fn default() -> Self {
        Self::Status(StatusCode::OK)
    }
}

/// What the fake made of one arrival.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// It failed a check before its body was read, and was answered `400`.
    Refused(Refusal),
    /// It passed the checks, but its body is not one event of the documented shape, and
    /// was answered `400`.
    Malformed,
    /// It brought a new event, which the fake took, with this answer.
    Taken(Answer),
    /// It brought a new event, which the fake left, with this answer.
    Declined(Answer),
    /// It brought an event taken before, and was answered `200`.
    Copy,
}

impl Verdict {
    /// The answer that goes out.
    fn answer(self) -> Answer {
        match self {
            Self::Refused(_) | Self::Malformed => Answer::Status(StatusCode::BAD_REQUEST),
            Self::Copy => Answer::Status(StatusCode::OK),
            Self::Taken(answer) | Self::Declined(answer) => answer,
        }
    }
}

/// One delivery as it arrived.
#[derive(Debug, Clone)]
pub struct Arrival {
    /// When it arrived, by the fake's clock.
    pub at: OffsetDateTime,
    /// The `webhook-id` as it came, if it came once in visible ASCII.
    pub id: Option<String>,
    /// The `webhook-timestamp` as it came, if it came once in visible ASCII.
    pub timestamp: Option<String>,
    /// The body's bytes as they came.
    pub body: Bytes,
    /// The event, once the delivery passed the checks and its body was read.
    pub event: Option<Event>,
    pub verdict: Verdict,
}

/// An event that the fake took, once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taken {
    /// Its `webhook-id`.
    pub id: String,
    pub event: Event,
}

/// What the fake knows and has received.
struct State {
    secrets: Vec<Secret>,
    standing: Answer,
    aimed: HashMap<Uuid, Answer>,
    taken: HashSet<String>,
    events: Vec<Taken>,
    arrivals: Vec<Arrival>,
}

/// What the route shares with the handle.
pub(crate) struct Shared {
    state: Mutex<State>,
    /// The deliveries being answered now.
    answering: AtomicUsize,
    /// The most that were ever answered at once.
    most_at_once: AtomicUsize,
}

impl Shared {
    fn new(secrets: Vec<Secret>) -> Self {
        Self {
            state: Mutex::new(State {
                secrets,
                standing: Answer::default(),
                aimed: HashMap::new(),
                taken: HashSet::new(),
                events: Vec::new(),
                arrivals: Vec::new(),
            }),
            answering: AtomicUsize::new(0),
            most_at_once: AtomicUsize::new(0),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("no test panicked while it held the fake's records")
    }

    /// Counts one delivery as being answered, until the guard drops.
    pub(crate) fn answering(&self) -> Answering<'_> {
        let now = self.answering.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_at_once.fetch_max(now, Ordering::SeqCst);
        Answering(self)
    }

    /// Judges one delivery and records it, and returns the answer to send. It all happens
    /// here, before the route awaits anything, so a client that hangs up cannot undo it.
    pub(crate) fn receive(&self, headers: &HeaderMap, body: Bytes, at: OffsetDateTime) -> Answer {
        let id = signature::one(headers, signature::ID).map(str::to_owned);
        let timestamp = signature::one(headers, signature::TIMESTAMP).map(str::to_owned);
        let mut state = self.state();
        let (event, verdict) = match signature::verify(headers, &body, &state.secrets, at) {
            Err(refusal) => (None, Verdict::Refused(refusal)),
            Ok(()) => match Event::read(&body) {
                Err(_) => (None, Verdict::Malformed),
                Ok(event) => {
                    let id = id
                        .as_deref()
                        .expect("a delivery that passed the checks has its webhook-id");
                    let verdict = state.judge(id, &event);
                    (Some(event), verdict)
                }
            },
        };
        state.arrivals.push(Arrival {
            at,
            id,
            timestamp,
            body,
            event,
            verdict,
        });
        verdict.answer()
    }
}

impl State {
    /// What the fake does with an event that passed the checks, as an idempotent consumer:
    /// a copy of one taken before is answered `200` whatever the script; a new one gets the
    /// answer aimed at its payment, or the standing one, and is taken if that is a success.
    fn judge(&mut self, id: &str, event: &Event) -> Verdict {
        if self.taken.contains(id) {
            return Verdict::Copy;
        }
        let answer = self
            .aimed
            .get(&event.data.payment_id)
            .copied()
            .unwrap_or(self.standing);
        if !answer.takes() {
            return Verdict::Declined(answer);
        }
        self.taken.insert(id.to_owned());
        self.events.push(Taken {
            id: id.to_owned(),
            event: event.clone(),
        });
        Verdict::Taken(answer)
    }
}

/// One delivery being answered. Dropping it counts the delivery out, also when hyper drops
/// the route's future because its client hung up.
pub(crate) struct Answering<'a>(&'a Shared);

impl Drop for Answering<'_> {
    fn drop(&mut self) {
        self.0.answering.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A fake merchant served on a local port, and the handle that a test drives it through.
/// Dropping it stops the server, as [`MockMerchant::stop`] does, without waiting.
pub struct MockMerchant {
    shared: Arc<Shared>,
    url: String,
    /// Dropping it stops the server.
    stop: oneshot::Sender<()>,
    server: JoinHandle<()>,
}

impl MockMerchant {
    /// Starts a fake for one test: see [`Config::for_tests`].
    ///
    /// # Errors
    ///
    /// No local port could be bound.
    pub async fn start() -> io::Result<Self> {
        Self::start_with(Config::for_tests()).await
    }

    /// Starts a fake with this configuration.
    ///
    /// # Errors
    ///
    /// The address could not be bound.
    pub async fn start_with(config: Config) -> io::Result<Self> {
        let listener = TcpListener::bind(config.address).await?;
        let url = format!("http://{}/webhooks", listener.local_addr()?);
        let shared = Arc::new(Shared::new(config.secrets));
        let (stop, stopped) = oneshot::channel::<()>();
        let router = server::router(Arc::clone(&shared));
        let server = tokio::spawn(async move {
            // The handle's sender drops when the handle stops or drops, which ends this
            // wait. The server then takes no new connection, closes each open one once its
            // answer is sent, and ends.
            let _ended = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _dropped = stopped.await;
                })
                .await;
        });
        Ok(Self {
            shared,
            url,
            stop,
            server,
        })
    }

    /// Where the relay delivers: `http://127.0.0.1:<port>/webhooks`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Trusts this secret from now on, written `whsec_` and base64: the one that
    /// registering the endpoint answered (ADR-0050).
    ///
    /// # Errors
    ///
    /// A secret written otherwise: see [`SecretError`].
    pub fn trust(&self, written: &str) -> Result<(), SecretError> {
        let secret = Secret::parse(written)?;
        self.shared.state().secrets.push(secret);
        Ok(())
    }

    /// Answers each new event this way from now on, unless an answer is aimed at its
    /// payment.
    pub fn answer(&self, answer: Answer) {
        self.shared.state().standing = answer;
    }

    /// Answers each new event about this payment this way from now on, over the standing
    /// answer.
    pub fn answer_payment(&self, payment_id: Uuid, answer: Answer) {
        self.shared.state().aimed.insert(payment_id, answer);
    }

    /// Every delivery that reached the route, in the order of arrival.
    #[must_use]
    pub fn arrivals(&self) -> Vec<Arrival> {
        self.shared.state().arrivals.clone()
    }

    /// The events taken, each once, in the order taken.
    #[must_use]
    pub fn events(&self) -> Vec<Taken> {
        self.shared.state().events.clone()
    }

    /// The most deliveries that the fake was answering at once.
    #[must_use]
    pub fn most_at_once(&self) -> usize {
        self.shared.most_at_once.load(Ordering::SeqCst)
    }

    /// Stops the server and waits until it is gone: a dead endpoint, which takes no new
    /// connection and has closed each open one.
    ///
    /// # Panics
    ///
    /// If the server's task panicked.
    pub async fn stop(self) {
        let Self { stop, server, .. } = self;
        drop(stop);
        server
            .await
            .expect("the server's task ends without a panic");
    }
}
