//! Failure-image enrichment preserves the action verdict and uses fresh capture only within
//! the original ownership/privacy scope. The synthetic runner cannot control a personal phone.
mod support;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use base64::Engine as _;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;
const TREE: &str = r#"{"value":{"type":"XCUIElementTypeApplication","label":"Fixture","rect":{"x":0,"y":0,"width":390,"height":844},"children":[]}}"#;
async fn call(
    state: Arc<server::http::AppState>,
    batch: bool,
    disabled: bool,
) -> (StatusCode, Value) {
    let action = json!({"type":"tap_locator","locator":{"label":"missing"}});
    let body = if batch {
        json!({"steps":[{"kind":"action","action":action},{"kind":"action","action":{"type":"home"}}]})
    } else {
        action
    };
    let mut req = Request::builder()
        .method("POST")
        .uri(if batch {
            "/agent/actions"
        } else {
            "/agent/input"
        })
        .header("x-phone-control", "1")
        .header("x-phone-owner", "recovery-test")
        .header(header::CONTENT_TYPE, "application/json");
    if disabled {
        req = req.header("x-phone-recovery-image", "none");
    }
    let response = server::http::router(state)
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[test]
fn visual_failure_has_one_fresh_png_and_keeps_single_and_batch_verdicts() {
    block(async {
        for batch in [false, true] {
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let png = include_bytes!("../../mcp/tests/fixtures/recovery.png").to_vec();
            let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
            let runner = mock_runner(move |request, _| {
                seen.lock().unwrap().push(request.into());
                let body = if request.starts_with("POST /session ") {
                    json!({"value":{"sessionId":"SESSION"}}).to_string()
                } else if request.contains("/source") {
                    TREE.into()
                } else if request.contains("/elements") {
                    json!({"value":[]}).to_string()
                } else if request.contains("/screenshot") {
                    json!({"value":b64}).to_string()
                } else {
                    json!({"value":null}).to_string()
                };
                Some((Duration::ZERO, body))
            });
            let state = build_state_with_runner(runner.url());
            state
                .runner
                .as_ref()
                .unwrap()
                .lock()
                .await
                .remember_settled_frame(server::runner::Capture::full_png(
                    include_bytes!("../../mcp/tests/fixtures/recovery-old.png").to_vec(),
                ));
            let (status, body) = call(state, batch, false).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(body["error"], "element_not_found", "{body}");
            assert_eq!(body["outcome"], "not_sent");
            assert_eq!(body["retry_safe"], true);
            assert_eq!(body["recovery"]["replay_action"], false);
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(body["image"]["png_base64"].as_str().unwrap())
                .unwrap();
            assert_eq!(decoded, png);
            assert_eq!(body["image"]["pixel_size"], json!([16, 16]));
            assert!(
                body["image"]["received_at_ms"].as_u64().unwrap()
                    >= body["image"]["requested_at_ms"].as_u64().unwrap()
            );
            if batch {
                assert_eq!(body["completed"], 0);
                assert_eq!(body["failed_step"], 0);
                assert_eq!(body["batch_outcome"], "nothing_applied");
            }
            let calls = calls.lock().unwrap();
            assert_eq!(
                calls.iter().filter(|r| r.contains("/screenshot")).count(),
                1
            );
            assert!(!calls.iter().any(|r| r.contains("/wda/keys")
                || r.contains("/homescreen")
                || r.contains("/pressButton")));
        }
    });
}
#[test]
fn unavailable_or_slow_capture_never_replaces_the_original_failure() {
    block(async {
        for slow in [false, true] {
            let runner = mock_runner(move |request, _| {
                if request.starts_with("POST /session ") {
                    return Some((
                        Duration::ZERO,
                        json!({"value":{"sessionId":"SESSION"}}).to_string(),
                    ));
                }
                if request.contains("/source") {
                    return Some((Duration::ZERO, TREE.into()));
                }
                if request.contains("/elements") {
                    return Some((Duration::ZERO, json!({"value":[]}).to_string()));
                }
                if request.contains("/screenshot") {
                    return if slow {
                        Some((Duration::from_secs(5), json!({"value":null}).to_string()))
                    } else {
                        None
                    };
                }
                Some((Duration::ZERO, json!({"value":null}).to_string()))
            });
            let start = Instant::now();
            let (status, body) = call(build_state_with_runner(runner.url()), false, false).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(body["error"], "element_not_found");
            assert_eq!(body["outcome"], "not_sent");
            assert_eq!(body["retry_safe"], true);
            assert!(body.get("image").is_none());
            assert_eq!(body["image_unavailable"], true);
            assert!(start.elapsed() < Duration::from_secs(4));
        }
    });
}
#[test]
fn opt_out_privacy_and_ownership_handoff_withhold_images() {
    block(async {
        for mode in ["off", "private", "handoff", "privacy-toggle"] {
            let shots = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let seen = shots.clone();
            let shared = Arc::new(Mutex::new(None::<Arc<server::http::AppState>>));
            let current = shared.clone();
            let png = base64::engine::general_purpose::STANDARD
                .encode(include_bytes!("../../mcp/tests/fixtures/recovery.png"));
            let runner = mock_runner(move |request, _| {
                let body = if request.starts_with("POST /session ") {
                    json!({"value":{"sessionId":"SESSION"}}).to_string()
                } else if request.contains("/source") {
                    TREE.into()
                } else if request.contains("/elements") {
                    json!({"value":[]}).to_string()
                } else if request.contains("/screenshot") {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if mode == "handoff" || mode == "privacy-toggle" {
                        let state = current.lock().unwrap().as_ref().unwrap().clone();
                        if mode == "handoff" {
                            state
                                .owner_epoch
                                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                        } else {
                            state
                                .privacy_epoch
                                .fetch_add(2, std::sync::atomic::Ordering::AcqRel);
                        }
                    }
                    json!({"value":png}).to_string()
                } else {
                    json!({"value":null}).to_string()
                };
                Some((Duration::ZERO, body))
            });
            let state = build_state_with_runner(runner.url());
            *shared.lock().unwrap() = Some(state.clone());
            if mode == "private" {
                *state.private_until.lock().unwrap() =
                    Some(Instant::now() + Duration::from_millis(500));
            }
            let (status, body) = call(state, false, mode == "off").await;
            // Private agent input is now refused before even resolving a
            // locator. Later handoff/capture races keep the original 422.
            assert_eq!(status, if mode=="private" {StatusCode::CONFLICT} else {StatusCode::UNPROCESSABLE_ENTITY}, "{body}");
            if mode=="private" {assert_eq!(body["error"],"input_privacy_changed");}
            assert!(body.get("image").is_none());
            assert_eq!(body["outcome"], "not_sent");
            assert_eq!(
                shots.load(std::sync::atomic::Ordering::SeqCst),
                if mode == "handoff" || mode == "privacy-toggle" {
                    1
                } else {
                    0
                }
            );
        }
    });
}

#[test]
fn a_tiny_image_budget_keeps_the_error_and_omits_pixels_in_an_isolated_process() {
    if std::env::var_os("RECOVERY_BUDGET_TEST_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_tiny_image_budget_keeps_the_error_and_omits_pixels_in_an_isolated_process",
                "--nocapture",
            ])
            .env("RECOVERY_BUDGET_TEST_CHILD", "1")
            .env("IPHONE_USE_IMAGE_BUDGET_BYTES", "512")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    block(async {
        let shots = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = shots.clone();
        let png = base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("../../mcp/tests/fixtures/recovery.png"));
        let runner = mock_runner(move |request, _| {
            let body = if request.starts_with("POST /session ") {
                json!({"value":{"sessionId":"SESSION"}}).to_string()
            } else if request.contains("/source") {
                TREE.into()
            } else if request.contains("/elements") {
                json!({"value":[]}).to_string()
            } else if request.contains("/screenshot") {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                json!({"value":png}).to_string()
            } else {
                json!({"value":null}).to_string()
            };
            Some((Duration::ZERO, body))
        });
        let (status, body) = call(build_state_with_runner(runner.url()), false, false).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "element_not_found");
        assert_eq!(body["outcome"], "not_sent");
        assert_eq!(body["retry_safe"], true);
        assert_eq!(body["image_omitted"], "too_large_for_response_budget");
        assert!(body.get("image").is_none());
        assert_eq!(shots.load(std::sync::atomic::Ordering::SeqCst), 3);
    });
}
