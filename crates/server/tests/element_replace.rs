//! Replacement never appends after a failed clear, reselects a target, or replays a prefix.
mod support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;
const SOURCE: &str = r#"{"value":{"type":"XCUIElementTypeApplication","label":"Fixture","rect":{"x":0,"y":0,"width":390,"height":844},"children":[{"type":"XCUIElementTypeTextField","label":"Draft","rawIdentifier":"field-a","value":"OLD","rect":{"x":20,"y":200,"width":300,"height":40}}]}}"#;
async fn call(
    state: Arc<server::http::AppState>,
    path: &str,
    payload: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .uri(path)
        .header("x-phone-control", "1")
        .header("x-phone-owner", "a")
        .header("content-type", "application/json");
    if payload.is_some() {
        req = req.method("POST");
    }
    let response = server::http::router(state)
        .oneshot(
            req.body(payload.map_or(Body::empty(), |v| Body::from(v.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn action(
    state: Arc<server::http::AppState>,
    batch: bool,
    text: &str,
) -> (StatusCode, Value) {
    let (_, screen) = call(state.clone(), "/agent/elements", None).await;
    let value = json!({"type":"set_value","element":1,"snapshot":screen["snapshot"],"value":text});
    call(state,if batch{"/agent/actions"}else{"/agent/input"},Some(if batch{json!({"steps":[{"kind":"action","action":value},{"kind":"action","action":{"type":"home"}}]})}else{value})).await
}
fn error() -> String {
    let body =
        json!({"value":{"error":"unknown error","message":"clear outcome unknown"}}).to_string();
    format!("HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())
}
fn base(r: &str, modern: bool) -> String {
    if r.starts_with("POST /session ") {
        json!({"value":{"sessionId":"SESSION","capabilities":{"ipu:elementReplace":modern}}})
            .to_string()
    } else if r.contains("/source?") {
        SOURCE.into()
    } else if r.contains("/elements") {
        json!({"value":[{"ELEMENT":"FIELD"}]}).to_string()
    } else if r.contains("/window/size") {
        json!({"value":{"width":390,"height":844}}).to_string()
    } else if r.contains("/attribute/value") {
        json!({"value":"OLD"}).to_string()
    } else {
        json!({"value":null}).to_string()
    }
}
#[test]
fn legacy_clear_failure_stops_single_and_batch_before_any_type_or_next_step() {
    block(async {
        for batch in [false, true] {
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let runner = mock_runner(move |r, _| {
                seen.lock().unwrap().push(r.into());
                Some((
                    Duration::ZERO,
                    if r.contains("/clear") {
                        error()
                    } else {
                        base(r, false)
                    },
                ))
            });
            let (status, body) = action(build_state_with_runner(runner.url()), batch, "NEW").await;
            assert!(!status.is_success(), "{body}");
            assert!(body.to_string().contains("unknown"), "{body}");
            let calls = calls.lock().unwrap();
            assert_eq!(calls.iter().filter(|r| r.contains("/clear")).count(), 1);
            assert!(
                !calls.iter().any(|r| r.starts_with("POST ")
                    && (r.contains("/value ")
                        || r.contains("/wda/keys")
                        || r.contains("/wda/homescreen"))),
                "{calls:?}"
            );
        }
    });
}
#[test]
fn modern_replacement_sends_one_bound_prefix_then_only_original_token_chunks() {
    block(async {
        let text = "a".repeat(199) + "👨‍👩‍👧‍👦" + &"z".repeat(295);
        assert_eq!(text.chars().count(), 501);
        let expected = text.clone();
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = calls.clone();
        let writes = Arc::new(Mutex::new(0usize));
        let count = writes.clone();
        let runner = mock_runner(move |r, _| {
            seen.lock().unwrap().push(r.into());
            let body = if r.contains("/ipu/element/FIELD/replace") {
                *count.lock().unwrap() += 1;
                let body: Value =
                    serde_json::from_str(r.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                assert_eq!(body["text"].as_str().unwrap(), "a".repeat(199));
                json!({"value":{"inputTarget":"ORIGINAL"}}).to_string()
            } else if r.contains("/wda/keys") {
                *count.lock().unwrap() += 1;
                let body: Value =
                    serde_json::from_str(r.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                assert_eq!(body["inputTarget"], "ORIGINAL");
                assert!(body["value"][0].as_str().unwrap().chars().count() <= 200);
                json!({"value":null}).to_string()
            } else if r.contains("/attribute/value") && *count.lock().unwrap() == 3 {
                json!({"value":expected}).to_string()
            } else {
                base(r, true)
            };
            Some((Duration::ZERO, body))
        });
        let (status, body) = action(build_state_with_runner(runner.url()), false, &text).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(*writes.lock().unwrap(), 3);
        let calls = calls.lock().unwrap();
        assert!(
            !calls.iter().any(|r| r.contains("/clear")
                || r.contains("/click")
                || (r.starts_with("POST ") && r.contains("/element/FIELD/value"))),
            "{calls:?}"
        );
    });
}
#[test]
fn modern_failed_or_missing_token_never_falls_back_or_sends_tail() {
    block(async {
        for missing in [false, true] {
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let runner = mock_runner(move |r, _| {
                seen.lock().unwrap().push(r.into());
                Some((
                    Duration::ZERO,
                    if r.contains("/replace") {
                        if missing {
                            json!({"value":null}).to_string()
                        } else {
                            error()
                        }
                    } else {
                        base(r, true)
                    },
                ))
            });
            let (status, body) = action(
                build_state_with_runner(runner.url()),
                false,
                &"字".repeat(501),
            )
            .await;
            assert!(!status.is_success(), "{body}");
            let calls = calls.lock().unwrap();
            assert_eq!(calls.iter().filter(|r| r.contains("/replace")).count(), 1);
            assert!(
                !calls.iter().any(|r| r.contains("/clear")
                    || r.contains("/wda/keys")
                    || (r.starts_with("POST ") && r.contains("/element/FIELD/value"))),
                "{calls:?}"
            );
        }
    });
}
#[test]
fn replacement_refuses_secure_or_single_line_newline_before_mutation() {
    block(async {
        for secure in [false, true] {
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let runner = mock_runner(move |r, _| {
                seen.lock().unwrap().push(r.into());
                let mut body = base(r, true);
                if secure && r.contains("/source?") {
                    body =
                        body.replace("XCUIElementTypeTextField", "XCUIElementTypeSecureTextField");
                }
                Some((Duration::ZERO, body))
            });
            let (status, body) = action(
                build_state_with_runner(runner.url()),
                false,
                if secure { "NEW" } else { "NEW\n" },
            )
            .await;
            assert!(!status.is_success(), "{body}");
            assert_eq!(body["outcome"], "not_sent");
            let calls = calls.lock().unwrap();
            assert!(
                !calls.iter().any(|r| r.contains("/replace")
                    || r.contains("/clear")
                    || r.contains("/wda/keys")
                    || r.contains("/click")),
                "{calls:?}"
            );
        }
    });
}

#[test]
fn privacy_change_during_prefix_preserves_ack_and_sends_no_tail() {
    block(async {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = calls.clone();
        let signal = Arc::new(tokio::sync::Notify::new());
        let started = signal.clone();
        let runner = mock_runner(move |r, _| {
            seen.lock().unwrap().push(r.into());
            if r.contains("/replace") {
                started.notify_one();
                Some((
                    Duration::from_millis(500),
                    json!({"value":{"inputTarget":"ORIGINAL"}}).to_string(),
                ))
            } else {
                Some((Duration::ZERO, base(r, true)))
            }
        });
        let state = build_state_with_runner(runner.url());
        let work = state.clone();
        let pending = tokio::spawn(async move { action(work, false, &"字".repeat(501)).await });
        tokio::time::timeout(Duration::from_secs(5), signal.notified())
            .await
            .unwrap();
        assert_eq!(
            call(
                state,
                "/agent/hold",
                Some(json!({"secs":60,"private":true}))
            )
            .await
            .0,
            StatusCode::OK
        );
        let (_, body) = pending.await.unwrap();
        assert_eq!(body["native_mutations_confirmed"], 1, "{body}");
        assert_eq!(body["native_mutations_uncertain"], 0, "{body}");
        assert!(!body["ok"].as_bool().unwrap_or(false), "{body}");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.iter().filter(|r| r.contains("/replace")).count(), 1);
        assert!(
            !calls
                .iter()
                .any(|r| r.contains("/wda/keys") || r.contains("/clear")),
            "{calls:?}"
        );
    });
}

#[test]
fn tail_rejection_or_unknown_preserves_prefix_and_never_runs_batch_successor() {
    block(async {
        for batch in [false, true] {
            for rejected in [false, true] {
                let calls = Arc::new(Mutex::new(Vec::<String>::new()));
                let seen = calls.clone();
                let runner = mock_runner(move |r, _| {
                    seen.lock().unwrap().push(r.into());
                    let response = if r.contains("/replace") {
                        json!({"value":{"inputTarget":"ORIGINAL"}}).to_string()
                    } else if r.contains("/wda/keys") {
                        if rejected {
                            let body=json!({"value":{"error":"invalid argument","message":"input_target_focus_changed; nothing was sent"}}).to_string();
                            format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())
                        } else {
                            error()
                        }
                    } else {
                        base(r, true)
                    };
                    Some((Duration::ZERO, response))
                });
                let (_, body) = action(
                    build_state_with_runner(runner.url()),
                    batch,
                    &"字".repeat(501),
                )
                .await;
                let result = &body;
                assert_eq!(
                    result["outcome"],
                    if rejected { "partial" } else { "unknown" },
                    "{body}"
                );
                assert_eq!(result["native_mutations_confirmed"], 1, "{body}");
                assert_eq!(
                    result["native_mutations_uncertain"],
                    if rejected { 0 } else { 1 },
                    "{body}"
                );
                assert_eq!(result["retry_safe"], false, "{body}");
                let calls = calls.lock().unwrap();
                assert_eq!(calls.iter().filter(|r| r.contains("/replace")).count(), 1);
                assert_eq!(calls.iter().filter(|r| r.contains("/wda/keys")).count(), 1);
                assert!(
                    !calls
                        .iter()
                        .any(|r| r.contains("/clear") || r.contains("/wda/homescreen")),
                    "{calls:?}"
                );
            }
        }
    });
}

#[test]
fn changed_or_missing_readback_after_ack_is_unknown_not_retry_safe_no_effect() {
    block(async {
        for missing in [false, true] {
            let replaced = Arc::new(Mutex::new(false));
            let written = replaced.clone();
            let runner = mock_runner(move |r, _| {
                let body = if r.contains("/replace") {
                    *written.lock().unwrap() = true;
                    json!({"value":{"inputTarget":"ORIGINAL"}}).to_string()
                } else if r.contains("/attribute/value") && *written.lock().unwrap() {
                    if missing {
                        json!({"value":null}).to_string()
                    } else {
                        json!({"value":"UNEXPECTED"}).to_string()
                    }
                } else {
                    base(r, true)
                };
                Some((Duration::ZERO, body))
            });
            let (_, body) = action(build_state_with_runner(runner.url()), false, "NEW").await;
            assert_eq!(body["outcome"], "unknown", "{body}");
            assert_eq!(body["retry_safe"], false, "{body}");
            assert_eq!(body["native_mutations_confirmed"], 1, "{body}");
        }
    });
}

#[test]
fn empty_replacement_accepts_confirmed_null_but_not_failed_or_malformed_reads() {
    block(async {
        for mode in ["null", "failed", "malformed"] {
            let replaced = Arc::new(Mutex::new(false));
            let written = replaced.clone();
            let runner = mock_runner(move |r, _| {
                let body = if r.contains("/replace") {
                    *written.lock().unwrap() = true;
                    json!({"value":{"inputTarget":"ORIGINAL"}}).to_string()
                } else if r.contains("/attribute/value") && *written.lock().unwrap() {
                    match mode {
                        "null" => json!({"value":null}).to_string(),
                        "failed" => error(),
                        _ => json!({"value":{}}).to_string(),
                    }
                } else {
                    base(r, true)
                };
                Some((Duration::ZERO, body))
            });
            let (status, body) = action(build_state_with_runner(runner.url()), false, "").await;
            if mode == "null" {
                assert_eq!(status, StatusCode::OK, "{body}");
                assert_eq!(body["ok"], true);
            } else {
                assert_eq!(body["outcome"], "unknown", "{body}");
                assert_eq!(body["retry_safe"], false, "{body}");
            }
        }
    });
}

#[test]
fn interrupted_replacement_keeps_delivery_counts_and_never_runs_batch_suffix() {
    block(async {
        for batch in [false,true] {
            for mode in ["error-in-200","refused","error","lost"] {
                let calls = Arc::new(Mutex::new(Vec::<String>::new()));
                let seen = calls.clone();
                let runner = mock_runner(move |r,_| {
                    seen.lock().unwrap().push(r.into());
                    if r.contains("/ipu/element/FIELD/replace") {
                        let payload: Value = serde_json::from_str(r.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                        assert_eq!(payload["text"].as_str().unwrap().chars().count(),199);
                        return Some((Duration::ZERO,json!({"value":{"inputTarget":"ORIGINAL"}}).to_string()));
                    }
                    if r.contains("/wda/keys") {
                        if mode == "lost" { return None; }
                        let status = match mode { "error-in-200"=>200,"refused"=>400,_=>500 };
                        let code = if mode=="refused" {"invalid argument"} else {"unknown error"};
                        let message = if mode=="refused" {"original focus changed; nothing was sent"} else {"current segment outcome unknown"};
                        let body = json!({"value":{"error":code,"message":message}}).to_string();
                        return Some((Duration::ZERO,format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())));
                    }
                    Some((Duration::ZERO,base(r,true)))
                });
                let text = "a".repeat(199)+"👨‍👩‍👧‍👦"+&"z".repeat(295);
                let (status,body) = action(build_state_with_runner(runner.url()),batch,&text).await;
                assert_eq!(status,if mode=="refused"{StatusCode::CONFLICT}else{StatusCode::BAD_GATEWAY},"{mode}: {body}");
                assert_eq!(body["error"],"replacement_incomplete","{body}");
                assert_eq!(body["outcome"],if mode=="refused"{"partial"}else{"unknown"}); assert_eq!(body["retry_safe"],false);
                assert_eq!(body["characters_confirmed"],199);
                assert_eq!(body["characters_uncertain"],if mode=="refused"{0}else{200});
                assert_eq!(body["characters_unsent"],if mode=="refused"{302}else{102});
                assert_eq!(body["characters_total"],501); assert_eq!(body["clear_outcome"],"confirmed");
                assert!(body.get("continue_token").is_none());
                if mode!="lost" { assert_eq!(body["detail"]["runner_error"]["message"],if mode=="refused"{"original focus changed; nothing was sent"}else{"current segment outcome unknown"}); }
                if batch {
                    assert_eq!(body["failed_step"],0); assert_eq!(body["completed"],0);
                    assert_eq!(body["batch_outcome"],if mode=="refused"{"partially_applied"}else{"unknown"});
                }
                let calls = calls.lock().unwrap();
                assert_eq!(calls.iter().filter(|r|r.contains("/replace")).count(),1,"{mode}");
                assert_eq!(calls.iter().filter(|r|r.contains("/wda/keys")).count(),1,"{mode}");
                assert!(!calls.iter().any(|r|r.contains("/clear")||r.contains("/click")||r.contains("/wda/homescreen")||(r.starts_with("POST ")&&r.contains("/element/FIELD/value"))),"{calls:?}");
            }
        }
    });
}

#[test]
fn legacy_clear_ack_is_not_erased_by_a_later_typing_failure() {
    block(async {
        for batch in [false,true] {
            for mode in ["error-in-200","refused","error","lost"] {
                let calls = Arc::new(Mutex::new(Vec::<String>::new())); let seen = calls.clone();
                let runner = mock_runner(move |r,_| {
                    seen.lock().unwrap().push(r.into());
                    if r.contains("/element/FIELD/value") && r.starts_with("POST ") {
                        if mode=="lost" {return None;}
                        let status = match mode {"error-in-200"=>200,"refused"=>400,_=>500};
                        let code = if mode=="refused" {"invalid argument"} else {"unknown error"};
                        let message = if mode=="refused" {"field changed; nothing was sent"} else {"typing outcome unknown"};
                        let body = json!({"value":{"error":code,"message":message}}).to_string();
                        return Some((Duration::ZERO,format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())));
                    }
                    Some((Duration::ZERO,base(r,false)))
                });
                let (status,body) = action(build_state_with_runner(runner.url()),batch,"中文🙂").await;
                assert_eq!(status,if mode=="refused"{StatusCode::CONFLICT}else{StatusCode::BAD_GATEWAY},"{body}");
                assert_eq!(body["error"],"replacement_incomplete"); assert_eq!(body["retry_safe"],false);
                assert_eq!(body["outcome"],if mode=="refused"{"partial"}else{"unknown"});
                assert_eq!(body["characters_confirmed"],0); assert_eq!(body["characters_total"],3);
                assert_eq!(body["characters_uncertain"],if mode=="refused"{0}else{3});
                assert_eq!(body["characters_unsent"],if mode=="refused"{3}else{0});
                assert_eq!(body["clear_outcome"],"confirmed"); assert_eq!(body["native_mutations_confirmed"],1);
                assert_eq!(body["native_mutations_uncertain"],if mode=="refused"{0}else{1});
                assert!(body.get("continue_token").is_none());
                let calls = calls.lock().unwrap();
                assert_eq!(calls.iter().filter(|r|r.contains("/clear")).count(),1);
                assert_eq!(calls.iter().filter(|r|r.starts_with("POST ")&&r.contains("/element/FIELD/value")).count(),1);
                assert!(!calls.iter().any(|r|r.contains("/replace")||r.contains("/wda/keys")||r.contains("/wda/homescreen")),"{calls:?}");
            }
        }
    });
}

#[test]
fn browser_control_replacement_failure_is_not_unsupported_or_retry_safe() {
    block(async {
        for refused in [false,true] {
            let calls = Arc::new(Mutex::new(Vec::<String>::new())); let seen = calls.clone();
            let runner = mock_runner(move |r,_| {
                seen.lock().unwrap().push(r.into());
                let response = if r.contains("/replace") {
                    json!({"value":{"inputTarget":"ORIGINAL"}}).to_string()
                } else if r.contains("/wda/keys") {
                    if refused {
                        let body=json!({"value":{"error":"invalid argument","message":"original changed; nothing was sent"}}).to_string();
                        format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())
                    } else {error()}
                } else {base(r,true)};
                Some((Duration::ZERO,response))
            });
            let state=build_state_with_runner(runner.url());
            let (_,screen)=call(state.clone(),"/agent/elements",None).await;
            let (status,body)=call(state,"/control",Some(json!({"type":"set_value","element":1,
                "snapshot":screen["snapshot"],"value":"字".repeat(201),"ttl_ms":2500}))).await;
            assert_eq!(status,if refused{StatusCode::CONFLICT}else{StatusCode::BAD_GATEWAY},"{body}");
            assert_eq!(body["error"],"replacement_incomplete"); assert_eq!(body["retry_safe"],false);
            assert_eq!(body["outcome"],if refused{"partial"}else{"unknown"});
            assert_eq!(body["characters_confirmed"],200); assert_eq!(body["characters_total"],201);
            assert_eq!(body["characters_uncertain"],if refused{0}else{1});
            assert_eq!(body["characters_unsent"],if refused{1}else{0});
            assert_eq!(body["clear_outcome"],"confirmed");
            let calls=calls.lock().unwrap();
            assert_eq!(calls.iter().filter(|r|r.contains("/replace")).count(),1);
            assert_eq!(calls.iter().filter(|r|r.contains("/wda/keys")).count(),1);
            assert!(!calls.iter().any(|r|r.contains("/clear")||r.contains("/click")||r.contains("/wda/homescreen")),"{calls:?}");
        }
    });
}
