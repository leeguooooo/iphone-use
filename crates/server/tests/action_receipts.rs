//! Router-level receipts and opt-in run isolation; no physical device needed.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use server as srv;
use server::http::AppState;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
include!("fixtures/app_state.rs");

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

async fn request(
    app: axum::Router,
    path: &str,
    owner: &str,
    value: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().uri(path).header("x-phone-owner", owner);
    let body = if let Some(value) = value {
        request = request
            .method("POST")
            .header("x-phone-control", "1")
            .header("content-type", "application/json");
        Body::from(value.to_string())
    } else {
        Body::empty()
    };
    let response = app.oneshot(request.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[test]
fn trace_is_opt_in_isolated_and_available_after_end() {
    block(async {
        let state = fixture_app_state(None);
        let app = srv::http::router(state.clone());
        let (status, _) = request(
            app.clone(),
            "/agent/run",
            "receipt-test",
            Some(serde_json::json!({"action":"start","run_id":"receipt-run","trace":true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        state.action_traces.lock().unwrap().record(
            "receipt-test",
            &serde_json::json!({"type":"text","text":"private payload"}),
            &serde_json::json!({"ok":true,"transport":"runner"}),
            10,
            false,
        );
        let (_, trace) = request(
            app.clone(),
            "/agent/trace?run_id=receipt-run",
            "receipt-test",
            None,
        )
        .await;
        assert_eq!(trace["trace"]["events"].as_array().unwrap().len(), 1);
        assert!(!trace.to_string().contains("private payload"));
        let (status, _) = request(
            app.clone(),
            "/agent/trace?run_id=receipt-run",
            "another-owner",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = request(
            app.clone(),
            "/agent/run",
            "receipt-test",
            Some(serde_json::json!({"action":"end","run_id":"receipt-run"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, trace) = request(
            app.clone(),
            "/agent/trace?run_id=receipt-run",
            "receipt-test",
            None,
        )
        .await;
        assert_eq!(trace["trace"]["active"], false);
        let (_, _) = request(
            app.clone(),
            "/agent/run",
            "receipt-no-trace",
            Some(serde_json::json!({"action":"start","run_id":"no-trace"})),
        )
        .await;
        assert!(state
            .action_traces
            .lock()
            .unwrap()
            .export("receipt-no-trace", "no-trace")
            .is_none());
    });
}

#[test]
fn subtree_rejects_bad_bounds_before_contacting_a_runner() {
    block(async {
        let app = srv::http::router(fixture_app_state(None));
        let (status, body) = request(
            app,
            "/agent/subtree",
            "subtree-test",
            Some(serde_json::json!({"snapshot":"s","element":0,"max_depth":0})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_subtree_bounds");
    });
}

#[test]
fn rejected_batch_has_one_summary_and_no_invented_actions() {
    block(async {
        let state = fixture_app_state(None);
        let app = srv::http::router(state.clone());
        let (status, _) = request(
            app.clone(),
            "/agent/run",
            "batch-trace-test",
            Some(serde_json::json!({"action":"start","run_id":"rejected-batch","trace":true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = request(
            app,
            "/agent/actions",
            "batch-trace-test",
            Some(serde_json::json!({"steps":[{"kind":"action","action":{"type":"unsupported"}}]})),
        )
        .await;
        assert!(!status.is_success());
        let trace = state
            .action_traces
            .lock()
            .unwrap()
            .export("batch-trace-test", "rejected-batch")
            .unwrap();
        let events = trace["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["action"]["kind"], "batch");
    });
}

#[test]
fn trace_export_requires_the_configured_credentials() {
    block(async {
        let state = fixture_app_state(Some("trace-test-password"));
        assert!(state
            .action_traces
            .lock()
            .unwrap()
            .start("protected-owner", "protected-run"));
        let app = srv::http::router(state);
        let (status, _) = request(
            app,
            "/agent/trace?run_id=protected-run",
            "protected-owner",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    });
}

#[test]
fn subtree_route_reads_the_selected_native_scope_and_keeps_screen_refs() {
    block(async {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = calls.clone();
        let mock = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let calls = observed.clone();
            async move {
                let path = request.uri().path().to_string();
                calls.lock().unwrap().push(path.clone());
                let (value, truncated) = match path.as_str() {
                    "/session" => (serde_json::json!({"sessionId":"S","capabilities":{"ipu:elementSubtree":true}}), false),
                    "/session/S/appium/settings" => (serde_json::Value::Null, false),
                    "/session/S/elements" => (serde_json::json!([{"ELEMENT":"ROOT"}]), false),
                    "/session/S/element/ROOT/ipu/subtree" => (serde_json::json!({"type":"XCUIElementTypeCell","label":"Section","children":[{"type":"XCUIElementTypeButton","label":"Inside"}]}), true),
                    _ => panic!("unexpected subtree runner request: {path}"),
                };
                axum::response::IntoResponse::into_response((
                    [("X-IPU-Tree-Complete", "1"), ("X-IPU-Truncated", if truncated {"1"} else {"0"})],
                    axum::Json(serde_json::json!({"value":value})),
                ))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });
        let mut state = fixture_app_state(None);
        Arc::get_mut(&mut state).unwrap().runner = Some(Arc::new(tokio::sync::Mutex::new(
            srv::runner::RunnerClient::new(base).unwrap(),
        )));
        state.element_snapshots.lock().unwrap().push_back((
            "cached-screen".into(),
            Arc::new(vec![srv::runner::ElementRow {
                kind: "Cell".into(),
                label: "Section".into(),
                ..Default::default()
            }]),
        ));
        let (status, body) = request(
            srv::http::router(state.clone()),
            "/agent/subtree",
            "subtree-route",
            Some(serde_json::json!({"snapshot":"cached-screen","element":0})),
        )
        .await;
        task.abort();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["action_refs"], false);
        assert_eq!(body["complete"], false);
        assert_eq!(body["coverage"], "partial");
        assert!(body.get("snapshot").is_none());
        assert_eq!(state.element_snapshots.lock().unwrap().len(), 1);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "/session",
                "/session/S/appium/settings",
                "/session/S/elements",
                "/session/S/element/ROOT/ipu/subtree"
            ]
        );
    });
}
