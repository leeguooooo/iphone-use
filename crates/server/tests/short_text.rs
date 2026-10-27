//! Agent short input must use one native bound transaction when supported. Failed native or
//! legacy clearing must never fall through to an unbound type, in single or batch entry points.
mod support;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;

async fn post(
    state: Arc<server::http::AppState>,
    batch: bool,
    action: Value,
) -> (StatusCode, Value) {
    let body = if batch {
        json!({"steps":[{"kind":"action","action":action},{"kind":"action","action":{"type":"home"}}]})
    } else {
        action
    };
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
                .header("x-phone-owner", "short-input-test")
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
fn wire_error(status: &str, message: &str) -> String {
    let body=json!({"value":{"error":if status.starts_with("400"){"invalid argument"}else{"unknown error"},"message":message}}).to_string();
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
#[test]
fn short_input_is_one_native_transaction_and_never_falls_back_after_failure() {
    block(async {
        for batch in [false, true] {
            for clear in [false, true] {
                for outcome in ["ok", "refused", "failed", "lost"] {
                    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
                    let seen = calls.clone();
                    let runner = mock_runner(move |request, _| {
                        seen.lock().unwrap().push(request.into());
                        if request.starts_with("POST /session ") {
                            return Some((Duration::ZERO,json!({"value":{"sessionId":"SESSION","capabilities":{"ipu:atomicText":true}}}).to_string()));
                        }
                        if request.contains("/ipu/text") {
                            let body: Value =
                                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap())
                                    .unwrap();
                            assert_eq!(body, json!({"text":"你好","clear":clear}));
                            return match outcome {
                                "refused" => Some((
                                    Duration::ZERO,
                                    wire_error(
                                        "400 Bad Request",
                                        "input_target_missing; nothing was sent",
                                    ),
                                )),
                                "failed" => Some((
                                    Duration::ZERO,
                                    wire_error(
                                        "500 Internal Server Error",
                                        "guarded clear incomplete",
                                    ),
                                )),
                                "lost" => None,
                                _ => Some((Duration::ZERO, json!({"value":null}).to_string())),
                            };
                        }
                        Some((Duration::ZERO, json!({"value":null}).to_string()))
                    });
                    let (status, body) = post(
                        build_state_with_runner(runner.url()),
                        batch,
                        json!({"type":"text","text":"你好","clear":clear}),
                    )
                    .await;
                    if outcome == "ok" {
                        assert_eq!(status, StatusCode::OK, "{body}");
                    } else {
                        assert!(!status.is_success(), "{body}");
                        assert_eq!(
                            body["outcome"],
                            if outcome == "refused" {
                                "not_sent"
                            } else {
                                "unknown"
                            },
                            "{body}"
                        );
                        assert_eq!(body["retry_safe"], outcome == "refused", "{body}");
                    }
                    let calls = calls.lock().unwrap();
                    assert_eq!(calls.iter().filter(|r| r.contains("/ipu/text")).count(), 1);
                    assert!(!calls.iter().any(|r| r.contains("/wda/keys")
                        || r.contains("/wda/input-target")
                        || r.contains("/clear")));
                    if outcome != "ok" {
                        assert!(!calls
                            .iter()
                            .any(|r| r.contains("/homescreen") || r.contains("/pressButton")));
                    }
                    drop(calls);
                    drop(runner);
                }
            }
        }
    });
}
#[test]
fn legacy_clear_failure_stops_both_short_and_long_input_without_typing() {
    block(async {
        for batch in [false, true] {
            for characters in [3, 500] {
                for outcome in ["ok", "clear-failed", "read-failed"] {
                    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
                    let seen = calls.clone();
                    let runner = mock_runner(move |request, _| {
                        seen.lock().unwrap().push(request.into());
                        let value = if request.starts_with("POST /session ") {
                            json!({"sessionId":"SESSION"})
                        } else if request.contains("/element/active") {
                            if outcome == "read-failed" {
                                return Some((
                                    Duration::ZERO,
                                    wire_error("404 Not Found", "no focused element"),
                                ));
                            }
                            json!({"ELEMENT":"original-field"})
                        } else if request.contains("/clear") && outcome == "clear-failed" {
                            return Some((
                                Duration::ZERO,
                                wire_error("500 Internal Server Error", "clear may be partial"),
                            ));
                        } else {
                            Value::Null
                        };
                        Some((Duration::ZERO, json!({"value":value}).to_string()))
                    });
                    let (status, body) = post(
                        build_state_with_runner(runner.url()),
                        batch,
                        json!({"type":"text","text":"字".repeat(characters),"clear":true}),
                    )
                    .await;
                    let calls = calls.lock().unwrap();
                    let keys = calls.iter().filter(|r| r.contains("/wda/keys")).count();
                    if outcome == "ok" {
                        assert_eq!(status, StatusCode::OK, "{body}");
                        assert_eq!(keys, if characters == 3 { 1 } else { 3 });
                    } else {
                        assert!(!status.is_success(), "{body}");
                        assert_eq!(keys, 0, "failed clear must not append text");
                        assert!(!calls
                            .iter()
                            .any(|r| r.contains("/homescreen") || r.contains("/pressButton")));
                    }
                    drop(calls);
                    drop(runner);
                }
            }
        }
    });
}
#[test]
fn multiline_intent_is_checked_before_long_text_can_clear_or_type() {
    block(async {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = calls.clone();
        let runner = mock_runner(move |request, _| {
            seen.lock().unwrap().push(request.into());
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO,json!({"value":{"sessionId":"SESSION","capabilities":{"ipu:guardedInput":true,"ipu:atomicText":true}}}).to_string()));
            }
            if request.contains("/wda/input-target") {
                assert!(request.contains("?multiline=1"));
                return Some((
                    Duration::ZERO,
                    wire_error(
                        "400 Bad Request",
                        "newline_requires_multiline_field; nothing was sent",
                    ),
                ));
            }
            Some((Duration::ZERO, json!({"value":null}).to_string()))
        });
        let (status, body) = post(
            build_state_with_runner(runner.url()),
            false,
            json!({"type":"text","text":format!("{}\nend","字".repeat(300)),"clear":true}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["outcome"], "not_sent");
        assert!(!calls
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains("/ipu/text") || r.contains("/wda/keys") || r.contains("/clear")));
    });
}
#[test]
fn atomic_capability_is_renegotiated_after_session_replacement() {
    block(async {
        let sessions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = sessions.clone();
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let records = calls.clone();
        let runner = mock_runner(move |request, _| {
            records.lock().unwrap().push(request.into());
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO,json!({"value":{"sessionId":"SESSION","capabilities":{"ipu:atomicText":seen.fetch_add(1,std::sync::atomic::Ordering::SeqCst)==0}}}).to_string()));
            }
            Some((Duration::ZERO, json!({"value":null}).to_string()))
        });
        let state = build_state_with_runner(runner.url());
        assert_eq!(
            post(state.clone(), false, json!({"type":"text","text":"a"}))
                .await
                .0,
            StatusCode::OK
        );
        state
            .runner
            .as_ref()
            .unwrap()
            .lock()
            .await
            .invalidate_session();
        assert_eq!(
            post(state, false, json!({"type":"text","text":"b"}))
                .await
                .0,
            StatusCode::OK
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.iter().filter(|r| r.contains("/ipu/text")).count(), 1);
        assert_eq!(calls.iter().filter(|r| r.contains("/wda/keys")).count(), 1);
    });
}
