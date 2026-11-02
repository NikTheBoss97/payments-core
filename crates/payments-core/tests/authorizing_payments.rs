// Author: Nicola Antonello

//! Authorizing a payment through the library (ADR-0016, ADR-0019, ADR-0022), as
//! the runtime role: the amount rule, the record, and what a retry compares. The
//! lock and the path that every command shares are tried in `payment_steps.rs`.

mod common;

use std::error::Error;

use common::payments::{
    AUTHORIZED, REQUESTED, authorize_command, books, committed_at, create_command, deadline,
    status_of, steps,
};
use common::{
    CHECK_VIOLATION, assert_guard, assert_violation, merchant_id, payment_id, runtime_pool,
    transition_id,
};
use futures_util::future::{join, join_all};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::{Event, IllegalTransition, PaymentStatus, Refused};
use payments_core::money::PositiveAmount;
use payments_core::payments::{
    Authorize, AuthorizeError, Outcome, Recorded, StepCommand, StepError, authorize, cancel,
    create_payment, expire, register_merchant, void,
};
use payments_core::times::BusinessTime;
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

/// What authorizing a payment that stands at its first step records.
const AUTHORIZED_REPLY: Recorded = Recorded {
    seq: 1,
    from: PaymentStatus::Created,
    to: PaymentStatus::Authorized,
    refundable: 0,
};

/// A runtime pool and a registered merchant.
async fn app_with_merchant(pool: &PgPool) -> Result<(PgPool, MerchantId), Box<dyn Error>> {
    let app = runtime_pool(pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&app, merchant).await?;
    Ok((app, merchant))
}

/// Creates payment `n` and returns its ID.
async fn created(app: &PgPool, merchant: MerchantId, n: u32) -> Result<PaymentId, Box<dyn Error>> {
    let command = create_command(n, merchant);
    create_payment(app, &command).await?;
    Ok(command.id)
}

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("valid amount")
}

