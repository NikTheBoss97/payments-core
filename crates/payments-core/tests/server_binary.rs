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

use common::api::Pools;
use common::keys::issued_key;
use common::merchant_id;
use common::payments::{create_command, fee_rate};
use payments_core::payments::{create_payment, register_merchant};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
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
fn configured(pool: &PgPool) -> Command {
    let mut command = Command::new(BINARY);
    command
        .env("TENANT_DATABASE_URL", url(pool, "payments_tenant"))
        .env("SYSTEM_DATABASE_URL", url(pool, "payments_app"))
        .env("BIND_ADDRESS", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// A running server, and everything that it has written so far.
struct Server {
    child: Child,
    address: String,
    log: Arc<Mutex<String>>,
}

/// Keeps a stream's lines in the log, so that the process never blocks on a full pipe.
fn collect(stream: impl AsyncRead + Unpin + Send + 'static, log: &Arc<Mutex<String>>) {
    let log = log.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut log = log.lock().expect("no test panics while holding this");
            log.push_str(&line);
            log.push('\n');
        }
    });
}

impl Server {
    /// Starts the binary, and waits until it says where it is listening. The port is one
    /// that the system chose, so tests never collide.
    async fn start(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let mut child = configured(pool).spawn()?;
        let stdout = child.stdout.take().expect("a piped stdout");
        let stderr = child.stderr.take().expect("a piped stderr");
        let log = Arc::new(Mutex::new(String::new()));
        collect(stderr, &log);
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
        collect(lines.into_inner(), &log);
        Ok(Self {
            child,
            address,
            log,
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
    /// open.
    async fn stop(mut self) -> Result<String, Box<dyn Error>> {
        self.child.kill().await?;
        self.child.wait().await?;
        let log = self.log.lock().expect("no test panics while holding this");
        Ok(log.clone())
    }
}

#[sqlx::test]
async fn the_binary_serves_a_payment_on_its_own_runtime(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&pools.system, merchant, fee_rate()).await?;
    let secret = issued_key(&pools.system, merchant, 1).await?;
    let command = create_command(1, merchant);
    create_payment(Tenant::new(&pools.tenant, merchant), &command).await?;
    let payment = command.id;
    pools.close().await;

    let server = Server::start(&pool).await?;

    let (status, body) = server
        .exchange(&format!(
            "GET /payments/{payment} HTTP/1.1\r\nAuthorization: Bearer {}",
            secret.expose()
        ))
        .await?;
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
    for name in ["TENANT_DATABASE_URL", "SYSTEM_DATABASE_URL"] {
        let mut command = configured(&pool);
        command.env_remove(name);
        let (succeeded, written) = refused(command).await?;
        assert!(!succeeded, "{name}: {written}");
        assert!(written.contains(name), "{name}: {written}");
    }
    Ok(())
}

#[sqlx::test]
async fn the_binary_stops_at_once_on_an_address_it_cannot_use(pool: PgPool) -> TestResult {
    let mut command = configured(&pool);
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
    let mut command = configured(&pool);
    command.env("TENANT_DATABASE_URL", wrong);
    let (succeeded, written) = refused(command).await?;
    assert!(!succeeded, "{written}");
    assert!(!written.is_empty(), "it stopped without saying why");
    assert!(!written.contains("not-the-password"), "{written}");
    Ok(())
}
