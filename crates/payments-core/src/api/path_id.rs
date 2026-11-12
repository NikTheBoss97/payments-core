// Author: Nicola Antonello

//! An ID in a path (ADR-0023).

use std::collections::HashMap;

use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use uuid::Uuid;

use super::problem::Problem;
use crate::ids::RequestId;

/// The ID that a request's path names `id`, in the one spelling that the API uses. Any other
/// spelling of a real ID reads as no such thing, because one thing has one path: a retry that
/// spelled it differently would read as another request (ADR-0026).
#[derive(Debug)]
pub(super) struct PathId<T>(pub(super) T);

impl<S: Send + Sync, T: TryFrom<Uuid>> FromRequestParts<S> for PathId<T> {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Problem> {
        named(parts, state, "id").await.map(Self)
    }
}

/// The ID that a request's path names `request_id`, read as [`PathId`] reads `id`.
#[derive(Debug)]
pub(super) struct PathRequestId(pub(super) RequestId);

impl<S: Send + Sync> FromRequestParts<S> for PathRequestId {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Problem> {
        named(parts, state, "request_id").await.map(Self)
    }
}

/// The ID that the path names `name`, in its one spelling, or a 404 like none. Reading it by
/// its name lets a path hold more than one ID.
async fn named<S: Send + Sync, T: TryFrom<Uuid>>(
    parts: &mut Parts,
    state: &S,
    name: &str,
) -> Result<T, Problem> {
    // `Path` decodes percent-encoding, which would give a real ID another spelling.
    if parts.uri.path().contains('%') {
        return Err(Problem::NotFound);
    }
    let Path(ids) = Path::<HashMap<String, String>>::from_request_parts(parts, state)
        .await
        .map_err(|_| Problem::NotFound)?;
    ids.get(name)
        .and_then(|text| canonical(text))
        .ok_or(Problem::NotFound)
}

fn canonical<T: TryFrom<Uuid>>(text: &str) -> Option<T> {
    let id = Uuid::try_parse(text).ok()?;
    if id.to_string() != text {
        return None;
    }
    T::try_from(id).ok()
}
