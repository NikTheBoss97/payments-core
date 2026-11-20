// Author: Nicola Antonello

//! One attempt to deliver an event (ADR-0053, ADR-0060, ADR-0062), against the fake
//! merchant, which checks each delivery with code of its own (ADR-0056), and against a raw
//! TCP server for what the fake never answers: a `Retry-After` date, a redirect, and a
//! body that does not end with its head. The system role reads the rows, as the relay
//! will. Master keys are made at runtime.

mod common;

use std::error::Error;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::outbox::{disable, event, insert_endpoint, new_payment};
use common::{delivery_id, endpoint_id, event_id, runtime_pool};
use mock_merchant::{Answer as Scripted, MockMerchant, Verdict};
use payments_core::deliveries::{DeliveryClient, NotSent, Outcome, TIMEOUT};
use payments_core::events::body;
use payments_core::relay::{Answer, Delivery};
use payments_core::signing::MasterKey;
use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

type TestResult = Result<(), Box<dyn Error>>;

/// The answer of an endpoint that took the event, as the relay reads it.
const OK: Outcome = Outcome::Answered(Answer::Status {
    status: 200,
    retry_after: None,
});

/// A master key of 32 bytes from the operating system.
fn master() -> MasterKey {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system's randomness");
    MasterKey::parse(&format!("whsec_{}", STANDARD.encode(bytes))).expect("32 bytes")
}

/// A new merchant's endpoint `n` at `url`, and its payment's event `n` with delivery
/// `1000 + n` to the endpoint, committed as the catch-up writes them. The delivery is
/// numbered apart, since two IDs of one number are the same UUID, and an attempt that sent
/// the delivery's ID for the event's would then pass.
async fn delivery_to(system: &PgPool, n: u32, url: &str) -> Result<Delivery, Box<dyn Error>> {
    let (merchant, payment) = new_payment(system).await?;
    insert_endpoint(system, endpoint_id(n), merchant, url).await?;
    let delivery = delivery_id(n.checked_add(1_000).ok_or("a small number")?);
    let written = event(
        system,
        merchant,
        payment,
        1,
        event_id(n),
        &[(delivery, endpoint_id(n))],
    )
    .await?;
    Ok(written[0])
}

