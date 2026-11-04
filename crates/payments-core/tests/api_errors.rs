// Author: Nicola Antonello

//! Errors as a client sees them: `application/problem+json` (RFC 9457, ADR-0023).

mod common;

use std::error::Error;

use axum::Router;
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::get as get_route;
use common::api::{Pools, get, send};
use payments_core::api::{in_its_own_task, router};
use sqlx::PgPool;

const PROBLEM_JSON: &str = "application/problem+json";

async fn panics() -> &'static str {
    panic!("a secret in a panic, which no answer may show")
}

#[sqlx::test]
async fn a_path_that_is_no_route_is_a_problem(pool: PgPool) -> Result<(), Box<dyn Error>> {
    let pools = Pools::login(&pool).await?;
    let reply = send(router(pools.state()), get("/nothing/here")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.header("content-type"), PROBLEM_JSON);
    assert_eq!(
        reply.body,
        r#"{"type":"https://github.com/NikTheBoss97/payments-core/blob/main/docs/api.md#not-found","title":"No such resource","status":404}"#
    );
    pools.close().await;
    Ok(())
}

#[tokio::test]
async fn a_panic_is_a_problem_that_says_nothing_of_its_cause() {
    let router = Router::new()
        .route("/panic", get_route(panics))
        .layer(middleware::from_fn(in_its_own_task));
    let reply = send(router, get("/panic")).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.header("content-type"), PROBLEM_JSON);
    assert_eq!(
        reply.body,
        r#"{"type":"https://github.com/NikTheBoss97/payments-core/blob/main/docs/api.md#internal-error","title":"Something went wrong on the server","status":500}"#
    );
}
