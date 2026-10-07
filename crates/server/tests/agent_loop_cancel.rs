//! A request cancelled mid-flight (the client went away and the server
//! dropped its future) is recorded as a cancelled call with an unknown
//! outcome — never left "in flight" until the run's 12-hour lifetime.
//!
//! Drives the real router and the real timing layer; the cancellation is the
//! future being dropped, exactly what the server does on a lost client.

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

use support::{block, build_state_with_wda, mock_wda};

#[test]
fn a_cancelled_request_is_counted_cancelled_not_left_in_flight() {
    block(async {
        // The tree read takes far longer than the client is willing to wait.
        let wda = mock_wda(|request, _| {
            let line = request.lines().next().unwrap_or("");
            if line.starts_with("POST /session ") {
                return Some((Duration::ZERO, r#"{"value":{"sessionId":"S"}}"#.to_string()));
            }
            if line.contains("/source") {
                return Some((Duration::from_secs(5), r#"{"value":null}"#.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let app = server::http::router(build_state_with_wda(wda.url()));
        let request = Request::builder()
            .uri("/agent/elements")
            .header("x-phone-owner", "cancel-owner")
            .body(Body::empty())
            .unwrap();
        let outcome = tokio::time::timeout(Duration::from_millis(300), app.oneshot(request)).await;
        assert!(outcome.is_err(), "the request was still running when dropped");

        let report = server::metrics::report(Some("cancel-owner"));
        let open = report["open"].as_array().unwrap();
        assert_eq!(open.len(), 1, "{report}");
        let run = &open[0];
        assert_eq!(run["in_flight_at_close"], 0, "not left in flight: {run}");
        assert_eq!(run["cancelled_events"], 1, "{run}");
        assert_eq!(run["outcome_unknown"], 1, "{run}");
        assert_eq!(run["incomplete"], true, "{run}");
    });
}
