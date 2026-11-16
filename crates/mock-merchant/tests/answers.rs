// Author: Nicola Antonello

//! How the fake answers a delivery that passes its checks (ADR-0056): the standing
//! answer, answers aimed at one payment, copies, delays, and what its handle returns.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::{Delivery, deliver, event_time, get, merchant, payment, post, sender};
use mock_merchant::event::{Data, Event, Kind};
use mock_merchant::{Answer, MockMerchant, Taken, Verdict};

fn verdicts(merchant: &MockMerchant) -> Vec<Verdict> {
    merchant
        .arrivals()
        .iter()
        .map(|arrival| arrival.verdict)
        .collect()
}

fn taken_ids(merchant: &MockMerchant) -> Vec<String> {
    merchant
        .events()
        .into_iter()
        .map(|taken| taken.id)
        .collect()
}

const OK: Answer = Answer::Status(StatusCode::OK);

#[tokio::test]
async fn a_new_event_is_taken_and_answered_200() {
    let (merchant, secret) = merchant().await;
    let delivery = Delivery::of("evt_1", payment(1));

    let answered = deliver(&sender(), merchant.url(), &secret, &delivery)
        .await
        .expect("the fake answers");

    assert_eq!(answered.status, StatusCode::OK);
    let event = Event {
        kind: Kind::PaymentUpdated,
        timestamp: event_time(),
        data: Data {
            payment_id: payment(1),
            request_id: None,
        },
    };
    assert_eq!(
        merchant.events(),
        [Taken {
            id: "evt_1".to_owned(),
            event: event.clone(),
        }]
    );
    let arrivals = merchant.arrivals();
    assert_eq!(arrivals.len(), 1);
    let arrival = &arrivals[0];
    assert_eq!(arrival.id.as_deref(), Some("evt_1"));
    assert_eq!(
        arrival.timestamp.as_deref(),
        Some(delivery.timestamp.as_str())
    );
    assert_eq!(arrival.body, delivery.body);
    assert_eq!(arrival.event, Some(event));
    assert_eq!(arrival.verdict, Verdict::Taken(OK));
}

#[tokio::test]
async fn a_copy_is_answered_200_and_never_taken_twice() {
    let (merchant, secret) = merchant().await;
    let delivery = Delivery::of("evt_1", payment(1));

    for _ in 0..2 {
        let answered = deliver(&sender(), merchant.url(), &secret, &delivery)
            .await
            .expect("the fake answers");
        assert_eq!(answered.status, StatusCode::OK);
    }

    assert_eq!(taken_ids(&merchant), ["evt_1"]);
    assert_eq!(verdicts(&merchant), [Verdict::Taken(OK), Verdict::Copy]);
}

#[tokio::test]
async fn a_copy_is_answered_200_whatever_the_script() {
    let (merchant, secret) = merchant().await;
    let delivery = Delivery::of("evt_1", payment(1));
    deliver(&sender(), merchant.url(), &secret, &delivery)
        .await
        .expect("the fake answers");

    merchant.answer(Answer::Status(StatusCode::INTERNAL_SERVER_ERROR));
    merchant.answer_payment(payment(1), Answer::Status(StatusCode::GONE));
    let again = deliver(&sender(), merchant.url(), &secret, &delivery)
        .await
        .expect("the fake answers");

    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(verdicts(&merchant), [Verdict::Taken(OK), Verdict::Copy]);
}

#[tokio::test]
async fn the_standing_answer_meets_every_new_event() {
    let (merchant, secret) = merchant().await;
    let unavailable = Answer::Status(StatusCode::SERVICE_UNAVAILABLE);
    merchant.answer(unavailable);

    for id in ["evt_1", "evt_2"] {
        let answered = deliver(
            &sender(),
            merchant.url(),
            &secret,
            &Delivery::of(id, payment(1)),
        )
        .await
        .expect("the fake answers");
        assert_eq!(answered.status, StatusCode::SERVICE_UNAVAILABLE);
    }
    assert_eq!(taken_ids(&merchant), Vec::<String>::new());

    // Changed between two steps of the relay, it meets the next attempt. The event was
    // never taken, so that attempt is not a copy.
    let no_content = Answer::Status(StatusCode::NO_CONTENT);
    merchant.answer(no_content);
    let answered = deliver(
        &sender(),
        merchant.url(),
        &secret,
        &Delivery::of("evt_1", payment(1)),
    )
    .await
    .expect("the fake answers");

    assert_eq!(answered.status, StatusCode::NO_CONTENT);
    assert_eq!(taken_ids(&merchant), ["evt_1"]);
    assert_eq!(
        verdicts(&merchant),
        [
            Verdict::Declined(unavailable),
            Verdict::Declined(unavailable),
            Verdict::Taken(no_content),
        ]
    );
}

#[tokio::test]
async fn an_answer_aimed_at_a_payment_wins_over_the_standing_one() {
    let (merchant, secret) = merchant().await;
    merchant.answer_payment(
        payment(1),
        Answer::Status(StatusCode::INTERNAL_SERVER_ERROR),
    );

    for (id, payment_id, status) in [
        ("evt_1", payment(1), StatusCode::INTERNAL_SERVER_ERROR),
        ("evt_2", payment(2), StatusCode::OK),
    ] {
        let answered = deliver(
            &sender(),
            merchant.url(),
            &secret,
            &Delivery::of(id, payment_id),
        )
        .await
        .expect("the fake answers");
        assert_eq!(answered.status, status, "{id}");
    }

    merchant.answer(Answer::Status(StatusCode::BAD_GATEWAY));
    merchant.answer_payment(payment(1), OK);
    for (id, payment_id, status) in [
        ("evt_1", payment(1), StatusCode::OK),
        ("evt_3", payment(2), StatusCode::BAD_GATEWAY),
    ] {
        let answered = deliver(
            &sender(),
            merchant.url(),
            &secret,
            &Delivery::of(id, payment_id),
        )
        .await
        .expect("the fake answers");
        assert_eq!(answered.status, status, "{id}");
    }

    assert_eq!(taken_ids(&merchant), ["evt_2", "evt_1"]);
}

