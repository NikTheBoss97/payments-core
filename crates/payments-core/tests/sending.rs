// Author: Nicola Antonello

//! Sending requests to the PSP (ADR-0028, ADR-0032, ADR-0042): the sender against the
//! fake over real HTTP (ADR-0029), and against PSPs that answer what the test writes.
//! Each step gets the time that the test chooses and a seeded source of jitter, so no
//! test sleeps.

mod common;

use std::error::Error;
use std::time::Duration;

use axum::http::StatusCode;
use common::payments::{REQUESTED, create_command, fee_rate};
use common::psp::{ScriptedPsp, client_of, send as post};
use common::{merchant_id, request_id, runtime_pool, runtime_transaction, tenant_pool};
use futures_util::future::join;
use mock_psp::wire::Detail;
use mock_psp::{Failure, MockPsp, Route};
use payments_core::catch_up::{StopReason, Swept, catch_up};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::money::PositiveAmount;
use payments_core::payments::{create_payment, register_merchant};
use payments_core::requests::{Ask, RecordRequest, record_request};
use payments_core::retries::RetryBudget;
use payments_core::sender::{Attempt, Report, Sender};
use payments_core::tenancy::Tenant;
use serde_json::json;
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a positive amount")
}

/// `seconds` after 2026-01-01T00:00:00Z.
fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(1_767_225_600_000_000 + seconds * 1_000_000)
        .expect("a time in range")
}

/// SplitMix64, as in the model's tests: seeded jitter, so that a failure repeats.
fn jitter(seed: u64) -> impl FnMut() -> u64 {
    let mut state = seed;
    move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Jitter that always draws zero: a retry is due at once, unless the PSP said later.
fn none() -> impl FnMut() -> u64 {
    || 0
}

/// A merchant with its pool, the system role's pool, on which the sender runs, and a
/// fake PSP.
struct World {
    system: PgPool,
    tenant: PgPool,
    merchant: MerchantId,
    fake: MockPsp,
}

impl World {
    async fn new(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let system = runtime_pool(pool).await?;
        let merchant = merchant_id(1);
        register_merchant(&system, merchant, fee_rate()).await?;
        Ok(Self {
            tenant: tenant_pool(pool).await?,
            system,
            merchant,
            fake: MockPsp::start().await?,
        })
    }

    fn me(&self) -> Tenant<'_> {
        Tenant::new(&self.tenant, self.merchant)
    }

    /// Payment `n`, with request `n` to authorize it recorded and committed, and not
    /// sent: sending is the sender's.
    async fn requested(&self, n: u32) -> Result<RecordRequest, Box<dyn Error>> {
        let created = create_command(n, self.merchant);
        create_payment(self.me(), &created).await?;
        let request = RecordRequest {
            request: request_id(n),
            payment: created.id,
            ask: Ask::Authorize(amount(REQUESTED)),
        };
        record_request(self.me(), &request).await?;
        Ok(request)
    }

    /// A sender of the fake, as a process starts one.
    fn sender(&self) -> Sender {
        Sender::new(client_of(&self.fake), at(0))
    }

    async fn close(self) {
        self.system.close().await;
        self.tenant.close().await;
    }
}

/// The answers recorded, by request, in the order of the IDs.
async fn answers(pool: &PgPool) -> sqlx::Result<Vec<(Uuid, String)>> {
    let rows =
        sqlx::query!("SELECT request_id, answer FROM payment_request_answers ORDER BY request_id")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.request_id, row.answer))
        .collect())
}

/// What each attempt of a step came to, by request, in short.
fn attempts(report: &Report) -> Vec<(Uuid, &'static str)> {
    report
        .attempts
        .iter()
        .map(|(request, attempt)| {
            let what = match attempt {
                Attempt::Sent => "sent",
                Attempt::Found => "found",
                Attempt::Malformed { .. } => "malformed",
                Attempt::TryLater { .. } => "try later",
                Attempt::Unknown(_) => "unknown",
                Attempt::LookupFailed(_) => "lookup failed",
                Attempt::Stopped => "stopped",
                Attempt::NotRecorded(_) => "not recorded",
                Attempt::NoToken => "no token",
            };
            (*request, what)
        })
        .collect()
}

