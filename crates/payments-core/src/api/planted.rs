// Author: Nicola Antonello

//! Deliberate vulnerabilities, on a branch that is never merged (ADR-0027). Does CodeQL read
//! an axum handler's arguments as untrusted, and does it see a secret that is logged?

use axum::Router;
use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use serde::Deserialize;
use sqlx::AssertSqlSafe;

use super::problem::Problem;
use super::{AppState, KeyedRequest};
use crate::credentials::ApiKeySecret;

/// A file path as the whole body, through axum's own extractor.
async fn file_from_a_text_body(body: String) -> String {
    std::fs::read_to_string(body).unwrap_or_default()
}

/// A file path from raw bytes, through axum's own extractor and the same conversion as the
/// plant through `KeyedRequest`.
async fn file_from_bytes(body: Bytes) -> String {
    let name = String::from_utf8_lossy(&body).into_owned();
    std::fs::read_to_string(name).unwrap_or_default()
}

#[derive(Deserialize)]
struct Parsed {
    name: String,
}

/// A file path parsed from JSON, as a real route's body is.
async fn file_from_json(body: Bytes) -> String {
    match serde_json::from_slice::<Parsed>(&body) {
        Ok(parsed) => std::fs::read_to_string(parsed.name).unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// An extractor of this project's own, with the text in a field.
struct Named {
    name: String,
}

impl Named {
    fn name(&self) -> &str {
        &self.name
    }
}

impl<S: Send + Sync> FromRequest<S> for Named {
    type Rejection = Problem;

    async fn from_request(request: Request, state: &S) -> Result<Self, Problem> {
        let name = String::from_request(request, state)
            .await
            .map_err(|_| Problem::MalformedBody)?;
        Ok(Self { name })
    }
}

/// A file path read straight from the field of such an extractor.
async fn file_from_a_field(named: Named) -> String {
    std::fs::read_to_string(&named.name).unwrap_or_default()
}

/// A file path read through a method of such an extractor.
async fn file_through_a_method(named: Named) -> String {
    std::fs::read_to_string(named.name()).unwrap_or_default()
}

/// SQL from a path, through the wrapper that sqlx 0.9 requires.
async fn sql_in_the_wrapper(State(state): State<AppState>, Path(name): Path<String>) -> String {
    let sql = format!("SELECT count(*) FROM merchants WHERE id = '{name}'");
    #[expect(clippy::disallowed_methods, reason = "a plant")]
    let outcome = sqlx::query(AssertSqlSafe(sql)).execute(&state.system).await;
    outcome.is_ok().to_string()
}

/// SQL from a path as a plain string. sqlx 0.9 does not compile this, and CodeQL needs no
/// build, so this branch fails CI on purpose.
async fn sql_as_a_str(State(state): State<AppState>, Path(name): Path<String>) -> String {
    let sql = format!("SELECT count(*) FROM merchants WHERE id = '{name}'");
    let outcome = sqlx::query(sql.as_str()).execute(&state.system).await;
    outcome.is_ok().to_string()
}

/// A file path from a path.
async fn file_from_a_path(Path(name): Path<String>) -> String {
    std::fs::read_to_string(name).unwrap_or_default()
}

/// A file path from the body, through this project's own extractor, which is how every
/// real route takes its request.
async fn file_from_our_extractor(request: KeyedRequest) -> String {
    let name = String::from_utf8_lossy(request.plant_body()).into_owned();
    std::fs::read_to_string(name).unwrap_or_default()
}

/// A credential from the request, printed.
async fn password_printed(headers: HeaderMap) {
    let password = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    println!("{password}");
}

/// The same credential, through the logger that this server uses.
async fn password_traced(headers: HeaderMap) {
    let password = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    tracing::error!("{password}");
}

/// A new API key's secret, printed.
async fn api_key_printed() {
    let secret = ApiKeySecret::generate();
    println!("{}", secret.expose());
}

/// A new API key's secret, through the logger that this server uses.
async fn api_key_traced() {
    let secret = ApiKeySecret::generate();
    tracing::error!("{}", secret.expose());
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/planted/sql-wrapper/{name}", get(sql_in_the_wrapper))
        .route("/planted/sql-str/{name}", get(sql_as_a_str))
        .route("/planted/file/{name}", get(file_from_a_path))
        .route("/planted/extractor", post(file_from_our_extractor))
        .route("/planted/text-body", post(file_from_a_text_body))
        .route("/planted/bytes", post(file_from_bytes))
        .route("/planted/json", post(file_from_json))
        .route("/planted/field", post(file_from_a_field))
        .route("/planted/method", post(file_through_a_method))
        .route("/planted/password-printed", get(password_printed))
        .route("/planted/password-traced", get(password_traced))
        .route("/planted/key-printed", get(api_key_printed))
        .route("/planted/key-traced", get(api_key_traced))
}
