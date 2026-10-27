//! Scope loss between physical requests must not authorize a later mutation.
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
async fn request(
    state: Arc<server::http::AppState>,
    path: &str,
    payload: Value,
    owner: &str,
    takeover: bool,
) -> (StatusCode, Value) {
    let mut r = Request::builder()
        .method("POST")
        .uri(path)
        .header("x-phone-control", "1")
        .header("x-phone-owner", owner)
        .header("content-type", "application/json");
    if takeover {
        r = r.header("x-phone-owner-takeover", "1");
    }
    let response = server::http::router(state)
        .oneshot(r.body(Body::from(payload.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let data = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&data).unwrap())
}
async fn transition(state: Arc<server::http::AppState>, owner: bool) {
    if owner {
        assert_eq!(
            request(
                state,
                "/agent/input",
                json!({"type":"text","text":"invalid","budget_ms":1}),
                "b",
                true
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    } else {
        for payload in [json!({"secs":60,"private":true}), json!({"secs":0})] {
            assert_eq!(
                request(state.clone(), "/agent/hold", payload, "a", false)
                    .await
                    .0,
                StatusCode::OK
            );
        }
    }
}
#[test]
fn scope_change_during_session_setup_never_sends_home() {
    block(async {
        for owner in [false, true] {
            let signal = Arc::new(tokio::sync::Notify::new());
            let started = signal.clone();
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let runner = mock_runner(move |r, _| {
                seen.lock().unwrap().push(r.into());
                if r.starts_with("POST /session ") {
                    started.notify_one();
                    return Some((
                        Duration::from_millis(500),
                        json!({"value":{"sessionId":"SESSION"}}).to_string(),
                    ));
                }
                Some((Duration::ZERO, json!({"value":null}).to_string()))
            });
            let state = build_state_with_runner(runner.url());
            let c = state.clone();
            let pending = tokio::spawn(async move {
                request(c, "/agent/input", json!({"type":"home"}), "a", false).await
            });
            tokio::time::timeout(Duration::from_secs(3), signal.notified())
                .await
                .unwrap();
            transition(state, owner).await;
            let (status, value) = pending.await.unwrap();
            assert_eq!(status, StatusCode::CONFLICT, "{value}");
            assert_eq!(value["outcome"], "not_sent");
            assert!(
                !calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.contains("/wda/pressButton")),
                "home must not follow revoked session discovery"
            );
        }
    });
}
#[test]
fn acknowledged_termination_survives_but_fresh_launch_is_not_sent_after_scope_loss() {
    block(async {
        for owner in [false, true] {
            for batch in [false, true] {
                let signal = Arc::new(tokio::sync::Notify::new());
                let started = signal.clone();
                let calls = Arc::new(Mutex::new(Vec::<String>::new()));
                let seen = calls.clone();
                let runner = mock_runner(move |r, _| {
                    seen.lock().unwrap().push(r.into());
                    if r.starts_with("POST /session ") {
                        return Some((
                            Duration::ZERO,
                            json!({"value":{"sessionId":"SESSION"}}).to_string(),
                        ));
                    }
                    if r.contains("/wda/apps/terminate") {
                        started.notify_one();
                        return Some((
                            Duration::from_millis(500),
                            json!({"value":true}).to_string(),
                        ));
                    }
                    Some((Duration::ZERO, json!({"value":null}).to_string()))
                });
                let state = build_state_with_runner(runner.url());
                state
                    .runner_actionable
                    .store(true, std::sync::atomic::Ordering::Release);
                let c = state.clone();
                let pending = tokio::spawn(async move {
                    let action =
                        json!({"type":"launch_app","bundle":"com.example.fixture","fresh":true});
                    let payload = if batch {
                        json!({"observe":false,"steps":[{"kind":"action","action":action},{"kind":"action","action":{"type":"home"}}]})
                    } else {
                        action
                    };
                    request(
                        c,
                        if batch {
                            "/agent/actions"
                        } else {
                            "/agent/input"
                        },
                        payload,
                        "a",
                        false,
                    )
                    .await
                });
                tokio::time::timeout(Duration::from_secs(3), signal.notified())
                    .await
                    .unwrap();
                transition(state.clone(), owner).await;
                let (_, value) = pending.await.unwrap();
                assert_eq!(
                    value["outcome"], "partial",
                    "the termination was acknowledged: {value}"
                );
                assert_eq!(value["native_mutations_confirmed"], 1);
                assert_eq!(value["native_mutations_uncertain"], 0);
                assert_eq!(value["retry_safe"], false);
                if batch {
                    assert_eq!(value["completed"], 0);
                    assert_eq!(value["applied_actions"], 0);
                    assert_eq!(value["batch_outcome"], "partially_applied");
                }
                assert!(
                    state
                        .runner_actionable
                        .load(std::sync::atomic::Ordering::Acquire),
                    "permission loss must not mark a healthy device broken"
                );
                let calls = calls.lock().unwrap();
                assert_eq!(
                    calls
                        .iter()
                        .filter(|r| r.contains("/wda/apps/terminate"))
                        .count(),
                    1
                );
                assert!(!calls.iter().any(|r| r.contains("/wda/apps/launch")));
            }
        }
    });
}
#[test]
fn one_in_flight_native_ack_is_not_downgraded_by_scope_loss() {
    block(async {
        let signal = Arc::new(tokio::sync::Notify::new());
        let started = signal.clone();
        let runner = mock_runner(move |r, _| {
            if r.starts_with("POST /session ") {
                return Some((
                    Duration::ZERO,
                    json!({"value":{"sessionId":"SESSION"}}).to_string(),
                ));
            }
            if r.contains("/wda/pressButton") {
                started.notify_one();
                return Some((
                    Duration::from_millis(500),
                    json!({"value":null}).to_string(),
                ));
            }
            Some((Duration::ZERO, json!({"value":null}).to_string()))
        });
        let state = build_state_with_runner(runner.url());
        let c = state.clone();
        let pending = tokio::spawn(async move {
            request(c, "/agent/input", json!({"type":"home"}), "a", false).await
        });
        tokio::time::timeout(Duration::from_secs(3), signal.notified())
            .await
            .unwrap();
        transition(state, false).await;
        let (status, value) = pending.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["ok"], true);
    });
}
