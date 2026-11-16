// Author: Nicola Antonello

//! The binary, run as a process, as for a local run (ADR-0056): it answers `200` to each
//! delivery that passes its checks, copies included, logs one line for each arrival, and
//! repeats no secret.

mod common;

use std::process::Stdio;
use std::time::Duration;

use axum::http::StatusCode;
use common::{Delivery, deliver, new_secret, payment, sender};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{ChildStderr, Command};

/// Every variable that the binary reads, so that a test starts it from none of them.
const VARIABLES: [&str; 2] = ["MOCK_MERCHANT_ADDRESS", "MOCK_MERCHANT_SECRETS"];

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mock-merchant"));
    for variable in VARIABLES {
        command.env_remove(variable);
    }
    command.kill_on_drop(true);
    command
}

/// The binary's next line, within five seconds.
async fn next_line(lines: &mut Lines<BufReader<ChildStderr>>) -> String {
    tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("a line within five seconds")
        .expect("readable")
        .expect("a line before the binary ends")
}

#[tokio::test]
async fn the_binary_takes_each_event_once_and_logs_each_arrival() {
    let (first, second) = (new_secret(), new_secret());
    let mut child = binary()
        .env("MOCK_MERCHANT_ADDRESS", "127.0.0.1:0")
        .env("MOCK_MERCHANT_SECRETS", format!("{first} {second}"))
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts");
    let mut lines = BufReader::new(child.stderr.take().expect("piped")).lines();
    let started = next_line(&mut lines).await;
    let url = started
        .strip_prefix("mock-merchant: listening on ")
        .unwrap_or_else(|| panic!("{started}"))
        .to_owned();

    let delivery = Delivery::of("evt_1", payment(1));
    let untrusted = new_secret();
    let sender = sender();
    for (secret, status) in [
        (&second, StatusCode::OK),
        (&first, StatusCode::OK),
        (&untrusted, StatusCode::BAD_REQUEST),
    ] {
        let answered = deliver(&sender, &url, secret, &delivery)
            .await
            .expect("the binary answers");
        assert_eq!(answered.status, status);
    }

    let mut logged = Vec::new();
    for _ in 0..3 {
        logged.push(next_line(&mut lines).await);
    }
    assert_eq!(
        logged,
        [
            "mock-merchant: took evt_1, about payment 00000000-0000-0000-0000-000000000001",
            "mock-merchant: dropped a copy of evt_1",
            "mock-merchant: refused evt_1: no v1 signature verifies under a trusted secret",
        ]
    );
    // Each arrival is logged once: nothing more comes in a second, four ticks of the
    // binary's timer.
    let more = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await;
    assert!(more.is_err(), "a fourth line: {more:?}");
}

#[tokio::test]
async fn the_binary_will_not_start_without_its_settings_or_repeat_one() {
    let valid = new_secret();
    let malformed = "not-a-whsec-secret-0123456789";
    for (variables, said) in [
        (vec![], "mock-merchant: MOCK_MERCHANT_SECRETS is not set"),
        (
            vec![("MOCK_MERCHANT_SECRETS", " ")],
            "mock-merchant: MOCK_MERCHANT_SECRETS holds no secret",
        ),
        (
            vec![("MOCK_MERCHANT_SECRETS", malformed)],
            "mock-merchant: MOCK_MERCHANT_SECRETS: a secret starts with whsec_",
        ),
        (
            vec![
                ("MOCK_MERCHANT_SECRETS", valid.as_str()),
                ("MOCK_MERCHANT_ADDRESS", "nowhere"),
            ],
            "mock-merchant: MOCK_MERCHANT_ADDRESS is not an address and port",
        ),
    ] {
        let output =
            tokio::time::timeout(Duration::from_secs(10), binary().envs(variables).output())
                .await
                .expect("the binary stops at once")
                .expect("the binary runs");
        assert!(!output.status.success(), "{said}");
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim_end(), said);
    }
}
