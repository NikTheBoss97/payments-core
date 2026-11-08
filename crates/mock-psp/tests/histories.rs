// Author: Nicola Antonello

//! The fake's `GET /v1/payments/{payment_id}/history`, over HTTP (ADR-0029, ADR-0031):
//! every fact in the PSP's order, at consecutive positions, in the PSP's words, and every
//! request that the PSP took, which the service looks up before it sends again.

mod common;

use axum::http::StatusCode;
use common::{id_of, read_history, request, send};
use mock_psp::MockPsp;
use mock_psp::wire::Detail;
use serde_json::json;
use uuid::Uuid;

async fn fake() -> MockPsp {
    MockPsp::start().await.expect("a free local port")
}

#[tokio::test]
async fn a_payment_that_the_psp_never_saw_is_a_404() {
    let psp = fake().await;
    let answer = read_history(&psp, Uuid::now_v7()).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    let answer = common::call(
        axum::http::Method::GET,
        &format!("{}/v1/payments/not-a-uuid/history", psp.url()),
        Some(psp.api_key()),
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_history_holds_each_decision_in_order_and_every_request() {
    let psp = fake().await;
    let payment = Uuid::now_v7();
    let authorize = request(payment, "authorize", 10_000);
    let capture = request(payment, "capture", 8_000);
    let refund = request(payment, "refund", 9_000);
    for request in [&authorize, &capture, &refund] {
        send(&psp, request).await;
    }
    psp.approve_for(id_of(&authorize), 8_000);
    psp.approve(id_of(&capture));
    // More than what was captured: a PSP would refuse it.
    psp.refuse(id_of(&refund));
    psp.settle(payment);

    let answer = read_history(&psp, payment).await;
    assert_eq!(answer.status, StatusCode::OK);
    let history = answer.json();
    assert_eq!(history["payment_id"], json!(payment));
    assert_eq!(history["requests"], json!([authorize, capture, refund]));
    let facts = history["facts"].as_array().expect("a list of facts");
    let kinds: Vec<&str> = facts
        .iter()
        .map(|fact| fact["kind"].as_str().expect("a kind"))
        .collect();
    assert_eq!(
        kinds,
        ["authorized", "captured", "refund_refused", "settled"]
    );
    let positions: Vec<u64> = facts
        .iter()
        .map(|fact| fact["position"].as_u64().expect("a position"))
        .collect();
    assert_eq!(positions, [1, 2, 3, 4]);
    assert_eq!(facts[0]["amount"], json!(8_000));
    assert_eq!(facts[0]["request_id"], authorize["id"]);
    assert!(facts[0]["expires_at"].is_string());
    assert!(facts[1]["captured_at"].is_string());
    // Each fact has the PSP's own ID, of one shape, unique across the PSP (ADR-0037).
    let ids: Vec<&str> = facts
        .iter()
        .map(|fact| fact["id"].as_str().expect("an ID"))
        .collect();
    for id in &ids {
        assert!(
            id.starts_with("fct_") && id.len() == 36,
            "{id} has not the fake's shape"
        );
    }
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len());
}

#[tokio::test]
async fn a_request_is_listed_before_the_psp_decides_it() {
    // What the sender's lookup relies on: a request that the PSP took but has not
    // decided is in its records, so the sender does not send it again (ADR-0028).
    let psp = fake().await;
    let payment = Uuid::now_v7();
    let authorize = request(payment, "authorize", 100);
    send(&psp, &authorize).await;
    let history = read_history(&psp, payment).await.json();
    assert_eq!(history["requests"], json!([authorize]));
    assert_eq!(history["facts"], json!([]));
}

#[tokio::test]
async fn each_fact_is_announced_by_one_notification() {
    let psp = fake().await;
    let payment = Uuid::now_v7();
    let authorize = request(payment, "authorize", 100);
    send(&psp, &authorize).await;
    assert!(psp.notifications().is_empty());
    psp.approve(id_of(&authorize));
    psp.expire(payment);
    let notifications = psp.notifications();
    assert_eq!(notifications.len(), 2);
    for message in &notifications {
        assert_eq!(message.notification.data.payment_id, payment);
        assert!(message.id.starts_with("msg_"));
    }
    assert_ne!(notifications[0].id, notifications[1].id);
}

#[tokio::test]
async fn an_appended_fact_bypasses_the_fakes_checks() {
    // A capture that fails after it succeeded, which the service's model has no place
    // for (ADR-0031): the fake writes it only when a test asks.
    let psp = fake().await;
    let payment = Uuid::now_v7();
    psp.append(payment, Detail::Settled);
    let history = read_history(&psp, payment).await.json();
    assert_eq!(history["facts"][0]["kind"], json!("settled"));
}

#[tokio::test]
#[should_panic(expected = "cannot")]
async fn a_decision_that_no_psp_could_make_is_a_mistake_in_the_test() {
    let psp = fake().await;
    let payment = Uuid::now_v7();
    let capture = request(payment, "capture", 100);
    send(&psp, &capture).await;
    // Nothing is authorized.
    psp.approve(id_of(&capture));
}
