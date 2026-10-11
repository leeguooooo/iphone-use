//! `GET /agent/elements?query=…&limit=…`: a filter of one fresh read that
//! keeps each row's original `index` and the full read's `snapshot`, so a tap
//! by element against the filtered view hits the same row.

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state_with_wda, mock_wda};

const TREE: &str = r#"{"value":{"type":"XCUIElementTypeApplication","label":"设置","rect":{"x":0,"y":0,"width":440,"height":956},"children":[
{"type":"XCUIElementTypeButton","label":"Sign In","rect":{"x":20,"y":100,"width":200,"height":44},"isEnabled":"1"},
{"type":"XCUIElementTypeTextField","label":"","placeholderValue":"Email address","rect":{"x":20,"y":160,"width":300,"height":44},"isEnabled":"1"},
{"type":"XCUIElementTypeButton","label":"通用","rect":{"x":20,"y":220,"width":200,"height":44},"isEnabled":"1"},
{"type":"XCUIElementTypeStaticText","label":"通用设置","rect":{"x":20,"y":280,"width":200,"height":44},"isEnabled":"1"},
{"type":"XCUIElementTypeButton","label":"Sign Out","rect":{"x":20,"y":340,"width":200,"height":44},"isEnabled":"1"}]}}"#;

async fn get(
    state: &std::sync::Arc<server::http::AppState>,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    let response = server::http::router(state.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn indexes(json: &serde_json::Value) -> Vec<u64> {
    json["elements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["index"].as_u64().unwrap())
        .collect()
}

#[test]
fn query_and_limit_filter_one_read_and_keep_its_indexes() {
    block(async {
        let wda = mock_wda(|request, _| {
            if request.starts_with("POST /session ") {
                return Some((
                    Duration::ZERO,
                    r#"{"value":{"sessionId":"SESSION"}}"#.to_string(),
                ));
            }
            if request.contains("/source") {
                return Some((Duration::ZERO, TREE.replace('\n', "")));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let state = build_state_with_wda(wda.url());

        let (status, full) = get(&state, "/agent/elements").await;
        assert_eq!(status, StatusCode::OK, "{full}");
        let rows = full["elements"].as_array().unwrap();
        let sign_in = rows
            .iter()
            .position(|row| row["label"] == "Sign In")
            .unwrap() as u64;
        let email = rows
            .iter()
            .position(|row| row["placeholder"] == "Email address")
            .expect("the placeholder is a row field") as u64;

        let (status, json) = get(&state, "/agent/elements?query=SIGN").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(indexes(&json), vec![sign_in, sign_in + 4], "{json}");
        assert_eq!(json["snapshot"], full["snapshot"], "the full read's token");
        assert_eq!(json["filter"]["matched"], 2, "{json}");
        assert_eq!(json["filter"]["of"], rows.len(), "{json}");

        let (_, json) = get(&state, "/agent/elements?query=email").await;
        assert_eq!(
            indexes(&json),
            vec![email],
            "placeholder text matches: {json}"
        );

        let (_, json) = get(&state, "/agent/elements?query=%E9%80%9A%E7%94%A8&limit=1").await;
        assert_eq!(json["elements"].as_array().unwrap().len(), 1, "{json}");
        assert_eq!(json["elements"][0]["label"], "通用", "{json}");
        assert_eq!(json["filter"]["matched"], 2, "{json}");
        assert_eq!(json["filter"]["omitted"], 1, "{json}");

        // Combined with a scope: the static text is not interactive.
        let (_, json) = get(
            &state,
            "/agent/elements?scope=interactive&query=%E9%80%9A%E7%94%A8",
        )
        .await;
        assert_eq!(json["filter"]["matched"], 1, "{json}");
        assert_eq!(json["scope"]["name"], "interactive", "{json}");

        for bad in [
            "limit=0",
            "limit=501",
            "limit=many",
            "query=",
            &format!("query={}", "a".repeat(201)),
        ] {
            let (status, json) = get(&state, &format!("/agent/elements?{bad}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {json}");
            assert!(json["hint"].is_string(), "{bad}: {json}");
        }
    });
}
