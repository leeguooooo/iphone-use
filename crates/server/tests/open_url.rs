//! `{"type":"open_url","url":…}` on `/agent/input` and `/agent/actions`: a
//! web link reaches runner's `POST /session/:sid/url`; a scheme the owner has not
//! allowed (`IPHONE_USE_ALLOW_URL_SCHEMES`, unset here) is refused with 403
//! before anything is sent.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state_with_runner, mock_runner};

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
fn web_links_open_and_other_schemes_never_reach_the_phone() {
    std::env::set_var("IPHONE_USE_FLOWS_NO_SUGGEST", "1");
    block(async {
        let opened: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = opened.clone();
        let runner = mock_runner(move |request, _| {
            if request.starts_with("POST /session ") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":{"sessionId":"SESSION"}}"#.to_string(),
                ));
            }
            if request.starts_with("POST /session/SESSION/url ") {
                let body = request.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                log.lock().unwrap().push(body.to_string());
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let state = build_state_with_runner(runner.url());
        let bodies = || opened.lock().unwrap().clone();

        let (status, json) = call(
            &state,
            "POST",
            "/agent/input",
            r#"{"type":"open_url","url":"https://example.com/reset?token=SECRET"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let sent = bodies();
        assert_eq!(sent.len(), 1, "{sent:?}");
        let body: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"url": "https://example.com/reset?token=SECRET"})
        );

        // tel: dials: refused, nothing sent, the hint names the setting.
        let (status, json) = call(
            &state,
            "POST",
            "/agent/input",
            r#"{"type":"open_url","url":"tel:10086"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["error"], "url_scheme_not_allowed", "{json}");
        assert_eq!(json["outcome"], "not_sent", "{json}");
        assert!(
            json["hint"]
                .as_str()
                .unwrap()
                .contains("IPHONE_USE_ALLOW_URL_SCHEMES"),
            "{json}"
        );

        // A batch with a refused link runs no step at all.
        let (status, json) = call(
            &state,
            "POST",
            "/agent/actions",
            r#"{"steps":[{"kind":"action","action":{"type":"home"}},
                         {"kind":"action","action":{"type":"open_url","url":"weixin://dl/scan"}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["error"], "url_scheme_not_allowed", "{json}");
        assert_eq!(json["batch_outcome"], "nothing_applied", "{json}");

        // Malformed: 400 before the phone is touched.
        let (status, json) = call(
            &state,
            "POST",
            "/agent/input",
            r#"{"type":"open_url","url":"example.com"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(json["outcome"], "not_sent", "{json}");
        assert_eq!(bodies().len(), 1, "only the web link reached runner");

        // A batch step opens it too.
        let (status, json) = call(
            &state,
            "POST",
            "/agent/actions",
            r#"{"steps":[{"kind":"action","action":{"type":"open_url","url":"http://example.org"}}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(bodies().len(), 2);

        // The feed names the host only.
        let (_, feed) = call(&state, "GET", "/agent/activity", "").await;
        let texts: Vec<&str> = feed["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect();
        assert_eq!(
            texts,
            vec![
                "Opened a link to example.com",
                "Opened a link to example.org"
            ],
            "{feed}"
        );
        assert!(!feed.to_string().contains("SECRET"), "{feed}");
    });
}