#[sqlx::test]
async fn an_attempt_posts_the_events_body_signed_under_its_endpoints_secret(
    pool: PgPool,
) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let master = master();
    let merchant = MockMerchant::start().await?;
    let delivery = delivery_to(&system, 1, merchant.url()).await?;
    merchant.trust(&master.secret_of(delivery.endpoint).written())?;
    let client = DeliveryClient::new(master, TIMEOUT);
    let at = OffsetDateTime::now_utc();

    assert_eq!(client.attempt(&system, delivery, at).await?, OK);
    // The fake verified the signature with code of its own, and took the event once, under
    // its ID (ADR-0051).
    let taken = merchant.events();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].id, delivery.event.to_string());
    let arrivals = merchant.arrivals();
    let sent = body(&system, delivery.event)
        .await?
        .ok_or("the event exists")?;
    assert_eq!(arrivals[0].body, sent);
    assert_eq!(arrivals[0].timestamp, Some(at.unix_timestamp().to_string()));
    merchant.stop().await;
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_attempt_posts_json_with_the_three_webhook_headers(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let server = RawServer::start(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_vec()).await?;
    let delivery = delivery_to(&system, 1, &server.url).await?;
    let client = DeliveryClient::new(master(), TIMEOUT);
    let at = OffsetDateTime::now_utc();

    assert_eq!(client.attempt(&system, delivery, at).await?, OK);
    let heads = server.heads();
    assert_eq!(heads.len(), 1);
    let head = heads[0].to_ascii_lowercase();
    let mut lines = head.split("\r\n");
    assert_eq!(lines.next(), Some("post /webhooks http/1.1"));
    // The messages name what is missing and print no field: the signature is a MAC under
    // the endpoint's secret.
    let fields: Vec<&str> = lines.collect();
    let has = |field: String| fields.contains(&field.as_str());
    assert!(
        has("content-type: application/json".to_owned()),
        "no JSON content type"
    );
    assert!(
        has(format!("webhook-id: {}", delivery.event)),
        "no webhook-id naming the event"
    );
    assert!(
        has(format!("webhook-timestamp: {}", at.unix_timestamp())),
        "no webhook-timestamp of the attempt's start"
    );
    assert!(
        fields
            .iter()
            .any(|field| field.starts_with("webhook-signature: v1,")),
        "no v1 signature"
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn each_attempt_is_signed_afresh_at_its_own_time(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let master = master();
    let merchant = MockMerchant::start().await?;
    let delivery = delivery_to(&system, 1, merchant.url()).await?;
    merchant.trust(&master.secret_of(delivery.endpoint).written())?;
    let client = DeliveryClient::new(master, TIMEOUT);
    let first = OffsetDateTime::now_utc();
    let second = first
        .checked_add(time::Duration::seconds(7))
        .ok_or("a time in range")?;

    for at in [first, second] {
        assert_eq!(client.attempt(&system, delivery, at).await?, OK);
    }
    // Both passed the fake's checks: the second is a copy of an event taken already.
    let seen: Vec<(Option<String>, Verdict)> = merchant
        .arrivals()
        .into_iter()
        .map(|arrival| (arrival.timestamp, arrival.verdict))
        .collect();
    assert_eq!(
        seen,
        [
            (
                Some(first.unix_timestamp().to_string()),
                Verdict::Taken(Scripted::Status(StatusCode::OK))
            ),
            (Some(second.unix_timestamp().to_string()), Verdict::Copy),
        ]
    );
    merchant.stop().await;
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_answers_status_and_retry_after_come_back_as_given(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let master = master();
    let merchant = MockMerchant::start().await?;
    let delivery = delivery_to(&system, 1, merchant.url()).await?;
    merchant.trust(&master.secret_of(delivery.endpoint).written())?;
    let client = DeliveryClient::new(master, TIMEOUT);

    for (scripted, status, retry_after) in [
        (Scripted::Status(StatusCode::GONE), 410, None),
        (
            Scripted::RetryAfter {
                status: StatusCode::TOO_MANY_REQUESTS,
                seconds: 30,
            },
            429,
            Some(30),
        ),
        (Scripted::Status(StatusCode::SERVICE_UNAVAILABLE), 503, None),
        (
            Scripted::Status(StatusCode::INTERNAL_SERVER_ERROR),
            500,
            None,
        ),
    ] {
        merchant.answer(scripted);
        let outcome = client
            .attempt(&system, delivery, OffsetDateTime::now_utc())
            .await?;
        assert_eq!(
            outcome,
            Outcome::Answered(Answer::Status {
                status,
                retry_after
            }),
            "{scripted:?}"
        );
    }
    merchant.stop().await;
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_after_date_counts_from_the_attempts_start(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    // The attempt started a minute ago, and the date is two minutes after the start of the
    // current second, which a date's whole seconds name: 180 seconds after the attempt's
    // start, at most a fraction of one less, rounded up. Counted from the answer's arrival,
    // the wait would be about 120.
    let now = OffsetDateTime::now_utc();
    let at = now
        .checked_sub(time::Duration::minutes(1))
        .ok_or("a time in range")?;
    let date = SystemTime::from(now.replace_nanosecond(0)?)
        .checked_add(Duration::from_secs(120))
        .ok_or("a time in range")?;
    let server = RawServer::start(
        format!(
            "HTTP/1.1 503 Service Unavailable\r\nretry-after: {}\r\ncontent-length: 0\r\n\r\n",
            httpdate::fmt_http_date(date)
        )
        .into_bytes(),
    )
    .await?;
    let delivery = delivery_to(&system, 1, &server.url).await?;
    let client = DeliveryClient::new(master(), TIMEOUT);

    assert_eq!(
        client.attempt(&system, delivery, at).await?,
        Outcome::Answered(Answer::Status {
            status: 503,
            retry_after: Some(180)
        })
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_redirect_is_an_answer_and_is_not_followed(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let elsewhere =
        RawServer::start(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_vec()).await?;
    let server = RawServer::start(
        format!(
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: {}\r\ncontent-length: 0\r\n\r\n",
            elsewhere.url
        )
        .into_bytes(),
    )
    .await?;
    let delivery = delivery_to(&system, 1, &server.url).await?;
    let client = DeliveryClient::new(master(), TIMEOUT);

    assert_eq!(
        client
            .attempt(&system, delivery, OffsetDateTime::now_utc())
            .await?,
        Outcome::Answered(Answer::Status {
            status: 307,
            retry_after: None
        })
    );
    assert_eq!(server.requests(), 1);
    assert_eq!(elsewhere.connections(), 0, "the redirect was followed");
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_answer_slower_than_the_timeout_is_no_answer(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let master = master();
    let merchant = MockMerchant::start().await?;
    let delivery = delivery_to(&system, 1, merchant.url()).await?;
    merchant.trust(&master.secret_of(delivery.endpoint).written())?;
    merchant.answer(Scripted::Delay(Duration::from_secs(2)));
    let client = DeliveryClient::new(master, Duration::from_millis(200));

    assert_eq!(
        client
            .attempt(&system, delivery, OffsetDateTime::now_utc())
            .await?,
        Outcome::Answered(Answer::NoAnswer)
    );
    // The delivery arrived: only its answer came too late.
    assert_eq!(merchant.arrivals().len(), 1);
    merchant.stop().await;
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_endpoint_where_nothing_listens_gives_no_answer(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    // A socket bound and not listening: a connection to its port is refused, and no test
    // running beside this one can bind the port while it is held.
    let held = TcpSocket::new_v4()?;
    held.bind("127.0.0.1:0".parse()?)?;
    let url = format!("http://{}/webhooks", held.local_addr()?);
    let delivery = delivery_to(&system, 1, &url).await?;
    let client = DeliveryClient::new(master(), TIMEOUT);

    assert_eq!(
        client
            .attempt(&system, delivery, OffsetDateTime::now_utc())
            .await?,
        Outcome::Answered(Answer::NoAnswer)
    );
    drop(held);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_success_needs_only_the_head_of_its_answer(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    // A head that promises 100 bytes of body, which never come.
    let server =
        RawServer::start(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n".to_vec()).await?;
    let delivery = delivery_to(&system, 1, &server.url).await?;
    let client = DeliveryClient::new(master(), TIMEOUT);

    let started = Instant::now();
    assert_eq!(
        client
            .attempt(&system, delivery, OffsetDateTime::now_utc())
            .await?,
        OK
    );
    // The head decided, and the body was waited for a second at most, not the timeout.
    let took = started.elapsed();
    assert!(took < Duration::from_secs(5), "the attempt took {took:?}");
    // The connection, which the missing bytes leave unusable, was closed.
    tokio::time::timeout(Duration::from_secs(5), server.closed.notified()).await?;
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_answers_body_is_read_only_so_that_its_connection_serves_again(
    pool: PgPool,
) -> TestResult {
    let system = runtime_pool(&pool).await?;
    // A body that comes with its head is read to its end, so the next attempt to the
    // endpoint takes the same connection.
    let short =
        RawServer::start(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".to_vec()).await?;
    // So is one that comes a tenth of a second after its head: too late to be found when
    // the head decides, so a client that only dropped the body would close the connection.
    let late = RawServer::in_pieces(vec![
        (
            Duration::ZERO,
            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n".to_vec(),
        ),
        (Duration::from_millis(100), b"ok".to_vec()),
    ])
    .await?;
    // A body of 4 MiB stops the read at 64 KiB, far below what the client holds of a
    // connection's input, so the connection is closed, and the next attempt opens another.
    let mut long = b"HTTP/1.1 200 OK\r\ncontent-length: 4194304\r\n\r\n".to_vec();
    long.resize(
        long.len().checked_add(4_194_304).ok_or("a small answer")?,
        b'x',
    );
    let long = RawServer::start(long).await?;
    let client = DeliveryClient::new(master(), TIMEOUT);

    for (n, server, connections) in [(1, &short, 1), (2, &late, 1), (3, &long, 2)] {
        let delivery = delivery_to(&system, n, &server.url).await?;
        for _ in 0..2 {
            assert_eq!(
                client
                    .attempt(&system, delivery, OffsetDateTime::now_utc())
                    .await?,
                OK
            );
        }
        assert_eq!(server.requests(), 2);
        assert_eq!(server.connections(), connections, "{}", server.url);
    }
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_stored_url_that_the_rules_refuse_gets_no_answer(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    // The database's check takes a `%` in a host's name, which ADR-0060's parser refuses
    // (ADR-0061): a row written past the route.
    let delivery = delivery_to(&system, 1, "http://merchant%example/webhooks").await?;
    let client = DeliveryClient::new(master(), TIMEOUT);

    assert_eq!(
        client
            .attempt(&system, delivery, OffsetDateTime::now_utc())
            .await?,
        Outcome::Answered(Answer::NoAnswer)
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_attempt_that_finds_its_endpoint_disabled_sends_nothing(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let master = master();
    let merchant = MockMerchant::start().await?;
    let delivery = delivery_to(&system, 1, merchant.url()).await?;
    merchant.trust(&master.secret_of(delivery.endpoint).written())?;
    disable(&system, delivery.endpoint, "operator").await?;
    let client = DeliveryClient::new(master, TIMEOUT);

    assert_eq!(
        client
            .attempt(&system, delivery, OffsetDateTime::now_utc())
            .await?,
        Outcome::Disabled
    );
    assert!(merchant.arrivals().is_empty());
    merchant.stop().await;
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_attempt_without_the_database_is_not_sent(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let master = master();
    let merchant = MockMerchant::start().await?;
    let delivery = delivery_to(&system, 1, merchant.url()).await?;
    let client = DeliveryClient::new(master, TIMEOUT);
    system.close().await;

    let outcome = client
        .attempt(&system, delivery, OffsetDateTime::now_utc())
        .await;
    assert!(matches!(outcome, Err(NotSent::Endpoint(_))), "{outcome:?}");
    assert!(merchant.arrivals().is_empty());
    merchant.stop().await;
    Ok(())
}

#[sqlx::test]
async fn an_attempt_whose_body_cannot_be_read_is_not_sent(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let master = master();
    let merchant = MockMerchant::start().await?;
    let delivery = delivery_to(&system, 1, merchant.url()).await?;
    let client = DeliveryClient::new(master, TIMEOUT);
    // In this test's database only: the endpoint can still be read, and the event cannot.
    sqlx::query!("REVOKE SELECT ON payment_events FROM payments_app")
        .execute(&pool)
        .await?;

    let outcome = client
        .attempt(&system, delivery, OffsetDateTime::now_utc())
        .await;
    assert!(matches!(outcome, Err(NotSent::Body(_))), "{outcome:?}");
    assert!(merchant.arrivals().is_empty());
    merchant.stop().await;
    system.close().await;
    Ok(())
}

/// A piece of an answer, written after a pause.
type Piece = (Duration, Vec<u8>);

/// A server that answers every request it reads in the same pieces, for the answers that
/// the fake merchant never gives. It keeps the head of each request, counts its
/// connections, and notifies `closed` each time a connection ends.
struct RawServer {
    url: String,
    connections: Arc<AtomicUsize>,
    heads: Arc<Mutex<Vec<String>>>,
    closed: Arc<Notify>,
    task: JoinHandle<()>,
}

impl RawServer {
    /// Answers each request with these bytes at once.
    async fn start(answer: Vec<u8>) -> io::Result<Self> {
        Self::in_pieces(vec![(Duration::ZERO, answer)]).await
    }

    /// Answers each request with these pieces, each written after its pause.
    async fn in_pieces(pieces: Vec<Piece>) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/webhooks", listener.local_addr()?);
        let connections = Arc::new(AtomicUsize::new(0));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(Notify::new());
        let pieces = Arc::new(pieces);
        let task = tokio::spawn({
            let (connections, heads, closed) = (
                Arc::clone(&connections),
                Arc::clone(&heads),
                Arc::clone(&closed),
            );
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    connections.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve(
                        stream,
                        Arc::clone(&pieces),
                        Arc::clone(&heads),
                        Arc::clone(&closed),
                    ));
                }
            }
        });
        Ok(Self {
            url,
            connections,
            heads,
            closed,
            task,
        })
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// The heads of the requests that arrived, in their order.
    fn heads(&self) -> Vec<String> {
        self.heads
            .lock()
            .expect("no thread panics holding it")
            .clone()
    }

    fn requests(&self) -> usize {
        self.heads().len()
    }
}

impl Drop for RawServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Answers each request that arrives on `stream`, until the client closes it or stops
/// reading.
async fn serve(
    mut stream: TcpStream,
    pieces: Arc<Vec<Piece>>,
    heads: Arc<Mutex<Vec<String>>>,
    closed: Arc<Notify>,
) {
    let mut received = Vec::new();
    'requests: while let Some(head) = read_request(&mut stream, &mut received).await {
        heads
            .lock()
            .expect("no thread panics holding it")
            .push(head);
        for (pause, piece) in pieces.iter() {
            tokio::time::sleep(*pause).await;
            if stream.write_all(piece).await.is_err() {
                break 'requests;
            }
        }
    }
    closed.notify_one();
}

/// Reads one request from `stream`: its head, then as many bytes as its `content-length`
/// says. Returns its head, or `None` once the client closes the connection.
async fn read_request(stream: &mut TcpStream, received: &mut Vec<u8>) -> Option<String> {
    let mut chunk = [0_u8; 4096];
    loop {
        if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            let head_end = end.checked_add(4)?;
            let head = String::from_utf8_lossy(received.get(..head_end)?).into_owned();
            let length: usize = head
                .to_ascii_lowercase()
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(Some(0), |value| value.trim().parse().ok())?;
            let total = head_end.checked_add(length)?;
            if received.len() >= total {
                received.drain(..total);
                return Some(head);
            }
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        received.extend_from_slice(chunk.get(..read)?);
    }
}
