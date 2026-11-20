// Author: Nicola Antonello

//! The server (ADR-0023). It reads its configuration, logs in as the two roles, and
//! serves, with the sender's task and the relay's beside the routes (ADR-0043, ADR-0044,
//! ADR-0052). It never runs migrations (ADR-0008), and it speaks plain HTTP: TLS ends in
//! front of it.

use std::env;
use std::error::Error;
use std::process::ExitCode;
use std::time::Duration;

use payments_core::accounts::Psp;
use payments_core::api::{AppState, Webhooks, router};
use payments_core::background::{self, Cadence};
use payments_core::credentials::SecretDigest;
use payments_core::deliveries::{self, DeliveryClient, Handover};
use payments_core::notifications::SigningKeys;
use payments_core::psp::{InvalidClient, PspClient};
use payments_core::relay::Relay;
use payments_core::relaying::{self, Relaying};
use payments_core::sender::Sender;
use payments_core::signing::MasterKey;
use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// How long each call to the PSP waits for its answer (ADR-0036, ADR-0042).
const PSP_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("the server stopped: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    // All of the configuration first, so that a missing value stops it at once.
    let tenant_url = required("TENANT_DATABASE_URL")?;
    let system_url = required("SYSTEM_DATABASE_URL")?;
    // The message names the variable and not its value, which is often the wrong secret.
    let operator = SecretDigest::from_hex(&required("OPERATOR_KEY_SHA256")?)
        .map_err(|_| "OPERATOR_KEY_SHA256 is not 64 lowercase hex digits")?;
    // Its errors say what is wrong with a key, never what the key holds.
    let keys = SigningKeys::parse(&required("MOCK_PSP_SIGNING_KEYS")?)
        .map_err(|error| format!("MOCK_PSP_SIGNING_KEYS: {error}"))?;
    // Its errors say what is wrong with the key, never what the key holds (ADR-0051).
    let master = MasterKey::parse(&required("WEBHOOK_MASTER_KEY")?)
        .map_err(|error| format!("WEBHOOK_MASTER_KEY: {error}"))?;
    let psp_url = required("MOCK_PSP_URL")?;
    let psp_key = required("MOCK_PSP_API_KEY")?;
    // One client for the route's catch-ups and one for the sender's task.
    let psp = PspClient::new(Psp::Mock, &psp_url, &psp_key, PSP_TIMEOUT).map_err(unusable)?;
    let sender = PspClient::new(Psp::Mock, &psp_url, &psp_key, PSP_TIMEOUT).map_err(unusable)?;
    let address = bind_address(env::var("BIND_ADDRESS").ok());

    let tenant = PgPool::connect(&tenant_url).await?;
    let system = PgPool::connect(&system_url).await?;
    let listener = TcpListener::bind(&address).await?;
    // Where the route's catch-ups and the sender's hand the deliveries that they commit over
    // to the relay's task (ADR-0052).
    let (to_relay, handed_to_relay) = mpsc::channel(deliveries::HANDOVERS);
    let to_relay = Handover::new(to_relay);
    let sender = Sender::new(sender, to_relay.clone(), OffsetDateTime::now_utc());
    let (handovers, handed_over) = mpsc::channel(background::HANDOVERS);
    let work = tokio::spawn(background::run(
        system.clone(),
        sender,
        Cadence::STANDARD,
        handed_over,
    ));
    let relaying = Relaying::new(
        system.clone(),
        DeliveryClient::new(master.clone(), deliveries::TIMEOUT),
        Relay::new(OffsetDateTime::now_utc()),
    );
    let relay = tokio::spawn(relaying.run(relaying::TICK, handed_to_relay));
    tracing::info!("listening on {}", listener.local_addr()?);
    let state = AppState::new(
        tenant,
        system,
        operator,
        master,
        Webhooks::new(psp, keys, to_relay),
        handovers,
    );
    // Neither task returns, so if one ends, it panicked. The server then stops, as from a
    // crash, rather than serve requests that nothing sends or events that nothing delivers
    // (ADR-0044).
    tokio::select! {
        served = axum::serve(listener, router(state)) => served?,
        ended = work => {
            let Err(ended) = ended;
            return Err(format!("the sender's task stopped: {ended}").into());
        }
        ended = relay => {
            let Err(ended) = ended;
            return Err(format!("the relay's task stopped: {ended}").into());
        }
    }
    Ok(())
}

/// A variable's value. One that is set but empty counts as not set, as `.env.example`
/// leaves the secrets (ADR-0043).
fn required(name: &str) -> Result<String, String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

/// Which of the PSP's variables a client could not use, and why. Neither message shows
/// the key.
fn unusable(error: InvalidClient) -> String {
    match error {
        InvalidClient::Address(_) => format!("MOCK_PSP_URL: {error}"),
        InvalidClient::Key => format!("MOCK_PSP_API_KEY: {error}"),
    }
}

fn bind_address(configured: Option<String>) -> String {
    configured.unwrap_or_else(|| "127.0.0.1:8080".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_listens_on_the_local_interface_by_default() {
        assert_eq!(bind_address(None), "127.0.0.1:8080");
        assert_eq!(bind_address(Some("0.0.0.0:80".to_owned())), "0.0.0.0:80");
    }
}
