// Author: Nicola Antonello

//! The fake's `POST /v1/requests`, over HTTP (ADR-0029, ADR-0032): it answers at once
//! whether it holds a request, drops a copy that its idempotency cache knows, and acts
//! on a copy again once the cache has forgotten it, as Stripe does.

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode, header};
use common::{call, id_of, request, send};
use mock_psp::{Failure, MockPsp, Route};
use serde_json::json;
use uuid::Uuid;

async fn fake() -> MockPsp {
    MockPsp::start().await.expect("a free local port")
}

#[tokio::test]
async fn a_request_is_received_at_once_and_held() {
    let psp = fake().await;
    let payment = Uuid::now_v7();
    let authorize = request(payment, "authorize", 10_000);
    let answer = send(&psp, &authorize).await;
    assert_eq!(answer.status, StatusCode::ACCEPTED);
    assert_eq!(
        answer.json(),
        json!({"id": authorize["id"], "status": "received"})
    );
    assert_eq!(
        answer
            .headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.as_bytes()),
        Some(&b"application/json"[..])
    );
    assert_eq!(psp.times_taken(id_of(&authorize)), 1);
    // Held: listed, but decided by nothing yet.
    let history = psp.history(payment).expect("the PSP holds the payment");
    assert_eq!(history.requests.len(), 1);
    assert!(history.facts.is_empty());
}

#[tokio::test]
async fn a_copy_gets_the_same_answer_and_other_content_a_422() {
    let psp = fake().await;
    let capture = request(Uuid::now_v7(), "capture", 500);
    let first = send(&psp, &capture).await;
    let copy = send(&psp, &capture).await;
    assert_eq!((copy.status, copy.body), (first.status, first.body));
    assert_eq!(psp.times_taken(id_of(&capture)), 1);

    let mut other = capture.clone();
    other["amount"] = json!(499);
    let answer = send(&psp, &other).await;
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(psp.times_taken(id_of(&capture)), 1);
}

#[tokio::test]
async fn a_copy_after_the_keys_are_forgotten_is_a_new_request() {
    // What I9 guards against: the PSP acts twice on one request (ADR-0028).
    let psp = fake().await;
    let refund = request(Uuid::now_v7(), "refund", 100);
    send(&psp, &refund).await;
    psp.forget_keys();
    let answer = send(&psp, &refund).await;
    assert_eq!(answer.status, StatusCode::ACCEPTED);
    assert_eq!(psp.times_taken(id_of(&refund)), 2);
    assert_eq!(psp.requests().len(), 2);
}

#[tokio::test]
async fn the_daily_pruning_forgets_only_keys_a_day_old() {
    let psp = fake().await;
    let capture = request(Uuid::now_v7(), "capture", 100);
    send(&psp, &capture).await;
    let taken_at = chrono::Utc::now();
    psp.prune_keys(taken_at + chrono::TimeDelta::hours(23));
    send(&psp, &capture).await;
    assert_eq!(psp.times_taken(id_of(&capture)), 1);
    psp.prune_keys(taken_at + chrono::TimeDelta::days(1));
    send(&psp, &capture).await;
    assert_eq!(psp.times_taken(id_of(&capture)), 2);
}

#[tokio::test]
async fn a_call_without_the_api_key_is_a_401_on_both_routes() {
    let psp = fake().await;
    let payment = Uuid::now_v7();
    let body = serde_json::to_vec(&request(payment, "authorize", 1)).expect("JSON");
    let history = format!("{}/v1/payments/{payment}/history", psp.url());
    let requests = format!("{}/v1/requests", psp.url());
    for key in [None, Some("mock_sk_wrong")] {
        let answer = call(Method::POST, &requests, key, Some(body.clone())).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{key:?}");
        let answer = call(Method::GET, &history, key, None).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{key:?}");
    }
    assert!(psp.requests().is_empty());
}

#[tokio::test]
async fn a_malformed_request_is_a_400_and_takes_nothing() {
    let psp = fake().await;
    let good = request(Uuid::now_v7(), "capture", 1);
    let with = |field: &str, value: serde_json::Value| {
        let mut request = good.clone();
        request[field] = value;
        request
    };
    let mut missing = good.clone();
    missing
        .as_object_mut()
        .expect("an object")
        .remove("currency");
    let mut unknown = good.clone();
    unknown["captured_at"] = json!("2026-01-01T00:00:00Z");
    for bad in [
        missing,
        unknown,
        with("kind", json!("settle")),
        with("id", json!("not a uuid")),
        with("amount", json!(0)),
        with("amount", json!(1.5)),
        with("currency", json!("eur")),
    ] {
        let answer = send(&psp, &bad).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let not_json = call(
        Method::POST,
        &format!("{}/v1/requests", psp.url()),
        Some(psp.api_key()),
        Some(b"{".to_vec()),
    )
    .await;
    assert_eq!(not_json.status, StatusCode::BAD_REQUEST);
    assert!(psp.requests().is_empty());
}

#[tokio::test]
async fn a_scripted_failure_answers_instead_and_serves_nothing() {
    let psp = fake().await;
    let payment = Uuid::now_v7();
    psp.fail_next(
        Route::Requests,
        [
            Failure::TooManyRequests { retry_after: 7 },
            Failure::Unavailable { retry_after: 30 },
            Failure::InternalError,
        ],
    );
    let authorize = request(payment, "authorize", 100);
    for (status, retry_after) in [
        (StatusCode::TOO_MANY_REQUESTS, Some("7")),
        (StatusCode::SERVICE_UNAVAILABLE, Some("30")),
        (StatusCode::INTERNAL_SERVER_ERROR, None),
    ] {
        let answer = send(&psp, &authorize).await;
        assert_eq!(answer.status, status);
        assert_eq!(
            answer
                .headers
                .get(header::RETRY_AFTER)
                .map(|v| v.to_str().expect("ASCII")),
            retry_after
        );
    }
    assert_eq!(psp.times_taken(id_of(&authorize)), 0);
    assert_eq!(send(&psp, &authorize).await.status, StatusCode::ACCEPTED);

    psp.fail_next(Route::History, [Failure::Unavailable { retry_after: 1 }]);
    let answer = common::read_history(&psp, payment).await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        common::read_history(&psp, payment).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_lost_reply_leaves_the_request_taken_and_the_caller_waiting() {
    let psp = fake().await;
    psp.lose_replies(1);
    let capture = request(Uuid::now_v7(), "capture", 100);
    let waited = tokio::time::timeout(Duration::from_millis(300), send(&psp, &capture)).await;
    assert!(waited.is_err(), "the reply came: {waited:?}");
    assert_eq!(psp.times_taken(id_of(&capture)), 1);
    // The next reply comes, and the copy is known.
    assert_eq!(send(&psp, &capture).await.status, StatusCode::ACCEPTED);
    assert_eq!(psp.times_taken(id_of(&capture)), 1);
}
