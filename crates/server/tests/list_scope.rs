//! List reads/swipes must stay in the entry owner/privacy scope.
mod support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::{
    sync::{atomic::Ordering, Arc, Mutex},
    time::Duration,
};
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;
const TREE: &str = r#"{"value":{"type":"XCUIElementTypeApplication","label":"Fixture","rect":{"x":0,"y":0,"width":390,"height":844},"children":[{"type":"XCUIElementTypeCell","label":"PRIVATE_ROW","rect":{"x":0,"y":100,"width":390,"height":60},"isVisible":"1","isEnabled":"1"}]}}"#;
async fn response(
    state: Arc<server::http::AppState>,
    path: &str,
    body: Value,
    owner: &str,
    takeover: bool,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("x-phone-control", "1")
        .header("x-phone-owner", owner)
        .header("content-type", "application/json");
    if takeover {
        request = request.header("x-phone-owner-takeover", "1");
    }
    server::http::router(state)
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}
async fn call(state: Arc<server::http::AppState>, path: &str, body: Value) -> (StatusCode, Value) {
    let result = response(state, path, body, "list-a", false).await;
    let status = result.status();
    let bytes = result.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn transition(state: Arc<server::http::AppState>, owner: bool) {
    if owner {
        assert_eq!(
            response(
                state,
                "/agent/input",
                json!({"type":"text","text":"invalid","budget_ms":1}),
                "list-b",
                true
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    } else {
        for body in [json!({"secs":60,"private":true}), json!({"secs":0})] {
            assert_eq!(
                call(state.clone(), "/agent/hold", body).await.0,
                StatusCode::OK
            );
        }
    }
}
fn payload(find: bool) -> (&'static str, Value) {
    if find {
        (
            "/agent/scroll_find",
            json!({"label":"missing","max_swipes":1}),
        )
    } else {
        ("/agent/collect", json!({"max_pages":3}))
    }
}
fn reply(request: &str) -> String {
    if request.starts_with("POST /session ") {
        json!({"value":{"sessionId":"SESSION"}}).to_string()
    } else if request.contains("/source") {
        TREE.into()
    } else if request.contains("/window/") {
        json!({"value":{"width":390,"height":844}}).to_string()
    } else {
        json!({"value":null}).to_string()
    }
}
#[test]
fn private_list_refuses_before_any_runner_request() {
    block(async {
        for find in [false, true] {
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let runner = mock_runner(move |r, _| {
                seen.lock().unwrap().push(r.into());
                Some((Duration::ZERO, reply(r)))
            });
            let state = build_state_with_runner(runner.url());
            assert_eq!(
                call(
                    state.clone(),
                    "/agent/hold",
                    json!({"secs":60,"private":true})
                )
                .await
                .0,
                StatusCode::OK
            );
            let (path, body) = payload(find);
            let (status, value) = call(state, path, body).await;
            assert_eq!(status, StatusCode::LOCKED, "{value}");
            assert_eq!(value["outcome"], "not_sent");
            assert!(calls.lock().unwrap().is_empty());
        }
    });
}
#[test]
fn source_in_flight_is_discarded_without_late_swipe() {
    block(async {
        for find in [false, true] {
            for owner in [false, true] {
                let started = Arc::new(tokio::sync::Notify::new());
                let signal = started.clone();
                let calls = Arc::new(Mutex::new(Vec::<String>::new()));
                let seen = calls.clone();
                let runner = mock_runner(move |r, _| {
                    seen.lock().unwrap().push(r.into());
                    let delay = if r.contains("/source") {
                        signal.notify_one();
                        Duration::from_millis(500)
                    } else {
                        Duration::ZERO
                    };
                    Some((delay, reply(r)))
                });
                let state = build_state_with_runner(runner.url());
                state.runner_actionable.store(true, Ordering::Release);
                let worker = state.clone();
                let (path, body) = payload(find);
                let pending = tokio::spawn(async move { call(worker, path, body).await });
                tokio::time::timeout(Duration::from_secs(3), started.notified())
                    .await
                    .unwrap();
                transition(state.clone(), owner).await;
                let (_, value) = pending.await.unwrap();
                assert!(!value.to_string().contains("PRIVATE_ROW"), "{value}");
                assert_eq!(value["outcome"], "not_sent", "{value}");
                assert!(state.runner_actionable.load(Ordering::Acquire));
                assert!(!calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.starts_with("POST /session/SESSION/actions")));
            }
        }
    });
}
#[test]
fn acknowledged_swipe_is_preserved_without_post_scope_reads() {
    block(async {
        for find in [false, true] {
            let started = Arc::new(tokio::sync::Notify::new());
            let signal = started.clone();
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let runner = mock_runner(move |r, _| {
                seen.lock().unwrap().push(r.into());
                let delay = if r.starts_with("POST /session/SESSION/actions") {
                    signal.notify_one();
                    Duration::from_millis(500)
                } else {
                    Duration::ZERO
                };
                Some((delay, reply(r)))
            });
            let state = build_state_with_runner(runner.url());
            state.runner_actionable.store(true, Ordering::Release);
            let worker = state.clone();
            let (path, body) = payload(find);
            let pending = tokio::spawn(async move { call(worker, path, body).await });
            tokio::time::timeout(Duration::from_secs(3), started.notified())
                .await
                .unwrap();
            transition(state.clone(), false).await;
            let (_, value) = pending.await.unwrap();
            assert_eq!(value["outcome"], "partial", "{value}");
            assert_eq!(value["swipes"], 1);
            assert_eq!(value["native_mutations_confirmed"], 1);
            assert_eq!(value["native_mutations_uncertain"], 0);
            assert_eq!(value["retry_safe"], false);
            assert!(!value.to_string().contains("PRIVATE_ROW"));
            assert!(state.runner_actionable.load(Ordering::Acquire));
            let calls = calls.lock().unwrap();
            assert_eq!(calls.iter().filter(|r| r.contains("/source")).count(), 1);
            assert_eq!(
                calls
                    .iter()
                    .filter(|r| r.starts_with("POST /session/SESSION/actions"))
                    .count(),
                1
            );
        }
    });
}
#[test]
fn queued_body_withholds_list_ui_after_private_transition() {
    block(async {
        for find in [false, true] {
            let runner = mock_runner(|r, _| Some((Duration::ZERO, reply(r))));
            let state = build_state_with_runner(runner.url());
            let (path, body) = if find {
                (
                    "/agent/scroll_find",
                    json!({"label":"PRIVATE_ROW","max_swipes":0}),
                )
            } else {
                ("/agent/collect", json!({"max_pages":1}))
            };
            let result = response(state.clone(), path, body, "list-a", false).await;
            assert_eq!(result.status(), StatusCode::OK);
            transition(state, false).await;
            let bytes = result.into_body().collect().await.unwrap().to_bytes();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(!value.to_string().contains("PRIVATE_ROW"), "{value}");
            assert_eq!(value["observation_omitted"], true);
            assert_eq!(value["native_mutations_confirmed"], 0);
        }
    });
}

#[test]
fn queued_list_cannot_reclaim_an_owner_after_a_to_b_to_a() {
    block(async {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = calls.clone();
        let runner = mock_runner(move |r, _| {
            seen.lock().unwrap().push(r.into());
            Some((Duration::ZERO, reply(r)))
        });
        let state = build_state_with_runner(runner.url());
        let runner_lock = state.runner.as_ref().unwrap().lock().await;
        let before = state.owner_epoch.load(Ordering::Acquire);
        let worker = state.clone();
        let pending = tokio::spawn(async move {
            call(worker, "/agent/scroll_find", json!({"label":"missing"})).await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while state.owner_epoch.load(Ordering::Acquire) == before {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for owner in ["list-b", "list-a"] {
            assert_eq!(
                response(
                    state.clone(),
                    "/agent/input",
                    json!({"type":"text","text":"invalid","budget_ms":1}),
                    owner,
                    true
                )
                .await
                .status(),
                StatusCode::BAD_REQUEST
            );
        }
        let epoch = state.owner_epoch.load(Ordering::Acquire);
        let (status, body) = tokio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["outcome"], "not_sent");
        assert_eq!(body["native_mutations_confirmed"], 0);
        assert_eq!(body["swipes"], 0);
        drop(runner_lock);
        assert_eq!(state.owner_epoch.load(Ordering::Acquire), epoch);
        assert!(calls.lock().unwrap().is_empty());
    });
}
