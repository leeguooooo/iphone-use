//! Owner-set limits (`IPHONE_USE_BLOCKED_APPS`, `IPHONE_USE_MAX_ACTIONS`)
//! over HTTP, and the plain-language activity feed that records what ran.
//!
//! The policy is read from the environment once per process, so this binary
//! holds a single test that sets it first.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state_with_wda, mock_wda_with_apps};

const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;

async fn call(
    state: &Arc<server::http::AppState>,
    method: &str,
    uri: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-phone-control", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = server::http::router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[test]
fn blocked_apps_and_the_run_budget_refuse_before_anything_is_sent() {
    std::env::set_var("IPHONE_USE_BLOCKED_APPS", "com.example.bank");
    std::env::set_var("IPHONE_USE_MAX_ACTIONS", "4");
    std::env::set_var("IPHONE_USE_FLOWS_NO_SUGGEST", "1");

    block(async {
        let sent: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = sent.clone();
        // The blocked app is in front for the whole test.
        let wda = mock_wda_with_apps(r#"[{"bundleId":"com.example.bank"}]"#, move |request, _| {
            log.lock()
                .unwrap()
                .push(request.lines().next().unwrap_or("").to_string());
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let state = build_state_with_wda(wda.url());
        let launches = |sent: &Arc<Mutex<Vec<String>>>| {
            sent.lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains("/apps/launch"))
                .count()
        };

        let (status, caps) = call(&state, "GET", "/agent/capabilities", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            caps["limits"]["blocked_apps"][0]["bundle"], "com.example.bank",
            "{caps}"
        );
        assert_eq!(caps["limits"]["max_actions_per_run"], 4, "{caps}");

        // Opening a blocked app is refused without reaching the phone.
        let (status, json) = call(
            &state,
            "POST",
            "/agent/input",
            r#"{"type":"launch_app","bundle":"com.example.bank"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["error"], "blocked_app");
        assert_eq!(json["outcome"], "not_sent");
        assert_eq!(launches(&sent), 0);

        // Inside it, anything but leaving is refused.
        let (status, json) = call(
            &state,
            "POST",
            "/agent/input",
            r#"{"type":"key","name":"return"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["error"], "blocked_app");
        assert!(json["hint"].as_str().unwrap().contains("home"), "{json}");

        // Reading a list swipes inside it: refused too.
        let (status, json) = call(&state, "POST", "/agent/collect", "").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["error"], "blocked_app", "{json}");

        // Leaving is allowed, and is the first action charged.
        let (status, json) = call(&state, "POST", "/agent/input", r#"{"type":"home"}"#).await;
        assert_eq!(status, StatusCode::OK, "{json}");

        // A batch: the launch runs, the next step finds the blocked app still
        // in front (this phone never leaves it) and stops there.
        let (status, json) = call(
            &state,
            "POST",
            "/agent/actions",
            r#"{"steps":[
                {"kind":"action","action":{"type":"launch_app","bundle":"com.example.notes"}},
                {"kind":"action","action":{"type":"key","name":"return"}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["error"], "blocked_app", "{json}");
        assert_eq!(json["failed_step"], 1, "{json}");
        assert_eq!(json["failed_step_outcome"], "not_sent", "{json}");
        assert_eq!(launches(&sent), 1);

        // 1 + 2 of 4 used: a two-step batch would pass the budget.
        let (status, json) = call(
            &state,
            "POST",
            "/agent/actions",
            r#"{"steps":[{"kind":"action","action":{"type":"home"}},
                         {"kind":"action","action":{"type":"home"}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["error"], "action_budget_exhausted", "{json}");
        assert_eq!(json["batch_outcome"], "nothing_applied", "{json}");
        assert_eq!(
            json["budget"],
            serde_json::json!({"max": 4, "used": 3}),
            "{json}"
        );

        // The feed shows what ran, in plain words; refused actions are absent,
        // applied steps of a stopped batch are not.
        let (status, feed) = call(&state, "GET", "/agent/activity", "").await;
        assert_eq!(status, StatusCode::OK, "{feed}");
        let texts: Vec<&str> = feed["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect();
        // The batch's launch applied before its next step was refused.
        assert_eq!(
            texts,
            vec!["Went to the Home Screen", "Opened com.example.notes"],
            "{feed}"
        );
        assert_eq!(feed["items"][0]["text_zh"], "回到主屏幕", "{feed}");
        assert_eq!(
            feed["budget"],
            serde_json::json!({"max": 4, "used": 3}),
            "{feed}"
        );
        let next = feed["next"].as_u64().unwrap();
        let (_, later) = call(&state, "GET", &format!("/agent/activity?since={next}"), "").await;
        assert_eq!(later["items"], serde_json::json!([]), "{later}");
    });
}
