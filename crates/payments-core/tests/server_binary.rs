// Author: Nicola Antonello

//! The real binary, started as a process and spoken to over a socket (ADR-0023).
//!
//! The other tests send a request straight to the router, on the single thread that
//! `#[sqlx::test]` gives them. Only here does a request cross `main`, its configuration,
//! hyper and tokio's multi-thread runtime, as it does in production.

mod common;

use std::error::Error;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::api::{Pools, written};
use common::crashes::{assert_books_are_whole, balances};
use common::keys::issued_key;
use common::payments::{AUTHORIZED, FEE, create_command, fee_rate, steps, take_lock};
use common::{merchant_id, test_uuid, wait_for_lock_waits};
use futures_util::future::{join, join_all};
use mock_psp::signing::SigningKey;
use mock_psp::{Attempt, MockPsp};
use payments_core::api::problem::Problem;
use payments_core::credentials::ApiKeySecret;
use payments_core::idempotency::Fingerprint;
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::payments::{create_payment, register_merchant};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

const BINARY: &str = env!("CARGO_BIN_EXE_payments-core");

const CREATED: &str = "HTTP/1.1 201 Created";
const OK: &str = "HTTP/1.1 200 OK";
const UNAUTHORIZED: &str = "HTTP/1.1 401 Unauthorized";

const REGISTER: &str = r#"{"fee_bps":250}"#;
const CREATE: &str = r#"{"currency":"EUR","amount":10000}"#;
const AUTHORIZE: &str = r#"{"amount":8000,"expires_at":"2027-01-01T00:00:00Z"}"#;
const CAPTURE: &str = r#"{"amount":8000,"captured_at":"2026-12-01T00:00:00Z"}"#;

/// How many copies of a request race. The server's pools hold 10 connections, and a copy
/// waiting on a lock holds one, so all of these can wait at once.
const COPIES: i64 = 8;

/// The URL of this test's database, as a login role whose password is its name, as in
/// `roles.sql`.
fn url(pool: &PgPool, role: &str) -> String {
    let options = pool.connect_options();
    format!(
        "postgres://{role}:{role}@{}:{}/{}",
        options.get_host(),
        options.get_port(),
        options.get_database().expect("a test's own database")
    )
}

/// The binary with the configuration that a test's database needs. The test's own
/// environment could hold anything, so nothing comes from it.
fn configured(pool: &PgPool, operator: &ApiKeySecret) -> Command {
    let mut command = Command::new(BINARY);
    command
        .env("TENANT_DATABASE_URL", url(pool, "payments_tenant"))
        .env("SYSTEM_DATABASE_URL", url(pool, "payments_app"))
        .env("OPERATOR_KEY_SHA256", written(operator.digest()))
        .env("MOCK_PSP_SIGNING_KEYS", SigningKey::generate().written())
        .env("BIND_ADDRESS", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// A running server, the operator's secret that it holds the digest of, and everything that
/// it has written since it said where it listens.
struct Server {
    child: Child,
    address: String,
    operator: ApiKeySecret,
    log: Arc<Mutex<String>>,
    collectors: Vec<JoinHandle<()>>,
}

/// What outlives a server that was killed: what starts it again, and what it wrote.
struct Killed {
    operator: ApiKeySecret,
    log: Arc<Mutex<String>>,
}

impl Killed {
    /// The binary started again, on another port, under the same operator's digest.
    async fn restart(self, pool: &PgPool) -> Result<Server, Box<dyn Error>> {
        Server::launched(pool, self.operator, self.log).await
    }
}

/// Keeps a stream's lines in the log, so that the process never blocks on a full pipe.
fn collect(
    stream: impl AsyncRead + Unpin + Send + 'static,
    log: &Arc<Mutex<String>>,
) -> JoinHandle<()> {
    let log = log.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut log = log.lock().expect("no test panics while holding this");
            log.push_str(&line);
            log.push('\n');
        }
    })
}

impl Server {
    /// Starts the binary, and waits until it says where it is listening. The port is one
    /// that the system chose, so tests never collide.
    async fn start(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let log = Arc::new(Mutex::new(String::new()));
        Self::launched(pool, ApiKeySecret::generate(), log).await
    }

    /// Starts the binary next to this fake: it verifies notifications with the fake's
    /// keys.
    async fn start_next_to(pool: &PgPool, fake: &MockPsp) -> Result<Self, Box<dyn Error>> {
        let operator = ApiKeySecret::generate();
        let mut command = configured(pool, &operator);
        command.env("MOCK_PSP_SIGNING_KEYS", fake.signing_keys().join(" "));
        let log = Arc::new(Mutex::new(String::new()));
        Self::spawned(command, operator, log).await
    }

    async fn launched(
        pool: &PgPool,
        operator: ApiKeySecret,
        log: Arc<Mutex<String>>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::spawned(configured(pool, &operator), operator, log).await
    }

