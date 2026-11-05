// Author: Nicola Antonello

//! An ID in a path (ADR-0023).

use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use uuid::Uuid;

use super::problem::Problem;

/// The ID in a request's path, in the one spelling that the API uses. Any other spelling of
/// a real ID reads as no such thing, because one thing has one path: a retry that spelled it
/// differently would read as another request (ADR-0026).
#[derive(Debug)]
pub(super) struct PathId<T>(pub(super) T);

impl<S: Send + Sync, T: TryFrom<Uuid>> FromRequestParts<S> for PathId<T> {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Problem> {
        let Path(text) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| Problem::NotFound)?;
        canonical(&text).map(Self).ok_or(Problem::NotFound)
    }
}

fn canonical<T: TryFrom<Uuid>>(text: &str) -> Option<T> {
    let id = Uuid::try_parse(text).ok()?;
    if id.to_string() != text {
        return None;
    }
    T::try_from(id).ok()
}
