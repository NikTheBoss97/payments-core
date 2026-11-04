// Author: Nicola Antonello

//! The operator's commands under its own idempotency keys (ADR-0024, ADR-0026), as
//! the system role runs them.
//!
//! As for a merchant's requests, the answer commits with the effect, and each attempt
//! has fresh IDs, so only the key can tell two attempts from two requests.

mod common;

use std::error::Error;

use common::keys::{key, print};
use common::payments::fee_rate;
use common::{api_key_id, merchant_id, runtime_pool, tenant_pool};
use futures_util::future::join_all;
use payments_core::credentials::SecretDigest;
use payments_core::idempotency::{Answer, IdempotencyKey, OperatorSeal, Recall, recall_operator};
use payments_core::ids::{ApiKeyId, MerchantId};
use payments_core::operator::{
    OperatorError, issue_api_key, register_merchant_sealed, revoke_api_key,
};
use payments_core::payments::register_merchant;
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

fn created() -> Answer {
    Answer {
        status: 201,
        body: r#"{"merchant_id":"the first"}"#.to_owned(),
    }
}

/// How many merchants are stored, whoever registered them.
async fn merchants(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM merchants"#)
        .fetch_one(pool)
        .await
}

#[sqlx::test]
async fn a_sealed_registration_stores_its_answer_with_the_merchant(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let k = key("register");
    let answer = created();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };
    assert_eq!(
        recall_operator(&system, &k, print(1)).await?,
        Recall::Unused
    );

    let merchant = merchant_id(1);
    register_merchant_sealed(&system, &seal, merchant, fee_rate()).await?;

    let stored = sqlx::query_scalar!(
        "SELECT fee_bps FROM merchants WHERE id = $1",
        merchant.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, i32::from(fee_rate().get()));
    assert_eq!(
        recall_operator(&system, &k, print(1)).await?,
        Recall::Answered(created())
    );
    assert_eq!(
        recall_operator(&system, &k, print(2)).await?,
        Recall::Reused
    );
    assert_eq!(
        recall_operator(&system, &key("another"), print(1)).await?,
        Recall::Unused
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_a_registration_racing_register_one_merchant(pool: PgPool) -> TestResult {
    // Eight merchant IDs, so nothing but the key's unique index is between them.
    let system = runtime_pool(&pool).await?;
    let k = key("register");
    let answer = created();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };
    let attempts = join_all(
        (1..=8).map(|n| register_merchant_sealed(&system, &seal, merchant_id(n), fee_rate())),
    )
    .await;

    let applied = attempts.iter().filter(|attempt| attempt.is_ok()).count();
    assert_eq!(applied, 1, "{attempts:?}");
    for attempt in &attempts {
        assert!(
            matches!(attempt, Ok(()) | Err(OperatorError::KeyTaken)),
            "{attempt:?}"
        );
    }
    assert_eq!(merchants(&pool).await?, 1);
    assert_eq!(
        recall_operator(&system, &k, print(1)).await?,
        Recall::Answered(created())
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_answer_that_cannot_be_stored_takes_the_effect_down_with_it(pool: PgPool) -> TestResult {
    // A 5xx is not final, so its CHECK refuses it (ADR-0026), and body must be JSON.
    let system = runtime_pool(&pool).await?;
    let k = key("register");
    for answer in [
        Answer {
            status: 500,
            body: "{}".to_owned(),
        },
        Answer {
            status: 200,
            body: "not JSON".to_owned(),
        },
    ] {
        let seal = OperatorSeal {
            key: &k,
            fingerprint: print(1),
            answer: &answer,
        };
        let err = register_merchant_sealed(&system, &seal, merchant_id(1), fee_rate())
            .await
            .unwrap_err();
        assert!(matches!(err, OperatorError::Invariant(_)), "{err:?}");
        assert_eq!(merchants(&pool).await?, 0);
        assert_eq!(
            recall_operator(&system, &k, print(1)).await?,
            Recall::Unused
        );
    }
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_commit_that_a_deferred_check_rejects_is_a_bug(pool: PgPool) -> TestResult {
    // None of these tables has a deferred check yet, so this adds one, as the owner.
    // Such a check rejects the transaction before the commit record, so it proves a
    // rollback (ADR-0014), and retrying cannot help.
    sqlx::query!(
        "CREATE FUNCTION reject_at_commit() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE SQLSTATE 'LG002' USING MESSAGE = 'rejected at commit'; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE CONSTRAINT TRIGGER reject_at_commit AFTER INSERT ON merchants \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION reject_at_commit()"
    )
    .execute(&pool)
    .await?;
    let system = runtime_pool(&pool).await?;
    let k = key("register");
    let answer = created();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };
    let err = register_merchant_sealed(&system, &seal, merchant_id(1), fee_rate())
        .await
        .unwrap_err();
    assert!(matches!(err, OperatorError::Invariant(_)), "{err:?}");
    assert_eq!(merchants(&pool).await?, 0);
    assert_eq!(
        recall_operator(&system, &k, print(1)).await?,
        Recall::Unused
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_tenant_role_cannot_run_an_operators_command(pool: PgPool) -> TestResult {
    // Refused for good, so it reads as a bug, and never as something to retry (ADR-0025).
    let tenant = tenant_pool(&pool).await?;
    let k = key("register");
    let answer = created();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };
    let err = register_merchant_sealed(&tenant, &seal, merchant_id(1), fee_rate())
        .await
        .unwrap_err();
    assert!(matches!(err, OperatorError::Invariant(_)), "{err:?}");
    assert_eq!(merchants(&pool).await?, 0);
    tenant.close().await;
    Ok(())
}

fn issued() -> Answer {
    Answer {
        status: 201,
        body: r#"{"api_key_id":"the first"}"#.to_owned(),
    }
}

/// How many API keys are stored, whoever they belong to.
async fn api_keys(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM api_keys"#)
        .fetch_one(pool)
        .await
}

/// A merchant that the system role registered.
async fn a_merchant(system: &PgPool) -> Result<MerchantId, Box<dyn Error>> {
    let merchant = merchant_id(1);
    register_merchant(system, merchant, fee_rate()).await?;
    Ok(merchant)
}

#[sqlx::test]
async fn an_issued_key_stores_its_answer_with_its_digest(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let merchant = a_merchant(&system).await?;
    let k = key("issue");
    let answer = issued();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };
    let digest = SecretDigest::of("a secret");

    issue_api_key(&system, &seal, api_key_id(1), merchant, &digest).await?;

    let row = sqlx::query!(
        "SELECT merchant_id, digest FROM api_keys WHERE id = $1",
        api_key_id(1).get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(row.merchant_id, merchant.get());
    assert_eq!(row.digest, digest.as_bytes());
    assert_eq!(
        recall_operator(&system, &k, print(1)).await?,
        Recall::Answered(issued())
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_key_for_no_merchant_is_refused_and_stores_nothing(pool: PgPool) -> TestResult {
    // IDs are made by the server, so this cannot become true later, and it is not one
    // of the answers that a key stores (ADR-0023).
    let system = runtime_pool(&pool).await?;
    let k = key("issue");
    let answer = issued();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };
    let err = issue_api_key(
        &system,
        &seal,
        api_key_id(1),
        merchant_id(1),
        &SecretDigest::of("a secret"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, OperatorError::NotFound), "{err:?}");
    assert_eq!(api_keys(&pool).await?, 0);
    assert_eq!(
        recall_operator(&system, &k, print(1)).await?,
        Recall::Unused
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_an_issue_racing_issue_one_key(pool: PgPool) -> TestResult {
    // Eight IDs and eight secrets, as eight attempts would have.
    let system = runtime_pool(&pool).await?;
    let merchant = a_merchant(&system).await?;
    let k = key("issue");
    let answer = issued();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };
    let digests: Vec<SecretDigest> = (1..=8)
        .map(|n| SecretDigest::of(&format!("secret {n}")))
        .collect();
    let attempts = join_all(
        (1..)
            .zip(&digests)
            .map(|(n, digest)| issue_api_key(&system, &seal, api_key_id(n), merchant, digest)),
    )
    .await;

    let applied = attempts.iter().filter(|attempt| attempt.is_ok()).count();
    assert_eq!(applied, 1, "{attempts:?}");
    for attempt in &attempts {
        assert!(
            matches!(attempt, Ok(()) | Err(OperatorError::KeyTaken)),
            "{attempt:?}"
        );
    }
    assert_eq!(api_keys(&pool).await?, 1);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn one_digest_cannot_belong_to_two_keys(pool: PgPool) -> TestResult {
    // 256 random bits never collide, so a second one is a bug, and retrying cannot help.
    let system = runtime_pool(&pool).await?;
    let merchant = a_merchant(&system).await?;
    let answer = issued();
    let digest = SecretDigest::of("a secret");
    let first = key("first");
    let seal = OperatorSeal {
        key: &first,
        fingerprint: print(1),
        answer: &answer,
    };
    issue_api_key(&system, &seal, api_key_id(1), merchant, &digest).await?;

    let second = key("second");
    let seal = OperatorSeal {
        key: &second,
        fingerprint: print(2),
        answer: &answer,
    };
    let err = issue_api_key(&system, &seal, api_key_id(2), merchant, &digest)
        .await
        .unwrap_err();
    assert!(matches!(err, OperatorError::Invariant(_)), "{err:?}");
    assert_eq!(
        recall_operator(&system, &second, print(2)).await?,
        Recall::Unused
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_tenant_role_cannot_issue_a_key(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let merchant = a_merchant(&system).await?;
    system.close().await;
    let tenant = tenant_pool(&pool).await?;
    let k = key("issue");
    let answer = issued();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };
    let err = issue_api_key(
        &tenant,
        &seal,
        api_key_id(1),
        merchant,
        &SecretDigest::of("a secret"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, OperatorError::Invariant(_)), "{err:?}");
    assert_eq!(api_keys(&pool).await?, 0);
    tenant.close().await;
    Ok(())
}

fn revoked() -> Answer {
    Answer {
        status: 200,
        body: r#"{"revoked":true}"#.to_owned(),
    }
}

/// How many revocations are stored, whichever keys they name.
async fn revocations(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM api_key_revocations"#)
        .fetch_one(pool)
        .await
}

/// API key 1, of a registered merchant.
async fn an_api_key(system: &PgPool) -> Result<ApiKeyId, Box<dyn Error>> {
    let merchant = a_merchant(system).await?;
    let k = key("issue");
    let answer = issued();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(100),
        answer: &answer,
    };
    let id = api_key_id(1);
    issue_api_key(system, &seal, id, merchant, &SecretDigest::of("a secret")).await?;
    Ok(id)
}

#[sqlx::test]
async fn a_revocation_stores_its_answer_with_its_row(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let id = an_api_key(&system).await?;
    let k = key("revoke");
    let answer = revoked();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };

    revoke_api_key(&system, &seal, id).await?;

    let stored = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM api_key_revocations WHERE api_key_id = $1"#,
        id.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, 1);
    assert_eq!(
        recall_operator(&system, &k, print(1)).await?,
        Recall::Answered(revoked())
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn revoking_a_revoked_key_answers_too_and_changes_nothing(pool: PgPool) -> TestResult {
    // What the request asks for already holds, and each key keeps its own answer.
    let system = runtime_pool(&pool).await?;
    let id = an_api_key(&system).await?;
    let answer = revoked();
    for (n, name) in [(1, "first"), (2, "second")] {
        let k = key(name);
        let seal = OperatorSeal {
            key: &k,
            fingerprint: print(n),
            answer: &answer,
        };
        revoke_api_key(&system, &seal, id).await?;
        assert_eq!(
            recall_operator(&system, &k, print(n)).await?,
            Recall::Answered(revoked()),
            "{name}"
        );
    }
    assert_eq!(revocations(&pool).await?, 1);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_revocation_of_no_key_is_refused_and_stores_nothing(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let k = key("revoke");
    let answer = revoked();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };
    let err = revoke_api_key(&system, &seal, api_key_id(1))
        .await
        .unwrap_err();
    assert!(matches!(err, OperatorError::NotFound), "{err:?}");
    assert_eq!(revocations(&pool).await?, 0);
    assert_eq!(
        recall_operator(&system, &k, print(1)).await?,
        Recall::Unused
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_a_revocation_racing_give_one_answer(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let id = an_api_key(&system).await?;
    let k = key("revoke");
    let answer = revoked();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };
    let attempts = join_all((0..8).map(|_| revoke_api_key(&system, &seal, id))).await;

    let applied = attempts.iter().filter(|attempt| attempt.is_ok()).count();
    assert_eq!(applied, 1, "{attempts:?}");
    for attempt in &attempts {
        assert!(
            matches!(attempt, Ok(()) | Err(OperatorError::KeyTaken)),
            "{attempt:?}"
        );
    }
    assert_eq!(revocations(&pool).await?, 1);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn revocations_of_one_key_under_different_keys_racing_all_answer(pool: PgPool) -> TestResult {
    // Nothing but the revocation's own primary key is between them, and each request
    // still gets its answer.
    let system = runtime_pool(&pool).await?;
    let id = an_api_key(&system).await?;
    let answer = revoked();
    let keys: Vec<IdempotencyKey> = (0..8).map(|n| key(&format!("revoke-{n}"))).collect();
    let seals: Vec<OperatorSeal<'_>> = (0..)
        .zip(&keys)
        .map(|(n, k)| OperatorSeal {
            key: k,
            fingerprint: print(n),
            answer: &answer,
        })
        .collect();
    let attempts = join_all(seals.iter().map(|seal| revoke_api_key(&system, seal, id))).await;

    for attempt in &attempts {
        assert!(attempt.is_ok(), "{attempt:?}");
    }
    assert_eq!(revocations(&pool).await?, 1);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_tenant_role_cannot_revoke_a_key(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let id = an_api_key(&system).await?;
    system.close().await;
    let tenant = tenant_pool(&pool).await?;
    let k = key("revoke");
    let answer = revoked();
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(1),
        answer: &answer,
    };
    let err = revoke_api_key(&tenant, &seal, id).await.unwrap_err();
    assert!(matches!(err, OperatorError::Invariant(_)), "{err:?}");
    assert_eq!(revocations(&pool).await?, 0);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_key_that_cannot_be_looked_up_is_an_error_and_not_unused(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    system.close().await;
    let recalled = recall_operator(&system, &key("register"), print(1)).await;
    assert!(recalled.is_err(), "{recalled:?}");
    Ok(())
}