#[tokio::test]
async fn each_answer_goes_out_as_scripted() {
    let unusual_success = StatusCode::from_u16(299).expect("a status code");
    for (answer, status, retry_after, taken) in [
        (
            Answer::Status(StatusCode::GONE),
            StatusCode::GONE,
            None,
            false,
        ),
        (
            Answer::Status(StatusCode::FOUND),
            StatusCode::FOUND,
            None,
            false,
        ),
        (Answer::Status(unusual_success), unusual_success, None, true),
        (
            Answer::RetryAfter {
                status: StatusCode::TOO_MANY_REQUESTS,
                seconds: 30,
            },
            StatusCode::TOO_MANY_REQUESTS,
            Some("30"),
            false,
        ),
        (
            Answer::RetryAfter {
                status: StatusCode::SERVICE_UNAVAILABLE,
                seconds: 1,
            },
            StatusCode::SERVICE_UNAVAILABLE,
            Some("1"),
            false,
        ),
    ] {
        let (merchant, secret) = merchant().await;
        merchant.answer(answer);

        let answered = deliver(
            &sender(),
            merchant.url(),
            &secret,
            &Delivery::of("evt_1", payment(1)),
        )
        .await
        .expect("the fake answers");

        assert_eq!(answered.status, status, "{answer:?}");
        let sent_retry_after = answered
            .headers
            .get("retry-after")
            .map(|value| value.to_str().expect("ASCII"));
        assert_eq!(sent_retry_after, retry_after, "{answer:?}");
        assert_eq!(merchant.events().len(), usize::from(taken), "{answer:?}");
    }
}

#[tokio::test]
async fn a_delayed_answer_takes_the_event_even_when_the_client_gives_up() {
    let (merchant, secret) = merchant().await;
    let delay = Answer::Delay(Duration::from_secs(30));
    merchant.answer(delay);
    let delivery = Delivery::of("evt_1", payment(1));

    let attempt = tokio::time::timeout(
        Duration::from_secs(1),
        deliver(&sender(), merchant.url(), &secret, &delivery),
    )
    .await;

    assert!(attempt.is_err(), "the client gave up before the answer");
    assert_eq!(taken_ids(&merchant), ["evt_1"]);
    // The relay tries again, and the copy is answered at once.
    let again = deliver(&sender(), merchant.url(), &secret, &delivery)
        .await
        .expect("the fake answers");
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(verdicts(&merchant), [Verdict::Taken(delay), Verdict::Copy]);
}

#[tokio::test]
async fn most_at_once_counts_the_deliveries_answered_together() {
    let (merchant, secret) = merchant().await;
    merchant.answer(Answer::Delay(Duration::from_secs(1)));
    let sender = sender();
    let [first, second, third] = ["evt_1", "evt_2", "evt_3"].map(|id| Delivery::of(id, payment(1)));

    let answered = tokio::join!(
        deliver(&sender, merchant.url(), &secret, &first),
        deliver(&sender, merchant.url(), &secret, &second),
        deliver(&sender, merchant.url(), &secret, &third),
    );

    for attempt in [answered.0, answered.1, answered.2] {
        assert_eq!(attempt.expect("the fake answers").status, StatusCode::OK);
    }
    assert_eq!(merchant.most_at_once(), 3);
    // One more alone leaves the most at three.
    merchant.answer(OK);
    deliver(
        &sender,
        merchant.url(),
        &secret,
        &Delivery::of("evt_4", payment(1)),
    )
    .await
    .expect("the fake answers");
    assert_eq!(merchant.most_at_once(), 3);
}

#[tokio::test]
async fn a_wrong_url_reaches_nothing() {
    let (merchant, secret) = merchant().await;
    let delivery = Delivery::of("evt_1", payment(1));
    let base = merchant
        .url()
        .strip_suffix("/webhooks")
        .expect("the route's path");

    let elsewhere = post(
        &sender(),
        &format!("{base}/webhook"),
        &delivery.headers(&secret),
        delivery.body.clone(),
    )
    .await
    .expect("the fake answers");
    let read = get(&sender(), merchant.url())
        .await
        .expect("the fake answers");

    assert_eq!(elsewhere.status, StatusCode::NOT_FOUND);
    assert_eq!(read.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(merchant.arrivals().is_empty());
}

#[tokio::test]
async fn a_stopped_fake_takes_no_delivery() {
    let (merchant, secret) = merchant().await;
    let url = merchant.url().to_owned();
    let sender = sender();
    let delivery = Delivery::of("evt_1", payment(1));
    // A first delivery leaves a connection open in the sender's pool.
    deliver(&sender, &url, &secret, &delivery)
        .await
        .expect("the fake answers");

    tokio::time::timeout(Duration::from_secs(5), merchant.stop())
        .await
        .expect("the fake stops");

    let pooled = deliver(&sender, &url, &secret, &delivery).await;
    assert!(
        pooled.is_err(),
        "the pooled connection was closed: {pooled:?}"
    );
    let fresh = deliver(&common::sender(), &url, &secret, &delivery).await;
    assert!(fresh.is_err(), "a new connection was refused: {fresh:?}");
}
