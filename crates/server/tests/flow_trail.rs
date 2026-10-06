//! Flow discovery and drafting over HTTP: what an agent driving the phone with
//! plain requests learns about the registry without being told to look.

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state_with_wda, mock_wda};

const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;

async fn call(
    state: &std::sync::Arc<server::http::AppState>,
    method: &str,
    uri: &str,
    body: &str,
    flow_run: bool,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-phone-control", "1")
        .header(header::CONTENT_TYPE, "application/json");
    if flow_run {
        request = request.header("x-phone-flow-run", "notes/new");
    }
    let response = server::http::router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

fn batch(bundle: &str, homes: usize) -> String {
    let mut steps = vec![format!(
        r#"{{"kind":"action","action":{{"type":"launch_app","bundle":"{bundle}"}}}}"#
    )];
    steps.extend((0..homes).map(|_| r#"{"kind":"action","action":{"type":"key","name":"return"}}"#.to_string()));
    format!(r#"{{"steps":[{}]}}"#, steps.join(","))
}

#[test]
fn launches_name_flows_offer_a_draft_and_flow_runs_stay_out_of_it() {
    let store = tempfile::tempdir().unwrap();
    std::fs::write(
        store.path().join(".index.json"),
        r#"{"version":1,
            "flows":{"notes/new":{"source":"official","sha256":"x","name":"New note","steps":3,
                     "app":"com.example.notes","risk":"navigation","inputs":["body"]}},
            "apps":[{"id":"notes","bundle":"com.example.notes","name":"Notes","aliases":["Notes"]}]}"#,
    )
    .unwrap();
    std::env::set_var("IPHONE_USE_FLOWS_DIR", store.path());
    std::env::remove_var("IPHONE_USE_FLOWS_NO_SUGGEST");

    block(async {
        let wda = mock_wda(|request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let state = build_state_with_wda(wda.url());

        // Entering an app that has flows names them in the same response.
        let (status, json) = call(&state, "POST", "/agent/actions", &batch("com.example.notes", 0), false).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["registry"]["flows"][0]["id"], "notes/new", "{json}");
        assert!(json.get("flow_suggestion").is_none(), "{json}");

        // A flow run's batch never becomes a suggestion of itself.
        let (_, json) = call(&state, "POST", "/agent/actions", &batch("com.example.notes", 6), true).await;
        assert!(json.get("registry").is_none() && json.get("flow_suggestion").is_none(), "{json}");

        // Driving an app with no flow for long enough suggests saving one, once.
        let (_, json) = call(&state, "POST", "/agent/actions", &batch("com.example.todo", 5), false).await;
        assert_eq!(json["registry"]["flows"], serde_json::json!([]), "{json}");
        assert_eq!(json["flow_suggestion"]["key"], "com.example.todo", "{json}");
        assert!(json["flow_suggestion"]["message"].as_str().unwrap().contains("ask the user"));
        let (_, json) = call(&state, "POST", "/agent/input", r#"{"type":"home"}"#, false).await;
        assert!(json.get("flow_suggestion").is_none(), "one-shot: {json}");

        // The draft is the trail, ready to validate.
        let (status, draft) = call(&state, "GET", "/agent/flow/draft", "", false).await;
        assert_eq!(status, StatusCode::OK, "{draft}");
        assert_eq!(draft["flow"]["version"], 1);
        assert_eq!(draft["flow"]["app"], "com.example.todo");
        let steps = draft["flow"]["steps"].as_array().unwrap();
        assert_eq!(steps[0], serde_json::json!({"kind":"launch_app","bundle":"com.example.todo"}));
        assert_eq!(steps.len(), 6, "{draft}");
        // The trailing Home from /agent/input above is tidying up, not the task.
        assert!(steps[1..].iter().all(|s| s == &serde_json::json!({"kind":"key","name":"return"})), "{draft}");

        // The skill's reference ships inside the daemon (the installer ships SKILL.md only).
        let response = server::http::router(state.clone())
            .oneshot(Request::builder().uri("/agent/reference").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let text = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&text).contains("## Phone states and recovery"));
    });
}
