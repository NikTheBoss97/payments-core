// Author: Nicola Antonello

//! Commands under an idempotency key (ADR-0026), as a merchant's requests run them.
//!
//! A key's record holds the final answer and commits in the same transaction as the
//! effect. Each attempt of a request has fresh IDs, so the fixtures' `n` differs
//! between attempts, and only the key can tell two attempts apart from two requests:
//! that is what these tests attack.

mod common;

use std::error::Error;

use common::keys::{answered, key, print, render};
use common::payments::{
    AUTHORIZED, FEE, REQUESTED, authorize_command, balance_of, capture_command, create_command,
    eur, fee_rate, refund_command, steps, unbalanced_currencies, uncaused_postings,
};
use common::{account_id, merchant_id, payment_id, runtime_pool, tenant_pool, transition_id};
use futures_util::future::{join, join_all};
use payments_core::accounts::{AccountRef, Psp};
use payments_core::idempotency::{Answer, IdempotencyKey, Recall, Seal, recall};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::Refused;
use payments_core::money::{Currency, PositiveAmount};
use payments_core::payments::{
    AccountCandidates, Authorize, AuthorizeError, Capture, CaptureError, CreateError,
    CreatePayment, Outcome, Recorded, Refund, RefundError, StepCommand, StepError, authorize,
    capture, create_payment, find_payment, refund, register_merchant, void,
};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use uuid::Uuid;

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

async fn authorized(
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = created(me, merchant, n).await?;
    authorize(me, &authorize_command(n, payment)).await?;
    Ok(payment)
}