/// Stops a payment as the catch-up does, as the system role (ADR-0041).
async fn stop(pool: &PgPool, payment: PaymentId) -> TestResult {
    let mut tx = runtime_transaction(pool).await?;
    sqlx::query!(
        "INSERT INTO payment_stops (payment_id, position, reason) VALUES ($1, $2, $3)",
        payment.get(),
        1,
        StopReason::NoPlace.as_str()
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// A history in which the PSP holds nothing of this payment.
fn empty_history(payment: PaymentId) -> Vec<u8> {
    serde_json::to_vec(&json!({"payment_id": payment.get(), "facts": [], "requests": []}))
        .expect("JSON")
}

#[sqlx::test]
async fn a_request_handed_over_is_sent_at_once_without_a_lookup(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let id = request.request.get();
    let psp = ScriptedPsp::start().await;
    psp.answer(StatusCode::ACCEPTED, None, Vec::new());
    let mut sender = Sender::new(psp.client(), at(0));
    sender.hand_over(&request.request);

    let report = sender
        .send_due(&world.system, at(0), &mut jitter(1))
        .await?;
    assert_eq!(attempts(&report), [(id, "sent")]);
    // One send, and no lookup: nothing can have sent it before (ADR-0042).
    assert_eq!(psp.calls(), (0, 1));
    assert_eq!(
        answers(&world.system).await?,
        [(id, "acknowledged".to_owned())]
    );
    // Acknowledged, it leaves the sender: were it still there, a retry would be due at
    // once with this jitter.
    let report = sender
        .send_due(&world.system, at(3_600), &mut none())
        .await?;
    assert!(report.attempts.is_empty(), "{report:?}");
    assert_eq!(psp.calls(), (0, 1));
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_request_that_the_search_finds_is_looked_up_before_it_is_sent(
    pool: PgPool,
) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let id = request.request.get();
    let mut sender = world.sender();

    // Not handed over, as after a restart: it may have been sent before a crash, so it
    // waits a jittered delay, here the longest, two seconds (ADR-0042).
    let report = sender.send_due(&world.system, at(0), &mut || 2_000).await?;
    assert!(report.attempts.is_empty(), "{report:?}");
    // A lookup that fails sends nothing.
    world
        .fake
        .fail_next(Route::History, [Failure::InternalError]);
    let report = sender.send_due(&world.system, at(2), &mut none()).await?;
    assert_eq!(attempts(&report), [(id, "lookup failed")]);
    assert_eq!(world.fake.times_taken(id), 0);
    // The next lookup finds the PSP without the request, so it is sent.
    let report = sender.send_due(&world.system, at(2), &mut none()).await?;
    assert_eq!(attempts(&report), [(id, "sent")]);
    assert_eq!(world.fake.times_taken(id), 1);
    assert_eq!(
        answers(&world.system).await?,
        [(id, "acknowledged".to_owned())]
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_lookup_that_finds_the_request_records_it_and_sends_nothing(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let id = request.request.get();
    // The PSP took it, and then its reply, or the process that sent it, was lost.
    post(&world.fake, &request).await;

    let mut sender = world.sender();
    let report = sender.send_due(&world.system, at(0), &mut none()).await?;
    assert_eq!(attempts(&report), [(id, "found")]);
    assert_eq!(world.fake.times_taken(id), 1);
    assert_eq!(
        answers(&world.system).await?,
        [(id, "acknowledged".to_owned())]
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_malformed_request_is_recorded_and_never_sent_again(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let id = request.request.get();
    let psp = ScriptedPsp::start().await;
    psp.answer_sends(StatusCode::BAD_REQUEST, None, Vec::new());
    let mut sender = Sender::new(psp.client(), at(0));
    sender.hand_over(&request.request);

    let report = sender.send_due(&world.system, at(0), &mut none()).await?;
    assert_eq!(attempts(&report), [(id, "malformed")]);
    assert_eq!(
        answers(&world.system).await?,
        [(id, "malformed".to_owned())]
    );
    // A bug of ours, which no retry can fix (ADR-0032).
    let report = sender
        .send_due(&world.system, at(86_400), &mut none())
        .await?;
    assert!(report.attempts.is_empty(), "{report:?}");
    assert_eq!(psp.calls(), (0, 1));
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn each_retry_waits_up_to_twice_as_long_as_the_one_before(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let psp = ScriptedPsp::start().await;
    psp.answer(StatusCode::SERVICE_UNAVAILABLE, None, Vec::new());
    let mut sender = Sender::new(psp.client(), at(0));
    sender.hand_over(&request.request);
    // Jitter that draws the top of each bound: 2 seconds, then 4, 8 and 16 (ADR-0032).
    let mut tops = [2_000_u64, 4_000, 8_000, 16_000].into_iter();
    let mut longest = move || tops.next().expect("four delays");

    let mut made = Vec::new();
    for second in 0..=14 {
        let report = sender
            .send_due(&world.system, at(second), &mut longest)
            .await?;
        if !report.attempts.is_empty() {
            made.push(second);
        }
    }
    // The first send at once, then retries 2, 4 and 8 seconds apart.
    assert_eq!(made, [0, 2, 6, 14]);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn when_tokens_run_short_the_request_due_longest_goes_first(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let first = world.requested(1).await?.request.get();
    let second = world.requested(2).await?.request.get();
    let psp = ScriptedPsp::start().await;
    psp.answer(StatusCode::SERVICE_UNAVAILABLE, None, Vec::new());
    let budget = RetryBudget::new(1, Duration::from_secs(60), at(0));
    let mut sender = Sender::with_budget(psp.client(), budget);
    // Found after a restart, in the order of their IDs: the first due in a second and a
    // half, the second in half a second.
    let mut draws = [1_500_u64, 500, 2_000].into_iter();
    let mut drawn = move || draws.next().expect("three draws");

    let report = sender.send_due(&world.system, at(0), &mut drawn).await?;
    assert!(report.attempts.is_empty(), "{report:?}");
    // Both are due, and the one token goes to the second, due longer.
    let report = sender.send_due(&world.system, at(2), &mut drawn).await?;
    assert_eq!(
        attempts(&report),
        [(second, "lookup failed"), (first, "no token")]
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_lookup_that_is_asked_to_wait_holds_the_next_attempt_back(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let id = request.request.get();
    let psp = ScriptedPsp::start().await;
    psp.answer(StatusCode::SERVICE_UNAVAILABLE, Some(300), Vec::new());
    let mut sender = Sender::new(psp.client(), at(0));

    let report = sender.send_due(&world.system, at(0), &mut none()).await?;
    assert_eq!(attempts(&report), [(id, "lookup failed")]);
    // The PSP asked for five minutes, whatever the jitter draws (ADR-0032).
    let report = sender.send_due(&world.system, at(299), &mut none()).await?;
    assert!(report.attempts.is_empty(), "{report:?}");
    let report = sender.send_due(&world.system, at(300), &mut none()).await?;
    assert_eq!(attempts(&report), [(id, "lookup failed")]);
    assert_eq!(psp.calls(), (2, 0));
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_after_holds_the_next_attempt_back(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let id = request.request.get();
    let psp = ScriptedPsp::start().await;
    psp.answer_sends(StatusCode::TOO_MANY_REQUESTS, Some(120), Vec::new());
    psp.answer_history(StatusCode::OK, None, empty_history(request.payment));
    let mut sender = Sender::new(psp.client(), at(0));
    sender.hand_over(&request.request);

    let report = sender.send_due(&world.system, at(0), &mut none()).await?;
    assert_eq!(attempts(&report), [(id, "try later")]);
    // Whatever the jitter draws, the next attempt waits the two minutes (ADR-0032).
    let report = sender.send_due(&world.system, at(119), &mut none()).await?;
    assert!(report.attempts.is_empty(), "{report:?}");
    assert_eq!(psp.calls(), (0, 1));
    psp.answer_sends(StatusCode::ACCEPTED, None, Vec::new());
    let report = sender.send_due(&world.system, at(120), &mut none()).await?;
    assert_eq!(attempts(&report), [(id, "sent")]);
    // A retry: the lookup came first (ADR-0028).
    assert_eq!(psp.calls(), (1, 2));
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn any_other_answer_is_an_unknown_outcome_that_the_next_attempt_looks_up(
    pool: PgPool,
) -> TestResult {
    let world = World::new(&pool).await?;
    let psp = ScriptedPsp::start().await;
    let mut sender = Sender::new(psp.client(), at(0));
    // A 503 without Retry-After, a 5xx, and 4xx answers that ADR-0032's table does not
    // name, a wrong key among them: none makes a request malformed (ADR-0042).
    for (n, status) in [
        (1, StatusCode::SERVICE_UNAVAILABLE),
        (2, StatusCode::INTERNAL_SERVER_ERROR),
        (3, StatusCode::UNAUTHORIZED),
        (4, StatusCode::NOT_FOUND),
        (5, StatusCode::CONFLICT),
    ] {
        let request = world.requested(n).await?;
        psp.answer_sends(status, None, Vec::new());
        sender.hand_over(&request.request);
        // The longest delay, so that the earlier requests are not due yet.
        let report = sender.send_due(&world.system, at(0), &mut || 2_000).await?;
        assert_eq!(
            attempts(&report),
            [(request.request.get(), "unknown")],
            "{status}"
        );
    }
    assert_eq!(psp.calls(), (0, 5));
    assert!(answers(&world.system).await?.is_empty());

    // Each next attempt is a retry: a lookup first, and a failed one sends nothing.
    psp.answer_history(StatusCode::SERVICE_UNAVAILABLE, None, Vec::new());
    let report = sender.send_due(&world.system, at(2), &mut || 2_000).await?;
    assert_eq!(report.attempts.len(), 5, "{report:?}");
    assert!(
        report
            .attempts
            .iter()
            .all(|(_, attempt)| matches!(attempt, Attempt::LookupFailed(_))),
        "{report:?}"
    );
    assert_eq!(psp.calls(), (5, 5));
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_budget_bounds_the_retries_of_an_outage(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    for n in 1..=5 {
        world.requested(n).await?;
    }
    let psp = ScriptedPsp::start().await;
    psp.answer(StatusCode::SERVICE_UNAVAILABLE, None, Vec::new());
    // Two tokens, and one more every ten seconds.
    let budget = RetryBudget::new(2, Duration::from_secs(10), at(0));
    let mut sender = Sender::with_budget(psp.client(), budget);

    let mut rng = jitter(7);
    let mut waited = 0;
    for second in 0..=60 {
        let report = sender.send_due(&world.system, at(second), &mut rng).await?;
        waited += report
            .attempts
            .iter()
            .filter(|(_, attempt)| matches!(attempt, Attempt::NoToken))
            .count();
        // Five requests found after a restart, each due within two seconds as a retry,
        // and only two tokens until the bucket earns its next (ADR-0042).
        if second == 9 {
            assert_eq!(psp.calls(), (2, 0));
        }
    }
    // A PSP down for a minute: the retries went on at the bucket's pace, never past two
    // at once and one every ten seconds, and a failed lookup sent nothing.
    let (lookups, sends) = psp.calls();
    assert!((3..=8).contains(&lookups), "{lookups} lookups");
    assert_eq!(sends, 0);
    assert!(waited > 0);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_stopped_payments_request_is_held(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let id = request.request.get();
    let psp = ScriptedPsp::start().await;
    psp.answer(StatusCode::ACCEPTED, None, Vec::new());
    let mut sender = Sender::new(psp.client(), at(0));
    sender.hand_over(&request.request);
    stop(&pool, request.payment).await?;

    // Money that the PSP moved now could not reach the ledger (ADR-0042).
    for seconds in [0, 3_600] {
        let report = sender
            .send_due(&world.system, at(seconds), &mut none())
            .await?;
        assert!(report.attempts.is_empty(), "{report:?}");
        assert_eq!(report.held, [id]);
    }
    assert_eq!(psp.calls(), (0, 0));
    assert!(answers(&world.system).await?.is_empty());
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_lookup_whose_catch_up_stops_the_payment_holds_the_request(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let id = request.request.get();
    // The PSP holds a void that the service never asked for, and a decline of a request
    // that it never sent: a contradiction (ADR-0041).
    let unknown = RecordRequest {
        request: request_id(2),
        payment: request.payment,
        ask: Ask::Void,
    };
    post(&world.fake, &unknown).await;
    world.fake.append(
        request.payment.get(),
        Detail::Declined {
            request_id: Uuid::now_v7(),
        },
    );

    let mut sender = world.sender();
    let report = sender.send_due(&world.system, at(0), &mut none()).await?;
    assert_eq!(attempts(&report), [(id, "stopped")]);
    assert_eq!(world.fake.times_taken(id), 0);
    let report = sender
        .send_due(&world.system, at(3_600), &mut none())
        .await?;
    assert!(report.attempts.is_empty(), "{report:?}");
    assert_eq!(report.held, [id]);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_decided_request_leaves_the_sender(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let id = request.request.get();
    let mut sender = world.sender();
    world
        .fake
        .fail_next(Route::Requests, [Failure::InternalError]);
    sender.hand_over(&request.request);
    let report = sender.send_due(&world.system, at(0), &mut || 2_000).await?;
    assert_eq!(attempts(&report), [(id, "unknown")]);

    // Meanwhile the PSP took a copy and decided it, and a catch-up applied the decision.
    post(&world.fake, &request).await;
    world.fake.approve(id);
    catch_up(&world.system, &client_of(&world.fake), request.payment).await?;
    // Decided, the request needs no answer of its own (ADR-0042).
    let report = sender
        .send_due(&world.system, at(3_600), &mut none())
        .await?;
    assert!(report.attempts.is_empty(), "{report:?}");
    assert_eq!(world.fake.times_taken(id), 1);
    assert!(answers(&world.system).await?.is_empty());
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_request_unacknowledged_for_an_hour_is_flagged(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let id = request.request.get();
    let psp = ScriptedPsp::start().await;
    psp.answer(StatusCode::SERVICE_UNAVAILABLE, None, Vec::new());
    let mut sender = Sender::new(psp.client(), at(0));
    sender.hand_over(&request.request);

    for (seconds, flagged) in [(0, false), (3_599, false), (3_600, true)] {
        let report = sender
            .send_due(&world.system, at(seconds), &mut none())
            .await?;
        let expected: &[Uuid] = if flagged { &[id] } else { &[] };
        assert_eq!(report.overdue, expected, "at {seconds} s");
    }
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn two_senders_racing_leave_one_request_and_one_answer(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let request = world.requested(1).await?;
    let id = request.request.get();
    // Two processes, each of which found the request after a restart.
    let (mut first, mut second) = (world.sender(), world.sender());
    let (a, b) = join(
        first.send_due(&world.system, at(0), &mut none()),
        second.send_due(&world.system, at(0), &mut none()),
    )
    .await;
    for report in [a?, b?] {
        let made = attempts(&report);
        assert!(matches!(made[..], [(_, "sent" | "found")]), "{report:?}");
    }
    // The PSP drops a copy by its key, and the answer is recorded once (ADR-0028).
    assert_eq!(world.fake.times_taken(id), 1);
    assert_eq!(
        answers(&world.system).await?,
        [(id, "acknowledged".to_owned())]
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_sweep_and_the_sender_share_one_budget(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let mut due = Vec::new();
    for n in 1..=3 {
        let request = world.requested(n).await?;
        // The PSP holds each request undecided, so each payment waits for it.
        post(&world.fake, &request).await;
        due.push(request.payment);
    }
    let budget = RetryBudget::new(2, Duration::from_secs(1), at(0));
    let mut sender = Sender::with_budget(client_of(&world.fake), budget);
    let swept = |swept: Swept| -> Vec<PaymentId> {
        swept.into_iter().map(|(payment, _)| payment).collect()
    };

    // The pass resumes after its last payment, and wraps round (ADR-0042).
    assert_eq!(
        swept(sender.sweep(&world.system, at(0)).await?),
        [due[0], due[1]]
    );
    assert_eq!(swept(sender.sweep(&world.system, at(1)).await?), [due[2]]);
    assert_eq!(swept(sender.sweep(&world.system, at(2)).await?), [due[0]]);
    // The sweep spent the tokens, so the sender's retries wait for later ones.
    let report = sender.send_due(&world.system, at(2), &mut none()).await?;
    assert_eq!(report.attempts.len(), 3, "{report:?}");
    assert!(
        report
            .attempts
            .iter()
            .all(|(_, attempt)| matches!(attempt, Attempt::NoToken)),
        "{report:?}"
    );
    world.close().await;
    Ok(())
}
