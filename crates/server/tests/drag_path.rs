//! Continuous drag paths are validated as a whole and dispatched once.
mod support;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;

fn path() -> serde_json::Value {
    serde_json::json!({"type":"drag","points":[
        {"x":0.3,"y":0.4},{"x":0.13,"y":0.5,"dwell_ms":900},{"x":1,"y":0.4}
    ],"hold_ms":400,"duration_ms":300})
}
async fn post_to(
    base: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    post_on_state(build_state_with_runner(base), uri, body).await
}
async fn post_on_state(
    state: Arc<server::http::AppState>, uri: &str, body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let app = server::http::router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("x-phone-control", "1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn post(base: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    post_to(base, "/agent/actions", body).await
}
fn batch(action: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"steps":[{"kind":"action","action":action}]})
}

#[test]
fn a_path_is_one_w3c_gesture_with_an_edge_pause_and_one_release() {
    block(async {
        for single in [false, true] {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let record = Arc::clone(&sent);
            let runner = mock_runner(move |request, _| {
                if request.starts_with("POST /session ") {
                    return Some((Duration::ZERO, r#"{"value":{"sessionId":"S"}}"#.into()));
                }
                if request.starts_with("GET /session/S ") {
                    return Some((
                        Duration::ZERO,
                        r#"{"value":{"capabilities":{"ipu:timedDragPath":true}}}"#.into(),
                    ));
                }
                if request.contains("/window/size") {
                    return Some((
                        Duration::ZERO,
                        r#"{"value":{"width":390,"height":844}}"#.into(),
                    ));
                }
                if request.starts_with("POST ") && request.contains("/actions ") {
                    let body: serde_json::Value =
                        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                    record.lock().unwrap().push(body);
                }
                Some((Duration::ZERO, r#"{"value":null}"#.into()))
            });
            let (status, body) = if single {
                post_to(runner.url(), "/agent/input", path()).await
            } else {
                post(runner.url(), batch(path())).await
            };
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["ok"], true);
            if !single {
                assert_eq!(body["applied_actions"], 1);
            }
            let sent = sent.lock().unwrap();
            assert_eq!(
                sent.len(),
                1,
                "one mutation, not a separate drag per segment"
            );
            let sources = sent[0]["actions"].as_array().unwrap();
            assert_eq!(sources.len(), 1);
            let actions = sources[0]["actions"].as_array().unwrap();
            assert_eq!(
                actions
                    .iter()
                    .filter(|s| s["type"] == "pointerDown")
                    .count(),
                1
            );
            assert_eq!(
                actions.iter().filter(|s| s["type"] == "pointerUp").count(),
                1
            );
            assert_eq!(
                actions[2],
                serde_json::json!({"type":"pause","duration":400})
            );
            assert_eq!(actions[3]["type"], "pointerMove");
            assert_eq!(
                actions[4],
                serde_json::json!({"type":"pause","duration":900})
            );
            assert_eq!(actions[5]["x"], 389.0, "edge stays inside the screen");
            assert_eq!(actions.last().unwrap()["type"], "pointerUp");
        }
    });
}

#[test]
fn an_invalid_path_later_in_a_batch_cannot_send_an_earlier_tap() {
    block(async {
        let calls = Arc::new(Mutex::new(0));
        let seen = Arc::clone(&calls);
        let runner = mock_runner(move |_, _| {
            *seen.lock().unwrap() += 1;
            Some((Duration::ZERO, r#"{"value":null}"#.into()))
        });
        for change in [
            serde_json::json!({"points":[]}),
            serde_json::json!({"points":null}),
            serde_json::json!({"x1":0.1}),
            serde_json::json!({"hold_ms":10000}),
            serde_json::json!({"points":[{"x":0.1,"y":0.2},{"x":1.1,"y":0.2}]}),
            serde_json::json!({"points":[{"x":0.1,"y":0.2},{"x":0.9,"y":0.2,"dwell_ms":-1}]}),
            serde_json::json!({"points":[{"x":0.1,"y":0.2},{"x":0.9,"y":0.2,"wait_ms":900}]}),
        ] {
            let mut invalid = path();
            for (key, value) in change.as_object().unwrap() {
                invalid[key] = value.clone();
            }
            let request = serde_json::json!({"steps":[
                {"kind":"action","action":{"type":"tap","x":0.5,"y":0.5}},
                {"kind":"action","action":invalid}
            ]});
            let (status, body) = post(runner.url(), request).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(body["batch_outcome"], "nothing_applied", "{body}");
            assert_eq!(body["retry_safe"], true, "{body}");
        }
        assert_eq!(
            *calls.lock().unwrap(),
            0,
            "all validation precedes all runner requests"
        );
    });
}

#[test]
fn losing_the_gesture_reply_does_not_replay_any_part_of_the_path() {
    block(async {
        let calls = Arc::new(Mutex::new(0));
        let sent = Arc::clone(&calls);
        let runner = mock_runner(move |request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, r#"{"value":{"sessionId":"S"}}"#.into()));
            }
            if request.starts_with("GET /session/S ") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":{"capabilities":{"ipu:timedDragPath":true}}}"#.into(),
                ));
            }
            if request.contains("/window/size") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":{"width":390,"height":844}}"#.into(),
                ));
            }
            if request.starts_with("POST ") && request.contains("/actions ") {
                *sent.lock().unwrap() += 1;
                return None;
            }
            Some((Duration::ZERO, r#"{"value":null}"#.into()))
        });
        let (status, body) = post(runner.url(), batch(path())).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["outcome"], "unknown");
        assert_eq!(body["retry_safe"], false);
        assert_eq!(*calls.lock().unwrap(), 1);
    });
}

