// Author: Nicola Antonello

//! What the tests of the HTTP API share (ADR-0023).

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

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
