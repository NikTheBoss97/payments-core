// Author: Nicola Antonello

//! What the tests of the HTTP API share (ADR-0023).

use std::error::Error;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use mock_psp::signing::SigningKey;
use payments_core::accounts::Psp;
use payments_core::api::{AppState, Webhooks};
use payments_core::credentials::{ApiKeySecret, SecretDigest};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::notifications::SigningKeys;
use payments_core::payments::{create_payment, register_merchant};
use payments_core::psp::PspClient;
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use tower::ServiceExt;

use super::keys::issued_key;
use super::payments::{create_command, fee_rate};
use super::{merchant_id, runtime_pool, tenant_pool};

/// A response, with its body read.
#[derive(Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Reply {
    /// The one value of this header, or a failure if it has none or several.
    #[track_caller]
    pub fn header(&self, name: &str) -> &str {
        let mut values = self.headers.get_all(name).iter();
        let value = values
            .next()
            .unwrap_or_else(|| panic!("no {name} header: {self:?}"));
        assert!(values.next().is_none(), "two {name} headers: {self:?}");
        value.to_str().expect("a readable header")
    }
}

/// Sends a request to the router without a socket.
pub async fn send(router: Router, request: Request<Body>) -> Reply {
    let response = router.oneshot(request).await.expect("a router never fails");
    let (parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .expect("a body that can be read")
        .to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8(bytes.to_vec()).expect("a body in UTF-8"),
    }
}

/// A request with no body and no headers.
pub fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a valid request")
}

/// The `Authorization` value that presents this secret.
pub fn bearer(secret: &ApiKeySecret) -> String {
    format!("Bearer {}", secret.expose())
}

/// A request that presents this `Authorization` value.
pub fn get_with_authorization(path: &str, value: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header("authorization", value)
        .body(Body::empty())
        .expect("a valid request")
}

/// A `POST` with this credential, idempotency key and body.
pub fn post(path: &str, secret: &ApiKeySecret, key: &str, body: impl Into<Body>) -> Request<Body> {
    post_with_authorization(path, &bearer(secret), key, body)
}

/// A `POST` that presents this `Authorization` value, with this idempotency key and body.
pub fn post_with_authorization(
    path: &str,
    value: &str,
    key: &str,
    body: impl Into<Body>,
) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("authorization", value)
        .header("idempotency-key", format!("\"{key}\""))
        .body(body.into())
        .expect("a valid request")
}

/// A digest as the environment holds it: 64 lowercase hex digits.
pub fn written(digest: SecretDigest) -> String {
    digest
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A registered merchant with an API key. `n` tells them apart.
pub async fn a_merchant(
    pools: &Pools,
    n: u32,
) -> Result<(MerchantId, ApiKeySecret), Box<dyn Error>> {
    let merchant = merchant_id(n);
    register_merchant(&pools.system, merchant, fee_rate()).await?;
    Ok((merchant, issued_key(&pools.system, merchant, n).await?))
}

/// Payment `n` of this merchant, just created.
pub async fn a_payment(
    pools: &Pools,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let command = create_command(n, merchant);
    create_payment(Tenant::new(&pools.tenant, merchant), &command).await?;
    Ok(command.id)
}

/// How many final answers are stored under merchants' keys. Pass the test's own pool,
/// which row-level security does not limit.
pub async fn records(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM idempotency_records"#)
        .fetch_one(pool)
        .await
}

/// How many payments are stored, whoever owns them. Pass the test's own pool.
pub async fn payments_stored(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payments"#)
        .fetch_one(pool)
        .await
}

/// A client of a PSP that the test never means to reach: port 9 of the local interface,
/// where nothing listens, so a call fails, which a catch-up only logs.
pub fn unused_psp() -> PspClient {
    PspClient::new(
        Psp::Mock,
        "http://127.0.0.1:9",
        "unused",
        Duration::from_secs(10),
    )
    .expect("a local address and a key that a header can carry")
}

/// What the PSP's route holds in a test that sends it nothing: a key of its own, which
/// signs nothing that the test sends, and a PSP that it never reaches.
pub fn unused_webhooks() -> Webhooks {
    let key = SigningKey::generate().written();
    Webhooks::new(
        unused_psp(),
        SigningKeys::parse(&key).expect("a key that the fake made"),
    )
}

/// The two login roles' pools, as the server holds them (ADR-0025), and the operator's
/// secret, of which the server holds the digest (ADR-0024). The system role's connections
/// act through `SET ROLE`, so close both before the test returns.
pub struct Pools {
    pub tenant: PgPool,
    pub system: PgPool,
    pub operator: ApiKeySecret,
}

impl Pools {
    pub async fn login(pool: &PgPool) -> sqlx::Result<Self> {
        Ok(Self {
            tenant: tenant_pool(pool).await?,
            system: runtime_pool(pool).await?,
            operator: ApiKeySecret::generate(),
        })
    }

    pub fn state(&self) -> AppState {
        AppState::new(
            self.tenant.clone(),
            self.system.clone(),
            self.operator.digest(),
            unused_webhooks(),
        )
    }

    pub async fn close(self) {
        self.tenant.close().await;
        self.system.close().await;
    }
}
