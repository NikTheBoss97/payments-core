// Author: Nicola Antonello

//! Finding the merchant behind a presented secret (ADR-0024), on the system role's
//! pool, because no merchant is known yet (ADR-0025).

mod common;

use std::error::Error;

use common::keys::{key, print};
use common::payments::fee_rate;
use common::{api_key_id, merchant_id, runtime_pool, tenant_pool};
use payments_core::credentials::{ApiKeySecret, authenticate};
use payments_core::idempotency::{Answer, OperatorSeal};
use payments_core::ids::MerchantId;
use payments_core::operator::{issue_api_key, revoke_api_key};
use payments_core::payments::register_merchant;
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

/// Issues API key `n` to this merchant, and returns its secret.
async fn issued(
    system: &PgPool,
    merchant: MerchantId,
    n: u32,
) -> Result<ApiKeySecret, Box<dyn Error>> {
    let secret = ApiKeySecret::generate();
    let k = key(&format!("issue-{n}"));
    let answer = Answer {
        status: 201,
        body: "{}".to_owned(),
    };
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(n),
        answer: &answer,
    };
    issue_api_key(system, &seal, api_key_id(n), merchant, &secret.digest()).await?;
    Ok(secret)
}

#[sqlx::test]
async fn a_secret_finds_the_merchant_it_was_issued_to(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let (a, b) = (merchant_id(1), merchant_id(2));
    register_merchant(&system, a, fee_rate()).await?;
    register_merchant(&system, b, fee_rate()).await?;
    let mine = issued(&system, a, 1).await?;
    let theirs = issued(&system, b, 2).await?;
    let another_of_mine = issued(&system, a, 3).await?;

    assert_eq!(authenticate(&system, mine.expose()).await?, Some(a));
    assert_eq!(authenticate(&system, theirs.expose()).await?, Some(b));
    assert_eq!(
        authenticate(&system, another_of_mine.expose()).await?,
        Some(a)
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn anything_else_finds_nobody(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&system, merchant, fee_rate()).await?;
    let secret = issued(&system, merchant, 1).await?;
    let good = secret.expose();
    assert_eq!(authenticate(&system, good).await?, Some(merchant));

    let shorter = &good[..good.len() - 1];
    let longer = format!("{good}0");
    let upper = good.to_uppercase();
    let padded = format!(" {good}");
    let digest: String = secret
        .digest()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let never_issued = ApiKeySecret::generate();
    for presented in [
        "",
        "pck_",
        shorter,
        longer.as_str(),
        upper.as_str(),
        padded.as_str(),
        // What the table holds is not a credential.
        digest.as_str(),
        never_issued.expose(),
    ] {
        assert_eq!(
            authenticate(&system, presented).await?,
            None,
            "{presented:?}"
        );
    }
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_revoked_key_stops_on_its_next_request_and_no_other_key_does(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let (a, b) = (merchant_id(1), merchant_id(2));
    register_merchant(&system, a, fee_rate()).await?;
    register_merchant(&system, b, fee_rate()).await?;
    let revoked = issued(&system, a, 1).await?;
    let another_of_mine = issued(&system, a, 2).await?;
    let theirs = issued(&system, b, 3).await?;
    assert_eq!(authenticate(&system, revoked.expose()).await?, Some(a));

    let k = key("revoke");
    let answer = Answer {
        status: 200,
        body: "{}".to_owned(),
    };
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(100),
        answer: &answer,
    };
    revoke_api_key(&system, &seal, api_key_id(1)).await?;

    assert_eq!(authenticate(&system, revoked.expose()).await?, None);
    assert_eq!(
        authenticate(&system, another_of_mine.expose()).await?,
        Some(a)
    );
    assert_eq!(authenticate(&system, theirs.expose()).await?, Some(b));
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn only_the_digest_of_a_secret_is_stored(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&system, merchant, fee_rate()).await?;
    let secret = issued(&system, merchant, 1).await?;
    system.close().await;

    // PostgreSQL's own hash, not this code's.
    let digest_matches = sqlx::query_scalar!(
        r#"SELECT digest = sha256(convert_to($1, 'UTF8')) AS "matches!" FROM api_keys"#,
        secret.expose()
    )
    .fetch_one(&pool)
    .await?;
    assert!(digest_matches);

    // A search of the row's every column. The merchant's ID is the control: the same
    // search finds what is there.
    let holding = |text: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar!(
                r#"SELECT count(*) AS "count!" FROM api_keys k
                    WHERE strpos(row_to_json(k)::text, $1) > 0"#,
                text
            )
            .fetch_one(&pool)
            .await
        }
    };
    assert_eq!(holding(merchant.get().to_string()).await?, 1);
    assert_eq!(holding(secret.expose().to_owned()).await?, 0);
    Ok(())
}

#[sqlx::test]
async fn a_key_that_cannot_be_looked_up_is_an_error_and_not_unknown(pool: PgPool) -> TestResult {
    // A client must never conclude that a good key is bad (ADR-0024). So a pool that
    // has gone, and a role with no privilege on the table, are both errors.
    let system = runtime_pool(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&system, merchant, fee_rate()).await?;
    let secret = issued(&system, merchant, 1).await?;
    system.close().await;
    let looked_up = authenticate(&system, secret.expose()).await;
    assert!(looked_up.is_err(), "{looked_up:?}");

    let tenant = tenant_pool(&pool).await?;
    let looked_up = authenticate(&tenant, secret.expose()).await;
    assert!(looked_up.is_err(), "{looked_up:?}");
    tenant.close().await;
    Ok(())
}
