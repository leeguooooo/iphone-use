//! Per-app notes over HTTP (`/agent/app-notes`), and their excerpt in the
//! `registry` block an agent gets when it enters the app.

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state, build_state_with_runner, mock_runner};

async fn call(
    state: &std::sync::Arc<server::http::AppState>,
    method: &str,
    uri: &str,
    control: bool,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "text/markdown");
    if control {
        request = request.header("x-phone-control", "1");
    }
    let response = server::http::router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn launch(bundle: &str) -> String {
    format!(r#"{{"steps":[{{"kind":"action","action":{{"type":"launch_app","bundle":"{bundle}"}}}}]}}"#)
}

#[test]
fn notes_are_read_written_and_shown_when_the_agent_enters_the_app() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("flows");
    std::fs::create_dir(&store).unwrap();
    std::fs::write(
        store.join(".index.json"),
        r#"{"version":1,
            "flows":{"notes/new":{"source":"official","sha256":"x","name":"New note","steps":3,
                     "app":"com.example.notes","risk":"navigation"}},
            "apps":[{"id":"notes","bundle":"com.example.notes","name":"Notes","aliases":["Notes"]}]}"#,
    )
    .unwrap();
    std::env::set_var("IPHONE_USE_FLOWS_DIR", &store);
    std::env::set_var("IPHONE_USE_FLOWS_NO_SUGGEST", "1");

    block(async {
        let locked = build_state(Some("pw"));
        let (status, _) = call(&locked, "GET", "/agent/app-notes?bundle=com.example.notes", false, "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let state = build_state(None);
        let uri = "/agent/app-notes?bundle=com.example.notes";
        let (status, json) = call(&state, "GET", uri, false, "").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json, serde_json::json!({"ok": true, "bundle": "com.example.notes", "text": ""}));
        for bad in ["/agent/app-notes", "/agent/app-notes?bundle=..%2Fx", "/agent/app-notes?bundle=a%20b"] {
            let (status, json) = call(&state, "GET", bad, false, "").await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {json}");
            assert_eq!(json["error"], "invalid_bundle", "{bad}");
        }

        let text = "# Notes\n- The compose button is top right.\n- Never tap Delete All.\n";
        let (status, json) = call(&state, "PUT", uri, false, text).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["error"], "missing_control_header");
        let (status, json) = call(&state, "PUT", uri, true, text).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        // Beside the flow store, not inside it.
        let file = root.path().join("app-notes").join("com.example.notes.md");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        let (_, json) = call(&state, "GET", uri, false, "").await;
        assert_eq!(json["text"], text);
        assert!(json["updated_at"].is_u64(), "{json}");

        let (status, json) = call(&state, "PUT", uri, true, &"x".repeat(16 * 1024 + 1)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{json}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "kept");

        // An app without flows gets a registry block for its notes alone.
        let other = "/agent/app-notes?bundle=com.example.other";
        let (status, _) = call(&state, "PUT", other, true, "Log in with the test account.").await;
        assert_eq!(status, StatusCode::OK);

        let runner = mock_runner(|request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, r#"{"value":{"sessionId":"SESSION"}}"#.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let driving = build_state_with_runner(runner.url());
        let (status, json) = call(&driving, "POST", "/agent/actions", true, &launch("com.example.notes")).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["registry"]["flows"][0]["id"], "notes/new", "{json}");
        assert_eq!(json["registry"]["app_notes"], text.trim(), "{json}");
        assert!(json["registry"]["app_notes_hint"].as_str().unwrap().contains("/agent/app-notes"));

        let (_, json) = call(&driving, "POST", "/agent/actions", true, &launch("com.example.other")).await;
        assert_eq!(json["registry"]["app_notes"], "Log in with the test account.", "{json}");

        // Empty text deletes.
        let (status, _) = call(&state, "PUT", other, true, "").await;
        assert_eq!(status, StatusCode::OK);
        let (_, json) = call(&state, "GET", other, false, "").await;
        assert_eq!(json["text"], "");
        assert!(json.get("updated_at").is_none(), "{json}");
    });
}
