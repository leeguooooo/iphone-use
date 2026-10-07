//! Point-sized coordinates on a coordinate action.
//!
//! Found on hardware: `{"type":"tap","x":82.5,"y":738}` (points, not
//! fractions) came back as 503 `wda_unavailable_or_unsupported`, which reads as
//! "the phone is down" and sends an agent off to reconnect a phone that is
//! fine. It is the caller's mistake, nothing was sent, and the answer must say
//! how to fix it.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state_with_wda, mock_wda};

const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;
const WINDOW: &str = r#"{"value":{"width":390,"height":844}}"#;

async fn post_input(base: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let app = server::http::router(build_state_with_wda(base));
    let request = Request::builder()
        .method("POST")
        .uri("/agent/input")
        .header("x-phone-control", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

fn scripted(seen: Arc<Mutex<Vec<String>>>) -> support::MockWda {
    mock_wda(move |request, _| {
        seen.lock().unwrap().push(request.lines().next().unwrap_or("").to_string());
        if request.starts_with("POST /session ") {
            return Some((Duration::ZERO, SESSION.to_string()));
        }
        if request.contains("/window/size") || request.contains("/window/rect") {
            return Some((Duration::ZERO, WINDOW.to_string()));
        }
        Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
    })
}

fn sent_a_gesture(seen: &Arc<Mutex<Vec<String>>>) -> bool {
    seen.lock().unwrap().iter().any(|line| line.contains("/actions"))
}

#[test]
fn point_sized_coordinates_are_the_callers_mistake_not_an_unavailable_phone() {
    block(async {
        for body in [
            r#"{"type":"tap","x":82.5,"y":738}"#,
            r#"{"type":"longpress","x":0.5,"y":1.4}"#,
            r#"{"type":"swipe","x1":0.5,"y1":0.8,"x2":195,"y2":100}"#,
            r#"{"type":"scroll","x":200,"y":0.5,"dy":100}"#,
        ] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let wda = scripted(seen.clone());
            let (status, json) = post_input(wda.url(), body).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {json}");
            assert_eq!(json["error"], "invalid_value", "{body}: {json}");
            assert_eq!(json["outcome"], "not_sent", "{body}: {json}");
            let hint = json["hint"].as_str().unwrap_or_default();
            assert!(hint.contains("fractions of the screen"), "{body}: {json}");
            assert!(!sent_a_gesture(&seen), "{body} must not reach the phone");
        }
    });
}

#[test]
fn fractional_coordinates_still_tap() {
    block(async {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let wda = scripted(seen.clone());
        let (status, json) = post_input(wda.url(), r#"{"type":"tap","x":0.21,"y":0.87}"#).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(sent_a_gesture(&seen), "an in-range tap reaches the phone");
    });
}
