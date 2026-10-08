//! A system alert that holds the foreground app makes every `/source` fail.
//! `/agent/elements` must name the alert at once instead of retrying for the
//! whole read budget (hardware: Xiaohongshu's "Allow Paste" — 109 failed
//! reads, 35 s, then a bare timeout).

mod support;

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state_with_wda, mock_wda};

#[test]
fn a_blocking_alert_is_named_instead_of_timing_out() {
    block(async {
        let wda = mock_wda(|request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, r#"{"value":{"sessionId":"SESSION"}}"#.to_string()));
            }
            if request.contains("/source") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":{"error":"unknown error","message":"snapshot failed"}}"#.to_string(),
                ));
            }
            if request.contains("/alert/text") {
                return Some((Duration::ZERO, r#"{"value":"“小红书”想要粘贴来自“备忘录”的内容"}"#.to_string()));
            }
            if request.contains("/wda/alert/buttons") {
                return Some((Duration::ZERO, r#"{"value":["不允许粘贴","允许粘贴"]}"#.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let state = build_state_with_wda(wda.url());
        let started = Instant::now();
        let response = server::http::router(state)
            .oneshot(Request::builder().uri("/agent/elements").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert_eq!(json["error"], "alert_blocking", "{json}");
        assert_eq!(json["alert"]["buttons"], serde_json::json!(["不允许粘贴", "允许粘贴"]));
        assert!(json["hint"].as_str().unwrap().contains("\"type\":\"alert\""));
        assert!(started.elapsed() < Duration::from_secs(5), "named at once, not after the read budget");
    });
}

/// A full runner response with the `X-IPU-Alert` answer beside the tree.
fn tree_with_alert_header(tree: &str, alert: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-IPU-Alert: {alert}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{tree}",
        tree.len()
    )
}

/// Hardware (17 Pro Max, GitHub's web sign-in sheet): a read of the sheet came
/// back with "no alert" beside it while an alert was up in another process, and
/// the `alert` action answered `no_alert`. A read's "no alert" may cover only
/// the processes that read searched, so an explicit alert action asks the
/// runner again (and the runner now searches every active process).
#[test]
fn an_alert_action_asks_again_after_a_read_said_no_alert() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    block(async {
        let asked = Arc::new(AtomicUsize::new(0));
        let pressed = Arc::new(AtomicUsize::new(0));
        let (asked_in, pressed_in) = (asked.clone(), pressed.clone());
        let tree = r#"{"value":{"type":"XCUIElementTypeApplication","label":"SafariViewService","rect":{"x":0,"y":0,"width":440,"height":956},"children":[{"type":"XCUIElementTypeButton","label":"关闭","rect":{"x":16,"y":65,"width":44,"height":44},"isEnabled":"1"}]}}"#;
        let wda = support::mock_native_runner(move |request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, r#"{"value":{"sessionId":"SESSION"}}"#.to_string()));
            }
            if request.contains("/source") {
                return Some((Duration::ZERO, tree_with_alert_header(tree, "0")));
            }
            if request.contains("/alert/text") {
                asked_in.fetch_add(1, Ordering::SeqCst);
                return Some((Duration::ZERO, r#"{"value":"github.com wants to open this page"}"#.to_string()));
            }
            if request.contains("/wda/alert/buttons") {
                return Some((Duration::ZERO, r#"{"value":["Cancel","Open"]}"#.to_string()));
            }
            if request.contains("/alert/accept") {
                pressed_in.fetch_add(1, Ordering::SeqCst);
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let state = build_state_with_wda(wda.url());
        let read = server::http::router(state.clone())
            .oneshot(Request::builder().uri("/agent/elements").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(read.status(), StatusCode::OK);
        let asked_by_read = asked.load(Ordering::SeqCst);

        let response = server::http::router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/input")
                    .header("x-phone-control", "1")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"type":"alert","button":"Open"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true, "{json}");
        assert!(
            asked.load(Ordering::SeqCst) > asked_by_read,
            "the action asked /alert/text again: {json}"
        );
        assert_eq!(pressed.load(Ordering::SeqCst), 1, "the named button was pressed");
    });
}
