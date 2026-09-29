//! `perform force_press` on an iPhone without 3D Touch (issue #90).
//!
//! WDA checks `XCUIDevice.supportsPressureInteraction` before it synthesizes
//! any touch and answers 400 "Force press is not supported on this device".
//! That is a refusal, not a lost acknowledgement: the daemon must say nothing
//! was sent (`retry_safe:true`) and point at `menu`, instead of the
//! `outcome_unknown` / `retry_safe:false` it used to return.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use server::http::AppState;
use support::{block, build_state_with_wda, mock_wda};

const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;

/// WDA 9.15.3's answer, captured verbatim from an iPhone 17 Pro Max on iOS 27.0.
const REFUSAL: &str = r#"{
  "value" : {
    "error" : "invalid element state",
    "message" : "Error Domain=com.facebook.WebDriverAgent Code=1 \"Force press is not supported on this device\" UserInfo={NSLocalizedDescription=Force press is not supported on this device}",
    "traceback" : ""
  },
  "sessionId" : "SESSION"
}"#;

fn tree() -> String {
    r#"{"value":{
        "type":"XCUIElementTypeApplication","label":"邮件",
        "rect":{"x":0,"y":0,"width":440,"height":956},
        "children":[
          {"type":"XCUIElementTypeCell","label":"Apple, Your receipt",
           "rect":{"x":0,"y":200,"width":440,"height":96},"isEnabled":true,"children":[]}
        ]
    }}"#
    .to_string()
}

fn http_400(body: &str) -> String {
    format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn scripted_wda(force_touches: Arc<Mutex<usize>>) -> support::MockWda {
    mock_wda(move |request, _| {
        let line = request.lines().next().unwrap_or("");
        let reply = |s: String| Some((Duration::ZERO, s));
        if line.starts_with("POST /session ") {
            return reply(SESSION.to_string());
        }
        if line.contains("/source") {
            return reply(tree());
        }
        if line.starts_with("POST /session/SESSION/elements ") {
            return reply(r#"{"value":[{"ELEMENT":"cell-1"}]}"#.to_string());
        }
        if line.starts_with("POST /session/SESSION/wda/element/cell-1/forceTouch ") {
            *force_touches.lock().unwrap() += 1;
            return reply(http_400(REFUSAL));
        }
        reply(r#"{"value":null}"#.to_string())
    })
}

async fn request_json(
    state: &Arc<AppState>,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let app = server::http::router(state.clone());
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-phone-control", "1");
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let request = builder
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn force_press(extra: serde_json::Value) -> (StatusCode, serde_json::Value, usize) {
    let force_touches = Arc::new(Mutex::new(0_usize));
    let wda = scripted_wda(force_touches.clone());
    let state = build_state_with_wda(wda.url());
    let (status, elements) = request_json(&state, "GET", "/agent/elements", None).await;
    assert_eq!(status, StatusCode::OK, "{elements}");
    let snapshot = elements["snapshot"].as_str().unwrap().to_string();
    let index = elements["elements"]
        .as_array()
        .unwrap()
        .iter()
        .position(|row| row["kind"] == "Cell")
        .unwrap_or_else(|| panic!("no Cell row in {elements}"));
    let mut body = serde_json::json!({
        "type": "perform", "element": index, "snapshot": snapshot, "action": "force_press",
    });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    let (status, json) =
        request_json(&state, "POST", "/agent/input", Some(&body.to_string())).await;
    let sent = *force_touches.lock().unwrap();
    (status, json, sent)
}

fn assert_unsupported(status: StatusCode, json: &serde_json::Value) {
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    assert_eq!(json["ok"], false, "{json}");
    assert_eq!(json["error"], "force_press_unsupported", "{json}");
    assert_eq!(json["outcome"], "not_sent", "{json}");
    assert_eq!(json["retry_safe"], true, "{json}");
    let hint = json["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("pressure"), "hint must say why: {json}");
    assert!(hint.contains("\"menu\""), "hint must name the alternative: {json}");
}

#[test]
fn force_press_without_3d_touch_is_a_clear_not_sent_error() {
    block(async {
        let (status, json, sent) = force_press(serde_json::json!({})).await;
        assert_eq!(sent, 1, "the request must have reached WDA once");
        assert_unsupported(status, &json);
    });
}

#[test]
fn explicit_pressure_and_duration_get_the_same_answer() {
    block(async {
        let (status, json, _) =
            force_press(serde_json::json!({"pressure": 0.8, "duration_ms": 900})).await;
        assert_unsupported(status, &json);
    });
}

#[test]
fn delta_mode_reports_the_same_error() {
    block(async {
        let force_touches = Arc::new(Mutex::new(0_usize));
        let wda = scripted_wda(force_touches);
        let state = build_state_with_wda(wda.url());
        let (_, elements) = request_json(&state, "GET", "/agent/elements", None).await;
        let snapshot = elements["snapshot"].as_str().unwrap();
        let index = elements["elements"]
            .as_array()
            .unwrap()
            .iter()
            .position(|row| row["kind"] == "Cell")
            .unwrap();
        let body = serde_json::json!({
            "type": "perform", "element": index, "snapshot": snapshot, "action": "force_press",
        });
        let (status, json) = request_json(
            &state,
            "POST",
            "/agent/input?return=delta",
            Some(&body.to_string()),
        )
        .await;
        assert_unsupported(status, &json);
    });
}