async fn captured(
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = authorized(me, merchant, n).await?;
    capture(me, &capture_command(n, payment)).await?;
    Ok(payment)
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

    let k = key("authorize");
    let seal = Seal {
        key: &k,
        fingerprint: print(2),
        answer: &render,
    };
    authorize(me.sealed(&seal), &authorize_command(1, payment)).await?;
    assert_eq!(
        recall(me, &k, print(2)).await?,
        answered(200, r#"{"seq":1,"refundable":0}"#)
    );

    let k = key("capture");
    let seal = Seal {
        key: &k,
        fingerprint: print(3),
        answer: &render,
    };
    capture(me.sealed(&seal), &capture_command(1, payment)).await?;
    assert_eq!(
        recall(me, &k, print(3)).await?,
        answered(200, r#"{"seq":2,"refundable":8000}"#)
    );

    let k = key("refund");
    let seal = Seal {
        key: &k,
        fingerprint: print(4),
        answer: &render,
    };
    refund(me.sealed(&seal), &refund_command(1, payment, 3_000)).await?;
    assert_eq!(
        recall(me, &k, print(4)).await?,
        answered(200, r#"{"seq":3,"refundable":5000}"#)
    );

    let other = authorized(me, merchant, 2).await?;
    let k = key("void");
    let seal = Seal {
        key: &k,
        fingerprint: print(5),
        answer: &render,
    };
    let command = StepCommand {
        step: transition_id(901),
        payment: other,
    };
    void(me.sealed(&seal), &command).await?;
    assert_eq!(
        recall(me, &k, print(5)).await?,
        answered(200, r#"{"seq":2,"refundable":0}"#)
    );

    // The effects are there too, one each.
    assert_eq!(steps(&pool, payment).await?, 4);
    assert_eq!(steps(&pool, other).await?, 3);
    assert_eq!(records(&pool).await?, 5);
    let owners = sqlx::query_scalar!("SELECT DISTINCT merchant_id FROM idempotency_records")
        .fetch_all(&pool)
        .await?;
    assert_eq!(owners, [merchant.get()]);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_refusal_is_stored_and_changes_nothing_else(pool: PgPool) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = created(me, merchant, 1).await?;

    // An illegal move, for a command that posts and for one that does not.
    let k = key("capture-too-early");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    let err = capture(me.sealed(&seal), &capture_command(1, payment))
        .await
        .unwrap_err();
    assert!(
        matches!(err, CaptureError::Refused(Refused::Illegal(_))),
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
    let command = StepCommand {
        step: transition_id(901),
        payment,
    };
    let err = void(me.sealed(&seal), &command).await.unwrap_err();
    assert!(matches!(err, StepError::Illegal(_)), "{err:?}");
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
    let command = Authorize {
        amount: PositiveAmount::new(REQUESTED + 1)?,
        ..authorize_command(1, payment)
    };
    let err = authorize(me.sealed(&seal), &command).await.unwrap_err();
    assert!(
        matches!(err, AuthorizeError::Refused(Refused::AboveLimit { .. })),
        "{err:?}"
    );
    assert_eq!(
        recall(me, &k, print(3)).await?,
        answered(422, r#"{"type":"limit","limit":10000}"#)
    );

    assert_eq!(steps(&pool, payment).await?, 1);
    assert_eq!(records(&pool).await?, 3);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_late_duplicate_of_a_refusal_cannot_become_an_effect(pool: PgPool) -> TestResult {
    // The hazard that storing refusals exists for: a client was told no, the payment
    // then moved on, and a delayed copy of its request arrives.
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = created(me, merchant, 1).await?;

    let k = key("capture");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    let err = capture(me.sealed(&seal), &capture_command(1, payment))
        .await
        .unwrap_err();
    assert!(matches!(err, CaptureError::Refused(_)), "{err:?}");

    // Now the same capture would be legal.
    authorize(me, &authorize_command(1, payment)).await?;
    assert_eq!(
        recall(me, &k, print(1)).await?,
        answered(409, r#"{"type":"illegal"}"#)
    );

    // Even an attempt that looked the key up before the refusal committed cannot
    // apply: fresh IDs, the same key, and it would otherwise succeed.
    let err = capture(me.sealed(&seal), &capture_command(2, payment))
        .await
        .unwrap_err();
    assert!(matches!(err, CaptureError::KeyTaken), "{err:?}");
    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(
        balance_of(&pool, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        0
    );
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn what_is_not_final_stores_nothing(pool: PgPool) -> TestResult {
    let (tenant, merchant, stranger) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let theirs = captured(Tenant::new(&tenant, stranger), stranger, 1).await?;
    let k = key("k");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };

    // No such payment, or another merchant's: IDs are made by the server, so this
    // cannot become true later.
    let err = capture(me.sealed(&seal), &capture_command(2, theirs))
        .await
        .unwrap_err();
    assert!(matches!(err, CaptureError::NotFound { .. }), "{err:?}");
    let err = refund(me.sealed(&seal), &refund_command(2, payment_id(404), 1))
        .await
        .unwrap_err();
    assert!(matches!(err, RefundError::NotFound { .. }), "{err:?}");
    let err = authorize(me.sealed(&seal), &authorize_command(2, payment_id(404)))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthorizeError::NotFound { .. }), "{err:?}");

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
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_answer_that_is_not_final_takes_the_effect_down_with_it(pool: PgPool) -> TestResult {
    // A 5xx means an unknown outcome, so a column's CHECK refuses to keep one, and
    // since the record shares the effect's transaction, the effect goes too.
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = authorized(me, merchant, 1).await?;
    let unfinished = |_: Result<&Recorded, &Refused>| Answer {
        status: 503,
        body: "{}".to_owned(),
    };
    let k = key("k");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &unfinished,
    };
    let err = capture(me.sealed(&seal), &capture_command(1, payment))
        .await
        .unwrap_err();
    assert!(matches!(err, CaptureError::Invariant { .. }), "{err:?}");
    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(records(&pool).await?, 0);
    assert_eq!(
        balance_of(&pool, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        0
    );
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_second_attempt_under_a_key_rolls_back_whole(pool: PgPool) -> TestResult {
    // The second refund fits what is left, and has fresh IDs, so nothing but the key
    // stops it. An attempt that wrote its step and its posting must take them back.
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = captured(me, merchant, 1).await?;
    let k = key("refund");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };

    refund(me.sealed(&seal), &refund_command(1, payment, 1_000)).await?;
    let err = refund(me.sealed(&seal), &refund_command(2, payment, 1_000))
        .await
        .unwrap_err();
    assert!(matches!(err, RefundError::KeyTaken), "{err:?}");

    assert_eq!(steps(&pool, payment).await?, 4);
    assert_eq!(
        balance_of(&pool, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        AUTHORIZED - 1_000
    );
    assert_eq!(
        balance_of(&pool, AccountRef::MerchantPayable(merchant, eur())).await?,
        FEE - AUTHORIZED + 1_000
    );
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_one_request_racing_give_one_effect(pool: PgPool) -> TestResult {
    // I4's own test. Each copy has fresh IDs and each would apply on its own, because
    // a partial refund of 1 fits many times over, so only the key can stop them.
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = captured(me, merchant, 1).await?;
    let k = key("refund");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    let commands: Vec<Refund> = (1..=8).map(|n| refund_command(n, payment, 1)).collect();
    let sealed = me.sealed(&seal);
    let results = join_all(commands.iter().map(|command| refund(sealed, command))).await;

    let mut applied = 0;
    for result in results {
        match result {
            Ok(Outcome::Applied(_)) => applied += 1,
            Err(RefundError::KeyTaken) => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(applied, 1);
    assert_eq!(steps(&pool, payment).await?, 4);
    assert_eq!(records(&pool).await?, 1);
    assert_eq!(
        balance_of(&pool, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        AUTHORIZED - 1
    );
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    assert_eq!(
        recall(me, &k, print(1)).await?,
        answered(200, r#"{"seq":3,"refundable":7999}"#)
    );
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
    let render_created = move |_: Result<&Recorded, &Refused>| Answer {
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
    let me = Tenant::new(&tenant, merchant);
    let first = authorized(me, merchant, 1).await?;
    let second = authorized(me, merchant, 2).await?;
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
    let (first_command, second_command): (Capture, Capture) =
        (capture_command(1, first), capture_command(2, second));

    let (one, other) = join(
        capture(me.sealed(&first_seal), &first_command),
        capture(me.sealed(&second_seal), &second_command),
    )
    .await;
    let (won, lost, loser_print) = match (one, other) {
        (Ok(_), Err(CaptureError::KeyTaken)) => (first, second, print(2)),
        (Err(CaptureError::KeyTaken), Ok(_)) => (second, first, print(1)),
        other => panic!("{other:?}"),
    };
    assert_eq!(steps(&pool, won).await?, 3);
    assert_eq!(steps(&pool, lost).await?, 2);
    assert_eq!(recall(me, &k, loser_print).await?, Recall::Reused);
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
