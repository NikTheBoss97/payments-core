// Author: Nicola Antonello

//! What the fake refuses, and whom it trusts (ADR-0056): a delivery that fails a check
//! before its body, and a body of another shape, each answered `400` and never taken.

mod common;

use axum::http::StatusCode;
use chrono::Utc;
use common::{Delivery, deliver, merchant, new_secret, payment, post, sender};
use mock_merchant::signature::{Refusal, Secret, SecretError};
use mock_merchant::{Answer, Config, MockMerchant, Verdict};

const OK: Answer = Answer::Status(StatusCode::OK);

#[tokio::test]
async fn a_delivery_that_fails_a_check_is_answered_400_and_never_taken() {
    let (merchant, secret) = merchant().await;
    let delivery = Delivery::of("evt_1", payment(1));
    let mut changed_body = delivery.body.clone();
    changed_body.push(b' ');
    let stale = Delivery {
        timestamp: (Utc::now().timestamp() - 600).to_string(),
        ..delivery.clone()
    };
    let mut unsigned = delivery.headers(&secret);
    unsigned.retain(|(name, _)| *name != "webhook-signature");

    for (headers, body) in [
        // Signed under a secret that the fake does not trust.
        (delivery.headers(&new_secret()), delivery.body.clone()),
        // A body changed after it was signed.
        (delivery.headers(&secret), changed_body),
        // Signed ten minutes ago.
        (stale.headers(&secret), stale.body.clone()),
        // Not signed.
        (unsigned, delivery.body.clone()),
    ] {
        let answered = post(&sender(), merchant.url(), &headers, body)
            .await
            .expect("the fake answers");
        assert_eq!(answered.status, StatusCode::BAD_REQUEST, "{headers:?}");
    }

    let arrivals = merchant.arrivals();
    let verdicts: Vec<Verdict> = arrivals.iter().map(|arrival| arrival.verdict).collect();
    assert_eq!(
        verdicts,
        [
            Verdict::Refused(Refusal::Signature),
            Verdict::Refused(Refusal::Signature),
            Verdict::Refused(Refusal::Window),
            Verdict::Refused(Refusal::Header("webhook-signature")),
        ]
    );
    assert!(arrivals.iter().all(|arrival| arrival.event.is_none()));
    assert!(merchant.events().is_empty());
}

#[tokio::test]
async fn a_verified_body_of_another_shape_is_answered_400_and_never_taken() {
    let (merchant, secret) = merchant().await;
    let delivery = Delivery {
        body: br#"{"type":"payment.created","timestamp":"2026-11-16T10:20:30Z","data":{"payment_id":"00000000-0000-0000-0000-000000000001"}}"#.to_vec(),
        ..Delivery::of("evt_1", payment(1))
    };

    let answered = deliver(&sender(), merchant.url(), &secret, &delivery)
        .await
        .expect("the fake answers");

    assert_eq!(answered.status, StatusCode::BAD_REQUEST);
    let arrivals = merchant.arrivals();
    assert_eq!(arrivals.len(), 1);
    assert_eq!(arrivals[0].verdict, Verdict::Malformed);
    assert_eq!(arrivals[0].event, None);
    assert!(merchant.events().is_empty());
}

#[tokio::test]
async fn a_secret_counts_from_when_the_fake_trusts_it() {
    let merchant = MockMerchant::start().await.expect("a free local port");
    let secret = new_secret();
    let delivery = Delivery::of("evt_1", payment(1));
    let before = deliver(&sender(), merchant.url(), &secret, &delivery)
        .await
        .expect("the fake answers");

    // "short" in base64: five bytes, too few to be a secret.
    assert_eq!(
        merchant.trust("whsec_c2hvcnQ="),
        Err(SecretError::Length(5))
    );
    merchant
        .trust(&secret)
        .expect("a secret written as the server writes one");
    let after = deliver(&sender(), merchant.url(), &secret, &delivery)
        .await
        .expect("the fake answers");

    assert_eq!(before.status, StatusCode::BAD_REQUEST);
    assert_eq!(after.status, StatusCode::OK);
    let verdicts: Vec<Verdict> = merchant
        .arrivals()
        .iter()
        .map(|arrival| arrival.verdict)
        .collect();
    assert_eq!(
        verdicts,
        [Verdict::Refused(Refusal::Signature), Verdict::Taken(OK)]
    );
}

#[tokio::test]
async fn a_fake_trusts_the_secrets_that_it_starts_with() {
    let secret = new_secret();
    let config = Config {
        secrets: vec![Secret::parse(&secret).expect("a new secret")],
        ..Config::for_tests()
    };
    let merchant = MockMerchant::start_with(config)
        .await
        .expect("a free local port");

    let answered = deliver(
        &sender(),
        merchant.url(),
        &secret,
        &Delivery::of("evt_1", payment(1)),
    )
    .await
    .expect("the fake answers");

    assert_eq!(answered.status, StatusCode::OK);
    assert_eq!(merchant.events().len(), 1);
}

#[tokio::test]
async fn during_a_rotation_both_endpoints_verify_and_the_second_brings_a_copy() {
    // Two endpoints at one URL during a rotation (ADR-0051): each event reaches both,
    // under the same webhook-id, each signed with its own secret.
    let (merchant, old) = merchant().await;
    let new = new_secret();
    merchant
        .trust(&new)
        .expect("a secret written as the server writes one");
    let delivery = Delivery::of("evt_1", payment(1));

    for secret in [&old, &new] {
        let answered = deliver(&sender(), merchant.url(), secret, &delivery)
            .await
            .expect("the fake answers");
        assert_eq!(answered.status, StatusCode::OK);
    }

    let verdicts: Vec<Verdict> = merchant
        .arrivals()
        .iter()
        .map(|arrival| arrival.verdict)
        .collect();
    assert_eq!(verdicts, [Verdict::Taken(OK), Verdict::Copy]);
    assert_eq!(merchant.events().len(), 1);
}
