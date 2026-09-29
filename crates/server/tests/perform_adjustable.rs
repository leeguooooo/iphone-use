//! `perform increment|decrement` on the two adjustable controls #57 fixed from
//! hardware findings, driven through the real router against a scripted WDA.
//!
//! The unit tests pin the pieces (`stepper_increment_is_second`, the force
//! press body); these pin what actually reaches WDA, end to end:
//! - a label-less PickerWheel (the stock Clock timer) still resolves by frame
//!   and moves ONE notch: `order` next/previous with `offset` 0.1, not 0.2;
//! - a Stepper on a non-English phone, whose child buttons are not labelled
//!   "Increment"/"Decrement", clicks the trailing child for increment and the
//!   leading one for decrement.

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
const WHEEL_RECT: [f64; 4] = [170.0, 197.0, 105.0, 292.0];
const LEADING_RECT: [f64; 4] = [250.0, 600.0, 47.0, 32.0];
const TRAILING_RECT: [f64; 4] = [297.0, 600.0, 47.0, 32.0];

/// A timer wheel with no label and no identifier (hardware shape, #57), and a
/// Stepper whose children carry Chinese labels.
fn tree() -> String {
    r#"{"value":{
        "type":"XCUIElementTypeApplication","label":"时钟",
        "rect":{"x":0,"y":0,"width":390,"height":844},
        "children":[
          {"type":"XCUIElementTypePickerWheel","label":"","value":"21 分钟",
           "rect":{"x":170,"y":197,"width":105,"height":292},"isEnabled":true,"children":[]},
          {"type":"XCUIElementTypeStepper","label":"数量",
           "rect":{"x":250,"y":600,"width":94,"height":32},"isEnabled":true,"children":[
             {"type":"XCUIElementTypeButton","label":"减少",
              "rect":{"x":250,"y":600,"width":47,"height":32},"isEnabled":true,"children":[]},
             {"type":"XCUIElementTypeButton","label":"增加",
              "rect":{"x":297,"y":600,"width":47,"height":32},"isEnabled":true,"children":[]}
          ]}
        ]
    }}"#
    .to_string()
}

fn ids(ids: &[&str]) -> String {
    let list: Vec<String> = ids
        .iter()
        .map(|id| format!(r#"{{"ELEMENT":"{id}"}}"#))
        .collect();
    format!(r#"{{"value":[{}]}}"#, list.join(","))
}

fn rect(r: [f64; 4]) -> String {
    format!(
        r#"{{"value":{{"x":{},"y":{},"width":{},"height":{}}}}}"#,
        r[0], r[1], r[2], r[3]
    )
}

/// Every mutation-shaped request WDA received, as `path body`.
type Seen = Arc<Mutex<Vec<String>>>;

fn scripted_wda(seen: Seen) -> support::MockWda {
    let child_queries = Arc::new(Mutex::new(0_usize));
    mock_wda(move |request, _| {
        let line = request.lines().next().unwrap_or("");
        let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
        let reply = |s: String| Some((Duration::ZERO, s));
        if line.starts_with("POST /session ") {
            return reply(SESSION.to_string());
        }
        if line.contains("/source") {
            return reply(tree());
        }
        // Top-level lookups: the Stepper resolves by its labelled predicate,
        // the label-less wheel only by the frame fallback's class chain.
        if line.starts_with("POST /session/SESSION/elements ") {
            if body.contains("PickerWheel") {
                return reply(ids(&["wheel-1"]));
            }
            if body.contains("Stepper") {
                return reply(ids(&["stepper-1"]));
            }
            return reply(ids(&[]));
        }
        // Children of the Stepper: the English label chain finds nothing on
        // this locale; the bare Button chain finds both halves.
        if line.starts_with("POST /session/SESSION/element/stepper-1/elements ") {
            let mut n = child_queries.lock().unwrap();
            *n += 1;
            return reply(if *n == 1 {
                ids(&[])
            } else {
                ids(&["btn-leading", "btn-trailing"])
            });
        }
        if line.starts_with("GET /session/SESSION/element/wheel-1/rect") {
            return reply(rect(WHEEL_RECT));
        }
        if line.starts_with("GET /session/SESSION/element/btn-leading/rect") {
            return reply(rect(LEADING_RECT));
        }
        if line.starts_with("GET /session/SESSION/element/btn-trailing/rect") {
            return reply(rect(TRAILING_RECT));
        }
        if line.contains("/click") || line.contains("/pickerwheel/") {
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            seen.lock().unwrap().push(format!("{path} {body}"));
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

/// Read the tree, then `perform` `action` on the first row of `kind`.
async fn perform_on(kind: &str, action: &str) -> Vec<String> {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let wda = scripted_wda(seen.clone());
    let state = build_state_with_wda(wda.url());
    let (status, elements) = request_json(&state, "GET", "/agent/elements", None).await;
    assert_eq!(status, StatusCode::OK, "{elements}");
    let snapshot = elements["snapshot"].as_str().unwrap().to_string();
    let index = elements["elements"]
        .as_array()
        .unwrap()
        .iter()
        .position(|row| row["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} row in {elements}"));
    let body = serde_json::json!({
        "type": "perform", "element": index, "snapshot": snapshot, "action": action,
    })
    .to_string();
    let (status, json) = request_json(&state, "POST", "/agent/input", Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "{kind} {action}: {json}");
    assert_eq!(json["ok"], true, "{kind} {action}: {json}");
    let seen = seen.lock().unwrap().clone();
    seen
}

fn assert_one_notch(seen: &[String], order: &str) {
    assert_eq!(seen.len(), 1, "exactly one wheel move: {seen:?}");
    let (path, body) = seen[0].split_once(' ').unwrap();
    assert_eq!(path, "/session/SESSION/wda/pickerwheel/wheel-1/select");
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["order"], order, "{body}");
    // 0.2 moved TWO notches on a stock timer wheel (21→23→25, #57).
    assert_eq!(body["offset"], 0.1, "{body}");
}

#[test]
fn a_label_less_picker_wheel_moves_one_notch_each_way() {
    block(async {
        assert_one_notch(&perform_on("PickerWheel", "increment").await, "next");
        assert_one_notch(&perform_on("PickerWheel", "decrement").await, "previous");
    });
}

#[test]
fn a_localized_stepper_clicks_the_trailing_half_to_increment() {
    block(async {
        let seen = perform_on("Stepper", "increment").await;
        assert_eq!(
            seen,
            vec!["/session/SESSION/element/btn-trailing/click {}".to_string()]
        );
        let seen = perform_on("Stepper", "decrement").await;
        assert_eq!(
            seen,
            vec!["/session/SESSION/element/btn-leading/click {}".to_string()]
        );
    });
}
