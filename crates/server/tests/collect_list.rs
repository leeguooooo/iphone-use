//! `/agent/collect` and `/agent/scroll_find` through the real router against
//! a scripted runner whose list moves one page per swipe.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use server::http::AppState;
use support::{block, build_state_with_wda, mock_wda};

const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;

/// One screen: a Cell per label (label-less, text in a child, as iOS lists
/// expose them), one every 100 pt from y=100.
fn screen(cells: &[&str], extra: &str) -> String {
    let children: Vec<String> = cells
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let y = 100 + i * 100;
            format!(
                r#"{{"type":"XCUIElementTypeCell","label":"","rect":{{"x":0,"y":{y},"width":390,"height":90}},"isEnabled":"1","isVisible":"1","children":[
                    {{"type":"XCUIElementTypeStaticText","label":"{label}","rect":{{"x":16,"y":{y},"width":300,"height":40}},"isEnabled":"1","isVisible":"1"}}]}}"#
            )
        })
        .collect();
    let mut children = children.join(",");
    if !extra.is_empty() {
        children.push(',');
        children.push_str(extra);
    }
    format!(
        r#"{{"value":{{"type":"XCUIElementTypeApplication","label":"列表","rect":{{"x":0,"y":0,"width":390,"height":844}},"isEnabled":"1","isVisible":"1","children":[{children}]}}}}"#
    )
}

/// A runner serving `pages[swipes]` (the last page once swipes run out),
/// counting the swipes it was sent.
fn list_runner(pages: Vec<String>, swipes: Arc<AtomicUsize>) -> support::MockWda {
    mock_wda(move |request, _| {
        let line = request.lines().next().unwrap_or("");
        let reply = |s: String| Some((Duration::ZERO, s));
        if line.starts_with("POST /session ") {
            return reply(SESSION.to_string());
        }
        if line.contains("/source") {
            let at = swipes.load(Ordering::SeqCst).min(pages.len() - 1);
            return reply(pages[at].clone());
        }
        if line.starts_with("POST /session/SESSION/actions") {
            swipes.fetch_add(1, Ordering::SeqCst);
        }
        if line.contains("/window/size") || line.contains("/window/rect") {
            return reply(r#"{"value":{"width":390,"height":844}}"#.to_string());
        }
        reply(r#"{"value":null}"#.to_string())
    })
}

async fn post(state: &Arc<AppState>, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
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

fn labels(json: &serde_json::Value) -> Vec<String> {
    json["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["label"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn collect_dedupes_across_pages_and_stops_when_the_list_stops_moving() {
    block(async {
        let swipes = Arc::new(AtomicUsize::new(0));
        let pages = vec![
            screen(&["A", "B", "C"], ""),
            screen(&["B", "C", "D"], ""),
            screen(&["B", "C", "D"], ""),
        ];
        let runner = list_runner(pages, swipes.clone());
        let state = build_state_with_wda(runner.url());
        let (status, json) = post(&state, "/agent/collect", "{}").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(labels(&json), ["A", "B", "C", "D"], "{json}");
        assert_eq!(json["stop_reason"], "duplicate_page", "{json}");
        assert_eq!(json["complete"], false);
        assert_eq!(json["swipes"], 2);
        assert_eq!(swipes.load(Ordering::SeqCst), 2, "one gesture per swipe");
        assert!(json["snapshot"].is_string());
        assert!(json["coverage"]
            .as_str()
            .unwrap()
            .contains("NOT proven complete"));
    });
}

#[test]
fn collect_is_complete_only_when_the_end_label_shows() {
    block(async {
        let swipes = Arc::new(AtomicUsize::new(0));
        let end = r#"{"type":"XCUIElementTypeStaticText","label":"没有更多了","rect":{"x":100,"y":700,"width":190,"height":30},"isEnabled":"1","isVisible":"1"}"#;
        let pages = vec![screen(&["A", "B"], ""), screen(&["C"], end)];
        let runner = list_runner(pages, swipes.clone());
        let state = build_state_with_wda(runner.url());
        let (status, json) = post(
            &state,
            "/agent/collect",
            r#"{"end_label":"没有更多了","max_pages":4}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["stop_reason"], "end_label", "{json}");
        assert_eq!(json["complete"], true);
        assert_eq!(labels(&json), ["A", "B", "C"]);
    });
}

#[test]
fn collect_refuses_a_bad_request_without_touching_the_phone() {
    block(async {
        let swipes = Arc::new(AtomicUsize::new(0));
        let runner = list_runner(vec![screen(&["A"], "")], swipes.clone());
        let state = build_state_with_wda(runner.url());
        let (status, json) = post(&state, "/agent/collect", r#"{"max_pages":50}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(json["error"], "invalid_request");
        assert_eq!(swipes.load(Ordering::SeqCst), 0);
        // Mutation header required, like every swiping call.
        let request = Request::builder()
            .method("POST")
            .uri("/agent/collect")
            .body(Body::from("{}"))
            .unwrap();
        let response = server::http::router(state.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    });
}

#[test]
fn scroll_find_swipes_once_and_finds_the_row() {
    block(async {
        let swipes = Arc::new(AtomicUsize::new(0));
        let pages = vec![screen(&["A", "B"], ""), screen(&["C", "目标"], "")];
        let runner = list_runner(pages, swipes.clone());
        let state = build_state_with_wda(runner.url());
        let (status, json) = post(&state, "/agent/scroll_find", r#"{"label":"目标"}"#).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["found"], true, "{json}");
        assert_eq!(json["swipes"], 1);
        assert_eq!(json["target"]["label"], "目标");
        assert!(json["snapshot"].is_string());
    });
}

#[test]
fn scroll_find_stops_at_once_on_an_ambiguous_label() {
    block(async {
        let swipes = Arc::new(AtomicUsize::new(0));
        let runner = list_runner(vec![screen(&["同名", "同名"], "")], swipes.clone());
        let state = build_state_with_wda(runner.url());
        let (status, json) = post(
            &state,
            "/agent/scroll_find",
            r#"{"label":"同名","max_swipes":3}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["found"], false);
        assert_eq!(json["stop_reason"], "ambiguous", "{json}");
        assert_eq!(json["candidates"].as_array().unwrap().len(), 2, "{json}");
        assert_eq!(
            swipes.load(Ordering::SeqCst),
            0,
            "no swipe after an ambiguity"
        );
    });
}

#[test]
fn scroll_find_gives_up_after_its_swipe_budget_with_near_misses() {
    block(async {
        let swipes = Arc::new(AtomicUsize::new(0));
        let pages = vec![screen(&["设置"], ""), screen(&["通用设置"], "")];
        let runner = list_runner(pages, swipes.clone());
        let state = build_state_with_wda(runner.url());
        let (status, json) = post(&state, "/agent/scroll_find", r#"{"label":"设"}"#).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["found"], false);
        assert_eq!(json["error"], "element_not_found", "{json}");
        assert_eq!(json["swipes"], 1, "default budget is one swipe");
        assert_eq!(swipes.load(Ordering::SeqCst), 1);
        assert_eq!(json["candidates"][0]["label"], "通用设置", "{json}");
    });
}
