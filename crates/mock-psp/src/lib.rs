// Author: Nicola Antonello

//! A fake payment service provider (PSP) that the tests of `payments-core` drive
//! (ADR-0029): a library, the fake, and a thin binary that serves it for local runs.
//!
//! It shares no code with the server, only the wire format: requests, histories and
//! signed notifications. A misunderstanding of that format then fails a test, instead
//! of hiding in code that both sides share.
//!
//! A test starts one fake of its own, on a free local port, and scripts every decision
//! and fault through [`MockPsp`]. By default the fake holds each request until the test
//! decides it, so that every fact appears at a moment that the test chose. Each fact is
//! announced by a thin, signed notification, which the test delivers when it chooses,
//! and as often, while the binary delivers on the schedule of Standard Webhooks.

mod deliveries;
mod model;
mod server;
pub mod signing;
pub mod wire;

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::http::Uri;
use chrono::{DateTime, TimeDelta, Utc};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

pub use deliveries::Attempt;
use deliveries::Courier;
use model::Model;
pub use model::{Decision, Impossible, Message, MessageState, Policy};
use signing::{SigningKey, signature_header};
use wire::{Detail, History, Request};

/// What a fake starts with.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where it listens. Port 0 picks a free one.
    pub address: SocketAddr,
    pub policy: Policy,
    /// The key that the service's calls carry, as `Authorization: Bearer <key>`.
    pub api_key: String,
    /// How long an authorization lasts before it may lapse.
    pub authorization_lifetime: TimeDelta,
    /// The keys that sign each notification, the current one first. During a rotation
    /// there are two (ADR-0030).
    pub signing_keys: Vec<SigningKey>,
    /// How long a delivery waits for the receiver's answer: the specification asks for
    /// 15 to 30 seconds.
    pub delivery_timeout: Duration,
}

impl Config {
    /// A fake for one test: on a free local port, holding every request, with a new API
    /// key and a new signing key.
    #[must_use]
    pub fn for_tests() -> Self {
        Self {
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
            policy: Policy::Hold,
            api_key: new_api_key(),
            authorization_lifetime: TimeDelta::days(7),
            signing_keys: vec![SigningKey::generate()],
            delivery_timeout: Duration::from_secs(15),
        }
    }
}

/// How a test spoils a delivery, to watch the server refuse it (ADR-0030).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tamper {
    /// Signed with a key that the server does not hold.
    WrongKey,
    /// A `webhook-timestamp` of the test's choosing, in Unix seconds, signed as if true.
    Timestamp(i64),
    /// One byte added to the body after it was signed, which keeps it valid JSON.
    ChangedBody,
}

/// A new key for the service's calls: `mock_sk_` and 64 hex digits from the operating
/// system.
#[must_use]
pub fn new_api_key() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("an API key needs the operating system's random bytes");
    let digits: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("mock_sk_{digits}")
}

/// A route of the fake, which a scripted failure can answer instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Requests,
    History,
}

/// An answer that the fake gives instead of serving a call (ADR-0032).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// 429, with a `Retry-After` of this many seconds.
    TooManyRequests { retry_after: u32 },
    /// 503, with a `Retry-After` of this many seconds.
    Unavailable { retry_after: u32 },
    /// 500: the caller cannot know whether anything happened.
    InternalError,
}

/// The faults that the test scripted and the routes have not met yet.
#[derive(Debug, Default)]
struct Faults {
    requests: VecDeque<Failure>,
    history: VecDeque<Failure>,
    lost_replies: u32,
}

/// What the routes share with the handle.
pub(crate) struct Shared {
    model: Mutex<Model>,
    faults: Mutex<Faults>,
    api_key: String,
    keys: Vec<SigningKey>,
    /// Where notifications go: the server's `POST /webhooks/mock`.
    endpoint: Mutex<Option<String>>,
    courier: Courier,
}

impl Shared {
    pub(crate) fn model(&self) -> MutexGuard<'_, Model> {
        self.model
            .lock()
            .expect("no test panicked while it held the fake's records")
    }

    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    /// The next failure scripted for this route, which answers the call instead.
    pub(crate) fn next_failure(&self, route: Route) -> Option<Failure> {
        let mut faults = self.faults();
        match route {
            Route::Requests => faults.requests.pop_front(),
            Route::History => faults.history.pop_front(),
        }
    }

    /// Whether the reply to the request just taken is lost.
    pub(crate) fn lose_this_reply(&self) -> bool {
        let mut faults = self.faults();
        if faults.lost_replies == 0 {
            return false;
        }
        faults.lost_replies -= 1;
        true
    }

    fn faults(&self) -> MutexGuard<'_, Faults> {
        self.faults
            .lock()
            .expect("no test panicked while it held the fake's faults")
    }
}

/// The time now, in whole microseconds, like every time on the wire (ADR-0022).
pub(crate) fn now() -> DateTime<Utc> {
    let now = Utc::now();
    DateTime::from_timestamp_micros(now.timestamp_micros()).expect("now is in range")
}

