// Author: Nicola Antonello

//! The fake merchant, served for local runs (ADR-0056). It answers `200` to each delivery
//! that passes its checks, copies included, and every quarter of a second logs one line
//! for each delivery that arrived since. Its records live in memory and end with the
//! process.
//!
//! It reads the environment:
//! - `MOCK_MERCHANT_ADDRESS`: where it listens, `127.0.0.1:4200` unless set;
//! - `MOCK_MERCHANT_SECRETS`: the secrets that registering its endpoints answered, written
//!   `whsec_` and base64 and separated by spaces.

use std::env;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use mock_merchant::signature::Secret;
use mock_merchant::{Arrival, Config, MockMerchant, Verdict};

fn read_config() -> Result<Config, String> {
    let address: SocketAddr = env::var("MOCK_MERCHANT_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:4200".to_owned())
        .parse()
        .map_err(|_| "MOCK_MERCHANT_ADDRESS is not an address and port".to_owned())?;
    let secrets = env::var("MOCK_MERCHANT_SECRETS")
        .map_err(|_| "MOCK_MERCHANT_SECRETS is not set".to_owned())?
        .split_whitespace()
        .map(Secret::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("MOCK_MERCHANT_SECRETS: {error}"))?;
    if secrets.is_empty() {
        return Err("MOCK_MERCHANT_SECRETS holds no secret".to_owned());
    }
    Ok(Config { address, secrets })
}

/// One line about one arrival: its event and payment, never a secret.
fn describe(arrival: &Arrival) -> String {
    let id = arrival
        .id
        .as_deref()
        .unwrap_or("a delivery without one webhook-id");
    match arrival.verdict {
        Verdict::Taken(_) => {
            let data = arrival.event.as_ref().expect("a taken event was read").data;
            match data.request_id {
                Some(request_id) => format!(
                    "took {id}, about payment {}, deciding request {request_id}",
                    data.payment_id
                ),
                None => format!("took {id}, about payment {}", data.payment_id),
            }
        }
        Verdict::Copy => format!("dropped a copy of {id}"),
        Verdict::Refused(refusal) => format!("refused {id}: {refusal}"),
        Verdict::Malformed => {
            format!("refused {id}: its body is not one event of the documented shape")
        }
        Verdict::Declined(answer) => format!("answered {id} with {answer:?}"),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let config = match read_config() {
        Ok(config) => config,
        Err(problem) => {
            // Nothing that the environment holds is repeated, since it holds secrets.
            eprintln!("mock-merchant: {problem}");
            return ExitCode::FAILURE;
        }
    };
    let merchant = match MockMerchant::start_with(config).await {
        Ok(merchant) => merchant,
        Err(error) => {
            eprintln!("mock-merchant: cannot listen: {error}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("mock-merchant: listening on {}", merchant.url());

    let mut logged = 0;
    let mut ticks = tokio::time::interval(Duration::from_millis(250));
    loop {
        ticks.tick().await;
        let arrivals = merchant.arrivals();
        for arrival in arrivals.iter().skip(logged) {
            eprintln!("mock-merchant: {}", describe(arrival));
        }
        logged = arrivals.len();
    }
}
