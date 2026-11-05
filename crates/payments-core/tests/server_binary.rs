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
use std::time::Duration;

use common::api::{Pools, written};
use common::keys::issued_key;
use common::merchant_id;
use common::payments::{create_command, fee_rate};
use payments_core::api::problem::Problem;
use payments_core::credentials::ApiKeySecret;
use payments_core::ids::PaymentId;
use payments_core::payments::{create_payment, register_merchant};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;

type TestResult = Result<(), Box<dyn Error>>;

const BINARY: &str = env!("CARGO_BIN_EXE_payments-core");

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
        .env("BIND_ADDRESS", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// A running server, and everything that it has written since it said where it listens.
struct Server {
    child: Child,
    address: String,
    log: Arc<Mutex<String>>,
    collectors: Vec<JoinHandle<()>>,
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
        let mut child = configured(pool, &ApiKeySecret::generate()).spawn()?;
        let stdout = child.stdout.take().expect("a piped stdout");
        let stderr = child.stderr.take().expect("a piped stderr");
        let log = Arc::new(Mutex::new(String::new()));
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
            log,
            collectors: vec![from_stderr, from_stdout],
        })
    }

    /// One request on a connection of its own. The answer's status line and its body.
    async fn exchange(&self, request_head: &str) -> Result<(String, String), Box<dyn Error>> {
        let mut stream = TcpStream::connect(&self.address).await?;
        stream
            .write_all(
                format!("{request_head}\r\nHost: test\r\nConnection: close\r\n\r\n").as_bytes(),
            )
            .await?;
        let mut answer = String::new();
        timeout(Duration::from_secs(30), stream.read_to_string(&mut answer)).await??;
        let (head, body) = answer.split_once("\r\n\r\n").unwrap_or((&answer, ""));
        let status = head.lines().next().unwrap_or_default().to_owned();
        Ok((status, body.to_owned()))
    }

    /// Ends the process, which has to happen before the test returns: `#[sqlx::test]`
    /// drops its database without `FORCE`, which PostgreSQL refuses while a connection is
    /// open. It returns everything that the server wrote: a pipe ends with its process,
    /// so waiting for the collectors means that no line is still on its way.
    async fn stop(mut self) -> Result<String, Box<dyn Error>> {
        self.child.kill().await?;
        self.child.wait().await?;
        for collector in self.collectors {
            collector.await?;
        }
        let log = self.log.lock().expect("no test panics while holding this");
        Ok(log.clone())
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
fn assert_no_credential(log: &str, secret: &ApiKeySecret) {
    assert!(!log.contains(secret.expose()), "the secret: {log}");
    assert!(!log.contains("Bearer"), "the header: {log}");
    assert!(!log.contains("pck_"), "the secret's prefix: {log}");
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
    assert_no_credential(&log, &secret);
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
    assert_no_credential(&log, &secret);
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
    assert_no_credential(&log, &secret);
    assert_no_credential(&log, &unknown);
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
    ] {
        let mut command = configured(&pool, &ApiKeySecret::generate());
        command.env_remove(name);
        let (succeeded, written) = refused(command).await?;
        assert!(!succeeded, "{name}: {written}");
        assert!(written.contains(name), "{name}: {written}");
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
