// Author: Nicola Antonello

//! The binary, run as a process, as for a local run (ADR-0029): it approves at once,
//! settles after its delay, and delivers signed notifications on its own timer.

mod common;

use std::process::Stdio;
use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use common::{call, receiver, request, verifies};
use mock_psp::new_api_key;
use mock_psp::signing::SigningKey;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use uuid::Uuid;

/// Every variable that the binary reads, so that a test starts it from none of them.
const VARIABLES: [&str; 5] = [
    "MOCK_PSP_ADDRESS",
    "MOCK_PSP_WEBHOOK_URL",
    "MOCK_PSP_API_KEY",
    "MOCK_PSP_SIGNING_KEYS",
    "MOCK_PSP_SETTLE_AFTER_SECONDS",
];

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mock-psp"));
    for variable in VARIABLES {
        command.env_remove(variable);
    }
    command.kill_on_drop(true);
    command
}

#[tokio::test]
async fn the_binary_approves_settles_and_delivers_on_its_own() {
    let (receiver, endpoint) = receiver().await;
    let key = SigningKey::generate();
    let api_key = new_api_key();
    let mut child = binary()
        .env("MOCK_PSP_ADDRESS", "127.0.0.1:0")
        .env("MOCK_PSP_WEBHOOK_URL", &endpoint)
        .env("MOCK_PSP_API_KEY", &api_key)
        .env("MOCK_PSP_SIGNING_KEYS", key.written())
        .env("MOCK_PSP_SETTLE_AFTER_SECONDS", "0")
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts");
    let mut lines = BufReader::new(child.stderr.take().expect("piped")).lines();
    let url = loop {
        let line = lines
            .next_line()
            .await
            .expect("readable")
            .expect("a line before the binary ends");
        if let Some(url) = line.strip_prefix("mock-psp: listening on ") {
            break url.to_owned();
        }
    };
    // Read the rest, so that the binary never waits on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    let payment = Uuid::now_v7();
    for request in [
        request(payment, "authorize", 100),
        request(payment, "capture", 100),
    ] {
        let body = serde_json::to_vec(&request).expect("JSON");
        let answer = call(
            Method::POST,
            &format!("{url}/v1/requests"),
            Some(&api_key),
            Some(body),
        )
        .await;
        assert_eq!(answer.status, StatusCode::ACCEPTED);
    }

    // Authorized and captured at once, settled on a tick, and each announced.
    let deadline = Instant::now() + Duration::from_secs(10);
    while receiver.received().len() < 3 {
        assert!(
            Instant::now() < deadline,
            "{} deliveries in 10 seconds",
            receiver.received().len()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let history = call(
        Method::GET,
        &format!("{url}/v1/payments/{payment}/history"),
        Some(&api_key),
        None,
    )
    .await
    .json();
    let kinds: Vec<&str> = history["facts"]
        .as_array()
        .expect("facts")
        .iter()
        .map(|fact| fact["kind"].as_str().expect("a kind"))
        .collect();
    assert_eq!(kinds, ["authorized", "captured", "settled"]);
    for (headers, body) in receiver.received() {
        assert!(verifies(&key.written(), &headers, &body));
    }
}

#[tokio::test]
async fn the_binary_will_not_start_without_its_configuration_or_repeat_a_secret() {
    let output = binary().output().await.expect("the binary runs");
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("MOCK_PSP_SIGNING_KEYS is not set"), "{said}");

    let secret = "not-a-whsec-key-0123456789";
    let output = binary()
        .env("MOCK_PSP_SIGNING_KEYS", secret)
        .output()
        .await
        .expect("the binary runs");
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("MOCK_PSP_SIGNING_KEYS: a signing key starts with whsec_"),
        "{said}"
    );
    assert!(!said.contains(secret), "{said}");
}
