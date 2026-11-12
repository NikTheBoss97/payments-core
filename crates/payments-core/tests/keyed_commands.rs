// Author: Nicola Antonello

//! Commands under an idempotency key (ADR-0026), as a merchant's requests run them.
//!
//! A key's record holds the final answer and commits in the same transaction as the
//! effect. Each attempt of a request has fresh IDs, so the fixtures' `n` differs
//! between attempts, and only the key can tell two attempts apart from two requests:
//! that is what these tests attack. The effect is a payment created, a cancel, or a
//! request to the PSP (ADR-0028), and the PSP's word moves the payments in between, as
//! the server takes them (ADR-0043).

mod common;

use std::error::Error;

use common::keys::{answered, key, print, render};
use common::payments::{
    AUTHORIZED, REQUESTED, balance_of, create_command, eur, fee_rate, requests_of, steps,
};
use common::psp::{PspSide, hand_over_and_step};
use common::{
    account_id, merchant_id, payment_id, request_id, runtime_pool, tenant_pool, transition_id,
};
use futures_util::future::{join, join_all};
use payments_core::accounts::{AccountRef, Psp};
use payments_core::idempotency::{Answer, Decided, IdempotencyKey, Recall, Seal, recall};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::Refused;
use payments_core::money::{Currency, PositiveAmount};
use payments_core::payments::{
    AccountCandidates, CreateError, CreatePayment, Outcome, StepCommand, cancel, create_payment,
    find_payment, register_merchant,
};
use payments_core::requests::{Ask, RecordRequest, Refusal, RequestError, record_request};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

/// The tenant role's pool and two merchants that the system role registered.
async fn two_merchants(pool: &PgPool) -> Result<(PgPool, MerchantId, MerchantId), Box<dyn Error>> {
    let system = runtime_pool(pool).await?;
    let (a, b) = (merchant_id(1), merchant_id(2));
    register_merchant(&system, a, fee_rate()).await?;
    register_merchant(&system, b, fee_rate()).await?;
    system.close().await;
    Ok((tenant_pool(pool).await?, a, b))
}

async fn created(
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let command = create_command(n, merchant);
    create_payment(me, &command).await?;
    Ok(command.id)
}

/// Payment `n`, authorized for [`AUTHORIZED`] by the PSP.
async fn authorized(
    psp: &mut PspSide,
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = created(me, merchant, n).await?;
    psp.authorized(me, payment).await;
    Ok(payment)
}

/// Payment `n`, captured for [`AUTHORIZED`] by the PSP.
async fn captured(
    psp: &mut PspSide,
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = authorized(psp, me, merchant, n).await?;
    psp.captured(me, payment).await;
    Ok(payment)
}

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a positive amount")
}

/// Request `n`, which asks this of this payment.
fn request(n: u32, payment: PaymentId, ask: Ask) -> RecordRequest {
    RecordRequest {
        request: request_id(n),
        payment,
        ask,
    }
}

