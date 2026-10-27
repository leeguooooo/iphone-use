//! Handler time budget includes queue waits, preparation, reads and capture.
//! Virtual time tests never wait 80 real seconds or contact a real phone.
mod support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;
const TREE: &str = r#"{"value":{"type":"XCUIElementTypeApplication","label":"Fixture","rect":{"x":0,"y":0,"width":390,"height":844},"children":[{"type":"XCUIElementTypeCell","label":"KNOWN_ROW","rect":{"x":0,"y":100,"width":390,"height":60},"isVisible":"1","isEnabled":"1"}]}}"#;
async fn call(state: Arc<server::http::AppState>, path: &str, body: Value) -> (StatusCode, Value) {
    let response = server::http::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("x-phone-control", "1")
                .header("x-phone-owner", "deadline-fixture")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn reply(r: &str) -> String {
    if r.starts_with("POST /session ") {
        json!({"value":{"sessionId":"SESSION"}}).to_string()
    } else if r.contains("/source") {
        TREE.into()
    } else if r.contains("/window/") {
        json!({"value":{"width":390,"height":844}}).to_string()
    } else {
        json!({"value":null}).to_string()
    }
}
async fn expire() {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(81)).await;
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
}
#[test]
fn queue_wait_consumes_the_original_budget_before_any_native_request() {
    block(async {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = calls.clone();
        let runner = mock_runner(move |r, _| {
            seen.lock().unwrap().push(r.into());
            Some((Duration::ZERO, reply(r)))
        });
        let state = build_state_with_runner(runner.url());
        let lock = state.runner.as_ref().unwrap().lock().await;
        let worker = state.clone();
        let before = state.owner_epoch.load(Ordering::Acquire);
        let pending = tokio::spawn(async move {
            call(worker, "/agent/scroll_find", json!({"label":"missing"})).await
        });
        while state.owner_epoch.load(Ordering::Acquire) == before {
            tokio::task::yield_now().await;
        }
        expire().await;
        assert!(
            pending.is_finished(),
            "queued list must stop at entry deadline"
        );
        let (status, body) = pending.await.unwrap();
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
        assert_eq!(body["error"], "list_deadline");
        assert_eq!(body["outcome"], "not_sent");
        assert_eq!(body["native_mutations_uncertain"], 0);
        assert_eq!(body["retry_safe"], true);
        drop(lock);
        assert!(calls.lock().unwrap().is_empty());
    });
}
#[test]
fn deadline_during_a_native_swipe_stays_unknown_and_never_reads_or_replays() {
    block(async {
        let signal = Arc::new(tokio::sync::Notify::new());
        let started = signal.clone();
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = calls.clone();
        let runner = mock_runner(move |r, _| {
            seen.lock().unwrap().push(r.into());
            let delay = if r.starts_with("POST /session/SESSION/actions") {
                started.notify_one();
                Duration::from_secs(10)
            } else {
                Duration::ZERO
            };
            Some((delay, reply(r)))
        });
        let state = build_state_with_runner(runner.url());
        let pending = tokio::spawn(async move {
            call(
                state,
                "/agent/scroll_find",
                json!({"label":"missing","max_swipes":2}),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), signal.notified())
            .await
            .unwrap();
        expire().await;
        assert!(
            pending.is_finished(),
            "in-flight mutation must not outlive the HTTP budget"
        );
        let (status, body) = pending.await.unwrap();
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
        assert_eq!(body["outcome"], "unknown");
        assert_eq!(body["native_mutations_confirmed"], 0);
        assert_eq!(body["native_mutations_uncertain"], 1);
        assert_eq!(body["retry_safe"], false);
        assert!(body.get("image").is_none());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.iter().filter(|r| r.contains("/source")).count(), 1);
        assert_eq!(
            calls
                .iter()
                .filter(|r| r.starts_with("POST /session/SESSION/actions"))
                .count(),
            1
        );
    });
}
#[test]
fn deadline_after_ack_retains_known_rows_but_never_claims_a_current_snapshot() {
    block(async {
        let signal = Arc::new(tokio::sync::Notify::new());
        let started = signal.clone();
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let runner = mock_runner(move |r, _| {
            let delay = if r.contains("/source") && count.fetch_add(1, Ordering::AcqRel) > 0 {
                started.notify_one();
                Duration::from_secs(10)
            } else {
                Duration::ZERO
            };
            Some((delay, reply(r)))
        });
        let state = build_state_with_runner(runner.url());
        state.runner_actionable.store(true, Ordering::Release);
        let worker = state.clone();
        let pending =
            tokio::spawn(
                async move { call(worker, "/agent/collect", json!({"max_pages":3})).await },
            );
        tokio::time::timeout(Duration::from_secs(3), signal.notified())
            .await
            .unwrap();
        expire().await;
        assert!(pending.is_finished());
        let (status, body) = pending.await.unwrap();
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
        assert_eq!(body["outcome"], "partial");
        assert_eq!(body["swipes"], 1);
        assert_eq!(body["native_mutations_confirmed"], 1);
        assert_eq!(body["native_mutations_uncertain"], 0);
        assert_eq!(body["rows"][0]["label"], "KNOWN_ROW");
        assert_eq!(body["complete"], false);
        assert_eq!(body["retry_safe"], false);
        assert!(body.get("snapshot").is_none());
        assert!(state.runner_actionable.load(Ordering::Acquire));
    });
}
#[test]
fn recovery_capture_cannot_extend_the_handler_deadline_or_replace_lookup_failure() {
    block(async {
        use base64::Engine as _;
        let signal = Arc::new(tokio::sync::Notify::new());
        let started = signal.clone();
        let png = base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("../../mcp/tests/fixtures/recovery.png"));
        let runner = mock_runner(move |r, _| {
            if r.contains("/screenshot") {
                started.notify_one();
                return Some((Duration::from_secs(10), json!({"value":png}).to_string()));
            }
            Some((Duration::ZERO, reply(r)))
        });
        let state = build_state_with_runner(runner.url());
        state.runner_actionable.store(true, Ordering::Release);
        let worker = state.clone();
        let pending = tokio::spawn(async move {
            call(
                worker,
                "/agent/scroll_find",
                json!({"label":"missing","max_swipes":0}),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), signal.notified())
            .await
            .unwrap();
        expire().await;
        assert!(pending.is_finished());
        let (status, body) = pending.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["error"], "element_not_found");
        assert_eq!(body["recovery"]["image_omitted"], "request_deadline");
        assert!(body.get("image").is_none());
        assert_eq!(body["native_mutations_uncertain"], 0);
        assert!(state.runner_actionable.load(Ordering::Acquire));
    });
}

