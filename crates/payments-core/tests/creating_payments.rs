// Author: Nicola Antonello

//! Registering merchants and creating payments through the library (ADR-0017,
//! ADR-0019), as the runtime role. A retry is the identical command sent again,
//! and gets the stored reply. Faults that the types rule out are injected with
//! triggers that the owner adds to the test's own database.

mod common;

use std::error::Error;

use common::payments::{REQUESTED, create_command, eur, fee_rate};
use common::{
    CHECK_VIOLATION, UNIQUE_VIOLATION, account_id, assert_guard, assert_violation, merchant_id,
    payment_id, runtime_pool, runtime_pool_of_one, transition_id,
};
use futures_util::future::join_all;
use payments_core::accounts::{AccountError, AccountRef, NormalSide, Psp, find_account};
use payments_core::ids::MerchantId;
use payments_core::lifecycle::PaymentStatus;
use payments_core::money::{BasisPoints, Currency, PositiveAmount};
use payments_core::payments::{
    AccountCandidates, CreateError, CreatePayment, Outcome, Payment, Recorded, RegisterError,
    create_payment, find_payment, register_merchant,
};
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

/// The reply to every create: the payment's first step.
const FIRST_STEP: Recorded = Recorded {
    seq: 0,
    from: PaymentStatus::Created,
    to: PaymentStatus::Created,
    refundable: 0,
};

/// How many payments, steps and accounts are stored.
async fn stored(pool: &PgPool) -> sqlx::Result<(i64, i64, i64)> {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM payments) AS "payments!",
                  (SELECT count(*) FROM payment_transitions) AS "steps!",
                  (SELECT count(*) FROM accounts) AS "accounts!""#
    )
    .fetch_one(pool)
    .await?;
    Ok((row.payments, row.steps, row.accounts))
}

/// A runtime pool and a registered merchant.
async fn app_with_merchant(pool: &PgPool) -> Result<(PgPool, MerchantId), Box<dyn Error>> {
    let app = runtime_pool(pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&app, merchant, fee_rate()).await?;
    Ok((app, merchant))
}

/// Applies `command` and asserts that the database rejected it as a bug,
/// returning the database's error.
async fn invariant(app: &PgPool, command: &CreatePayment) -> sqlx::Error {
    let err = create_payment(app, command).await.unwrap_err();
    let CreateError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    source
}

