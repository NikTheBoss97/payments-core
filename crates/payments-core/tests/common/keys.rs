// Author: Nicola Antonello

//! What the tests of commands under an idempotency key share (ADR-0026).

use payments_core::idempotency::{Answer, Fingerprint, IdempotencyKey, Recall};
use payments_core::lifecycle::Refused;
use payments_core::payments::Recorded;

pub fn key(text: &str) -> IdempotencyKey {
    IdempotencyKey::new(text).expect("a valid key")
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
