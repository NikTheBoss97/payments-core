// Author: Nicola Antonello

//! Who a request comes from (ADR-0024).
//!
//! The merchant comes from the credential, never from the request. Every failure gets
//! one answer, so that a client learns nothing about a key that it does not hold, except
//! that a failure to look a key up says so, so that it never concludes that a good key
//! is bad.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, header};
use sqlx::PgPool;

use super::AppState;
use super::problem::Problem;
use crate::credentials::authenticate;
use crate::ids::MerchantId;
use crate::tenancy::Tenant;

/// The merchant that a request's credential names, and its one way to the database. It
/// holds the tenant role's pool only, so a path that forgets to name its merchant fails
/// closed (ADR-0025).
#[derive(Debug, Clone)]
pub struct Merchant {
    id: MerchantId,
    pool: PgPool,
}

impl Merchant {
    #[must_use]
    pub fn id(&self) -> MerchantId {
        self.id
    }

    /// This merchant's scope, for the library's commands and reads.
    #[must_use]
    pub fn tenant(&self) -> Tenant<'_> {
        Tenant::new(&self.pool, self.id)
    }
}

impl FromRequestParts<AppState> for Merchant {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let secret = bearer_secret(&parts.headers).ok_or(Problem::Unauthorized)?;
        match authenticate(&state.system, secret).await {
            Ok(Some(id)) => Ok(Self {
                id,
                pool: state.tenant.clone(),
            }),
            Ok(None) => Err(Problem::Unauthorized),
            Err(err) => Err(Problem::Unavailable.because(&err)),
        }
    }
}

/// The secret in a request's one `Authorization` header: `Bearer` in any case, one or more
/// spaces, and the secret (RFC 9110 section 11.1, RFC 6750 section 2.1). The secret's
/// own format is not checked: anything else has a digest that no key has.
fn bearer_secret(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, rest) = value.to_str().ok()?.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| rest.trim_start_matches(' '))
}
