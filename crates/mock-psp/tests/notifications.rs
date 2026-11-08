// Author: Nicola Antonello

//! The fake's notifications, delivered over HTTP to a receiver that stands in for the
//! server's `POST /webhooks/mock` (ADR-0030, ADR-0031, ADR-0032).
//!
//! The receiver checks each signature with HMAC code of its own, not the fake's signer,
//! as the server will.

mod common;

use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use chrono::{DateTime, TimeDelta, Utc};
use common::{Receiver, header_of, id_of, receiver, request, send, verifies};
use mock_psp::signing::SigningKey;
use mock_psp::{Attempt, Config, MessageState, MockPsp, Tamper};
use serde_json::json;
use tokio::net::TcpListener;
use uuid::Uuid;

/// A fake that delivers to a new receiver, and one fact about a payment of its, which
/// owes one notification.
async fn one_fact(config: Config) -> (MockPsp, Receiver, Uuid, String) {
    let psp = MockPsp::start_with(config)
        .await
        .expect("a free local port");
    let (receiver, endpoint) = receiver().await;
    psp.deliver_to(&endpoint);
    let payment = Uuid::now_v7();
    let authorize = request(payment, "authorize", 100);
    send(&psp, &authorize).await;
    psp.approve(id_of(&authorize));
    let message = psp.notifications().pop().expect("one notification");
    (psp, receiver, payment, message.id)
}

