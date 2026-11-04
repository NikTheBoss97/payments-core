// Author: Nicola Antonello

//! The backend killed around each statement of an operator's command (ADR-0026).
//!
//! A retry has fresh IDs, so nothing but the key's record can tell it that a first
//! attempt committed. So at every point the key must still be unused if the effect is
//! not there, and hold the answer if it is. Each test also sends a late duplicate,
//! with fresh IDs again, which the key alone must stop.

mod common;

use std::error::Error;

use common::crashes::{
    ATTEMPTS, OPERATOR_AT_COMMIT, OPERATOR_KEY_POINTS, Role, install_crash_points,
    kill_during_commit, plan_death, slow_commit_pool,
};
use common::keys::{key, print};
use common::payments::fee_rate;
use common::{merchant_id, runtime_pool_of_one};
use futures_util::future::join;
use payments_core::idempotency::{Answer, OperatorSeal, Recall, recall_operator};
use payments_core::operator::{OperatorError, register_merchant_sealed};
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

fn answer() -> Answer {
    Answer {
        status: 201,
        body: "{}".to_owned(),
    }
}

/// How many rows each table that an operator's command writes holds. A command that
/// died must leave these as they were.
async fn stored(pool: &PgPool) -> sqlx::Result<[i64; 2]> {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM merchants) AS "merchants!",
                  (SELECT count(*) FROM operator_idempotency_records) AS "answers!""#
    )
    .fetch_one(pool)
    .await?;
    Ok([row.merchants, row.answers])
}

#[sqlx::test]
async fn a_sealed_registration_killed_anywhere_leaves_no_answer_and_settles(
    pool: PgPool,
) -> TestResult {
    install_crash_points(&pool).await?;
    let app = runtime_pool_of_one(&pool).await?;
    let answer = answer();
    let points = ["die_before merchants", "die_after merchants"]
        .into_iter()
        .chain(OPERATOR_KEY_POINTS)
        .chain([OPERATOR_AT_COMMIT]);
    let mut registered = 0;
    for (n, point) in (1_u32..).zip(points) {
        let k = key(&format!("register-{n}"));
        let seal = OperatorSeal {
            key: &k,
            fingerprint: print(n),
            answer: &answer,
        };
        let before = stored(&pool).await?;
        plan_death(&app, point).await?;
        let err = register_merchant_sealed(&app, &seal, merchant_id(n * 10), fee_rate())
            .await
            .unwrap_err();
        assert!(
            matches!(err, OperatorError::NotConfirmed(_)),
            "{point}: {err:?}"
        );
        assert_eq!(stored(&pool).await?, before, "{point}");
        assert_eq!(
            recall_operator(&app, &k, print(n)).await?,
            Recall::Unused,
            "{point}"
        );

        register_merchant_sealed(&app, &seal, merchant_id(n * 10 + 1), fee_rate()).await?;
        assert_eq!(
            recall_operator(&app, &k, print(n)).await?,
            Recall::Answered(answer.clone()),
            "{point}"
        );
        registered += 1;

        // A late duplicate would register a second merchant under its own fresh ID.
        let err = register_merchant_sealed(&app, &seal, merchant_id(n * 10 + 2), fee_rate())
            .await
            .unwrap_err();
        assert!(matches!(err, OperatorError::KeyTaken), "{point}: {err:?}");
    }
    assert_eq!(stored(&pool).await?, [registered, registered]);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_commit_that_commits_and_loses_its_session_is_settled_by_the_key(
    pool: PgPool,
) -> TestResult {
    // The hardest point. The operator loses its session for a transaction that
    // committed, cannot tell, and its retry has fresh IDs. Only the record says what
    // happened. Every attempt, hit or miss, must end with exactly one merchant.
    let app = slow_commit_pool(&pool, Role::System).await?;
    let answer = answer();
    let mut hits = 0;
    for n in 1..=ATTEMPTS {
        let k = key(&format!("register-{n}"));
        let seal = OperatorSeal {
            key: &k,
            fingerprint: print(n),
            answer: &answer,
        };
        // The last kill may have ended the session, and a new one has a new ID.
        let pid = sqlx::query_scalar!(r#"SELECT pg_backend_pid() AS "pid!""#)
            .fetch_one(&app)
            .await?;
        let (attempt, killed) = join(
            register_merchant_sealed(&app, &seal, merchant_id(n * 10), fee_rate()),
            kill_during_commit(&pool, pid),
        )
        .await;
        killed?;
        match attempt {
            Ok(()) => {}
            Err(OperatorError::NotConfirmed(_)) => {
                match recall_operator(&app, &k, print(n)).await? {
                    Recall::Answered(_) => hits += 1,
                    Recall::Unused => {
                        register_merchant_sealed(&app, &seal, merchant_id(n * 10 + 1), fee_rate())
                            .await?;
                    }
                    Recall::Reused => panic!("attempt {n}: the same request reads as reused"),
                }
            }
            Err(other) => panic!("attempt {n}: {other:?}"),
        }
        assert_eq!(
            recall_operator(&app, &k, print(n)).await?,
            Recall::Answered(answer.clone()),
            "attempt {n}"
        );
    }
    assert!(
        hits > 0,
        "no kill landed inside a commit in {ATTEMPTS} attempts"
    );
    let attempts = i64::from(ATTEMPTS);
    assert_eq!(stored(&pool).await?, [attempts, attempts]);
    app.close().await;
    Ok(())
}
