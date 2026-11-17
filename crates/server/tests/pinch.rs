//! A pinch is two fingers moving apart or together around one centre,
//! dispatched once; invalid pinches send nothing.
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

fn gap(source_a: &serde_json::Value, source_b: &serde_json::Value, step: usize) -> f64 {
    let point = |s: &serde_json::Value| {
        let a = &s["actions"][step];
        (a["x"].as_f64().unwrap(), a["y"].as_f64().unwrap())
    };
    let (a, b) = (point(source_a), point(source_b));
    (a.0 - b.0).hypot(a.1 - b.1)
}

#[test]
fn pinch_single_and_batch_dispatch_two_fingers_once_at_the_requested_scale() {
    block(async {
        for (single, scale) in [(true, 0.7), (false, 0.7), (true, 1.4), (false, 1.4)] {
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
            let action =
                serde_json::json!({"type":"pinch","x":0.5,"y":0.5,"scale":scale,"duration_ms":600});
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
            assert_eq!(sent.len(), 1, "one gesture, one POST");
            let sources = sent[0]["actions"].as_array().unwrap();
            assert_eq!(sources.len(), 2);
            for source in sources {
                let steps = source["actions"].as_array().unwrap();
                assert_eq!(steps[1]["type"], "pointerDown");
                assert_eq!(steps[3]["type"], "pointerMove");
                assert_eq!(steps[3]["duration"], 600);
                assert_eq!(steps[5]["type"], "pointerUp");
            }
            // Step 0 is where the fingers go down, step 3 where they end.
            let ratio = gap(&sources[0], &sources[1], 3) / gap(&sources[0], &sources[1], 0);
            assert!((ratio - scale).abs() < 1e-6, "scale {scale}: ratio {ratio}");
            let mid_x = (sources[0]["actions"][3]["x"].as_f64().unwrap()
                + sources[1]["actions"][3]["x"].as_f64().unwrap())
                / 2.0;
            assert!((mid_x - 195.0).abs() < 1e-6, "centre moved to {mid_x}");
        }
    });
}

#[test]
fn invalid_pinches_send_no_runner_mutations() {
    block(async {
        let runner = mock_runner(|request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, r#"{"value":{"sessionId":"S"}}"#.into()));
            }
            if request.contains("/window/size") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":{"width":390,"height":844}}"#.into(),
                ));
            }
            assert!(
                !(request.starts_with("POST ") && request.contains("/actions ")),
                "invalid pinch dispatched: {request}"
            );
            Some((Duration::ZERO, r#"{"value":null}"#.into()))
        });
        for action in [
            serde_json::json!({"type":"pinch"}),
            serde_json::json!({"type":"pinch","scale":1.0}),
            serde_json::json!({"type":"pinch","scale":0.7,"x":1.3}),
            serde_json::json!({"type":"pinch","scale":0.7,"duration_ms":9000}),
            // Centre inside the left edge zone iOS keeps for back-swipe.
            serde_json::json!({"type":"pinch","scale":0.7,"x":0.03,"y":0.5}),
        ] {
            let (status, response) = post_to(runner.url(), "/agent/input", action.clone()).await;
            assert!(status.is_client_error(), "{action}: {status} {response}");
        }
    });
}