#[test]
fn a_system_alert_blocks_the_whole_path_before_motion() {
    block(async {
        let calls = Arc::new(Mutex::new(0));
        let sent = Arc::clone(&calls);
        let runner = mock_runner(move |request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, r#"{"value":{"sessionId":"S"}}"#.into()));
            }
            if request.contains("/window/size") {
                return Some((Duration::ZERO,r#"{"value":{"width":390,"height":844}}"#.into()));
            }
            if request.contains("/alert/text") {
                return Some((Duration::ZERO, r#"{"value":"Permission?"}"#.into()));
            }
            if request.contains("/wda/alert/buttons") {
                return Some((Duration::ZERO, r#"{"value":["Allow","Cancel"]}"#.into()));
            }
            if request.starts_with("POST ") && request.contains("/actions ") {
                *sent.lock().unwrap() += 1;
            }
            Some((Duration::ZERO, r#"{"value":null}"#.into()))
        });
        // A live agent first probes status, which establishes the session.
        // Keep cold-session setup out of the short alert-only probe budget.
        let state=build_state_with_runner(runner.url());
        state.runner.as_ref().unwrap().lock().await.window_size().await.unwrap();
        let (status, body) = post_on_state(state,"/agent/actions",batch(path())).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "alert_present");
        assert_eq!(body["retry_safe"], true);
        assert_eq!(*calls.lock().unwrap(), 0);
    });
}

#[test]
fn old_runner_refuses_a_path_without_sending_a_gesture() {
    block(async {
        for single in [false, true] {
            for capability in [
                serde_json::json!({}),
                serde_json::json!({"ipu:timedDragPath":false}),
                serde_json::json!({"ipu:timedDragPath":"true"}),
            ] {
                let calls = Arc::new(Mutex::new(0));
                let sent = Arc::clone(&calls);
                let runner = mock_runner(move |request, _| {
                    if request.starts_with("POST /session ") {
                        return Some((Duration::ZERO, r#"{"value":{"sessionId":"S"}}"#.into()));
                    }
                    if request.starts_with("GET /session/S ") {
                        return Some((
                            Duration::ZERO,
                            serde_json::json!({"value":{"capabilities":capability}}).to_string(),
                        ));
                    }
                    if request.starts_with("POST ") && request.contains("/actions ") {
                        *sent.lock().unwrap() += 1;
                    }
                    Some((Duration::ZERO, r#"{"value":null}"#.into()))
                });
                let (status, body) = if single {
                    post_to(runner.url(), "/agent/input", path()).await
                } else {
                    post(runner.url(), batch(path())).await
                };
                assert_eq!(status, StatusCode::CONFLICT, "{body}");
                assert_eq!(body["error"], "runner_upgrade_required", "{body}");
                assert_eq!(body["outcome"], "not_sent", "{body}");
                assert_eq!(*calls.lock().unwrap(), 0);
            }
        }
    });
}

#[test]
fn browser_control_refuses_paths_before_any_runner_request() {
    block(async {
        let calls = Arc::new(Mutex::new(0));
        let seen = Arc::clone(&calls);
        let runner = mock_runner(move |_, _| {
            *seen.lock().unwrap() += 1;
            Some((Duration::ZERO, r#"{"value":null}"#.into()))
        });
        let mut action = path();
        action["ttl_ms"] = serde_json::json!(2500);
        action["points"][1]["dwell_ms"] = serde_json::json!(3000);
        let (status, body) = post_to(runner.url(), "/control", action).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"], "drag_path_requires_agent_endpoint");
        assert_eq!(body["outcome"], "not_sent");
        assert_eq!(*calls.lock().unwrap(), 0);
    });
}

#[test]
fn lost_capability_probe_is_not_misreported_as_an_upgrade() {
    block(async {
        for single in [false, true] {
            let calls = Arc::new(Mutex::new(0));
            let sent = Arc::clone(&calls);
            let runner = mock_runner(move |request, _| {
                if request.starts_with("POST /session ") {
                    return Some((Duration::ZERO, r#"{"value":{"sessionId":"S"}}"#.into()));
                }
                if request.starts_with("GET /session/S ") {
                    return None;
                }
                if request.starts_with("POST ") && request.contains("/actions ") {
                    *sent.lock().unwrap() += 1;
                }
                Some((Duration::ZERO, r#"{"value":null}"#.into()))
            });
            let (status, body) = if single {
                post_to(runner.url(), "/agent/input", path()).await
            } else {
                post(runner.url(), batch(path())).await
            };
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["error"], "runner_capability_unavailable");
            assert_eq!(body["outcome"], "not_sent");
            assert_eq!(*calls.lock().unwrap(), 0);
        }
    });
}