#[test]
fn session_preparation_timeout_is_unsent_and_does_not_degrade_device_health() {
    block(async {
        let signal = Arc::new(tokio::sync::Notify::new());
        let started = signal.clone();
        let runner = mock_runner(move |r, _| {
            let delay = if r.starts_with("POST /session ") {
                started.notify_one();
                Duration::from_secs(10)
            } else {
                Duration::ZERO
            };
            Some((delay, reply(r)))
        });
        let state = build_state_with_runner(runner.url());
        state.runner_actionable.store(true, Ordering::Release);
        let worker = state.clone();
        let pending =
            tokio::spawn(
                async move { call(worker, "/agent/collect", json!({"max_pages":3})).await },
            );
        tokio::time::timeout(Duration::from_secs(3), signal.notified())
            .await
            .unwrap();
        expire().await;
        assert!(pending.is_finished());
        let (status, body) = pending.await.unwrap();
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
        assert_eq!(body["outcome"], "not_sent");
        assert_eq!(body["native_mutations_uncertain"], 0);
        assert_eq!(body["retry_safe"], true);
        assert!(state.runner_actionable.load(Ordering::Acquire));
    });
}

#[test]
fn deadline_checkpoint_waiting_for_publication_is_redacted_after_privacy_change() {
    block(async {
        let signal = Arc::new(tokio::sync::Notify::new());
        let started = signal.clone();
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let runner = mock_runner(move |r, _| {
            let delay = if r.contains("/source") && count.fetch_add(1, Ordering::AcqRel) > 0 {
                started.notify_one();
                Duration::from_secs(10)
            } else {
                Duration::ZERO
            };
            Some((delay, reply(r)))
        });
        let state = build_state_with_runner(runner.url());
        let worker = state.clone();
        let pending = tokio::spawn(async move {
            server::http::router(worker)
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/agent/collect")
                        .header("x-phone-control", "1")
                        .header("x-phone-owner", "deadline-fixture")
                        .header("content-type", "application/json")
                        .body(Body::from(json!({"max_pages":3}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(3), signal.notified())
            .await
            .unwrap();
        expire().await;
        assert!(pending.is_finished());
        let response = pending.await.unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            call(state, "/agent/hold", json!({"secs":60,"private":true}))
                .await
                .0,
            StatusCode::OK
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(!body.to_string().contains("KNOWN_ROW"));
        assert_eq!(body["observation_omitted"], true);
        assert_eq!(body["native_mutations_confirmed"], 1);
        assert_eq!(body["outcome"], "partial");
    });
}
