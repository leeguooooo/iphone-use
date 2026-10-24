//! Continuous drag paths are validated as a whole and dispatched once.
mod support;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;

async fn post_to(
    base: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let app = server::http::router(build_state_with_runner(base));
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
#[test]
fn multi_hold_single_and_batch_dispatch_three_simultaneous_sources_once() {
    block(async {
        for single in [true, false] {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let record = Arc::clone(&sent);
            let runner = mock_runner(move |request, _| {
                if request.starts_with("POST /session ") {
                    return Some((Duration::ZERO, r#"{"value":{"sessionId":"S"}}"#.into()));
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
            let action = serde_json::json!({"type":"multi_longpress", "points":[
                {"x":0.3,"y":0.4},{"x":0.5,"y":0.4},{"x":0.7,"y":0.4}],"duration_ms":2500});
            let (path, body) = if single {
                ("/agent/input", action)
            } else {
                (
                    "/agent/actions",
                    serde_json::json!({"steps":[{"kind":"action","action":action}]}),
                )
            };
            let (status, response) = post_to(runner.url(), path, body).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            let sent = sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            let sources = sent[0]["actions"].as_array().unwrap();
            assert_eq!(sources.len(), 3);
            for (i, source) in sources.iter().enumerate() {
                assert_eq!(source["id"], format!("finger{}", i + 1));
                assert_eq!(source["actions"][0]["duration"], 0);
                assert_eq!(source["actions"][1]["type"], "pointerDown");
                assert_eq!(source["actions"][2]["duration"], 2500);
                assert_eq!(source["actions"][3]["type"], "pointerUp");
            }
        }
    });
}

#[test]
fn invalid_finger_requests_send_no_runner_mutations() {
    block(async {
        let runner = mock_runner(|request, _| {
            assert!(
                !request.starts_with("POST "),
                "invalid gesture dispatched: {request}"
            );
            Some((Duration::ZERO, r#"{"value":null}"#.into()))
        });
        for action in [
            serde_json::json!({"type":"multi_longpress","points":[]}),
            serde_json::json!({"type":"multi_longpress","points":[{"x":0.3,"y":0.4},{"x":0.3,"y":0.4}]}),
            serde_json::json!({"type":"multi_longpress","points":[{"x":0.3,"y":0.4},{"x":1.2,"y":0.4}]}),
            serde_json::json!({"type":"longpress","x":0.5,"y":0.5,"fingers":3}),
        ] {
            for single in [true, false] {
                let (path, body) = if single {
                    ("/agent/input", action.clone())
                } else {
                    (
                        "/agent/actions",
                        serde_json::json!({"steps":[{"kind":"action","action":action}]}),
                    )
                };
                let (status, _) = post_to(runner.url(), path, body).await;
                assert!(status.is_client_error());
            }
        }
    });
}