    async fn spawned(
        mut command: Command,
        operator: ApiKeySecret,
        log: Arc<Mutex<String>>,
    ) -> Result<Self, Box<dyn Error>> {
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("a piped stdout");
        let stderr = child.stderr.take().expect("a piped stderr");
        let from_stderr = collect(stderr, &log);
        let mut lines = BufReader::new(stdout).lines();
        let address = timeout(Duration::from_secs(30), async {
            while let Some(line) = lines.next_line().await.expect("a readable stdout") {
                if let Some((_, address)) = line.split_once("listening on ") {
                    return Some(address.to_owned());
                }
            }
            None
        })
        .await
        .expect("the server said where it listens within 30 seconds");
        let Some(address) = address else {
            let status = child.wait().await?;
            let log = log
                .lock()
                .expect("no test panics while holding this")
                .clone();
            return Err(format!("the server ended ({status}) with: {log}").into());
        };
        let from_stdout = collect(lines.into_inner(), &log);
        Ok(Self {
            child,
            address,
            operator,
            log,
            collectors: vec![from_stderr, from_stdout],
        })
    }

    /// Waits, at most 10 seconds, until the server has written this.
    async fn until_logged(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self
            .log
            .lock()
            .expect("no test panics while holding this")
            .contains(text)
        {
            assert!(Instant::now() < deadline, "the server never wrote {text:?}");
            sleep(Duration::from_millis(20)).await;
        }
    }

    /// One request with no body on a connection of its own. The answer's status line and its
    /// body.
    async fn exchange(&self, request_head: &str) -> Result<(String, String), Box<dyn Error>> {
        self.exchange_with_body(request_head, "").await
    }

    /// One request on a connection of its own.
    async fn exchange_with_body(
        &self,
        request_head: &str,
        body: &str,
    ) -> Result<(String, String), Box<dyn Error>> {
        let mut stream = TcpStream::connect(&self.address).await?;
        let length = body.len();
        stream
            .write_all(
                format!(
                    "{request_head}\r\nHost: test\r\nConnection: close\r\n\
                     Content-Length: {length}\r\n\r\n{body}"
                )
                .as_bytes(),
            )
            .await?;
        let mut answer = String::new();
        timeout(Duration::from_secs(30), stream.read_to_string(&mut answer)).await??;
        let (head, body) = answer.split_once("\r\n\r\n").unwrap_or((&answer, ""));
        let status = head.lines().next().unwrap_or_default().to_owned();
        Ok((status, body.to_owned()))
    }

    /// A `GET` that presents this secret.
    async fn get(&self, path: &str, secret: &str) -> Result<(String, String), Box<dyn Error>> {
        self.exchange(&format!(
            "GET {path} HTTP/1.1\r\nAuthorization: Bearer {secret}"
        ))
        .await
    }

    /// A `POST` that presents this secret, under this idempotency key.
    async fn post(
        &self,
        path: &str,
        secret: &str,
        key: &str,
        body: &str,
    ) -> Result<(String, String), Box<dyn Error>> {
        let head = format!(
            "POST {path} HTTP/1.1\r\nAuthorization: Bearer {secret}\r\nIdempotency-Key: \"{key}\""
        );
        self.exchange_with_body(&head, body).await
    }

    /// A `POST` that applies, sent again: the retry must get the same status and bytes.
    async fn applied(
        &self,
        status: &str,
        path: &str,
        secret: &str,
        key: &str,
        body: &str,
    ) -> Result<String, Box<dyn Error>> {
        let (got, first) = self.post(path, secret, key, body).await?;
        assert_eq!(got, status, "{path}: {first}");
        let (got, again) = self.post(path, secret, key, body).await?;
        assert_eq!(got, status, "{path}, sent again: {again}");
        assert_eq!(again, first, "{path}, sent again");
        Ok(first)
    }

    /// Copies of a `POST`, all at once and each on a connection of its own, on the server's
    /// several threads. Every copy must get this status. Their bodies.
    async fn raced(
        &self,
        status: &str,
        path: &str,
        secret: &str,
        key: &str,
        body: &str,
    ) -> Result<Vec<String>, Box<dyn Error>> {
        check_copies(
            status,
            path,
            join_all(self.copies(path, secret, key, body)).await,
        )
    }

