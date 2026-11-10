// Author: Nicola Antonello

//! The server (ADR-0023). It reads its configuration, logs in as the two roles, and
//! serves. It never runs migrations (ADR-0008), and it speaks plain HTTP: TLS ends in
//! front of it.

use std::env;
use std::error::Error;
use std::process::ExitCode;

use payments_core::api::{AppState, Webhooks, router};
use payments_core::credentials::SecretDigest;
use payments_core::notifications::SigningKeys;
use sqlx::PgPool;
use tokio::net::TcpListener;

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
    let address = bind_address(env::var("BIND_ADDRESS").ok());

    let tenant = PgPool::connect(&tenant_url).await?;
    let system = PgPool::connect(&system_url).await?;
    let listener = TcpListener::bind(&address).await?;
    tracing::info!("listening on {}", listener.local_addr()?);
    let state = AppState::new(tenant, system, operator, Webhooks::new(keys));
    axum::serve(listener, router(state)).await?;
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
