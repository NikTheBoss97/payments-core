// Author: Nicola Antonello

//! Reading a payment's history from the PSP (ADR-0031, ADR-0032, ADR-0036, ADR-0041):
//! against the fake over real HTTP (ADR-0029), and against PSPs that answer what the
//! test writes, or never answer, for what the fake never does.

mod common;

use std::error::Error;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use common::psp::{ScriptedPsp, TIMEOUT, client_of, closed_address, send, silent_psp};
use common::{payment_id, request_id};
use mock_psp::wire::Detail;
use mock_psp::{Failure, MockPsp, Route};
use payments_core::accounts::Psp;
use payments_core::money::PositiveAmount;
use payments_core::psp::{FactDetail, MAX_BODY, PspClient, ReadError, Unreadable};
use payments_core::requests::{Ask, RecordRequest};
use payments_core::times::BusinessTime;
use serde_json::json;

type TestResult = Result<(), Box<dyn Error>>;

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a positive amount")
}

/// Request `n` of payment 1.
fn request(n: u32, ask: Ask) -> RecordRequest {
    RecordRequest {
        request: request_id(n),
        payment: payment_id(1),
        ask,
    }
}

#[tokio::test]
async fn a_history_reads_every_fact_in_the_psps_order() -> TestResult {
    let fake = MockPsp::start().await?;
    let payment = payment_id(1);
    let authorize = request(1, Ask::Authorize(amount(10_000)));
    let capture = request(2, Ask::Capture(amount(8_000)));
    let refund = request(3, Ask::Refund(amount(9_000)));
    send(&fake, &authorize).await;
    fake.approve_for(request_id(1).get(), 8_000);
    send(&fake, &capture).await;
    fake.approve(request_id(2).get());
    send(&fake, &refund).await;
    fake.refuse(request_id(3).get());
    fake.settle(payment.get());

    let history = client_of(&fake).history(payment).await?;
    assert_eq!(history.payment(), payment);
    let theirs = fake.history(payment.get()).expect("the fake holds it");
    let time = |at| BusinessTime::new(at).expect("a time that can be stored");
    let expected: Vec<_> = theirs
        .facts
        .iter()
        .map(|fact| {
            let detail = match fact.detail {
                Detail::Authorized {
                    request_id,
                    amount: minor,
                    expires_at,
                } => FactDetail::Authorized {
                    request: request_id,
                    amount: amount(minor),
                    expires_at: time(expires_at),
                },
                Detail::Captured {
                    request_id,
                    amount: minor,
                    captured_at,
                } => FactDetail::Captured {
                    request: request_id,
                    amount: amount(minor),
                    captured_at: time(captured_at),
                },
                Detail::RefundRefused { request_id } => FactDetail::RefundRefused {
                    request: request_id,
                },
                Detail::Settled => FactDetail::Settled,
                ref other => panic!("this test made no {other:?}"),
            };
            (fact.id.clone(), i32::try_from(fact.position), Ok(detail))
        })
        .collect();
    let read: Vec<_> = history
        .facts()
        .iter()
        .map(|fact| {
            (
                fact.id().to_owned(),
                Ok(fact.position()),
                fact.detail().map_err(Clone::clone),
            )
        })
        .collect();
    assert_eq!(read, expected);
    assert_eq!(
        history
            .facts()
            .iter()
            .map(|fact| fact.detail().map(FactDetail::amount))
            .collect::<Vec<_>>(),
        [Ok(8_000), Ok(8_000), Ok(0), Ok(0)]
    );
    Ok(())
}

#[tokio::test]
async fn a_payment_that_the_psp_does_not_know_has_no_facts() -> TestResult {
    let fake = MockPsp::start().await?;
    let history = client_of(&fake).history(payment_id(1)).await?;
    assert_eq!(history.payment(), payment_id(1));
    assert!(history.facts().is_empty());
    Ok(())
}