    /// The copies of a `POST`, not yet sent.
    fn copies<'a>(
        &'a self,
        path: &'a str,
        secret: &'a str,
        key: &'a str,
        body: &'a str,
    ) -> Vec<impl Future<Output = Result<(String, String), Box<dyn Error>>> + 'a> {
        (0..COPIES)
            .map(|_| self.post(path, secret, key, body))
            .collect()
    }

    /// Ends the process, which has to happen before the test returns: `#[sqlx::test]`
    /// drops its database without `FORCE`, which PostgreSQL refuses while a connection is
    /// open. It returns everything that the server wrote: a pipe ends with its process,
    /// so waiting for the collectors means that no line is still on its way.
    async fn stop(self) -> Result<String, Box<dyn Error>> {
        let killed = self.kill().await?;
        let log = killed
            .log
            .lock()
            .expect("no test panics while holding this");
        Ok(log.clone())
    }

    /// Ends the process at once. It has no graceful shutdown to skip, so this is what any stop
    /// amounts to, and a crash as well (`docs/api.md`).
    async fn kill(mut self) -> Result<Killed, Box<dyn Error>> {
        self.child.kill().await?;
        self.child.wait().await?;
        for collector in self.collectors {
            collector.await?;
        }
        Ok(Killed {
            operator: self.operator,
            log: self.log,
        })
    }

    /// A `POST` on a connection of its own, whose answer nobody waits for. The connection stays
    /// open for as long as the test holds it.
    async fn sent(
        &self,
        path: &str,
        secret: &str,
        key: &str,
        body: &str,
    ) -> Result<TcpStream, Box<dyn Error>> {
        let mut stream = TcpStream::connect(&self.address).await?;
        let length = body.len();
        stream
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {secret}\r\n\
                     Idempotency-Key: \"{key}\"\r\nContent-Length: {length}\r\n\r\n{body}"
                )
                .as_bytes(),
            )
            .await?;
        Ok(stream)
    }
}

/// A merchant with an API key and a payment, there before the server starts.
async fn seeded(pool: &PgPool) -> Result<(ApiKeySecret, PaymentId), Box<dyn Error>> {
    let pools = Pools::login(pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&pools.system, merchant, fee_rate()).await?;
    let secret = issued_key(&pools.system, merchant, 1).await?;
    let command = create_command(1, merchant);
    create_payment(Tenant::new(&pools.tenant, merchant), &command).await?;
    pools.close().await;
    Ok((secret, command.id))
}

/// The head of a read of this payment, presenting this secret.
fn read(payment: PaymentId, secret: &ApiKeySecret) -> String {
    format!(
        "GET /payments/{payment} HTTP/1.1\r\nAuthorization: Bearer {}",
        secret.expose()
    )
}

/// What no log line may hold, whatever else happens (ADR-0024).
#[track_caller]
fn assert_no_credential(log: &str, secret: &str) {
    assert!(!log.contains(secret), "the secret: {log}");
    assert!(!log.contains("Bearer"), "the header: {log}");
    assert!(!log.contains("pck_"), "the secret's prefix: {log}");
}

/// That each copy got this status, and their bodies.
fn check_copies(
    status: &str,
    path: &str,
    replies: Vec<Result<(String, String), Box<dyn Error>>>,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut bodies = Vec::new();
    for reply in replies {
        let (got, body) = reply?;
        assert_eq!(got, status, "{path}: {body}");
        bodies.push(body);
    }
    Ok(bodies)
}

/// The one answer that all of these copies must have got.
#[track_caller]
fn one_answer(bodies: &[String]) -> &str {
    for body in bodies {
        assert_eq!(body, &bodies[0], "{bodies:?}");
    }
    &bodies[0]
}

/// A text member of a JSON body.
#[track_caller]
fn member(body: &str, name: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(body).expect("a JSON body");
    parsed[name]
        .as_str()
        .unwrap_or_else(|| panic!("no {name}: {body}"))
        .to_owned()
}

/// A merchant's ID as a reply wrote it.
fn merchant_of(text: &str) -> Result<MerchantId, Box<dyn Error>> {
    Ok(MerchantId::try_from(Uuid::parse_str(text)?)?)
}

/// How many merchants are stored. Pass the test's own pool.
async fn merchants(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM merchants"#)
        .fetch_one(pool)
        .await
}

async fn api_keys(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM api_keys"#)
        .fetch_one(pool)
        .await
}

async fn revocations(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM api_key_revocations"#)
        .fetch_one(pool)
        .await
}

async fn payments(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payments"#)
        .fetch_one(pool)
        .await
}

/// Every final answer stored, under anyone's key.
async fn stored_answers(pool: &PgPool) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        r#"SELECT body AS "body!" FROM operator_idempotency_records
           UNION ALL SELECT body FROM idempotency_records"#
    )
    .fetch_all(pool)
    .await
}