#[test]
fn the_receivers_check_passes_the_specifications_test_vector() {
    // So that a failure below is the fake's, not this check's.
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("webhook-id", "msg_27UH4WbU6Z5A5EzD8u03UvzRbpk"),
        ("webhook-timestamp", "1649367553"),
        (
            "webhook-signature",
            "v1,tZ1I4/hDygAJgO5TYxiSd6Sd0kDW6hPenDe+bTa3Kkw=",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    let body = br#"{"email":"test@example.com","username":"test_user"}"#;
    assert!(verifies(
        "whsec_C2FVsBQIhrscChlQIMV+b5sSYspob7oD",
        &headers,
        body
    ));
    assert!(!verifies(
        "whsec_C2FVsBQIhrscChlQIMV+b5sSYspob7oE",
        &headers,
        body
    ));
}

#[tokio::test]
async fn a_fact_is_announced_thin_and_signed_and_delivered_once() {
    let (psp, receiver, payment, message_id) = one_fact(Config::for_tests()).await;
    let before = Utc::now().timestamp();
    let attempt = psp.deliver(&message_id).await;
    assert_eq!(
        attempt,
        Attempt::Answered {
            status: 202,
            retry_after: None
        }
    );
    let received = receiver.received();
    assert_eq!(received.len(), 1);
    let (headers, body) = &received[0];
    assert_eq!(header_of(headers, "webhook-id"), message_id);
    let timestamp: i64 = header_of(headers, "webhook-timestamp")
        .parse()
        .expect("digits only");
    assert!((before..=Utc::now().timestamp()).contains(&timestamp));
    assert_eq!(header_of(headers, "content-type"), "application/json");
    let key = &psp.signing_keys()[0];
    assert!(verifies(key, headers, body));
    let notification: serde_json::Value = serde_json::from_slice(body).expect("JSON");
    assert_eq!(notification["type"], json!("payment.updated"));
    assert_eq!(notification["data"], json!({"payment_id": payment}));
    assert!(notification["timestamp"].is_string());
    assert_eq!(
        notification.as_object().map(serde_json::Map::len),
        Some(3),
        "a thin notification carries no amount or status: {notification}"
    );
    let message = &psp.notifications()[0];
    assert_eq!(
        (message.attempts, message.state),
        (1, MessageState::Delivered)
    );
    // Delivered, it is due no more.
    assert!(
        psp.deliver_due(Utc::now() + TimeDelta::days(4))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_duplicate_has_the_same_id_and_body() {
    let (psp, receiver, _, message_id) = one_fact(Config::for_tests()).await;
    psp.deliver(&message_id).await;
    psp.deliver(&message_id).await;
    let received = receiver.received();
    assert_eq!(received.len(), 2);
    assert_eq!(
        header_of(&received[0].0, "webhook-id"),
        header_of(&received[1].0, "webhook-id")
    );
    assert_eq!(received[0].1, received[1].1);
}

#[tokio::test]
async fn one_change_can_arrive_in_two_events() {
    let (psp, _, payment, first) = one_fact(Config::for_tests()).await;
    let second = psp.notify_again(payment);
    assert_ne!(second, first);
    let notifications = psp.notifications();
    assert_eq!(notifications.len(), 2);
    assert!(
        notifications
            .iter()
            .all(|message| message.notification.data.payment_id == payment)
    );
}

#[tokio::test]
async fn during_a_rotation_each_key_signs() {
    let (old, new) = (SigningKey::generate(), SigningKey::generate());
    let config = Config {
        signing_keys: vec![new.clone(), old.clone()],
        ..Config::for_tests()
    };
    let (psp, receiver, _, message_id) = one_fact(config).await;
    psp.deliver(&message_id).await;
    let (headers, body) = &receiver.received()[0];
    assert_eq!(
        header_of(headers, "webhook-signature").split(' ').count(),
        2
    );
    assert!(verifies(&old.written(), headers, body));
    assert!(verifies(&new.written(), headers, body));
}

#[tokio::test]
async fn a_tampered_delivery_fails_the_check_that_it_targets() {
    let (psp, receiver, _, message_id) = one_fact(Config::for_tests()).await;
    let key = psp.signing_keys()[0].clone();
    let stale = Utc::now().timestamp() - 6 * 60;
    psp.deliver_tampered(&message_id, Tamper::WrongKey).await;
    psp.deliver_tampered(&message_id, Tamper::ChangedBody).await;
    psp.deliver_tampered(&message_id, Tamper::Timestamp(stale))
        .await;
    let received = receiver.received();
    let (wrong_key, changed_body, old_timestamp) = (&received[0], &received[1], &received[2]);
    assert!(!verifies(&key, &wrong_key.0, &wrong_key.1));
    assert!(!verifies(&key, &changed_body.0, &changed_body.1));
    assert!(
        serde_json::from_slice::<serde_json::Value>(&changed_body.1).is_ok(),
        "the changed body is still JSON"
    );
    // Signed truly, six minutes old: only the server's window can refuse it.
    assert!(verifies(&key, &old_timestamp.0, &old_timestamp.1));
    assert_eq!(
        header_of(&old_timestamp.0, "webhook-timestamp"),
        stale.to_string()
    );
}

fn due_at(psp: &MockPsp) -> DateTime<Utc> {
    match psp.notifications()[0].state {
        MessageState::Due(due) => due,
        state => panic!("not due: {state:?}"),
    }
}

#[tokio::test]
async fn a_failure_is_due_again_on_the_schedule_and_after_any_retry_after() {
    let (psp, receiver, _, message_id) = one_fact(Config::for_tests()).await;
    receiver.answer([
        (StatusCode::INTERNAL_SERVER_ERROR, None),
        (StatusCode::SERVICE_UNAVAILABLE, Some(3_600)),
    ]);
    let failed_at = Utc::now();
    let attempt = psp.deliver(&message_id).await;
    assert_eq!(
        attempt,
        Attempt::Answered {
            status: 500,
            retry_after: None
        }
    );
    // Five seconds later, and up to a fifth more.
    let due = due_at(&psp);
    assert!(due >= failed_at + TimeDelta::seconds(5), "{due}");
    assert!(due <= Utc::now() + TimeDelta::seconds(6), "{due}");
    assert!(psp.deliver_due(failed_at).await.is_empty());

    // The next attempt asks for an hour, more than the schedule's five minutes.
    let attempts = psp.deliver_due(due).await;
    assert_eq!(attempts.len(), 1);
    assert_eq!(due_at(&psp), due + TimeDelta::hours(1));

    let attempts = psp.deliver_due(due + TimeDelta::hours(1)).await;
    assert_eq!(
        attempts,
        [(
            message_id,
            Attempt::Answered {
                status: 202,
                retry_after: None
            }
        )]
    );
    assert_eq!(psp.notifications()[0].state, MessageState::Delivered);
    assert_eq!(psp.notifications()[0].attempts, 3);
}

#[tokio::test]
async fn the_fake_gives_up_after_the_last_attempt_of_the_schedule() {
    let (psp, receiver, _, _) = one_fact(Config::for_tests()).await;
    receiver.answer([(StatusCode::INTERNAL_SERVER_ERROR, None); 10]);
    // Each step comes at the time the last one set: a failure is recorded at the step's
    // time, so the steps walk through the schedule, as the binary's timer does.
    let mut at = Utc::now();
    for attempt in 1..=10 {
        assert_eq!(psp.deliver_due(at).await.len(), 1, "attempt {attempt}");
        if let MessageState::Due(next) = psp.notifications()[0].state {
            at = next;
        }
    }
    let message = &psp.notifications()[0];
    assert_eq!(
        (message.attempts, message.state),
        (10, MessageState::GaveUp)
    );
    assert!(psp.deliver_due(at + TimeDelta::days(30)).await.is_empty());
    assert_eq!(receiver.received().len(), 10);
}

#[tokio::test]
async fn a_dropped_notification_is_never_delivered() {
    let (psp, receiver, _, message_id) = one_fact(Config::for_tests()).await;
    psp.drop_notification(&message_id);
    assert!(
        psp.deliver_due(Utc::now() + TimeDelta::days(4))
            .await
            .is_empty()
    );
    assert!(receiver.received().is_empty());
    assert_eq!(psp.notifications()[0].state, MessageState::Dropped);
}

#[tokio::test]
async fn no_answer_is_a_failed_attempt() {
    let config = Config {
        delivery_timeout: Duration::from_millis(200),
        ..Config::for_tests()
    };
    let psp = MockPsp::start_with(config)
        .await
        .expect("a free local port");
    // A port that nothing listens on: bound, then let go.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free local port");
    let closed = listener.local_addr().expect("bound");
    drop(listener);
    psp.deliver_to(&format!("http://{closed}/webhooks/mock"));
    let payment = Uuid::now_v7();
    let authorize = request(payment, "authorize", 100);
    send(&psp, &authorize).await;
    psp.approve(id_of(&authorize));
    let message_id = psp.notifications()[0].id.clone();
    assert_eq!(psp.deliver(&message_id).await, Attempt::NoAnswer);
    assert!(matches!(psp.notifications()[0].state, MessageState::Due(_)));
}