#[sqlx::test]
async fn a_merchant_registers_once_however_often_it_is_called(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let id = merchant_id(1);
    register_merchant(&app, id, fee_rate()).await?;
    register_merchant(&app, id, fee_rate()).await?;
    let merchants = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM merchants WHERE id = $1"#,
        id.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(merchants, 1);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_merchant_keeps_the_rate_it_registered_at(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let id = merchant_id(1);
    let rate = BasisPoints::new(300)?;
    register_merchant(&app, id, rate).await?;
    register_merchant(&app, id, rate).await?;
    let stored = sqlx::query_scalar!("SELECT fee_bps FROM merchants WHERE id = $1", id.get())
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 300);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn registering_at_another_rate_is_a_mismatch_and_changes_nothing(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let id = merchant_id(1);
    register_merchant(&app, id, fee_rate()).await?;
    let err = register_merchant(&app, id, BasisPoints::new(300)?)
        .await
        .unwrap_err();
    assert!(
        matches!(err, RegisterError::Mismatch { merchant } if merchant == id),
        "{err:?}"
    );
    let stored = sqlx::query_scalar!("SELECT fee_bps FROM merchants WHERE id = $1", id.get())
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 250);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_new_payment_has_its_terms_its_first_step_and_its_accounts(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let command = create_command(1, merchant);
    assert_eq!(
        create_payment(&app, &command).await?,
        Outcome::Applied(FIRST_STEP)
    );
    assert_eq!(
        find_payment(&app, command.id).await?,
        Some(Payment {
            merchant,
            psp: Psp::Mock,
            currency: eur(),
            amount: PositiveAmount::new(REQUESTED)?,
            fee_rate: BasisPoints::new(250)?,
            status: PaymentStatus::Created,
            refundable: 0,
        })
    );
    let step = sqlx::query!(
        r#"SELECT id, seq, prev_seq, kind, from_status, to_status, amount,
                  prev_refundable, refundable
             FROM payment_transitions WHERE payment_id = $1"#,
        command.id.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(step.id, command.step.get());
    assert_eq!((step.seq, step.prev_seq), (0, 0));
    assert_eq!(step.kind, "create");
    assert_eq!(
        (step.from_status.as_str(), step.to_status.as_str()),
        ("created", "created")
    );
    assert_eq!(
        (step.amount, step.prev_refundable, step.refundable),
        (0, 0, 0)
    );
    for (reference, candidate, side) in [
        (
            AccountRef::FeeRevenue(eur()),
            command.accounts.fee_revenue,
            NormalSide::Credit,
        ),
        (
            AccountRef::MerchantPayable(merchant, eur()),
            command.accounts.merchant_payable,
            NormalSide::Credit,
        ),
        (
            AccountRef::PspReceivable(Psp::Mock, eur()),
            command.accounts.psp_receivable,
            NormalSide::Debit,
        ),
    ] {
        let account = find_account(&app, &reference.key())
            .await?
            .expect("the account was opened");
        assert_eq!(account.id, candidate, "{reference:?}");
        assert_eq!(account.normal_side, side, "{reference:?}");
    }
    assert_eq!(stored(&pool).await?, (1, 1, 3));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn payments_share_the_accounts_of_their_keys(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    create_payment(&app, &create_command(1, merchant)).await?;
    // The same merchant, so no new account, and then another merchant, who
    // adds only a payable account of its own.
    create_payment(&app, &create_command(2, merchant)).await?;
    assert_eq!(stored(&pool).await?, (2, 2, 3));
    let other = merchant_id(2);
    register_merchant(&app, other, fee_rate()).await?;
    create_payment(&app, &create_command(3, other)).await?;
    assert_eq!(stored(&pool).await?, (3, 3, 4));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unknown_merchant_is_refused_before_any_account_opens(pool: PgPool) -> TestResult {
    // A typo in a merchant's ID must not leave a payable account behind
    // (ADR-0019).
    let app = runtime_pool(&pool).await?;
    let unknown = merchant_id(1);
    let err = create_payment(&app, &create_command(1, unknown))
        .await
        .unwrap_err();
    let CreateError::UnknownMerchant { merchant } = err else {
        panic!("expected UnknownMerchant, got {err:?}");
    };
    assert_eq!(merchant, unknown);
    assert_eq!(stored(&pool).await?, (0, 0, 0));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unknown_currency_is_refused(pool: PgPool) -> TestResult {
    // XYZ is a well-formed code, but not in the currencies table. The first
    // account's key finds that out, so nothing is stored.
    let (app, merchant) = app_with_merchant(&pool).await?;
    let command = CreatePayment {
        currency: Currency::parse("XYZ")?,
        ..create_command(1, merchant)
    };
    let err = create_payment(&app, &command).await.unwrap_err();
    let CreateError::UnknownCurrency { currency, .. } = err else {
        panic!("expected UnknownCurrency, got {err:?}");
    };
    assert_eq!(currency.as_str(), "XYZ");
    assert_eq!(stored(&pool).await?, (0, 0, 0));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn no_payment_is_found_under_an_unknown_id(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    assert_eq!(find_payment(&app, payment_id(1)).await?, None);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_gets_the_stored_reply_and_stores_nothing_new(pool: PgPool) -> TestResult {
    // The pool has one connection, so the lookup after the duplicate key cannot
    // keep the failed transaction's.
    let app = runtime_pool_of_one(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&app, merchant, fee_rate()).await?;
    let command = create_command(1, merchant);
    assert_eq!(
        create_payment(&app, &command).await?,
        Outcome::Applied(FIRST_STEP)
    );
    for _ in 0..2 {
        assert_eq!(
            create_payment(&app, &command).await?,
            Outcome::AlreadyApplied(FIRST_STEP)
        );
    }
    assert_eq!(stored(&pool).await?, (1, 1, 3));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_changed_retry_is_a_mismatch_for_each_term(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let other_merchant = merchant_id(2);
    register_merchant(&app, other_merchant, fee_rate()).await?;
    create_payment(&app, &create_command(1, merchant)).await?;
    let before = find_payment(&app, payment_id(1)).await?;
    // Another merchant or currency means accounts with new keys, so those
    // commands need candidates of their own. `psp` has one value today, so it
    // cannot differ.
    let fresh = |n| AccountCandidates {
        fee_revenue: account_id(n),
        merchant_payable: account_id(n + 1),
        psp_receivable: account_id(n + 2),
    };
    for changed in [
        CreatePayment {
            merchant: other_merchant,
            accounts: fresh(100),
            ..create_command(1, merchant)
        },
        CreatePayment {
            currency: Currency::parse("USD")?,
            accounts: fresh(200),
            ..create_command(1, merchant)
        },
        CreatePayment {
            amount: PositiveAmount::new(REQUESTED + 1)?,
            ..create_command(1, merchant)
        },
        CreatePayment {
            fee_rate: BasisPoints::new(251)?,
            ..create_command(1, merchant)
        },
    ] {
        let err = create_payment(&app, &changed).await.unwrap_err();
        let CreateError::Mismatch { step } = err else {
            panic!("expected Mismatch for {changed:?}, got {err:?}");
        };
        assert_eq!(step, transition_id(1).get());
    }
    assert_eq!(find_payment(&app, payment_id(1)).await?, before);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_step_id_of_another_payment_is_a_mismatch(pool: PgPool) -> TestResult {
    // A new payment under a step ID that is taken: its insert succeeds, the
    // step's fails, and the whole transaction rolls back.
    let (app, merchant) = app_with_merchant(&pool).await?;
    create_payment(&app, &create_command(1, merchant)).await?;
    let reused = CreatePayment {
        step: transition_id(1),
        ..create_command(2, merchant)
    };
    let err = create_payment(&app, &reused).await.unwrap_err();
    assert!(matches!(err, CreateError::Mismatch { .. }), "{err:?}");
    assert_eq!(find_payment(&app, payment_id(2)).await?, None);
    assert_eq!(stored(&pool).await?, (1, 1, 3));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_payment_id_under_a_new_step_id_is_a_bug(pool: PgPool) -> TestResult {
    // The duplicate key has no stored step to explain it. An ID had to be
    // reused by hand: neither the command nor its step ID can be cloned.
    let (app, merchant) = app_with_merchant(&pool).await?;
    create_payment(&app, &create_command(1, merchant)).await?;
    let reused = CreatePayment {
        step: transition_id(2),
        ..create_command(1, merchant)
    };
    let source = invariant(&app, &reused).await;
    assert_violation(source, UNIQUE_VIOLATION, "payments_pkey");
    assert_eq!(stored(&pool).await?, (1, 1, 3));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_candidate_that_another_account_has_is_a_bug(pool: PgPool) -> TestResult {
    // The second merchant's payable account is a new key, and its candidate
    // is the first merchant's account. Accounts open before the insert, so
    // this is reported as such, and nothing of the payment is stored.
    let (app, merchant) = app_with_merchant(&pool).await?;
    create_payment(&app, &create_command(1, merchant)).await?;
    let other = merchant_id(2);
    register_merchant(&app, other, fee_rate()).await?;
    let reused = CreatePayment {
        accounts: create_command(1, merchant).accounts,
        ..create_command(2, other)
    };
    let err = create_payment(&app, &reused).await.unwrap_err();
    let CreateError::Account {
        source: AccountError::Invariant { source, .. },
        ..
    } = err
    else {
        panic!("expected Account, got {err:?}");
    };
    assert_violation(source, UNIQUE_VIOLATION, "accounts_pkey");
    assert_eq!(stored(&pool).await?, (1, 1, 3));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn eight_concurrent_copies_create_one_payment(pool: PgPool) -> TestResult {
    // Eight retries racing each other: one inserts, and the others wait on the
    // primary key, then find its step and compare (ADR-0019).
    let (app, merchant) = app_with_merchant(&pool).await?;
    let copies: Vec<CreatePayment> = (0..8).map(|_| create_command(1, merchant)).collect();
    let results = join_all(copies.iter().map(|copy| create_payment(&app, copy))).await;
    let mut applied = 0;
    for result in results {
        match result? {
            Outcome::Applied(recorded) => {
                applied += 1;
                assert_eq!(recorded, FIRST_STEP);
            }
            Outcome::AlreadyApplied(recorded) => assert_eq!(recorded, FIRST_STEP),
        }
    }
    assert_eq!(applied, 1);
    assert_eq!(stored(&pool).await?, (1, 1, 3));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_write_that_a_trigger_corrupts_is_a_bug(pool: PgPool) -> TestResult {
    // The types allow the write; only the CHECK can refuse what reaches it.
    sqlx::query!(
        "CREATE FUNCTION corrupt_fee_rate() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.fee_bps := 10001; RETURN NEW; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER corrupt_fee_rate BEFORE INSERT ON payments \
         FOR EACH ROW EXECUTE FUNCTION corrupt_fee_rate()"
    )
    .execute(&pool)
    .await?;
    let (app, merchant) = app_with_merchant(&pool).await?;
    let source = invariant(&app, &create_command(1, merchant)).await;
    assert_violation(source, CHECK_VIOLATION, "payments_fee_bps_check");
    assert_eq!(stored(&pool).await?, (0, 0, 3));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_deferred_checks_rejection_is_a_bug_and_rolls_back(pool: PgPool) -> TestResult {
    // A trigger drops the first step, so only the check at COMMIT can notice:
    // LG005 proves a rollback (ADR-0014).
    sqlx::query!(
        "CREATE FUNCTION lose_row() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RETURN NULL; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER lose_first_step BEFORE INSERT ON payment_transitions \
         FOR EACH ROW EXECUTE FUNCTION lose_row()"
    )
    .execute(&pool)
    .await?;
    let (app, merchant) = app_with_merchant(&pool).await?;
    let source = invariant(&app, &create_command(1, merchant)).await;
    assert_guard(source, "LG005", "payments_commit_check", "payments");
    assert_eq!(stored(&pool).await?, (0, 0, 3));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unproven_commit_failure_is_settled_by_creating_again(pool: PgPool) -> TestResult {
    // A deferred trigger ends the session during COMMIT, which reports FATAL
    // 57P01: no proof of a rollback (ADR-0014). The function runs as its owner,
    // the superuser, which may end a session that the superuser opened.
    sqlx::query!(
        "CREATE FUNCTION end_own_session() RETURNS trigger LANGUAGE plpgsql \
         SECURITY DEFINER AS $$ \
         BEGIN PERFORM pg_terminate_backend(pg_backend_pid()); RETURN NULL; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE CONSTRAINT TRIGGER zz_end_session AFTER INSERT ON payments \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION end_own_session()"
    )
    .execute(&pool)
    .await?;
    let (app, merchant) = app_with_merchant(&pool).await?;
    let command = create_command(1, merchant);
    let err = create_payment(&app, &command).await.unwrap_err();
    assert!(matches!(err, CreateError::NotConfirmed { .. }), "{err:?}");
    assert_eq!(stored(&pool).await?, (0, 0, 3));
    // The caller's one rule: send the same command again.
    sqlx::query!("DROP TRIGGER zz_end_session ON payments")
        .execute(&pool)
        .await?;
    assert_eq!(
        create_payment(&app, &command).await?,
        Outcome::Applied(FIRST_STEP)
    );
    app.close().await;
    Ok(())
}