#[sqlx::test]
async fn the_whole_flow_runs_on_the_binary_and_moves_the_money_once(pool: PgPool) -> TestResult {
    let server = Server::start(&pool).await?;
    let operator = server.operator.expose().to_owned();

    // The operator takes a merchant on.
    let body = server
        .applied(CREATED, "/merchants", &operator, "register", REGISTER)
        .await?;
    let merchant = member(&body, "merchant_id");
    let keys = format!("/merchants/{merchant}/api-keys");
    let (status, first) = server.post(&keys, &operator, "issue", "").await?;
    assert_eq!(status, CREATED, "{first}");
    let (api_key, secret) = (member(&first, "api_key_id"), member(&first, "secret"));
    let (status, second) = server.post(&keys, &operator, "issue", "").await?;
    assert_eq!(status, CREATED, "{second}");
    assert_eq!(second, format!(r#"{{"api_key_id":"{api_key}"}}"#));

    // The merchant takes a payment through its life, each step sent twice.
    let body = server
        .applied(CREATED, "/payments", &secret, "create", CREATE)
        .await?;
    let payment = member(&body, "payment_id");
    let at = |verb: &str| format!("/payments/{payment}/{verb}");
    let authorize = at("authorize");
    let body = server
        .applied(OK, &authorize, &secret, "authorize", AUTHORIZE)
        .await?;
    assert_eq!(
        body,
        r#"{"seq":1,"from":"created","to":"authorized","refundable":0}"#
    );
    let capture = at("capture");
    let body = server
        .applied(OK, &capture, &secret, "capture", CAPTURE)
        .await?;
    assert_eq!(
        body,
        r#"{"seq":2,"from":"authorized","to":"captured","refundable":8000}"#
    );
    let refund = at("refund");
    let partial = r#"{"amount":3000,"refunded_at":"2026-12-02T00:00:00Z"}"#;
    let body = server
        .applied(OK, &refund, &secret, "refund", partial)
        .await?;
    assert_eq!(
        body,
        r#"{"seq":3,"from":"captured","to":"captured","refundable":5000}"#
    );

    // The money moved once, whatever was sent twice.
    assert_eq!(
        balances(&pool, merchant_of(&merchant)?).await?,
        [5_000, FEE - 5_000, -FEE]
    );
    assert_books_are_whole(&pool).await?;
    let read = format!("/payments/{payment}");
    let (status, body) = server.get(&read, &secret).await?;
    assert_eq!(status, OK, "{body}");
    assert!(body.contains(r#""fee_bps":250"#), "{body}");
    assert!(body.contains(r#""status":"captured""#), "{body}");
    assert!(body.contains(r#""refundable":5000"#), "{body}");

    // The operator revokes the key, which stops on its next request.
    let revoke = format!("/api-keys/{api_key}/revoke");
    let body = server.applied(OK, &revoke, &operator, "revoke", "").await?;
    assert_eq!(body, format!(r#"{{"api_key_id":"{api_key}"}}"#));
    let (status, _) = server.get(&read, &secret).await?;
    assert_eq!(status, UNAUTHORIZED);

    // Neither the database's answers nor the server's log holds a credential. The control:
    // the first answer did.
    assert!(first.contains("pck_"), "{first}");
    let answers = stored_answers(&pool).await?;
    assert_eq!(answers.len(), 7, "{answers:?}");
    for answer in &answers {
        assert!(!answer.contains("pck_"), "{answer}");
    }
    let log = server.stop().await?;
    assert!(!log.contains("ERROR"), "{log}");
    assert_no_credential(&log, &operator);
    assert_no_credential(&log, &secret);
    Ok(())
}

#[sqlx::test]
async fn copies_of_every_command_racing_on_the_binary_give_one_effect(pool: PgPool) -> TestResult {
    // On the server's own threads, as far as timing lets them overlap. The two tests below
    // make certain that they do.
    let server = Server::start(&pool).await?;
    let operator = server.operator.expose().to_owned();

    let bodies = server
        .raced(CREATED, "/merchants", &operator, "register", REGISTER)
        .await?;
    let merchant = member(one_answer(&bodies), "merchant_id");
    assert_eq!(merchants(&pool).await?, 1);

    let keys = format!("/merchants/{merchant}/api-keys");
    let bodies = server.raced(CREATED, &keys, &operator, "issue", "").await?;
    let api_key = member(&bodies[0], "api_key_id");
    for body in &bodies {
        assert_eq!(member(body, "api_key_id"), api_key, "{bodies:?}");
    }
    let shown: Vec<&String> = bodies.iter().filter(|b| b.contains("pck_")).collect();
    assert_eq!(shown.len(), 1, "{bodies:?}");
    let secret = member(shown[0], "secret");
    assert_eq!(api_keys(&pool).await?, 1);

    let bodies = server
        .raced(CREATED, "/payments", &secret, "create", CREATE)
        .await?;
    let payment = member(one_answer(&bodies), "payment_id");
    assert_eq!(payments(&pool).await?, 1);
    let id = PaymentId::try_from(Uuid::parse_str(&payment)?)?;
    let at = |verb: &str| format!("/payments/{payment}/{verb}");

    let authorize = at("authorize");
    let bodies = server
        .raced(OK, &authorize, &secret, "authorize", AUTHORIZE)
        .await?;
    one_answer(&bodies);
    assert_eq!(steps(&pool, id).await?, 2);

    let capture = at("capture");
    let bodies = server
        .raced(OK, &capture, &secret, "capture", CAPTURE)
        .await?;
    one_answer(&bodies);
    assert_eq!(steps(&pool, id).await?, 3);

    // The sharpest: a partial refund still fits after it applies, so without the key each
    // copy would move the money again.
    let refund = at("refund");
    let partial = r#"{"amount":1000,"refunded_at":"2026-12-02T00:00:00Z"}"#;
    let bodies = server
        .raced(OK, &refund, &secret, "refund", partial)
        .await?;
    one_answer(&bodies);
    assert_eq!(steps(&pool, id).await?, 4);
    assert_eq!(
        balances(&pool, merchant_of(&merchant)?).await?,
        [7_000, FEE - 7_000, -FEE]
    );
    assert_books_are_whole(&pool).await?;

    let revoke = format!("/api-keys/{api_key}/revoke");
    let bodies = server.raced(OK, &revoke, &operator, "revoke", "").await?;
    one_answer(&bodies);
    assert_eq!(revocations(&pool).await?, 1);

    let log = server.stop().await?;
    assert!(!log.contains("ERROR"), "{log}");
    assert_no_credential(&log, &operator);
    assert_no_credential(&log, &secret);
    Ok(())
}

#[sqlx::test]
async fn copies_held_behind_a_key_at_once_on_the_binary_register_one_merchant(
    pool: PgPool,
) -> TestResult {
    // A registration has no lock, so it waits in its key's unique index. The test counts every
    // copy waiting there at the same moment, behind an attempt of its own that then rolls
    // back. One then wins, and the others lose the race for the key and find its answer.
    let server = Server::start(&pool).await?;
    let operator = server.operator.expose().to_owned();
    let fingerprint = Fingerprint::of("POST", "/merchants", REGISTER.as_bytes());
    let mut first = pool.begin().await?;
    sqlx::query!(
        "INSERT INTO operator_idempotency_records (key, fingerprint, status, body) \
         VALUES ('race', $1, 201, '{}')",
        fingerprint.as_bytes()
    )
    .execute(&mut *first)
    .await?;

    let (replies, released) = join(
        join_all(server.copies("/merchants", &operator, "race", REGISTER)),
        async {
            wait_for_lock_waits(&pool, "transactionid", COPIES).await?;
            first.rollback().await
        },
    )
    .await;

    released?;
    let bodies = check_copies(CREATED, "/merchants", replies)?;
    one_answer(&bodies);
    assert_eq!(merchants(&pool).await?, 1);
    let log = server.stop().await?;
    assert!(!log.contains("ERROR"), "{log}");
    Ok(())
}

#[sqlx::test]
async fn copies_held_behind_a_payments_lock_at_once_on_the_binary_move_the_money_once(
    pool: PgPool,
) -> TestResult {
    // The test counts every copy of a capture waiting on the payment's lock at the same moment.
    // The first to get it applies. Each of the others then finds a payment already captured,
    // loses the race for the key, and rolls back whole, so it gets the first answer and not
    // a refusal.
    let (secret, payment) = seeded(&pool).await?;
    let server = Server::start(&pool).await?;
    let authorize = format!("/payments/{payment}/authorize");
    let (status, body) = server
        .post(&authorize, secret.expose(), "authorize", AUTHORIZE)
        .await?;
    assert_eq!(status, OK, "{body}");
    let capture = format!("/payments/{payment}/capture");
    let mut holder = pool.begin().await?;
    take_lock(&mut holder, payment).await?;

    let (replies, released) = join(
        join_all(server.copies(&capture, secret.expose(), "race", CAPTURE)),
        async {
            wait_for_lock_waits(&pool, "advisory", COPIES).await?;
            holder.rollback().await
        },
    )
    .await;

    released?;
    let bodies = check_copies(OK, &capture, replies)?;
    assert_eq!(
        one_answer(&bodies),
        r#"{"seq":2,"from":"authorized","to":"captured","refundable":8000}"#
    );
    assert_eq!(steps(&pool, payment).await?, 3);
    assert_eq!(
        balances(&pool, merchant_id(1)).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );
    assert_books_are_whole(&pool).await?;
    let log = server.stop().await?;
    assert!(!log.contains("ERROR"), "{log}");
    assert_no_credential(&log, secret.expose());
    Ok(())
}

/// Waits until the sessions of a server that was killed have ended. A session ends when it next
/// reads from its client, and one waiting on a lock reads nothing until it has that lock, so
/// call this once those locks are free.
async fn wait_for_sessions_to_end(pool: &PgPool) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = sqlx::query_scalar!(
            r#"SELECT count(*) AS "left!" FROM pg_stat_activity
                WHERE datname = current_database()
                  AND usename IN ('payments_tenant', 'payments_app')"#
        )
        .fetch_one(pool)
        .await?;
        if left == 0 {
            return Ok(());
        }
        assert!(
            Instant::now() < deadline,
            "{left} sessions outlived the server"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

#[sqlx::test]
async fn what_waits_when_the_server_is_killed_is_gone_and_applies_once_after_a_restart(
    pool: PgPool,
) -> TestResult {
    // A capture waits on its payment's lock, and a registration on its key's unique index, so
    // neither can have reached its COMMIT when the process goes. Its sessions then roll back,
    // however they end, and the server that comes up next has nothing to clean up.
    let (secret, payment) = seeded(&pool).await?;
    let server = Server::start(&pool).await?;
    let operator = server.operator.expose().to_owned();
    let authorize = format!("/payments/{payment}/authorize");
    let (status, body) = server
        .post(&authorize, secret.expose(), "authorize", AUTHORIZE)
        .await?;
    assert_eq!(status, OK, "{body}");
    let capture = format!("/payments/{payment}/capture");

    let mut holder = pool.begin().await?;
    take_lock(&mut holder, payment).await?;
    let fingerprint = Fingerprint::of("POST", "/merchants", REGISTER.as_bytes());
    let mut first = pool.begin().await?;
    sqlx::query!(
        "INSERT INTO operator_idempotency_records (key, fingerprint, status, body) \
         VALUES ('race', $1, 201, '{}')",
        fingerprint.as_bytes()
    )
    .execute(&mut *first)
    .await?;
    let answers = stored_answers(&pool).await?.len();

    let capturing = server
        .sent(&capture, secret.expose(), "capture", CAPTURE)
        .await?;
    let registering = server
        .sent("/merchants", &operator, "race", REGISTER)
        .await?;
    wait_for_lock_waits(&pool, "advisory", 1).await?;
    wait_for_lock_waits(&pool, "transactionid", 1).await?;

    let killed = server.kill().await?;
    // They outlive the process for as long as they wait: PostgreSQL meets a lost client only
    // when it next touches the socket, which is when each gets what it waits for.
    wait_for_lock_waits(&pool, "advisory", 1).await?;
    wait_for_lock_waits(&pool, "transactionid", 1).await?;
    holder.rollback().await?;
    first.rollback().await?;
    wait_for_sessions_to_end(&pool).await?;
    drop((capturing, registering));

    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(merchants(&pool).await?, 1);
    assert_eq!(balances(&pool, merchant_id(1)).await?, [0, 0, 0]);
    assert_eq!(stored_answers(&pool).await?.len(), answers);

    let server = killed.restart(&pool).await?;
    let body = server
        .applied(OK, &capture, secret.expose(), "capture", CAPTURE)
        .await?;
    assert_eq!(
        body,
        r#"{"seq":2,"from":"authorized","to":"captured","refundable":8000}"#
    );
    assert_eq!(steps(&pool, payment).await?, 3);
    assert_eq!(
        balances(&pool, merchant_id(1)).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );
    server
        .applied(CREATED, "/merchants", &operator, "race", REGISTER)
        .await?;
    assert_eq!(merchants(&pool).await?, 2);
    assert_books_are_whole(&pool).await?;

    let log = server.stop().await?;
    assert!(!log.contains("ERROR"), "{log}");
    assert_no_credential(&log, &operator);
    assert_no_credential(&log, secret.expose());
    Ok(())
}

#[sqlx::test]
async fn an_answer_that_nobody_received_outlives_the_server_that_gave_it(
    pool: PgPool,
) -> TestResult {
    // The other side of a crash: the capture committed, and the process went before anyone read
    // its answer. Only the key's record knows, and it needs nothing from the process.
    let (secret, payment) = seeded(&pool).await?;
    let server = Server::start(&pool).await?;
    let authorize = format!("/payments/{payment}/authorize");
    let (status, body) = server
        .post(&authorize, secret.expose(), "authorize", AUTHORIZE)
        .await?;
    assert_eq!(status, OK, "{body}");
    let capture = format!("/payments/{payment}/capture");

    let capturing = server
        .sent(&capture, secret.expose(), "capture", CAPTURE)
        .await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while steps(&pool, payment).await? < 3 {
        assert!(Instant::now() < deadline, "the capture did not land");
        sleep(Duration::from_millis(10)).await;
    }
    let killed = server.kill().await?;
    wait_for_sessions_to_end(&pool).await?;
    drop(capturing);

    let server = killed.restart(&pool).await?;
    let (status, body) = server
        .post(&capture, secret.expose(), "capture", CAPTURE)
        .await?;
    assert_eq!(status, OK, "{body}");
    assert_eq!(
        body,
        r#"{"seq":2,"from":"authorized","to":"captured","refundable":8000}"#
    );
    assert_eq!(steps(&pool, payment).await?, 3);
    assert_eq!(
        balances(&pool, merchant_id(1)).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );
    assert_books_are_whole(&pool).await?;

    // The control: under a new key this is a new request, so the table of moves decides, and
    // refuses. So the answer above came from the key, and this server does decide.
    let (status, body) = server
        .post(&capture, secret.expose(), "another", CAPTURE)
        .await?;
    assert_eq!(status, "HTTP/1.1 409 Conflict", "{body}");
    server.stop().await?;
    Ok(())
}

#[sqlx::test]
async fn a_failed_lookup_logs_its_cause_and_never_the_credential(pool: PgPool) -> TestResult {
    let (secret, payment) = seeded(&pool).await?;
    let server = Server::start(&pool).await?;
    // The table goes as the owner, so the server's next statement fails. No statement has
    // been prepared on these connections yet, so none still points at the table.
    sqlx::query!("ALTER TABLE api_keys RENAME TO api_keys_gone")
        .execute(&pool)
        .await?;

    let (status, body) = server.exchange(&read(payment, &secret)).await?;

    assert_eq!(status, "HTTP/1.1 503 Service Unavailable", "{body}");
    // The client hears nothing of the cause, and the log hears all of it.
    assert_eq!(body, Problem::Unavailable.body());
    let log = server.stop().await?;
    assert!(log.contains("an API key could not be looked up"), "{log}");
    assert!(log.contains("api_keys"), "the database's own cause: {log}");
    assert!(log.contains("503"), "{log}");
    assert_no_credential(&log, secret.expose());
    Ok(())
}

#[sqlx::test]
async fn a_failed_read_logs_its_cause_and_never_the_credential(pool: PgPool) -> TestResult {
    let (secret, payment) = seeded(&pool).await?;
    let server = Server::start(&pool).await?;
    sqlx::query!("ALTER TABLE payments RENAME TO payments_gone")
        .execute(&pool)
        .await?;

    let (status, body) = server.exchange(&read(payment, &secret)).await?;

    assert_eq!(status, "HTTP/1.1 503 Service Unavailable", "{body}");
    assert_eq!(body, Problem::Unavailable.body());
    let log = server.stop().await?;
    assert!(log.contains("could not be read"), "{log}");
    assert!(log.contains("payments"), "the database's own cause: {log}");
    assert_no_credential(&log, secret.expose());
    Ok(())
}

#[sqlx::test]
async fn a_request_refused_for_its_own_sake_logs_nothing(pool: PgPool) -> TestResult {
    // Only what is wrong with the server belongs in its log. Anyone can send these, and
    // a log that grew with them could be flooded.
    let (secret, payment) = seeded(&pool).await?;
    let server = Server::start(&pool).await?;
    let unknown = ApiKeySecret::generate();
    for head in [
        read(payment, &secret),
        read(payment, &unknown),
        format!("GET /payments/{payment} HTTP/1.1"),
        "GET /nothing HTTP/1.1".to_owned(),
        "DELETE /nothing HTTP/1.1".to_owned(),
        // Someone trying the operator's routes with a key that opens none of them.
        format!(
            "POST /merchants HTTP/1.1\r\nAuthorization: Bearer {}",
            unknown.expose()
        ),
    ] {
        server.exchange(&head).await?;
    }
    let log = server.stop().await?;
    assert!(!log.contains("ERROR"), "{log}");
    assert_no_credential(&log, secret.expose());
    assert_no_credential(&log, unknown.expose());
    Ok(())
}

#[sqlx::test]
async fn the_binary_serves_a_payment_on_its_own_runtime(pool: PgPool) -> TestResult {
    let (secret, payment) = seeded(&pool).await?;

    let server = Server::start(&pool).await?;

    let (status, body) = server.exchange(&read(payment, &secret)).await?;
    assert_eq!(status, "HTTP/1.1 200 OK", "{body}");
    assert!(body.contains(r#""status":"created""#), "{body}");

    let (status, _) = server
        .exchange(&format!("GET /payments/{payment} HTTP/1.1"))
        .await?;
    assert_eq!(status, "HTTP/1.1 401 Unauthorized");

    let (status, _) = server.exchange("GET /nothing HTTP/1.1").await?;
    assert_eq!(status, "HTTP/1.1 404 Not Found");

    server.stop().await?;
    Ok(())
}

/// What the binary does with this configuration, when it cannot start: its exit status
/// and everything that it wrote.
async fn refused(mut command: Command) -> Result<(bool, String), Box<dyn Error>> {
    let output = timeout(Duration::from_secs(30), command.output()).await??;
    let written = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok((output.status.success(), written))
}

#[sqlx::test]
async fn the_binary_stops_at_once_without_its_configuration(pool: PgPool) -> TestResult {
    for name in [
        "TENANT_DATABASE_URL",
        "SYSTEM_DATABASE_URL",
        "OPERATOR_KEY_SHA256",
        "MOCK_PSP_SIGNING_KEYS",
    ] {
        let mut command = configured(&pool, &ApiKeySecret::generate());
        command.env_remove(name);
        let (succeeded, written) = refused(command).await?;
        assert!(!succeeded, "{name}: {written}");
        assert!(written.contains(name), "{name}: {written}");
        // Set but empty, as .env.example leaves the secrets, counts as not set.
        let mut command = configured(&pool, &ApiKeySecret::generate());
        command.env(name, "");
        let (succeeded, written) = refused(command).await?;
        assert!(!succeeded, "{name} empty: {written}");
        assert!(written.contains(name), "{name} empty: {written}");
    }
    Ok(())
}

#[sqlx::test]
async fn the_binary_flags_a_notification_about_a_payment_that_it_does_not_hold(
    pool: PgPool,
) -> TestResult {
    let fake = MockPsp::start().await?;
    let server = Server::start_next_to(&pool, &fake).await?;
    fake.deliver_to(&format!("http://{}/webhooks/mock", server.address));
    let unknown = test_uuid(9);
    let event = fake.notify_again(unknown);
    let answer = fake.deliver(&event).await;
    assert_eq!(
        answer,
        Attempt::Answered {
            status: 202,
            retry_after: None
        }
    );
    // An operator learns of it from the log, since no retry can change it.
    server
        .until_logged(&format!(
            "notification {event} names payment {unknown}, which the service does not hold"
        ))
        .await;
    server.stop().await?;
    Ok(())
}

#[sqlx::test]
async fn the_binary_stops_at_once_on_signing_keys_it_cannot_read_and_never_shows_them(
    pool: PgPool,
) -> TestResult {
    // Each value, and the part of it that must never be shown: a key's base64, or the
    // whole value when it is no key at all.
    let good = SigningKey::generate().written();
    let bare = good.trim_start_matches("whsec_").to_owned();
    for (value, secret) in [
        ("not a key".to_owned(), "not a key".to_owned()),
        (bare.clone(), bare.clone()),
        ("whsec_c2hvcnQ=".to_owned(), "c2hvcnQ=".to_owned()),
        ("whsec_not!base64".to_owned(), "not!base64".to_owned()),
        (format!("{good} whsec_oops"), bare),
    ] {
        let mut command = configured(&pool, &ApiKeySecret::generate());
        command.env("MOCK_PSP_SIGNING_KEYS", &value);
        let (succeeded, said) = refused(command).await?;
        assert!(!succeeded, "{said}");
        assert!(said.contains("MOCK_PSP_SIGNING_KEYS"), "{said}");
        assert!(!said.contains("listening on"), "{said}");
        assert!(!said.contains(&secret), "{said}");
    }
    Ok(())
}

#[sqlx::test]
async fn the_binary_stops_at_once_on_an_operator_key_that_is_no_digest_and_never_shows_it(
    pool: PgPool,
) -> TestResult {
    // The likeliest mistake is pasting the secret itself, so what it read is never repeated.
    let operator = ApiKeySecret::generate();
    let digest = written(operator.digest());
    for value in [
        operator.expose(),
        "not a digest",
        &digest.to_uppercase(),
        &digest[..63],
    ] {
        let mut command = configured(&pool, &operator);
        command.env("OPERATOR_KEY_SHA256", value);
        let (succeeded, said) = refused(command).await?;
        assert!(!succeeded, "{said}");
        assert!(said.contains("OPERATOR_KEY_SHA256"), "{said}");
        assert!(!said.contains("listening on"), "{said}");
        assert!(!said.contains(value), "{said}");
    }
    // Nothing is no digest either, which is what .env.example holds.
    let mut command = configured(&pool, &operator);
    command.env("OPERATOR_KEY_SHA256", "");
    let (succeeded, said) = refused(command).await?;
    assert!(!succeeded, "{said}");
    assert!(said.contains("OPERATOR_KEY_SHA256"), "{said}");
    Ok(())
}

#[sqlx::test]
async fn the_binary_stops_at_once_on_an_address_it_cannot_use(pool: PgPool) -> TestResult {
    let mut command = configured(&pool, &ApiKeySecret::generate());
    command.env("BIND_ADDRESS", "not an address");
    let (succeeded, written) = refused(command).await?;
    assert!(!succeeded, "{written}");
    assert!(!written.contains("listening on"), "{written}");
    Ok(())
}

#[sqlx::test]
async fn a_login_that_fails_stops_the_binary_and_shows_no_password(pool: PgPool) -> TestResult {
    let options = pool.connect_options();
    let wrong = format!(
        "postgres://payments_tenant:not-the-password@{}:{}/{}",
        options.get_host(),
        options.get_port(),
        options.get_database().expect("a test's own database")
    );
    let mut command = configured(&pool, &ApiKeySecret::generate());
    command.env("TENANT_DATABASE_URL", wrong);
    let (succeeded, written) = refused(command).await?;
    assert!(!succeeded, "{written}");
    assert!(!written.is_empty(), "it stopped without saying why");
    assert!(!written.contains("not-the-password"), "{written}");
    Ok(())
}