#[sqlx::test]
async fn an_authorization_stores_its_step_and_its_record(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = created(&app, merchant, 1).await?;
    let command = authorize_command(1, payment);
    assert_eq!(
        authorize(&app, &command).await?,
        Outcome::Applied(AUTHORIZED_REPLY)
    );
    assert_eq!(status_of(&app, payment).await?, PaymentStatus::Authorized);
    let step = sqlx::query!(
        "SELECT payment_id, seq, prev_seq, kind, from_status, to_status, amount, \
                prev_refundable, refundable \
           FROM payment_transitions WHERE id = $1",
        command.step.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(step.payment_id, payment.get());
    assert_eq!((step.seq, step.prev_seq), (1, 0));
    assert_eq!(step.kind, "authorize");
    assert_eq!(
        (step.from_status.as_str(), step.to_status.as_str()),
        ("created", "authorized")
    );
    assert_eq!(step.amount, AUTHORIZED);
    assert_eq!((step.prev_refundable, step.refundable), (0, 0));
    let record = sqlx::query!(
        "SELECT payment_id, amount, payment_amount, expires_at \
           FROM payment_authorizations WHERE transition_id = $1",
        command.step.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(record.payment_id, payment.get());
    assert_eq!(record.amount, AUTHORIZED);
    assert_eq!(record.payment_amount, REQUESTED);
    assert_eq!(record.expires_at, deadline().get());
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_amount_is_at_most_the_request(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    // A partial authorization and the whole amount.
    for (n, minor) in [(1, 1), (2, AUTHORIZED), (3, REQUESTED)] {
        let payment = created(&app, merchant, n).await?;
        let command = Authorize {
            amount: amount(minor),
            ..authorize_command(n, payment)
        };
        assert_eq!(
            authorize(&app, &command).await?,
            Outcome::Applied(AUTHORIZED_REPLY),
            "{minor}"
        );
    }
    let payment = created(&app, merchant, 4).await?;
    let command = Authorize {
        amount: amount(REQUESTED + 1),
        ..authorize_command(4, payment)
    };
    let err = authorize(&app, &command).await.unwrap_err();
    assert!(
        matches!(
            err,
            AuthorizeError::Refused(Refused::AboveLimit { amount, limit })
                if amount == REQUESTED + 1 && limit == REQUESTED
        ),
        "{err:?}"
    );
    assert_eq!(status_of(&app, payment).await?, PaymentStatus::Created);
    assert_eq!(steps(&pool, payment).await?, 1);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_status_is_judged_before_the_amount(pool: PgPool) -> TestResult {
    // Every status but `created`, with an amount above the limit as well: the
    // answer is the illegal move (ADR-0016).
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let mut n = 0;
    for &status in PaymentStatus::ALL {
        if status == PaymentStatus::Created {
            continue;
        }
        n += 1;
        let newest = committed_at(&pool, &books, status).await?;
        let payment = PaymentId::try_from(newest.payment_id)?;
        let command = Authorize {
            amount: amount(REQUESTED + 1),
            ..authorize_command(n, payment)
        };
        let err = authorize(&app, &command).await.unwrap_err();
        assert!(
            matches!(
                err,
                AuthorizeError::Refused(Refused::Illegal(IllegalTransition {
                    status: got,
                    event: Event::Authorize,
                })) if got == status
            ),
            "{status}: {err:?}"
        );
        assert_eq!(steps(&pool, payment).await?, i64::from(newest.seq) + 1);
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unknown_payment_is_not_found(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let unknown = payment_id(1);
    let err = authorize(&app, &authorize_command(1, unknown))
        .await
        .unwrap_err();
    assert!(
        matches!(err, AuthorizeError::NotFound { payment } if payment == unknown),
        "{err:?}"
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_gets_the_stored_reply_although_the_status_has_moved_on(
    pool: PgPool,
) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = created(&app, merchant, 1).await?;
    let command = authorize_command(1, payment);
    authorize(&app, &command).await?;
    assert_eq!(
        authorize(&app, &command).await?,
        Outcome::AlreadyApplied(AUTHORIZED_REPLY)
    );
    let voiding = StepCommand {
        step: transition_id(2),
        payment,
    };
    void(&app, &voiding).await?;
    assert_eq!(
        authorize(&app, &command).await?,
        Outcome::AlreadyApplied(AUTHORIZED_REPLY)
    );
    assert_eq!(steps(&pool, payment).await?, 3);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_changed_retry_is_a_mismatch_for_each_field(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = created(&app, merchant, 1).await?;
    authorize(&app, &authorize_command(1, payment)).await?;
    let later = BusinessTime::new("2026-01-09T12:00:00Z".parse()?)?;
    let other = created(&app, merchant, 2).await?;
    for changed in [
        Authorize {
            amount: amount(AUTHORIZED - 1),
            ..authorize_command(1, payment)
        },
        Authorize {
            expires_at: later,
            ..authorize_command(1, payment)
        },
        Authorize {
            payment: other,
            ..authorize_command(1, payment)
        },
    ] {
        let err = authorize(&app, &changed).await.unwrap_err();
        let AuthorizeError::Mismatch { step } = err else {
            panic!("expected Mismatch for {changed:?}, got {err:?}");
        };
        assert_eq!(step, changed.step.get());
    }
    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(steps(&pool, other).await?, 1);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_step_id_of_another_kind_is_a_mismatch_both_ways(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    // An authorization's ID, sent as a cancel.
    let authorized = created(&app, merchant, 1).await?;
    let command = authorize_command(1, authorized);
    authorize(&app, &command).await?;
    let same_id = StepCommand {
        step: authorize_command(1, authorized).step,
        payment: authorized,
    };
    let err = cancel(&app, &same_id).await.unwrap_err();
    assert!(matches!(err, StepError::Mismatch { .. }), "{err:?}");
    // A cancel's ID, sent as an authorization.
    let cancelled = created(&app, merchant, 2).await?;
    let command = authorize_command(2, cancelled);
    let same_id = StepCommand {
        step: authorize_command(2, cancelled).step,
        payment: cancelled,
    };
    cancel(&app, &same_id).await?;
    let err = authorize(&app, &command).await.unwrap_err();
    assert!(matches!(err, AuthorizeError::Mismatch { .. }), "{err:?}");
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_time_finer_than_the_database_still_settles_a_retry(pool: PgPool) -> TestResult {
    // The reason for the business-time type (ADR-0022): had these gone in as
    // given, the stored time would differ from the command's, and a retry would
    // be reported as a bug.
    let (app, merchant) = app_with_merchant(&pool).await?;
    for (n, text) in [
        (1, "2026-01-08T12:00:00.123456789Z"),
        (2, "1999-12-31T23:59:59.123456789Z"),
        (3, "2016-12-31T23:59:60.5Z"),
    ] {
        let payment = created(&app, merchant, n).await?;
        let command = Authorize {
            expires_at: BusinessTime::new(text.parse()?)?,
            ..authorize_command(n, payment)
        };
        assert_eq!(
            authorize(&app, &command).await?,
            Outcome::Applied(AUTHORIZED_REPLY),
            "{text}"
        );
        assert_eq!(
            authorize(&app, &command).await?,
            Outcome::AlreadyApplied(AUTHORIZED_REPLY),
            "{text}"
        );
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_refused_command_leaves_its_step_id_free(pool: PgPool) -> TestResult {
    // Rejections are not stored (ADR-0019), so the ID is still unused, and the
    // same refusal is decided again each time.
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = created(&app, merchant, 1).await?;
    let too_much = Authorize {
        amount: amount(REQUESTED + 1),
        ..authorize_command(1, payment)
    };
    for _ in 0..2 {
        let err = authorize(&app, &too_much).await.unwrap_err();
        assert!(
            matches!(err, AuthorizeError::Refused(Refused::AboveLimit { .. })),
            "{err:?}"
        );
    }
    assert_eq!(
        authorize(&app, &authorize_command(1, payment)).await?,
        Outcome::Applied(AUTHORIZED_REPLY)
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn eight_concurrent_copies_apply_once(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = created(&app, merchant, 1).await?;
    let copies: Vec<Authorize> = (0..8).map(|_| authorize_command(1, payment)).collect();
    let results = join_all(copies.iter().map(|copy| authorize(&app, copy))).await;
    let mut applied = 0;
    for result in results {
        match result? {
            Outcome::Applied(recorded) => {
                applied += 1;
                assert_eq!(recorded, AUTHORIZED_REPLY);
            }
            Outcome::AlreadyApplied(recorded) => assert_eq!(recorded, AUTHORIZED_REPLY),
        }
    }
    assert_eq!(applied, 1);
    assert_eq!(steps(&pool, payment).await?, 2);
    let records = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM payment_authorizations WHERE payment_id = $1"#,
        payment.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(records, 1);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_authorization_racing_a_cancel_gets_one_winner(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    for n in 1..=8 {
        let payment = created(&app, merchant, n).await?;
        let authorizing = authorize_command(n, payment);
        let cancelling = StepCommand {
            step: transition_id(n + 2000),
            payment,
        };
        let results = join(authorize(&app, &authorizing), cancel(&app, &cancelling)).await;
        let winner = match results {
            (Ok(Outcome::Applied(_)), Err(StepError::Illegal(illegal))) => {
                assert_eq!(
                    illegal,
                    IllegalTransition {
                        status: PaymentStatus::Authorized,
                        event: Event::Cancel,
                    }
                );
                PaymentStatus::Authorized
            }
            (Err(AuthorizeError::Refused(Refused::Illegal(illegal))), Ok(Outcome::Applied(_))) => {
                assert_eq!(
                    illegal,
                    IllegalTransition {
                        status: PaymentStatus::Voided,
                        event: Event::Authorize,
                    }
                );
                PaymentStatus::Voided
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(status_of(&app, payment).await?, winner);
        assert_eq!(steps(&pool, payment).await?, 2);
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn void_and_expire_end_a_real_authorization(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let voided = Recorded {
        seq: 2,
        from: PaymentStatus::Authorized,
        to: PaymentStatus::Voided,
        refundable: 0,
    };
    let voiding = created(&app, merchant, 1).await?;
    authorize(&app, &authorize_command(1, voiding)).await?;
    let command = StepCommand {
        step: transition_id(2001),
        payment: voiding,
    };
    assert_eq!(void(&app, &command).await?, Outcome::Applied(voided));
    let expiring = created(&app, merchant, 2).await?;
    authorize(&app, &authorize_command(2, expiring)).await?;
    let command = StepCommand {
        step: transition_id(2002),
        payment: expiring,
    };
    assert_eq!(expire(&app, &command).await?, Outcome::Applied(voided));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_that_a_trigger_loses_fails_at_commit(pool: PgPool) -> TestResult {
    // Only the check at COMMIT can notice a step without its record, and LG004
    // proves a rollback (ADR-0014).
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = created(&app, merchant, 1).await?;
    sqlx::query!(
        "CREATE FUNCTION lose_row() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RETURN NULL; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER lose_record BEFORE INSERT ON payment_authorizations \
         FOR EACH ROW EXECUTE FUNCTION lose_row()"
    )
    .execute(&pool)
    .await?;
    let err = authorize(&app, &authorize_command(1, payment))
        .await
        .unwrap_err();
    let AuthorizeError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_guard(
        source,
        "LG004",
        "payment_transitions_commit_check",
        "payment_transitions",
    );
    assert_eq!(steps(&pool, payment).await?, 1);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_that_a_trigger_corrupts_is_a_bug(pool: PgPool) -> TestResult {
    // The types allow the write; only the CHECK can refuse what reaches it.
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = created(&app, merchant, 1).await?;
    sqlx::query!(
        "CREATE FUNCTION raise_amount() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.amount := NEW.payment_amount + 1; RETURN NEW; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER raise_amount BEFORE INSERT ON payment_authorizations \
         FOR EACH ROW EXECUTE FUNCTION raise_amount()"
    )
    .execute(&pool)
    .await?;
    let err = authorize(&app, &authorize_command(1, payment))
        .await
        .unwrap_err();
    let AuthorizeError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_violation(
        source,
        CHECK_VIOLATION,
        "payment_authorizations_amount_check",
    );
    assert_eq!(steps(&pool, payment).await?, 1);
    app.close().await;
    Ok(())
}
