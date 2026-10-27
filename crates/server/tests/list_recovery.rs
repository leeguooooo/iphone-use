//! Fresh list diagnostics retain coverage/fate and remain in the original scope.
mod support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;
const TREE: &str = r#"{"value":{"type":"XCUIElementTypeApplication","label":"Fixture","rect":{"x":0,"y":0,"width":390,"height":844},"children":[{"type":"XCUIElementTypeCell","label":"ROW","rect":{"x":0,"y":100,"width":390,"height":60},"isVisible":"1","isEnabled":"1"}]}}"#;
async fn call(
    state: Arc<server::http::AppState>,
    path: &str,
    body: Value,
    disabled: bool,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .header("x-phone-control", "1")
        .header("x-phone-owner", "list-recovery")
        .header("content-type", "application/json");
    if disabled {
        req = req.header("x-phone-recovery-image", "none");
    }
    let result = server::http::router(state)
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = result.status();
    let bytes = result.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn reply(request: &str, png: &str) -> String {
    if request.starts_with("POST /session ") {
        json!({"value":{"sessionId":"SESSION"}}).to_string()
    } else if request.contains("/source") {
        TREE.into()
    } else if request.contains("/window/") {
        json!({"value":{"width":390,"height":844}}).to_string()
    } else if request.contains("/screenshot") {
        json!({"value":png}).to_string()
    } else {
        json!({"value":null}).to_string()
    }
}
#[test]
fn lookup_and_unreadable_collection_have_fresh_diagnostics_without_extra_swipes() {
    block(async {
        for disabled in [false, true] {
            for find in [false, true] {
                let captures = Arc::new(AtomicUsize::new(0));
                let count = captures.clone();
                let png = include_bytes!("../../mcp/tests/fixtures/recovery.png");
                let b64 = STANDARD.encode(png);
                let runner = mock_runner(move |r, _| {
                    if r.contains("/screenshot") {
                        count.fetch_add(1, Ordering::AcqRel);
                    }
                    Some((Duration::ZERO, reply(r, &b64)))
                });
                let state = build_state_with_runner(runner.url());
                let (path, body) = if find {
                    (
                        "/agent/scroll_find",
                        json!({"label":"missing","max_swipes":0}),
                    )
                } else {
                    ("/agent/collect", json!({"row_kind":"Button","max_pages":1}))
                };
                let (status, body) = call(state, path, body, disabled).await;
                assert_eq!(status, StatusCode::OK, "{body}");
                assert_eq!(body["swipes"], 0);
                if find {
                    assert_eq!(body["error"], "element_not_found");
                    assert_eq!(body["found"], false);
                } else {
                    assert_eq!(body["ok"], true);
                    assert_eq!(body["complete"], false);
                    assert_eq!(body["stop_reason"], "no_rows");
                }
                if disabled {
                    assert!(body.get("image").is_none());
                    assert_eq!(captures.load(Ordering::Acquire), 0);
                } else {
                    assert_eq!(body["recovery"]["replay_action"], false);
                    assert_eq!(
                        STANDARD
                            .decode(body["image"]["png_base64"].as_str().unwrap())
                            .unwrap(),
                        png
                    );
                    assert_eq!(captures.load(Ordering::Acquire), 1);
                }
            }
        }
    });
}
#[test]
fn found_targets_and_intentional_page_budget_do_not_capture() {
    block(async {
        for find in [false, true] {
            let captures = Arc::new(AtomicUsize::new(0));
            let count = captures.clone();
            let runner = mock_runner(move |r, _| {
                if r.contains("/screenshot") {
                    count.fetch_add(1, Ordering::AcqRel);
                }
                Some((Duration::ZERO, reply(r, "")))
            });
            let state = build_state_with_runner(runner.url());
            let (path, body) = if find {
                ("/agent/scroll_find", json!({"label":"ROW","max_swipes":0}))
            } else {
                ("/agent/collect", json!({"max_pages":1}))
            };
            let (_, body) = call(state, path, body, false).await;
            assert_eq!(body["ok"], true, "{body}");
            assert!(body.get("image").is_none());
            assert!(body.get("recovery").is_none());
            assert_eq!(captures.load(Ordering::Acquire), 0);
        }
    });
}
#[test]
fn private_transition_during_recovery_withholds_pixels_and_prior_rows() {
    block(async {
        let started = Arc::new(tokio::sync::Notify::new());
        let signal = started.clone();
        let b64 = STANDARD.encode(include_bytes!("../../mcp/tests/fixtures/recovery.png"));
        let runner = mock_runner(move |r, _| {
            let delay = if r.contains("/screenshot") {
                signal.notify_one();
                Duration::from_millis(500)
            } else {
                Duration::ZERO
            };
            Some((delay, reply(r, &b64)))
        });
        let state = build_state_with_runner(runner.url());
        let worker = state.clone();
        let pending = tokio::spawn(async move {
            call(
                worker,
                "/agent/scroll_find",
                json!({"label":"missing","max_swipes":0}),
                false,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        for body in [json!({"secs":60,"private":true}), json!({"secs":0})] {
            assert_eq!(
                call(state.clone(), "/agent/hold", body, true).await.0,
                StatusCode::OK
            );
        }
        let (_, body) = pending.await.unwrap();
        assert!(body.get("image").is_none(), "{body}");
        assert_eq!(body["observation_omitted"], true);
        assert_eq!(body["native_mutations_confirmed"], 0);
    });
}

#[test]
fn uncertain_swipe_keeps_partial_rows_and_never_becomes_retry_safe_after_capture() {
    block(async {
        for find in [false, true] {
            let b64 = STANDARD.encode(include_bytes!("../../mcp/tests/fixtures/recovery.png"));
            let runner = mock_runner(move |r, _| {
                let body = if r.starts_with("POST /session/SESSION/actions") {
                    json!({"value":{"error":"unknown error","message":"uncertain gesture delivery"}}).to_string()
                } else {
                    reply(r, &b64)
                };
                Some((Duration::ZERO, body))
            });
            let state = build_state_with_runner(runner.url());
            let (path, payload) = if find {
                (
                    "/agent/scroll_find",
                    json!({"label":"missing","max_swipes":1}),
                )
            } else {
                ("/agent/collect", json!({"max_pages":2}))
            };
            let (status, body) = call(state, path, payload, false).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["native_mutations_confirmed"], 0);
            assert_eq!(body["native_mutations_uncertain"], 1);
            assert_eq!(body["swipes"], 0);
            assert_eq!(body["retry_safe"], false);
            assert_eq!(body["stop_reason"], "swipe_failed");
            assert_eq!(body["recovery"]["replay_action"], false);
            assert!(body["image"]["png_base64"].is_string());
            if !find {
                assert_eq!(body["complete"], false);
                assert_eq!(body["rows"].as_array().unwrap().len(), 1);
            }
        }
    });
}
