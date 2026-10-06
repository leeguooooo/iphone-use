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
