// Author: Nicola Antonello

//! Delivering notifications (ADR-0030, ADR-0032). Each attempt is signed afresh with the
//! time of the attempt and posted with the legacy client of ADR-0036, under a timeout.
//! An attempt that fails is due again on the example schedule of Standard Webhooks.

use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, Request, header};
use chrono::{DateTime, TimeDelta, Utc};
use http_body_util::Full;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

/// The delays before each attempt after the first, which is immediate: the specification's
/// example schedule, ten attempts over 75 hours and a half.
pub(crate) const SCHEDULE: [TimeDelta; 9] = [
    TimeDelta::seconds(5),
    TimeDelta::minutes(5),
    TimeDelta::minutes(30),
    TimeDelta::hours(2),
    TimeDelta::hours(5),
    TimeDelta::hours(10),
    TimeDelta::hours(14),
    TimeDelta::hours(20),
    TimeDelta::hours(24),
];

/// What one attempt got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// The receiver answered with this status, and with a `Retry-After` of this many
    /// seconds, if it gave one.
    Answered {
        status: u16,
        retry_after: Option<u32>,
    },
    /// No answer came: a refused connection, a reset, or the timeout.
    NoAnswer,
}

impl Attempt {
    /// Whether the receiver took the notification, with a `2xx`: the PSP then stops.
    #[must_use]
    pub fn delivered(self) -> bool {
        matches!(
            self,
            Self::Answered {
                status: 200..=299,
                ..
            }
        )
    }
}

/// When the next attempt is due, after `attempts` attempts that failed, the last at
/// `failed_at`; `None` once the schedule is spent. `jitter`, from 0 to 999, adds up to a
/// fifth of the delay, so that the notifications to one endpoint do not all retry at
/// once, and a `Retry-After` can only delay it further.
pub(crate) fn next_attempt(
    attempts: u32,
    failed_at: DateTime<Utc>,
    retry_after: Option<u32>,
    jitter: u16,
) -> Option<DateTime<Utc>> {
    let index = usize::try_from(attempts.checked_sub(1)?).ok()?;
    let delay = *SCHEDULE.get(index)?;
    let micros = delay
        .num_microseconds()
        .expect("a delay of hours fits in microseconds");
    let jittered = delay + TimeDelta::microseconds(micros * i64::from(jitter.min(999)) / 5_000);
    let asked = TimeDelta::seconds(i64::from(retry_after.unwrap_or(0)));
    Some(failed_at + jittered.max(asked))
}

/// A `Retry-After` in seconds. The HTTP-date form reads as none.
fn retry_after(headers: &HeaderMap) -> Option<u32> {
    headers
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Posts notifications. The client's one retry of its own is switched off, so that
/// nothing below the schedule retries (ADR-0036).
pub(crate) struct Courier {
    client: Client<HttpConnector, Full<Bytes>>,
    timeout: Duration,
}

impl Courier {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            client: Client::builder(TokioExecutor::new())
                .retry_canceled_requests(false)
                .build_http(),
            timeout,
        }
    }

    /// One attempt, with the headers of Standard Webhooks.
    pub(crate) async fn post(
        &self,
        endpoint: &str,
        message_id: &str,
        timestamp: i64,
        signature: &str,
        payload: Vec<u8>,
    ) -> Attempt {
        let request = Request::builder()
            .method(Method::POST)
            .uri(endpoint)
            .header(header::CONTENT_TYPE, "application/json")
            .header("webhook-id", message_id)
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", signature)
            .body(Full::new(Bytes::from(payload)))
            .expect("an endpoint that the handle checked, and valid headers");
        match tokio::time::timeout(self.timeout, self.client.request(request)).await {
            Ok(Ok(response)) => Attempt::Answered {
                status: response.status().as_u16(),
                retry_after: retry_after(response.headers()),
            },
            Ok(Err(_)) | Err(_) => Attempt::NoAnswer,
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp_micros(1_767_225_600_000_000).expect("a time in range")
    }

    #[test]
    fn the_schedule_is_the_specifications_example() {
        // Its table: 00:00:05, 00:05:05, 00:35:05, 02:35:05, 07:35:05, 17:35:05, 31:35:05,
        // 51:35:05 and 75:35:05 since the first attempt.
        let mut since_start = TimeDelta::zero();
        let mut elapsed = Vec::new();
        for delay in SCHEDULE {
            since_start += delay;
            elapsed.push(since_start.num_seconds());
        }
        let hms = |h: i64, m: i64, s: i64| (h * 60 + m) * 60 + s;
        assert_eq!(
            elapsed,
            [
                hms(0, 0, 5),
                hms(0, 5, 5),
                hms(0, 35, 5),
                hms(2, 35, 5),
                hms(7, 35, 5),
                hms(17, 35, 5),
                hms(31, 35, 5),
                hms(51, 35, 5),
                hms(75, 35, 5),
            ]
        );
    }

    #[test]
    fn each_attempt_waits_its_delay_and_up_to_a_fifth_more() {
        for (index, delay) in SCHEDULE.into_iter().enumerate() {
            let attempts = u32::try_from(index + 1).expect("ten attempts");
            assert_eq!(next_attempt(attempts, at(), None, 0), Some(at() + delay));
            let longest = next_attempt(attempts, at(), None, 999).expect("not spent");
            assert!(longest > at() + delay, "attempt {attempts}");
            assert!(longest < at() + delay + delay / 5, "attempt {attempts}");
        }
        assert_eq!(next_attempt(10, at(), None, 0), None);
        assert_eq!(next_attempt(0, at(), None, 0), None);
    }

    #[test]
    fn a_retry_after_can_only_delay_the_next_attempt() {
        assert_eq!(
            next_attempt(1, at(), Some(600), 0),
            Some(at() + TimeDelta::seconds(600))
        );
        assert_eq!(
            next_attempt(4, at(), Some(1), 0),
            Some(at() + TimeDelta::hours(2))
        );
    }

    #[test]
    fn a_retry_after_is_read_in_seconds_only() {
        let mut headers = HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
        for (value, read) in [
            ("120", Some(120)),
            (" 7 ", Some(7)),
            ("Wed, 21 Oct 2015 07:28:00 GMT", None),
            ("-1", None),
        ] {
            headers.insert(header::RETRY_AFTER, HeaderValue::from_static(value));
            assert_eq!(retry_after(&headers), read, "{value}");
        }
    }

    #[test]
    fn only_a_2xx_is_a_delivery() {
        for (status, delivered) in [
            (200, true),
            (202, true),
            (299, true),
            (301, false),
            (500, false),
        ] {
            let attempt = Attempt::Answered {
                status,
                retry_after: None,
            };
            assert_eq!(attempt.delivered(), delivered, "{status}");
        }
        assert!(!Attempt::NoAnswer.delivered());
    }
}
