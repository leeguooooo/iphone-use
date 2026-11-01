//! A single match in an incomplete scope cannot authorize a locator tap.
mod support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;

fn response(body: &str, coverage: &str) -> String {
    let headers = match coverage {
        "complete" => "X-IPU-Truncated: 0\r\nX-IPU-Tree-Complete: 1\r\n",
        "partial" => "X-IPU-Truncated: 1\r\nX-IPU-Tree-Complete: 0\r\n",
        _ => "",
    };
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}
const TREE: &str = r#"{"value":{"type":"XCUIElementTypeApplication","label":"Fixture","rect":{"x":0,"y":0,"width":390,"height":844},"children":[{"type":"XCUIElementTypeButton","label":"Done","rect":{"x":10,"y":10,"width":100,"height":40}}]}}"#;

#[test]
fn locator_taps_require_complete_source_and_complete_live_matches_before_dispatch() {
    block(async {
        for source in ["complete", "partial", "unknown"] {
            for query in ["complete", "partial", "unknown"] {
                for via in ["element", "point"] {
                    for cached in [false, true] {
                        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
                        let record = seen.clone();
                        let runner = mock_runner(move |request, _| {
                            record.lock().unwrap().push(request.to_string());
                            let body = if request.starts_with("POST /session ") {
                                r#"{"value":{"sessionId":"SESSION"}}"#
                            } else if request.contains("/source?") {
                                return Some((Duration::ZERO, response(TREE, source)));
                            } else if request.starts_with("POST ") && request.contains("/elements ")
                            {
                                return Some((
                                    Duration::ZERO,
                                    response(r#"{"value":[{"ELEMENT":"E1"}]}"#, query),
                                ));
                            } else if request.contains("/element/E1/rect") {
                                r#"{"value":{"x":10,"y":10,"width":100,"height":40}}"#
                            } else if request.contains("/window/size") {
                                r#"{"value":{"width":390,"height":844}}"#
                            } else {
                                r#"{"value":null}"#
                            };
                            Some((Duration::ZERO, body.to_string()))
                        });
                        let mut steps = vec![];
                        if cached {
                            steps.push(serde_json::json!({"kind":"wait_for","expect":{"present":[{"label":"Done"}]},"timeout_ms":0}));
                        }
                        steps.push(serde_json::json!({"kind":"action","action":{"type":"tap_locator","locator":{"label":"Done"},"via":via,"if_present":true}}));
                        let request = Request::builder()
                            .method("POST")
                            .uri("/agent/actions")
                            .header("x-phone-control", "1")
                            .header("content-type", "application/json")
                            .body(Body::from(
                                serde_json::json!({"observe":false,"steps":steps}).to_string(),
                            ))
                            .unwrap();
                        let reply = server::http::router(build_state_with_runner(runner.url()))
                            .oneshot(request)
                            .await
                            .unwrap();
                        let status = reply.status();
                        let body: serde_json::Value = serde_json::from_slice(
                            &reply.into_body().collect().await.unwrap().to_bytes(),
                        )
                        .unwrap();
                        let sent = seen.lock().unwrap();
                        let mutations: Vec<_> = sent
                            .iter()
                            .filter(|r| {
                                r.starts_with("POST ")
                                    && (r.contains("/actions ")
                                        || r.contains("/click ")
                                        || r.contains("/wda/tap/"))
                            })
                            .collect();
                        let expected = source == "complete" && query == "complete";
                        assert_eq!(mutations.len(),usize::from(expected),"source={source} query={query} via={via} cached={cached}: {body}; {sent:?}");
                        assert_eq!(body["ok"], expected, "{body}");
                        if expected {
                            assert_eq!(status, StatusCode::OK);
                        } else {
                            assert_eq!(body["error"], "target_scope_unproven", "{body}");
                            assert_eq!(body["outcome"], "not_sent", "{body}");
                            assert_eq!(body["applied_actions"], 0, "{body}");
                            assert_eq!(body["retry_safe"], true, "{body}");
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn losing_scope_after_reveal_stops_the_tap_and_never_authorizes_replay() {
    block(async {
        for typ in ["tap", "tap_locator"] {
            for coverage in ["partial", "unknown"] {
                let seen = Arc::new(Mutex::new(Vec::<String>::new()));
                let record = seen.clone();
                let moved = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let scrolled = moved.clone();
                let runner = mock_runner(move |request, _| {
                    record.lock().unwrap().push(request.to_string());
                    if request.contains("/scrollTo") {
                        scrolled.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    let body = if request.starts_with("POST /session ") {
                        r#"{"value":{"sessionId":"SESSION"}}"#
                    } else if request.contains("/source?") {
                        let after = scrolled.load(std::sync::atomic::Ordering::SeqCst);
                        let y = if after { 400 } else { 745 };
                        let body = format!(
                            r#"{{"value":{{"type":"XCUIElementTypeApplication","label":"Fixture","rect":{{"x":0,"y":0,"width":390,"height":844}},"children":[{{"type":"XCUIElementTypeButton","label":"Done","rect":{{"x":16,"y":{y},"width":358,"height":51}}}},{{"type":"XCUIElementTypeSearchField","label":"Search","rect":{{"x":28,"y":778,"width":334,"height":28}}}}]}}}}"#
                        );
                        return Some((
                            Duration::ZERO,
                            response(&body, if after { coverage } else { "complete" }),
                        ));
                    } else if request.starts_with("POST ") && request.contains("/elements ") {
                        return Some((
                            Duration::ZERO,
                            response(r#"{"value":[{"ELEMENT":"E1"}]}"#, "complete"),
                        ));
                    } else if request.contains("/element/E1/rect") {
                        let y = if scrolled.load(std::sync::atomic::Ordering::SeqCst) {
                            400
                        } else {
                            745
                        };
                        return Some((
                            Duration::ZERO,
                            format!(r#"{{"value":{{"x":16,"y":{y},"width":358,"height":51}}}}"#),
                        ));
                    } else if request.contains("/window/size") {
                        r#"{"value":{"width":390,"height":844}}"#
                    } else {
                        r#"{"value":null}"#
                    };
                    Some((Duration::ZERO, body.to_string()))
                });
                let action = if typ == "tap" {
                    serde_json::json!({"type":typ,"label":"Done"})
                } else {
                    serde_json::json!({"type":typ,"locator":{"label":"Done"}})
                };
                let request=Request::builder().method("POST").uri("/agent/actions").header("x-phone-control","1").header("content-type","application/json").body(Body::from(serde_json::json!({"observe":false,"steps":[{"kind":"action","action":action}]}).to_string())).unwrap();
                let reply = server::http::router(build_state_with_runner(runner.url()))
                    .oneshot(request)
                    .await
                    .unwrap();
                let body: serde_json::Value =
                    serde_json::from_slice(&reply.into_body().collect().await.unwrap().to_bytes())
                        .unwrap();
                let sent = seen.lock().unwrap();
                assert!(
                    sent.iter()
                        .any(|r| r.starts_with("POST ") && r.contains("/scrollTo ")),
                    "reveal must be attempted: {body}; {sent:?}"
                );
                assert!(
                    !sent.iter().any(|r| r.starts_with("POST ")
                        && (r.contains("/click ") || r.contains("/actions "))),
                    "no further tap after losing scope: {body}; {sent:?}"
                );
                assert_eq!(body["ok"], false, "{body}");
                assert_eq!(body["retry_safe"], false, "{body}");
                assert_ne!(
                    body["outcome"], "not_sent",
                    "reveal was already sent: {body}"
                );
            }
        }
    });
}

#[test]
fn point_locator_needs_a_known_viewport_even_with_one_complete_match() {
    block(async {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let runner = mock_runner(move |request, _| {
            record.lock().unwrap().push(request.to_string());
            let body = if request.starts_with("POST /session ") {
                r#"{"value":{"sessionId":"SESSION"}}"#
            } else if request.contains("/source?") {
                return Some((
                    Duration::ZERO,
                    response(
                        &TREE.replace("XCUIElementTypeApplication", "XCUIElementTypeOther"),
                        "complete",
                    ),
                ));
            } else if request.starts_with("POST ") && request.contains("/elements ") {
                return Some((
                    Duration::ZERO,
                    response(r#"{"value":[{"ELEMENT":"E1"}]}"#, "complete"),
                ));
            } else if request.contains("/element/E1/rect") {
                r#"{"value":{"x":10,"y":10,"width":100,"height":40}}"#
            } else if request.contains("/window/size") {
                return Some((Duration::ZERO,"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()));
            } else {
                r#"{"value":null}"#
            };
            Some((Duration::ZERO, body.into()))
        });
        let body = serde_json::json!({"observe":false,"steps":[{"kind":"action","action":{"type":"tap_locator","locator":{"label":"Done"},"via":"point"}}]});
        let request = Request::builder()
            .method("POST")
            .uri("/agent/actions")
            .header("x-phone-control", "1")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let reply = server::http::router(build_state_with_runner(runner.url()))
            .oneshot(request)
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&reply.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let sent = seen.lock().unwrap();
        assert!(
            sent.iter().any(|r| r.contains("/window/size")),
            "viewport proof was required: {sent:?}"
        );
        assert!(
            !sent
                .iter()
                .any(|r| r.starts_with("POST ")
                    && (r.contains("/click ") || r.contains("/actions "))),
            "unknown viewport must not tap: {body}; {sent:?}"
        );
        assert_eq!(body["ok"], false, "{body}");
        assert_eq!(body["outcome"], "not_sent", "{body}");
        assert_eq!(body["retry_safe"], true, "{body}");
    });
}

#[test]
fn locator_modes_check_live_geometry_and_refresh_handles_after_reveal() {
    block(async {
        for via in ["element", "point"] {
            for (source_covered, live_covered, clear_after) in [
                (false, false, true),
                (true, false, true),
                (false, true, false),
                (true, true, false),
                (false, true, true),
                (true, true, true),
            ] {
                let seen = Arc::new(Mutex::new(Vec::<String>::new()));
                let record = seen.clone();
                let moved = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let scrolled = moved.clone();
                let runner = mock_runner(move |request, _| {
                    record.lock().unwrap().push(request.to_string());
                    if request.contains("/scrollTo") {
                        scrolled.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    let after = scrolled.load(std::sync::atomic::Ordering::SeqCst);
                    let body = if request.starts_with("POST /session ") {
                        r#"{"value":{"sessionId":"SESSION"}}"#
                    } else if request.contains("/source?") {
                        let y = if source_covered && !after { 745 } else { 400 };
                        let tree = format!(
                            r#"{{"value":{{"type":"XCUIElementTypeApplication","label":"Fixture","rect":{{"x":0,"y":0,"width":390,"height":844}},"children":[{{"type":"XCUIElementTypeButton","label":"Done","rect":{{"x":16,"y":{y},"width":358,"height":51}}}},{{"type":"XCUIElementTypeSearchField","label":"Search","rect":{{"x":28,"y":778,"width":334,"height":28}}}}]}}}}"#
                        );
                        return Some((Duration::ZERO, response(&tree, "complete")));
                    } else if request.starts_with("POST ") && request.contains("/elements ") {
                        let id = if after { "E2" } else { "E1" };
                        return Some((
                            Duration::ZERO,
                            response(
                                &format!(r#"{{"value":[{{"ELEMENT":"{id}"}}]}}"#),
                                "complete",
                            ),
                        ));
                    } else if request.contains("/rect") {
                        let y = if live_covered && (!after || !clear_after) {
                            780
                        } else {
                            420
                        };
                        return Some((
                            Duration::ZERO,
                            format!(r#"{{"value":{{"x":16,"y":{y},"width":358,"height":51}}}}"#),
                        ));
                    } else if request.contains("/window/size") {
                        r#"{"value":{"width":390,"height":844}}"#
                    } else {
                        r#"{"value":null}"#
                    };
                    Some((Duration::ZERO, body.into()))
                });
                let request = Request::builder().method("POST").uri("/agent/actions").header("x-phone-control","1").header("content-type","application/json").body(Body::from(serde_json::json!({"observe":false,"steps":[{"kind":"action","action":{"type":"tap_locator","locator":{"label":"Done"},"via":via}}]}).to_string())).unwrap();
                let reply = server::http::router(build_state_with_runner(runner.url()))
                    .oneshot(request)
                    .await
                    .unwrap();
                let body: serde_json::Value =
                    serde_json::from_slice(&reply.into_body().collect().await.unwrap().to_bytes())
                        .unwrap();
                let sent = seen.lock().unwrap();
                let clicks: Vec<_> = sent
                    .iter()
                    .filter(|r| r.starts_with("POST ") && r.contains("/click "))
                    .collect();
                let taps: Vec<_> = sent
                    .iter()
                    .filter(|r| r.starts_with("POST ") && r.contains("/actions "))
                    .collect();
                assert_eq!(
                    sent.iter()
                        .any(|r| r.starts_with("POST ") && r.contains("/scrollTo ")),
                    live_covered,
                    "{body}; {sent:?}"
                );
                if live_covered && !clear_after && via == "point" {
                    assert!(
                        clicks.is_empty() && taps.is_empty(),
                        "do not tap the covered centre: {body}; {sent:?}"
                    );
                    assert_eq!(body["ok"], false, "{body}");
                    assert_eq!(body["retry_safe"], false, "{body}");
                    assert_ne!(body["outcome"], "not_sent", "{body}");
                } else {
                    assert_eq!(body["ok"], true, "{body}; {sent:?}");
                    assert_eq!(clicks.len() + taps.len(), 1, "{sent:?}");
                    if via == "element" && (!live_covered || clear_after) {
                        let id = if live_covered { "E2" } else { "E1" };
                        assert!(
                            clicks[0].contains(&format!("/{id}/click ")),
                            "fresh handle must match new proof: {sent:?}"
                        );
                    } else {
                        assert!(clicks.is_empty(), "{sent:?}");
                        let payload: serde_json::Value =
                            serde_json::from_str(taps[0].split_once("\r\n\r\n").unwrap().1)
                                .unwrap();
                        let actions = payload["actions"].as_array().unwrap();
                        let motion = actions
                            .iter()
                            .flat_map(|device| device["actions"].as_array().unwrap())
                            .find(|step| step["type"] == "pointerMove")
                            .unwrap();
                        let y = motion["y"].as_f64().unwrap();
                        if live_covered && !clear_after {
                            assert!(
                                y > 820.0 && y <= 831.0,
                                "clear-part fallback must avoid the pill: {payload}"
                            );
                        } else {
                            assert!(
                                (y - 445.5).abs() <= 1.0,
                                "tap live, not source, centre: {payload}"
                            );
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn native_click_unknown_after_internal_reveal_never_retries_or_falls_back() {
    block(async {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let runner = mock_runner(move |request, _| {
            record.lock().unwrap().push(request.to_string());
            let body = if request.starts_with("POST /session ") {
                r#"{"value":{"sessionId":"SESSION"}}"#
            } else if request.contains("/source?") {
                return Some((Duration::ZERO, response(TREE, "complete")));
            } else if request.starts_with("POST ") && request.contains("/elements ") {
                return Some((
                    Duration::ZERO,
                    response(r#"{"value":[{"ELEMENT":"E1"}]}"#, "complete"),
                ));
            } else if request.contains("/element/E1/rect") {
                r#"{"value":{"x":10,"y":10,"width":100,"height":40}}"#
            } else if request.contains("/window/size") {
                r#"{"value":{"width":390,"height":844}}"#
            } else if request.contains("/E1/click ") {
                let body = r#"{"value":{"error":"unknown error","message":"click outcome unknown after reveal; no automatic retry"}}"#;
                return Some((Duration::ZERO,format!("HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())));
            } else {
                r#"{"value":null}"#
            };
            Some((Duration::ZERO, body.into()))
        });
        let request=Request::builder().method("POST").uri("/agent/actions").header("x-phone-control","1").header("content-type","application/json").body(Body::from(serde_json::json!({"observe":false,"steps":[{"kind":"action","action":{"type":"tap_locator","locator":{"label":"Done"}}}]}).to_string())).unwrap();
        let reply = server::http::router(build_state_with_runner(runner.url()))
            .oneshot(request)
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&reply.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let sent = seen.lock().unwrap();
        assert_eq!(
            sent.iter()
                .filter(|r| r.starts_with("POST ") && r.contains("/E1/click "))
                .count(),
            1,
            "{sent:?}"
        );
        assert!(
            !sent
                .iter()
                .any(|r| r.starts_with("POST ") && r.contains("/actions ")),
            "no coordinate fallback after native500: {sent:?}"
        );
        assert_eq!(body["outcome"], "unknown", "{body}");
        assert_eq!(body["retry_safe"], false, "{body}");
        assert_eq!(body["applied_actions"], 0, "{body}");
    });
}
