//! Real HTTP entry coverage for native-runner target-bound text. These tests inject target
//! loss and ambiguous transport failure after confirmed chunks; they never operate a phone.
mod support;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;

#[test]
fn guarded_text_entry_preserves_partial_counts_and_stops_batches() {
    block(async {
        for batch in [false, true] {
            for transport_loss in [false, true] {
                let keys = Arc::new(AtomicUsize::new(0));
                let bindings = Arc::new(AtomicUsize::new(0));
                let later_actions = Arc::new(AtomicUsize::new(0));
                let (seen_keys, seen_bindings, seen_later) =
                    (keys.clone(), bindings.clone(), later_actions.clone());
                let runner = mock_runner(move |request, _| {
                    if request.starts_with("POST /session ") {
                        return Some((Duration::ZERO, r#"{"value":{"sessionId":"SESSION","capabilities":{"ipu:guardedInput":true}}}"#.into()));
                    }
                    if request.contains("/wda/input-target") {
                        seen_bindings.fetch_add(1, Ordering::SeqCst);
                        return Some((
                            Duration::ZERO,
                            r#"{"value":{"token":"bound-field"}}"#.into(),
                        ));
                    }
                    if request.contains("/wda/keys") {
                        let body: serde_json::Value =
                            serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap())
                                .unwrap();
                        assert_eq!(body["inputTarget"], "bound-field");
                        if seen_keys.fetch_add(1, Ordering::SeqCst) == 2 {
                            if transport_loss {
                                return None;
                            }
                            let body = r#"{"value":{"error":"invalid argument","message":"input_target_app_changed; nothing was sent"}}"#;
                            return Some((Duration::ZERO, format!(
                                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)));
                        }
                    }
                    if request.contains("/homescreen") || request.contains("/pressButton") {
                        seen_later.fetch_add(1, Ordering::SeqCst);
                    }
                    Some((Duration::ZERO, r#"{"value":null}"#.into()))
                });
                let action = serde_json::json!({"type":"text","text":"字".repeat(800)});
                let payload = if batch {
                    serde_json::json!({"steps":[
                        {"kind":"action","action":action},
                        {"kind":"action","action":{"type":"home"}}
                    ]})
                } else {
                    action
                };
                let state = build_state_with_runner(runner.url());
                let response = server::http::router(state)
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(if batch {
                                "/agent/actions"
                            } else {
                                "/agent/input"
                            })
                            .header("x-phone-control", "1")
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(payload.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(body["error"], "text_partially_typed", "{body}");
                assert_eq!(body["characters_confirmed"], 400, "{body}");
                assert_eq!(
                    body["characters_uncertain"],
                    if transport_loss { 200 } else { 0 },
                    "{body}"
                );
                assert_eq!(body["remaining_text"], "字".repeat(400), "{body}");
                assert_eq!(body["retry_safe"], false, "{body}");
                if !transport_loss {
                    assert_eq!(
                        body["detail"]["runner_error"]["message"],
                        "input_target_app_changed; nothing was sent",
                        "{body}"
                    );
                }
                if batch {
                    assert_eq!(body["failed_step"], 0, "{body}");
                    assert_eq!(body["completed"], 0, "{body}");
                }
                assert_eq!(
                    keys.load(Ordering::SeqCst),
                    3,
                    "rejected/uncertain chunk is never replayed"
                );
                assert_eq!(
                    bindings.load(Ordering::SeqCst),
                    1,
                    "target is never reselected"
                );
                assert_eq!(
                    later_actions.load(Ordering::SeqCst),
                    0,
                    "batch stops before Home"
                );
                drop(runner);
            }
        }
    });
}
