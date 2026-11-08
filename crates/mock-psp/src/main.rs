// Author: Nicola Antonello

//! The fake, served for local runs (ADR-0029). It approves each request as it arrives,
//! or refuses one that it could not approve, and every second takes the steps that a
//! test takes by hand: it delivers the notifications that are due, settles the captures
//! older than a delay, lets authorizations lapse at their expiry, and forgets the
//! idempotency keys a day old. Its records live in memory and end with the process.
//!
//! It reads the environment:
//! - `MOCK_PSP_ADDRESS`: where it listens, `127.0.0.1:4100` unless set;
//! - `MOCK_PSP_WEBHOOK_URL`: the server's `POST /webhooks/mock`;
//! - `MOCK_PSP_API_KEY`: the key that the server's calls carry;
//! - `MOCK_PSP_SIGNING_KEYS`: the keys that sign, written `whsec_` and base64 and
//!   separated by spaces, the current one first;
//! - `MOCK_PSP_SETTLE_AFTER_SECONDS`: how long a capture waits to settle, 60 unless set.

use std::env;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use mock_psp::signing::SigningKey;
use mock_psp::{Config, MockPsp, Policy};

/// What the environment configures.
struct Settings {
    config: Config,
    webhook_url: String,
    settle_after: TimeDelta,
}

fn variable(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is not set"))
}

fn read_settings() -> Result<Settings, String> {
    let address: SocketAddr = env::var("MOCK_PSP_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:4100".to_owned())
        .parse()
        .map_err(|_| "MOCK_PSP_ADDRESS is not an address and port".to_owned())?;
    let signing_keys = variable("MOCK_PSP_SIGNING_KEYS")?
        .split_whitespace()
        .map(SigningKey::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("MOCK_PSP_SIGNING_KEYS: {error}"))?;
    if signing_keys.is_empty() {
        return Err("MOCK_PSP_SIGNING_KEYS holds no key".to_owned());
    }
    let settle_after = match env::var("MOCK_PSP_SETTLE_AFTER_SECONDS") {
        Ok(seconds) => seconds
            .parse::<u32>()
            .map_err(|_| "MOCK_PSP_SETTLE_AFTER_SECONDS is not a number of seconds".to_owned())?,
        Err(_) => 60,
    };
    Ok(Settings {
        config: Config {
            address,
            policy: Policy::ApproveAtOnce,
            api_key: variable("MOCK_PSP_API_KEY")?,
            signing_keys,
            ..Config::for_tests()
        },
        webhook_url: variable("MOCK_PSP_WEBHOOK_URL")?,
        settle_after: TimeDelta::seconds(i64::from(settle_after)),
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let settings = match read_settings() {
        Ok(settings) => settings,
        Err(problem) => {
            // Nothing that the environment holds is repeated, since it holds secrets.
            eprintln!("mock-psp: {problem}");
            return ExitCode::FAILURE;
        }
    };
    let psp = match MockPsp::start_with(settings.config).await {
        Ok(psp) => psp,
        Err(error) => {
            eprintln!("mock-psp: cannot listen: {error}");
            return ExitCode::FAILURE;
        }
    };
    if settings.webhook_url.parse::<axum::http::Uri>().is_err() {
        eprintln!("mock-psp: MOCK_PSP_WEBHOOK_URL is not a URI");
        return ExitCode::FAILURE;
    }
    psp.deliver_to(&settings.webhook_url);
    eprintln!("mock-psp: listening on {}", psp.url());

    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    loop {
        ticks.tick().await;
        let now = Utc::now();
        for (message_id, attempt) in psp.deliver_due(now).await {
            eprintln!("mock-psp: delivered {message_id}: {attempt:?}");
        }
        psp.settle_captured_by(now - settings.settle_after);
        psp.expire_lapsed(now);
        psp.prune_keys(now);
    }
}
