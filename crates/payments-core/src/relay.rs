// Author: Nicola Antonello

//! The relay's rules, apart from its input and output (ADR-0052, ADR-0053): what an
//! endpoint's answer means, which deliveries to attempt at each step, when an endpoint is
//! disabled, and the mark below which a search need not look. The server's task reads the
//! database, posts the attempts and carries out the effects; this module decides. The
//! time and the random draws are inputs, so every rule is tested without a clock.

#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

/// What one attempt to deliver got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The endpoint answered with this status, and with a `Retry-After` of this many
    /// seconds, if it gave one in seconds.
    Status {
        status: u16,
        retry_after: Option<u32>,
    },
    /// No answer came: a refused connection, a reset, or the timeout, after which the
    /// outcome is unknown (ADR-0053).
    NoAnswer,
}

/// What an answer means, by ADR-0053's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A `2xx`: the success is recorded, and the delivery is finished.
    Delivered,
    /// A `410`: the endpoint is disabled, and its deliveries are finished.
    Gone,
    /// Anything else: tried again, and never before `at_least` seconds when the endpoint
    /// asked for them.
    Again { at_least: Option<u32> },
}

/// Reads an answer by ADR-0053's table. A redirect is a failure, since the client follows
/// none, and a 429, 502 or 504 without `Retry-After` backs off like any other failure.
#[must_use]
pub fn verdict(answer: Answer) -> Verdict {
    match answer {
        Answer::Status {
            status: 200..=299, ..
        } => Verdict::Delivered,
        Answer::Status { status: 410, .. } => Verdict::Gone,
        Answer::Status { retry_after, .. } => Verdict::Again {
            at_least: retry_after,
        },
        Answer::NoAnswer => Verdict::Again { at_least: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(status: u16) -> Answer {
        Answer::Status {
            status,
            retry_after: None,
        }
    }

    fn asking(status: u16, seconds: u32) -> Answer {
        Answer::Status {
            status,
            retry_after: Some(seconds),
        }
    }

    #[test]
    fn a_2xx_is_delivered_whatever_else_it_says() {
        for code in [200, 201, 202, 204, 299] {
            assert_eq!(verdict(status(code)), Verdict::Delivered, "{code}");
        }
        assert_eq!(verdict(asking(200, 60)), Verdict::Delivered);
    }

    #[test]
    fn a_410_is_gone_whatever_else_it_says() {
        assert_eq!(verdict(status(410)), Verdict::Gone);
        assert_eq!(verdict(asking(410, 60)), Verdict::Gone);
    }

    #[test]
    fn anything_else_is_tried_again() {
        // Redirects, which the client does not follow, every other client or server
        // error, and the codes that ask to slow down, sent without a Retry-After.
        for code in [
            100, 199, 300, 301, 302, 304, 307, 308, 400, 401, 403, 404, 409, 411, 429, 500, 502,
            503, 504, 599,
        ] {
            assert_eq!(
                verdict(status(code)),
                Verdict::Again { at_least: None },
                "{code}"
            );
        }
        assert_eq!(verdict(Answer::NoAnswer), Verdict::Again { at_least: None });
    }

    #[test]
    fn a_retry_after_is_the_least_wait() {
        for code in [301, 429, 500, 502, 503, 504] {
            assert_eq!(
                verdict(asking(code, 120)),
                Verdict::Again {
                    at_least: Some(120)
                },
                "{code}"
            );
        }
    }
}