/// A fake PSP served on a local port, and the handle that a test drives it through.
/// Dropping it stops the server.
pub struct MockPsp {
    shared: Arc<Shared>,
    url: String,
    server: JoinHandle<()>,
}

impl MockPsp {
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
        let url = format!("http://{}", listener.local_addr()?);
        let shared = Arc::new(Shared {
            model: Mutex::new(Model::new(config.policy, config.authorization_lifetime)),
            faults: Mutex::new(Faults::default()),
            api_key: config.api_key,
            keys: config.signing_keys,
            endpoint: Mutex::new(None),
            courier: Courier::new(config.delivery_timeout),
        });
        let router = server::router(Arc::clone(&shared));
        let server = tokio::spawn(async move {
            // It ends only on an error accepting a connection, after which the fake is
            // gone, as a PSP that is down would be.
            let _ended = axum::serve(listener, router).await;
        });
        Ok(Self {
            shared,
            url,
            server,
        })
    }

    /// Where the fake listens, as `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The key that the service's calls must carry.
    #[must_use]
    pub fn api_key(&self) -> &str {
        self.shared.api_key()
    }

    /// Every request taken as new, in the order of arrival.
    #[must_use]
    pub fn requests(&self) -> Vec<Request> {
        self.shared.model().requests().to_vec()
    }

    /// How many times this request was taken as new. More than once means that the PSP
    /// acted on one request more than once, which I9 forbids.
    #[must_use]
    pub fn times_taken(&self, request_id: Uuid) -> usize {
        self.shared.model().times_taken(request_id)
    }

    /// Decides the first undecided request with this ID, and queues the notification of
    /// the fact that it makes.
    ///
    /// # Panics
    ///
    /// On a decision that no PSP could make: see [`Impossible`].
    pub fn decide(&self, request_id: Uuid, decision: Decision) {
        let decided = self.shared.model().decide(request_id, decision, now());
        if let Err(impossible) = decided {
            panic!("{impossible}");
        }
    }

    /// Approves a request for the whole amount that it asks.
    ///
    /// # Panics
    ///
    /// See [`MockPsp::decide`].
    pub fn approve(&self, request_id: Uuid) {
        self.decide(request_id, Decision::Approve);
    }

    /// Authorizes less than the amount asked (ADR-0016).
    ///
    /// # Panics
    ///
    /// See [`MockPsp::decide`].
    pub fn approve_for(&self, request_id: Uuid, amount: i64) {
        self.decide(request_id, Decision::ApproveFor(amount));
    }

    /// Declines an authorization.
    ///
    /// # Panics
    ///
    /// See [`MockPsp::decide`].
    pub fn decline(&self, request_id: Uuid) {
        self.decide(request_id, Decision::Decline);
    }

    /// Refuses a capture, a void or a refund.
    ///
    /// # Panics
    ///
    /// See [`MockPsp::decide`].
    pub fn refuse(&self, request_id: Uuid) {
        self.decide(request_id, Decision::Refuse);
    }

    /// Settles a captured payment, which only the PSP decides.
    ///
    /// # Panics
    ///
    /// On a payment that is not captured, or is settled already.
    pub fn settle(&self, payment_id: Uuid) {
        let settled = self.shared.model().settle(payment_id, now());
        if let Err(impossible) = settled {
            panic!("{impossible}");
        }
    }

    /// Lets an authorization lapse, which only the PSP decides.
    ///
    /// # Panics
    ///
    /// On a payment that is not authorized.
    pub fn expire(&self, payment_id: Uuid) {
        let expired = self.shared.model().expire(payment_id, now());
        if let Err(impossible) = expired {
            panic!("{impossible}");
        }
    }

    /// Appends any fact, unchecked, for the tests of a history that the service cannot
    /// place, such as a capture that fails after it succeeded (ADR-0031).
    pub fn append(&self, payment_id: Uuid, detail: Detail) {
        self.shared.model().append(payment_id, detail, now());
    }

    /// What the PSP holds for a payment, as its history route serves it.
    #[must_use]
    pub fn history(&self, payment_id: Uuid) -> Option<History> {
        self.shared.model().history(payment_id)
    }

    /// Forgets every idempotency key, as when the cache prunes them: a copy of a request
    /// then counts as a new one, as at Stripe (ADR-0028).
    pub fn forget_keys(&self) {
        self.shared.model().forget_keys();
    }

    /// Forgets the idempotency keys taken a day or more before `now`, as Stripe may
    /// (ADR-0028). The binary takes this step on a timer, and a test takes it with any
    /// time it likes, since nothing in the fake reads a clock to decide.
    pub fn prune_keys(&self, now: DateTime<Utc>) {
        self.shared.model().prune_keys(now);
    }

    /// Answers the next calls to this route with these failures, one per call, without
    /// serving them.
    pub fn fail_next(&self, route: Route, failures: impl IntoIterator<Item = Failure>) {
        let mut faults = self.shared.faults();
        match route {
            Route::Requests => faults.requests.extend(failures),
            Route::History => faults.history.extend(failures),
        }
    }

    /// Takes each of the next `count` requests, and never answers it: the PSP acts, and
    /// its reply is lost.
    pub fn lose_replies(&self, count: u32) {
        self.shared.faults().lost_replies += count;
    }

    /// The notifications that the PSP owes the server, one for each fact, in order.
    #[must_use]
    pub fn notifications(&self) -> Vec<Message> {
        self.shared.model().messages().to_vec()
    }

    /// Sends notifications to this endpoint: the server's `POST /webhooks/mock`.
    ///
    /// # Panics
    ///
    /// On an endpoint that is not a URI.
    pub fn deliver_to(&self, endpoint: &str) {
        endpoint
            .parse::<Uri>()
            .unwrap_or_else(|_| panic!("{endpoint} is not a URI"));
        *self.endpoint() = Some(endpoint.to_owned());
    }

    /// The keys that sign each notification, as the server reads them from its
    /// environment: `whsec_` and base64.
    #[must_use]
    pub fn signing_keys(&self) -> Vec<String> {
        self.shared.keys.iter().map(SigningKey::written).collect()
    }

    /// Makes one attempt to deliver this notification, now, whatever its state: one that
    /// was delivered goes again, as a duplicate, with the same ID and body.
    ///
    /// # Panics
    ///
    /// Before [`MockPsp::deliver_to`], or on a notification that the fake does not owe.
    pub async fn deliver(&self, message_id: &str) -> Attempt {
        self.attempt(message_id, None, now()).await
    }

    /// Makes one attempt, spoiled in this way.
    ///
    /// # Panics
    ///
    /// As [`MockPsp::deliver`].
    pub async fn deliver_tampered(&self, message_id: &str, tamper: Tamper) -> Attempt {
        self.attempt(message_id, Some(tamper), now()).await
    }

    /// Makes one attempt at each notification due by `now`, in order, and records any
    /// failure at `now`: the binary takes this step on a timer, and a test takes it with
    /// any time it likes. The `webhook-timestamp` of each attempt is still the true time,
    /// which the server checks against its own clock.
    ///
    /// # Panics
    ///
    /// Before [`MockPsp::deliver_to`].
    pub async fn deliver_due(&self, now: DateTime<Utc>) -> Vec<(String, Attempt)> {
        let due = self.shared.model().due(now);
        let mut attempts = Vec::with_capacity(due.len());
        for message_id in due {
            let attempt = self.attempt(&message_id, None, now).await;
            attempts.push((message_id, attempt));
        }
        attempts
    }

    /// Drops a notification, which is then never delivered unless a test delivers it:
    /// the sweep must find its fact (ADR-0031).
    pub fn drop_notification(&self, message_id: &str) {
        self.shared.model().drop_message(message_id);
    }

    /// Announces again that this payment changed, in a new event: the same fact in two
    /// events, as Stripe may send it. Returns the new notification's ID.
    pub fn notify_again(&self, payment_id: Uuid) -> String {
        self.shared.model().notify(payment_id, now())
    }

    /// One attempt, recorded as made at `at`.
    async fn attempt(
        &self,
        message_id: &str,
        tamper: Option<Tamper>,
        at: DateTime<Utc>,
    ) -> Attempt {
        let endpoint = self
            .endpoint()
            .clone()
            .expect("deliver_to names the server's endpoint first");
        let message = self
            .shared
            .model()
            .message(message_id)
            .cloned()
            .unwrap_or_else(|| panic!("the fake owes no notification {message_id}"));
        // Each attempt is signed with the time it is made (Standard Webhooks).
        let timestamp = match tamper {
            Some(Tamper::Timestamp(chosen)) => chosen,
            _ => Utc::now().timestamp(),
        };
        let keys = match tamper {
            Some(Tamper::WrongKey) => vec![SigningKey::generate()],
            _ => self.shared.keys.clone(),
        };
        let mut payload = message.notification.payload();
        let signature = signature_header(&keys, message_id, timestamp, &payload);
        if tamper == Some(Tamper::ChangedBody) {
            payload.push(b' ');
        }
        let attempt = self
            .shared
            .courier
            .post(&endpoint, message_id, timestamp, &signature, payload)
            .await;
        self.shared
            .model()
            .record(message_id, attempt, at, jitter());
        attempt
    }

    fn endpoint(&self) -> MutexGuard<'_, Option<String>> {
        self.shared
            .endpoint
            .lock()
            .expect("no test panicked while it held the endpoint")
    }
}

/// A jitter from 0 to 999, from the operating system.
fn jitter() -> u16 {
    let mut bytes = [0_u8; 2];
    getrandom::fill(&mut bytes).expect("the operating system's random bytes");
    u16::from_le_bytes(bytes) % 1_000
}

impl Drop for MockPsp {
    fn drop(&mut self) {
        self.server.abort();
    }
}
