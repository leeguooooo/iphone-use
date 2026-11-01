//! A strict locator must keep its identity when the daemon re-resolves the
//! target live. A source precheck cannot protect a later query that drops it.
mod support;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;

const TREE: &str = r#"{"value":{
    "type":"XCUIElementTypeApplication","label":"Test app",
    "rect":{"x":0,"y":0,"width":390,"height":844},"children":[
        {"type":"XCUIElementTypeButton","label":"Open","rawIdentifier":"target-row",
         "rect":{"x":20,"y":100,"width":350,"height":40}},
        {"type":"XCUIElementTypeButton","label":"Open","rawIdentifier":"other-row",
         "rect":{"x":20,"y":200,"width":350,"height":40}}
    ]}}"#;

#[derive(Clone, Copy)]
enum LiveScreen {
    Both,
    TargetGone,
    DuplicateIdentity,
    LabelMimicsIdentifier,
}

async fn tap_on(
    screen: LiveScreen,
    identifier_only: bool,
) -> (StatusCode, serde_json::Value, Vec<String>) {
    let identity = match screen {
        LiveScreen::LabelMimicsIdentifier => "Open",
        _ => "target-row",
    };
    let source = TREE.replace("target-row", identity);
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let sent = Arc::clone(&clicks);
    let runner = mock_runner(move |request, _| {
        if request.starts_with("POST /session ") {
            return Some((Duration::ZERO, r#"{"value":{"sessionId":"S"}}"#.into()));
        }
        if request.contains("/source?format=json") {
            return Some((Duration::ZERO, source.clone()));
        }
        if request.starts_with("POST ") && request.contains("/elements ") {
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").expect("query body").1)
                    .unwrap();
            let predicate = body["value"].as_str().expect("predicate");
            // Both controls have the requested kind and label. Only the
            // actual identifier distinguishes them; name can fall back to label.
            let live = match screen {
                LiveScreen::Both => vec![("TARGET", Some(identity)), ("OTHER", Some("other-row"))],
                LiveScreen::TargetGone => vec![("OTHER", Some("other-row"))],
                LiveScreen::DuplicateIdentity => {
                    vec![("TARGET", Some(identity)), ("OTHER", Some(identity))]
                }
                LiveScreen::LabelMimicsIdentifier => vec![("OTHER", None)],
            };
            let found: Vec<_> = live
                .into_iter()
                .filter(|(_, identifier)| {
                    !predicate.contains(&format!("identifier == '{identity}'"))
                        || *identifier == Some(identity)
                })
                .map(|(id, _)| serde_json::json!({"ELEMENT":id}))
                .collect();
            return Some((
                Duration::ZERO,
                serde_json::json!({"value":found}).to_string(),
            ));
        }
        if request.contains("/element/TARGET/rect") {
            return Some((Duration::ZERO, r#"{"value":{"x":20,"y":100,"width":350,"height":40}}"#.into()));
        }
        if request.contains("/window/size") {
            return Some((Duration::ZERO, r#"{"value":{"width":390,"height":844}}"#.into()));
        }
        if request.starts_with("POST ") && request.contains("/ipu/tap ") {
            sent.lock()
                .unwrap()
                .push(request.lines().next().unwrap().to_string());
        }
        Some((Duration::ZERO, r#"{"value":null}"#.into()))
    });
    let app = server::http::router(build_state_with_runner(runner.url()));
    let locator = if identifier_only {
        serde_json::json!({"identifier":identity})
    } else {
        serde_json::json!({"kind":"Button","label":"Open","identifier":identity})
    };
    let action = serde_json::json!({"steps":[{"kind":"action","action":{
        "type":"tap_locator","locator":locator
    }}]});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agent/actions")
                .header("x-phone-control", "1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(action.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap();
    let sent = clicks.lock().unwrap().clone();
    (status, body, sent)
}

#[test]
fn an_identifier_disambiguates_same_label_buttons_in_the_live_query() {
    block(async {
        let (status, body, clicks) = tap_on(LiveScreen::Both, false).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["applied_actions"], 1);
        assert_eq!(clicks, ["POST /session/S/element/TARGET/ipu/tap HTTP/1.1"]);
    });
}

#[test]
fn a_target_that_disappeared_cannot_be_replaced_by_a_same_label_button() {
    block(async {
        let (status, body, clicks) = tap_on(LiveScreen::TargetGone, false).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"], "element_not_found");
        assert_eq!(body["applied_actions"], 0);
        assert_eq!(body["retry_safe"], true);
        assert!(
            clicks.is_empty(),
            "must not click the remaining namesake: {clicks:?}"
        );
    });
}

#[test]
fn a_duplicate_identifier_is_still_ambiguous() {
    block(async {
        let (status, body, clicks) = tap_on(LiveScreen::DuplicateIdentity, false).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"], "ambiguous_element_label");
        assert_eq!(body["applied_actions"], 0);
        assert!(clicks.is_empty());
    });
}

#[test]
fn a_label_cannot_impersonate_an_identifier_in_a_compound_locator() {
    block(async {
        let (status, body, clicks) = tap_on(LiveScreen::LabelMimicsIdentifier, false).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"], "element_not_found");
        assert_eq!(body["applied_actions"], 0);
        assert!(
            clicks.is_empty(),
            "a name fallback is not an identifier: {clicks:?}"
        );
    });
}

#[test]
fn an_identifier_only_locator_cannot_fall_back_to_a_label() {
    block(async {
        let (status, body, clicks) = tap_on(LiveScreen::LabelMimicsIdentifier, true).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"], "element_not_found");
        assert_eq!(body["applied_actions"], 0);
        assert!(
            clicks.is_empty(),
            "a name fallback is not an identifier: {clicks:?}"
        );
    });
}
