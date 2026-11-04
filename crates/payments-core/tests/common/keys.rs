// Author: Nicola Antonello

//! What the tests of commands under an idempotency key share (ADR-0026).

use std::error::Error;

use payments_core::credentials::ApiKeySecret;
use payments_core::idempotency::{Answer, Fingerprint, IdempotencyKey, OperatorSeal, Recall};
use payments_core::ids::MerchantId;
use payments_core::lifecycle::Refused;
use payments_core::operator::issue_api_key;
use payments_core::payments::Recorded;
use sqlx::PgPool;

use super::api_key_id;

pub fn key(text: &str) -> IdempotencyKey {
    IdempotencyKey::new(text).expect("a valid key")
}

/// Issues API key `n` to this merchant, as the system role, and returns its secret.
pub async fn issued_key(
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

/// The fingerprint of a request, told apart by `n`.
pub fn print(n: u32) -> Fingerprint {
    Fingerprint::of("POST", &format!("/request/{n}"), b"{}")
}

/// What these tests answer: the status a route would give, and a body that shows
/// which answer this was.
pub fn render(answer: Result<&Recorded, &Refused>) -> Answer {
    match answer {
        Ok(recorded) => Answer {
            status: 200,
            body: format!(
                r#"{{"seq":{},"refundable":{}}}"#,
                recorded.seq, recorded.refundable
            ),
        },
        Err(Refused::Illegal(_)) => Answer {
            status: 409,
            body: r#"{"type":"illegal"}"#.to_owned(),
        },
        Err(Refused::AboveLimit { limit, .. }) => Answer {
            status: 422,
            body: format!(r#"{{"type":"limit","limit":{limit}}}"#),
        },
    }
}

/// What looking a key up finds once it holds this answer.
pub fn answered(status: i16, body: &str) -> Recall {
    Recall::Answered(Answer {
        status,
        body: body.to_owned(),
    })
}
