// Author: Nicola Antonello

//! What the tests of the HTTP API share (ADR-0023).

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use payments_core::api::AppState;
use payments_core::credentials::ApiKeySecret;
use sqlx::PgPool;
use tower::ServiceExt;

use super::{runtime_pool, tenant_pool};

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

/// The two login roles' pools, as the server holds them (ADR-0025). The system role's
/// connections act through `SET ROLE`, so close both before the test returns.
pub struct Pools {
    pub tenant: PgPool,
    pub system: PgPool,
}

impl Pools {
    pub async fn login(pool: &PgPool) -> sqlx::Result<Self> {
        Ok(Self {
            tenant: tenant_pool(pool).await?,
            system: runtime_pool(pool).await?,
        })
    }

    pub fn state(&self) -> AppState {
        AppState::new(self.tenant.clone(), self.system.clone())
    }

    pub async fn close(self) {
        self.tenant.close().await;
        self.system.close().await;
    }
}
