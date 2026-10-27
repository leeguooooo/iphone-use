//! Read-only selected values have explicit bounded completeness and publication scope.
mod support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;
async fn response(state: Arc<server::http::AppState>, body: Value) -> axum::response::Response {
    server::http::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agent/value")
                .header("x-phone-owner", "reader")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn decode(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn hold(state: Arc<server::http::AppState>) {
    let response = server::http::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agent/hold")
                .header("x-phone-owner", "human")
                .header("x-phone-control", "1")
                .header("content-type", "application/json")
                .body(Body::from(json!({"secs":60,"private":true}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
fn contract(limit: u32) -> Value {
    let total = 6015;
    let returned = (limit as usize).min(total);
    json!({"value":"字".repeat(returned),"source":"ax_value_attribute","utf16_length":total,"returned_utf16_length":returned,"limit_utf16":limit,"truncated":returned<total,"complete":returned==total})
}
fn base(r: &str, ids: usize, supports: bool) -> String {
    if r.starts_with("POST /session ") {
        json!({"value":{"sessionId":"SESSION","capabilities":{"ipu:valueRead":supports}}})
            .to_string()
    } else if r.contains("/elements") {
        json!({"value":(0..ids).map(|i|json!({"ELEMENT":format!("E{i}")})).collect::<Vec<_>>(),})
            .to_string()
    } else {
        json!({"value":null}).to_string()
    }
}
#[test]
fn full_or_intentionally_bounded_value_never_takes_a_control_lease() {
    block(async {
        for limit in [8192, 128] {
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let runner = mock_runner(move |r, _| {
                seen.lock().unwrap().push(r.into());
                let value = if r.starts_with("GET ") && r.contains("/element/E0/value") {
                    json!({"value":contract(limit)}).to_string()
                } else {
                    base(r, 1, true)
                };
                Some((Duration::ZERO, value))
            });
            let state = build_state_with_runner(runner.url());
            let (status, body) = decode(
                response(
                    state.clone(),
                    json!({"identifier":"field-a","max_utf16":limit}),
                )
                .await,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["complete"], limit >= 6015);
            assert_eq!(body["truncated"], limit < 6015);
            assert_eq!(
                body["value"].as_str().unwrap().chars().count(),
                (limit as usize).min(6015)
            );
            assert!(state.owner.lock().unwrap().is_none());
            assert!(calls
                .lock()
                .unwrap()
                .iter()
                .all(|r| !r.contains("x-phone-control")
                    && !r.contains("/actions")
                    && !r.contains("/wda/keys")));
        }
    });
}
#[test]
fn bad_selection_ambiguity_and_old_or_malformed_contracts_never_promote_previews() {
    block(async {
        for (ids, supports, bad) in [
            (2, true, false),
            (0, true, false),
            (1, false, false),
            (1, true, true),
        ] {
            let runner = mock_runner(move |r, _| {
                let value = if r.starts_with("GET ") && r.contains("/element/E0/value") {
                    json!({"value":{"value":"PREVIEW","complete":true}}).to_string()
                } else {
                    base(r, ids, supports)
                };
                Some((Duration::ZERO, value))
            });
            let state = build_state_with_runner(runner.url());
            let (status, body) = decode(response(state, json!({"label":"Draft"})).await).await;
            assert!(!status.is_success(), "{ids} {supports} {bad}: {body}");
            assert!(body.get("value").is_none());
            assert!(!body.to_string().contains("PREVIEW"));
        }
        let runner = mock_runner(|_, _| panic!("invalid request must not access runner"));
        let state = build_state_with_runner(runner.url());
        for payload in [
            json!({}),
            json!({"identifier":"a","label":"b"}),
            json!({"identifier":"a","max_utf16":0}),
        ] {
            assert_eq!(
                response(state.clone(), payload).await.status(),
                StatusCode::BAD_REQUEST
            );
        }
    });
}
#[test]
fn private_and_in_flight_scope_loss_return_no_value() {
    block(async {
        let signal = Arc::new(tokio::sync::Notify::new());
        let started = signal.clone();
        let runner = mock_runner(move |r, _| {
            let (delay, value) = if r.starts_with("GET ") && r.contains("/element/E0/value") {
                started.notify_one();
                (
                    Duration::from_millis(500),
                    json!({"value":contract(8192)}).to_string(),
                )
            } else {
                (Duration::ZERO, base(r, 1, true))
            };
            Some((delay, value))
        });
        let state = build_state_with_runner(runner.url());
        let worker = state.clone();
        let pending = tokio::spawn(async move {
            decode(response(worker, json!({"identifier":"field-a"})).await).await
        });
        tokio::time::timeout(Duration::from_secs(3), signal.notified())
            .await
            .unwrap();
        hold(state.clone()).await;
        let (_, body) = pending.await.unwrap();
        assert!(body.get("value").is_none(), "{body}");
        assert_eq!(body["ok"], false);
        assert_eq!(
            response(state, json!({"identifier":"field-a"}))
                .await
                .status(),
            StatusCode::LOCKED
        );
    });
}
#[test]
fn a_queued_value_body_rechecks_scope_before_publication() {
    block(async {
        let runner = mock_runner(|r, _| {
            Some((
                Duration::ZERO,
                if r.starts_with("GET ") && r.contains("/element/E0/value") {
                    json!({"value":contract(8192)}).to_string()
                } else {
                    base(r, 1, true)
                },
            ))
        });
        let state = build_state_with_runner(runner.url());
        let reply = response(state.clone(), json!({"identifier":"field-a"})).await;
        assert_eq!(reply.status(), StatusCode::OK);
        hold(state).await;
        let (_, body) = decode(reply).await;
        assert_eq!(body["ok"], false);
        assert!(body.get("value").is_none());
        assert_eq!(body["value_omitted"], true);
    });
}