#[tokio::test]
async fn the_psp_can_ask_to_be_called_later_or_fail() -> TestResult {
    let fake = MockPsp::start().await?;
    fake.fail_next(
        Route::History,
        [
            Failure::TooManyRequests { retry_after: 5 },
            Failure::Unavailable { retry_after: 7 },
            Failure::InternalError,
        ],
    );
    let client = client_of(&fake);
    for expected in [Some(5), Some(7)] {
        match client.history(payment_id(1)).await {
            Err(ReadError::TryLater { retry_after }) => assert_eq!(retry_after, expected),
            other => panic!("{other:?}"),
        }
    }
    match client.history(payment_id(1)).await {
        Err(ReadError::Status { status }) => assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR),
        other => panic!("{other:?}"),
    }
    // Each failure answered one call, and the next is served.
    assert!(client.history(payment_id(1)).await?.facts().is_empty());
    Ok(())
}

#[tokio::test]
async fn a_503_without_retry_after_is_any_other_status() -> TestResult {
    let psp = ScriptedPsp::start().await;
    let client = psp.client();
    psp.answer(StatusCode::SERVICE_UNAVAILABLE, None, Vec::new());
    match client.history(payment_id(1)).await {
        Err(ReadError::Status { status }) => assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE),
        other => panic!("{other:?}"),
    }
    psp.answer(StatusCode::TOO_MANY_REQUESTS, None, Vec::new());
    match client.history(payment_id(1)).await {
        Err(ReadError::TryLater { retry_after }) => assert_eq!(retry_after, None),
        other => panic!("{other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn every_call_carries_the_services_key() -> TestResult {
    let fake = MockPsp::start().await?;
    let wrong = PspClient::new(Psp::Mock, fake.url(), "mock_sk_wrong", TIMEOUT)?;
    match wrong.history(payment_id(1)).await {
        Err(ReadError::Status { status }) => assert_eq!(status, StatusCode::UNAUTHORIZED),
        other => panic!("{other:?}"),
    }
    assert!(client_of(&fake).history(payment_id(1)).await.is_ok());
    Ok(())
}

#[tokio::test]
async fn a_psp_that_never_answers_is_timed_out() -> TestResult {
    let (url, holder) = silent_psp().await;
    let client = PspClient::new(Psp::Mock, &url, "key", Duration::from_millis(300))?;
    let started = Instant::now();
    let read = client.history(payment_id(1)).await;
    let waited = started.elapsed();
    holder.abort();
    assert!(matches!(read, Err(ReadError::TimedOut)), "{read:?}");
    assert!(waited >= Duration::from_millis(300), "{waited:?}");
    assert!(waited < Duration::from_secs(5), "{waited:?}");
    Ok(())
}

#[tokio::test]
async fn a_psp_that_is_down_gives_no_answer() -> TestResult {
    let client = PspClient::new(Psp::Mock, &closed_address().await, "key", TIMEOUT)?;
    let read = client.history(payment_id(1)).await;
    assert!(matches!(read, Err(ReadError::NoAnswer { .. })), "{read:?}");
    Ok(())
}

#[tokio::test]
async fn a_body_above_the_bound_is_unreadable() -> TestResult {
    let psp = ScriptedPsp::start().await;
    let client = psp.client();
    psp.answer(StatusCode::OK, None, vec![b' '; MAX_BODY + 1]);
    match client.history(payment_id(1)).await {
        Err(ReadError::Unreadable { source }) => assert_eq!(source, Unreadable::TooLarge),
        other => panic!("{other:?}"),
    }
    // At the bound, the body is read whole, and then it is not a history.
    psp.answer(StatusCode::OK, None, vec![b' '; MAX_BODY]);
    match client.history(payment_id(1)).await {
        Err(ReadError::Unreadable {
            source: Unreadable::Shape(_),
        }) => {}
        other => panic!("{other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn a_200_that_is_not_the_payments_history_is_unreadable() -> TestResult {
    let psp = ScriptedPsp::start().await;
    let other = payment_id(2).get();
    psp.answer(
        StatusCode::OK,
        None,
        serde_json::to_vec(&json!({"payment_id": other, "facts": []}))?,
    );
    match psp.client().history(payment_id(1)).await {
        Err(ReadError::Unreadable { source }) => {
            assert_eq!(source, Unreadable::OtherPayment { found: other });
        }
        other => panic!("{other:?}"),
    }
    Ok(())
}