/// The answer that a recorded request `n` stores.
fn requested(n: u32) -> Recall {
    answered(202, &format!(r#"{{"request":"{}"}}"#, request_id(n).get()))
}

/// How many answers are stored, whoever owns them.
async fn records(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM idempotency_records"#)
        .fetch_one(pool)
        .await
}

#[sqlx::test]
async fn an_applied_command_stores_its_answer_with_its_effect(pool: PgPool) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let mut psp = PspSide::start(&pool).await?;
    let me = Tenant::new(&tenant, merchant);

    let create = create_command(1, merchant);
    let payment = create.id;
    let k = key("create");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    create_payment(me.sealed(&seal), &create).await?;
    assert_eq!(
        recall(me, &k, print(1)).await?,
        answered(200, r#"{"seq":0,"refundable":0}"#)
    );

    // Each request stores its 202 with it, and the PSP's word then moves the payment on
    // to where the next one is taken.
    for (n, name, ask) in [
        (2, "authorize", Ask::Authorize(amount(REQUESTED))),
        (3, "capture", Ask::Capture(amount(AUTHORIZED))),
        (4, "refund", Ask::Refund(amount(3_000))),
    ] {
        let k = key(name);
        let seal = Seal {
            key: &k,
            fingerprint: print(n),
            answer: &render,
        };
        let command = request(n, payment, ask);
        record_request(me.sealed(&seal), &command).await?;
        assert_eq!(recall(me, &k, print(n)).await?, requested(n));
        hand_over_and_step(&mut psp.sender, &psp.system, &command.request).await;
        match ask {
            Ask::Authorize(_) => psp.fake.approve_for(request_id(n).get(), AUTHORIZED),
            _ => psp.fake.approve(request_id(n).get()),
        }
        psp.caught_up(payment).await;
    }

    let other = authorized(&mut psp, me, merchant, 2).await?;
    let k = key("void");
    let seal = Seal {
        key: &k,
        fingerprint: print(5),
        answer: &render,
    };
    record_request(me.sealed(&seal), &request(5, other, Ask::Void)).await?;
    assert_eq!(recall(me, &k, print(5)).await?, requested(5));

    let cancelled = created(me, merchant, 3).await?;
    let k = key("cancel");
    let seal = Seal {
        key: &k,
        fingerprint: print(6),
        answer: &render,
    };
    let command = StepCommand {
        step: transition_id(901),
        payment: cancelled,
    };
    cancel(me.sealed(&seal), &command).await?;
    assert_eq!(
        recall(me, &k, print(6)).await?,
        answered(200, r#"{"seq":1,"refundable":0}"#)
    );

    // The effects are there too, one each.
    assert_eq!(requests_of(&pool, payment).await?, 3);
    assert_eq!(requests_of(&pool, other).await?, 2);
    assert_eq!(steps(&pool, cancelled).await?, 2);
    assert_eq!(records(&pool).await?, 6);
    let owners = sqlx::query_scalar!("SELECT DISTINCT merchant_id FROM idempotency_records")
        .fetch_all(&pool)
        .await?;
    assert_eq!(owners, [merchant.get()]);
    psp.close().await;
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_refusal_is_stored_and_changes_nothing_else(pool: PgPool) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = created(me, merchant, 1).await?;

    // An illegal move, for a request whose decision would post and for one whose would
    // not.
    let k = key("capture-too-early");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    let err = record_request(
        me.sealed(&seal),
        &request(1, payment, Ask::Capture(amount(AUTHORIZED))),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            RequestError::Refused(Refusal::Refused(Refused::Illegal(_)))
        ),
        "{err:?}"
    );
    assert_eq!(
        recall(me, &k, print(1)).await?,
        answered(409, r#"{"type":"illegal"}"#)
    );

    let k = key("void-too-early");
    let seal = Seal {
        key: &k,
        fingerprint: print(2),
        answer: &render,
    };
    let err = record_request(me.sealed(&seal), &request(2, payment, Ask::Void))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RequestError::Refused(Refusal::Refused(Refused::Illegal(_)))
        ),
        "{err:?}"
    );
    assert_eq!(
        recall(me, &k, print(2)).await?,
        answered(409, r#"{"type":"illegal"}"#)
    );

    // An amount above its limit.
    let k = key("authorize-too-much");
    let seal = Seal {
        key: &k,
        fingerprint: print(3),
        answer: &render,
    };
    let err = record_request(
        me.sealed(&seal),
        &request(3, payment, Ask::Authorize(amount(REQUESTED + 1))),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            RequestError::Refused(Refusal::Refused(Refused::AboveLimit { .. }))
        ),
        "{err:?}"
    );
    assert_eq!(
        recall(me, &k, print(3)).await?,
        answered(422, r#"{"type":"limit","limit":10000}"#)
    );

    assert_eq!(steps(&pool, payment).await?, 1);
    assert_eq!(requests_of(&pool, payment).await?, 0);
    assert_eq!(records(&pool).await?, 3);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_late_duplicate_of_a_refusal_cannot_become_an_effect(pool: PgPool) -> TestResult {
    // The hazard that storing refusals exists for: a client was told no, the payment
    // then moved on, and a delayed copy of its request arrives.
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let mut psp = PspSide::start(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = created(me, merchant, 1).await?;

    let k = key("capture");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    let capture = |n| request(n, payment, Ask::Capture(amount(AUTHORIZED)));
    let err = record_request(me.sealed(&seal), &capture(1))
        .await
        .unwrap_err();
    assert!(matches!(err, RequestError::Refused(_)), "{err:?}");

    // Now the same capture would be recorded.
    psp.authorized(me, payment).await;
    assert_eq!(
        recall(me, &k, print(1)).await?,
        answered(409, r#"{"type":"illegal"}"#)
    );

    // Even an attempt that looked the key up before the refusal committed cannot be
    // recorded: fresh IDs, the same key, and it would otherwise succeed.
    let err = record_request(me.sealed(&seal), &capture(2))
        .await
        .unwrap_err();
    assert!(matches!(err, RequestError::KeyTaken), "{err:?}");
    // The authorization's request alone, and no money moved.
    assert_eq!(requests_of(&pool, payment).await?, 1);
    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(
        balance_of(&pool, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        0
    );
    psp.close().await;
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn what_is_not_final_stores_nothing(pool: PgPool) -> TestResult {
    let (tenant, merchant, stranger) = two_merchants(&pool).await?;
    let mut psp = PspSide::start(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let theirs = captured(&mut psp, Tenant::new(&tenant, stranger), stranger, 1).await?;
    let k = key("k");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };

    // No such payment, or another merchant's: IDs are made by the server, so this
    // cannot become true later. Were the stranger's payment mine, the refund would be
    // recorded.
    for (n, payment, ask) in [
        (2, theirs, Ask::Refund(amount(1))),
        (3, payment_id(404), Ask::Refund(amount(1))),
        (4, payment_id(404), Ask::Authorize(amount(REQUESTED))),
    ] {
        let err = record_request(me.sealed(&seal), &request(n, payment, ask))
            .await
            .unwrap_err();
        assert!(matches!(err, RequestError::NotFound { .. }), "{err:?}");
    }

    // A merchant the session cannot see, and a currency that changes only by migration.
    let err = create_payment(me.sealed(&seal), &create_command(2, stranger))
        .await
        .unwrap_err();
    assert!(
        matches!(err, CreateError::UnknownMerchant { .. }),
        "{err:?}"
    );
    let command = CreatePayment {
        currency: Currency::parse("XTS")?,
        accounts: AccountCandidates {
            fee_revenue: account_id(801),
            merchant_payable: account_id(802),
            psp_receivable: account_id(803),
        },
        ..create_command(3, merchant)
    };
    let err = create_payment(me.sealed(&seal), &command)
        .await
        .unwrap_err();
    assert!(
        matches!(err, CreateError::UnknownCurrency { .. }),
        "{err:?}"
    );

    assert_eq!(records(&pool).await?, 0);
    assert_eq!(recall(me, &k, print(1)).await?, Recall::Unused);
    psp.close().await;
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_answer_that_is_not_final_takes_the_effect_down_with_it(pool: PgPool) -> TestResult {
    // A 5xx means an unknown outcome, so a column's CHECK refuses to keep one, and
    // since the record shares the effect's transaction, the effect goes too.
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = created(me, merchant, 1).await?;
    let unfinished = |_: Decided<'_>| Answer {
        status: 503,
        body: "{}".to_owned(),
    };
    let k = key("k");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &unfinished,
    };
    // A request that would be recorded, so that there is an effect to take down.
    let err = record_request(
        me.sealed(&seal),
        &request(1, payment, Ask::Authorize(amount(REQUESTED))),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, RequestError::Invariant { .. }), "{err:?}");
    assert_eq!(requests_of(&pool, payment).await?, 0);
    assert_eq!(records(&pool).await?, 0);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_second_attempt_under_a_key_rolls_back_whole(pool: PgPool) -> TestResult {
    // The second refund fits what is left, and has fresh IDs, so nothing but the key
    // stops it. An attempt that recorded its request must take it back.
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let mut psp = PspSide::start(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = captured(&mut psp, me, merchant, 1).await?;
    let before = requests_of(&pool, payment).await?;
    let k = key("refund");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };

    let refund = |n| request(n, payment, Ask::Refund(amount(1_000)));
    record_request(me.sealed(&seal), &refund(1)).await?;
    let err = record_request(me.sealed(&seal), &refund(2))
        .await
        .unwrap_err();
    assert!(matches!(err, RequestError::KeyTaken), "{err:?}");

    assert_eq!(requests_of(&pool, payment).await?, before + 1);
    assert_eq!(recall(me, &k, print(1)).await?, requested(1));
    psp.close().await;
    tenant.close().await;
    Ok(())
}

/// An attempt at creating a payment under a key, whose answer names the payment.
async fn create_under_key(
    me: Tenant<'_>,
    key: &IdempotencyKey,
    command: &CreatePayment,
) -> Result<Outcome, CreateError> {
    let payment = command.id;
    let render_created = move |_: Decided<'_>| Answer {
        status: 201,
        body: format!(r#"{{"payment":"{payment}"}}"#),
    };
    let seal = Seal {
        key,
        fingerprint: print(1),
        answer: &render_created,
    };
    create_payment(me.sealed(&seal), command).await
}

#[sqlx::test]
async fn copies_of_a_create_racing_give_one_payment(pool: PgPool) -> TestResult {
    // No lock to queue on, and each copy has its own payment ID, so nothing but the
    // key's unique index stops eight payments. The answer names the winner's payment.
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let k = key("create");
    let commands: Vec<CreatePayment> = (1..=8).map(|n| create_command(n, merchant)).collect();
    let results = join_all(
        commands
            .iter()
            .map(|command| create_under_key(me, &k, command)),
    )
    .await;

    let mut winners = Vec::new();
    for (command, result) in commands.iter().zip(results) {
        match result {
            Ok(Outcome::Applied(_)) => winners.push(command.id),
            Err(CreateError::KeyTaken) => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(winners.len(), 1);
    let stored = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payments"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 1);
    assert_eq!(
        recall(me, &k, print(1)).await?,
        answered(201, &format!(r#"{{"payment":"{}"}}"#, winners[0]))
    );
    assert!(find_payment(me, winners[0]).await?.is_some());
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn one_key_on_two_payments_gives_one_effect(pool: PgPool) -> TestResult {
    // Two locks, so neither waits on the other until the key's index makes one wait.
    // The loser's request differs, so looking the key up says that it was reused.
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let mut psp = PspSide::start(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let first = authorized(&mut psp, me, merchant, 1).await?;
    let second = authorized(&mut psp, me, merchant, 2).await?;
    let k = key("k");
    let (first_seal, second_seal) = (
        Seal {
            key: &k,
            fingerprint: print(1),
            answer: &render,
        },
        Seal {
            key: &k,
            fingerprint: print(2),
            answer: &render,
        },
    );
    let capture = |n, payment| request(n, payment, Ask::Capture(amount(AUTHORIZED)));
    let (first_command, second_command) = (capture(1, first), capture(2, second));

    let (one, other) = join(
        record_request(me.sealed(&first_seal), &first_command),
        record_request(me.sealed(&second_seal), &second_command),
    )
    .await;
    let (won, lost, loser_print) = match (one, other) {
        (Ok(_), Err(RequestError::KeyTaken)) => (first, second, print(2)),
        (Err(RequestError::KeyTaken), Ok(_)) => (second, first, print(1)),
        other => panic!("{other:?}"),
    };
    // Each payment's authorization, and the winner's capture.
    assert_eq!(requests_of(&pool, won).await?, 2);
    assert_eq!(requests_of(&pool, lost).await?, 1);
    assert_eq!(recall(me, &k, loser_print).await?, Recall::Reused);
    psp.close().await;
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn two_merchants_may_hold_one_key(pool: PgPool) -> TestResult {
    let (tenant, a, b) = two_merchants(&pool).await?;
    let (of_a, of_b) = (Tenant::new(&tenant, a), Tenant::new(&tenant, b));
    let k = key("the-same-key");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    create_payment(of_a.sealed(&seal), &create_command(1, a)).await?;
    assert_eq!(recall(of_b, &k, print(1)).await?, Recall::Unused);
    create_payment(of_b.sealed(&seal), &create_command(2, b)).await?;
    let stored = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payments"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 2);
    assert_eq!(records(&pool).await?, 2);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn looking_a_key_up_tells_a_retry_from_a_reuse(pool: PgPool) -> TestResult {
    let (tenant, merchant, stranger) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let k = key("k");
    assert_eq!(recall(me, &k, print(1)).await?, Recall::Unused);

    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    create_payment(me.sealed(&seal), &create_command(1, merchant)).await?;

    assert_eq!(
        recall(me, &k, print(1)).await?,
        answered(200, r#"{"seq":0,"refundable":0}"#)
    );
    assert_eq!(recall(me, &k, print(2)).await?, Recall::Reused);
    // Another merchant's record is not there to be seen, or to be reused.
    assert_eq!(
        recall(Tenant::new(&tenant, stranger), &k, print(1)).await?,
        Recall::Unused
    );
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn looking_a_key_up_names_its_merchant_without_the_policy(pool: PgPool) -> TestResult {
    // Enforced twice (ADR-0025): the test's own pool is a superuser's, which no policy
    // binds, so only the lookup's own filter keeps the other merchant's answer out.
    let (tenant, merchant, stranger) = two_merchants(&pool).await?;
    let k = key("the-same-key");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    create_payment(
        Tenant::new(&tenant, stranger).sealed(&seal),
        &create_command(1, stranger),
    )
    .await?;
    assert_eq!(
        recall(Tenant::new(&pool, merchant), &k, print(1)).await?,
        Recall::Unused
    );
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_key_the_type_accepts_is_a_key_the_column_stores(pool: PgPool) -> TestResult {
    // Rust parses first and the column stands behind it, so they must agree, on every
    // ASCII character and at the limit of the length.
    let print = print(1);
    let digest = print.as_bytes();
    let mut candidates: Vec<String> = (1_u8..=127)
        .map(|byte| char::from(byte).to_string())
        .collect();
    candidates.extend([
        "".to_owned(),
        "é".to_owned(),
        "k".repeat(255),
        "k".repeat(256),
    ]);
    for candidate in candidates {
        let parsed = IdempotencyKey::new(&candidate).is_ok();
        let stored = sqlx::query!(
            "INSERT INTO operator_idempotency_records (key, fingerprint, status, body) \
             VALUES ($1, $2, 200, '{}')",
            candidate,
            digest
        )
        .execute(&pool)
        .await
        .is_ok();
        assert_eq!(parsed, stored, "{candidate:?}");
    }
    Ok(())
}
