//! Read-only selected values have explicit bounded completeness and publication scope.
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
async fn response(state: Arc<server::http::AppState>, body: Value) -> axum::response::Response {
    server::http::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agent/value")
                .header("x-phone-owner", "reader")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn decode(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn hold(state: Arc<server::http::AppState>) {
    let response = server::http::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agent/hold")
                .header("x-phone-owner", "human")
                .header("x-phone-control", "1")
                .header("content-type", "application/json")
                .body(Body::from(json!({"secs":60,"private":true}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
fn contract(limit: u32) -> Value {
    let total = 6015;
    let returned = (limit as usize).min(total);
    json!({"value":"字".repeat(returned),"source":"ax_value_attribute","utf16_length":total,"returned_utf16_length":returned,"limit_utf16":limit,"truncated":returned<total,"complete":returned==total})
}
fn base(r: &str, ids: usize, supports: bool) -> String {
    if r.starts_with("POST /session ") {
        json!({"value":{"sessionId":"SESSION","capabilities":{"ipu:valueRead":supports}}})
            .to_string()
    } else if r.contains("/elements") {
        json!({"value":(0..ids).map(|i|json!({"ELEMENT":format!("E{i}")})).collect::<Vec<_>>(),})
            .to_string()
    } else {
        json!({"value":null}).to_string()
    }
}
#[test]
fn full_or_intentionally_bounded_value_never_takes_a_control_lease() {
    block(async {
        for limit in [8192, 128] {
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let runner = mock_runner(move |r, _| {
                seen.lock().unwrap().push(r.into());
                let value = if r.starts_with("GET ") && r.contains("/element/E0/value") {
                    json!({"value":contract(limit)}).to_string()
                } else {
                    base(r, 1, true)
                };
                Some((Duration::ZERO, value))
            });
            let state = build_state_with_runner(runner.url());
            let (status, body) = decode(
                response(
                    state.clone(),
                    json!({"identifier":"field-a","max_utf16":limit}),
                )
                .await,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["complete"], limit >= 6015);
            assert_eq!(body["truncated"], limit < 6015);
            assert_eq!(
                body["value"].as_str().unwrap().chars().count(),
                (limit as usize).min(6015)
            );
            assert!(state.owner.lock().unwrap().is_none());
            assert!(calls
                .lock()
                .unwrap()
                .iter()
                .all(|r| !r.contains("x-phone-control")
                    && !r.contains("/actions")
                    && !r.contains("/wda/keys")));
        }
    });
}
#[test]
fn bad_selection_ambiguity_and_old_or_malformed_contracts_never_promote_previews() {
    block(async {
        for (ids, supports, bad) in [
            (2, true, false),
            (0, true, false),
            (1, false, false),
            (1, true, true),
        ] {
            let runner = mock_runner(move |r, _| {
                let value = if r.starts_with("GET ") && r.contains("/element/E0/value") {
                    json!({"value":{"value":"PREVIEW","complete":true}}).to_string()
                } else {
                    base(r, ids, supports)
                };
                Some((Duration::ZERO, value))
            });
            let state = build_state_with_runner(runner.url());
            let (status, body) = decode(response(state, json!({"label":"Draft"})).await).await;
            assert!(!status.is_success(), "{ids} {supports} {bad}: {body}");
            assert!(body.get("value").is_none());
            assert!(!body.to_string().contains("PREVIEW"));
        }
        let runner = mock_runner(|_, _| panic!("invalid request must not access runner"));
        let state = build_state_with_runner(runner.url());
        for payload in [
            json!({}),
            json!({"identifier":"a","label":"b"}),
            json!({"identifier":"a","max_utf16":0}),
        ] {
            assert_eq!(
                response(state.clone(), payload).await.status(),
                StatusCode::BAD_REQUEST
            );
        }
    });
}
#[test]
fn private_and_in_flight_scope_loss_return_no_value() {
    block(async {
        let signal = Arc::new(tokio::sync::Notify::new());
        let started = signal.clone();
        let runner = mock_runner(move |r, _| {
            let (delay, value) = if r.starts_with("GET ") && r.contains("/element/E0/value") {
                started.notify_one();
                (
                    Duration::from_millis(500),
                    json!({"value":contract(8192)}).to_string(),
                )
            } else {
                (Duration::ZERO, base(r, 1, true))
            };
            Some((delay, value))
        });
        let state = build_state_with_runner(runner.url());
        let worker = state.clone();
        let pending = tokio::spawn(async move {
            decode(response(worker, json!({"identifier":"field-a"})).await).await
        });
        tokio::time::timeout(Duration::from_secs(3), signal.notified())
            .await
            .unwrap();
        hold(state.clone()).await;
        let (_, body) = pending.await.unwrap();
        assert!(body.get("value").is_none(), "{body}");
        assert_eq!(body["ok"], false);
        assert_eq!(
            response(state, json!({"identifier":"field-a"}))
                .await
                .status(),
            StatusCode::LOCKED
        );
    });
}
#[test]
fn a_queued_value_body_rechecks_scope_before_publication() {
    block(async {
        let runner = mock_runner(|r, _| {
            Some((
                Duration::ZERO,
                if r.starts_with("GET ") && r.contains("/element/E0/value") {
                    json!({"value":contract(8192)}).to_string()
                } else {
                    base(r, 1, true)
                },
            ))
        });
        let state = build_state_with_runner(runner.url());
        let reply = response(state.clone(), json!({"identifier":"field-a"})).await;
        assert_eq!(reply.status(), StatusCode::OK);
        hold(state).await;
        let (_, body) = decode(reply).await;
        assert_eq!(body["ok"], false);
        assert!(body.get("value").is_none());
        assert_eq!(body["value_omitted"], true);
    });
}

async fn public_hold(state: Arc<server::http::AppState>, name: Option<&str>) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/agent/hold")
        .header("x-phone-control", "1")
        .header("content-type", "application/json");
    if let Some(name) = name {
        request = request.header("x-phone-owner", name);
    }
    let response = server::http::router(state)
        .oneshot(
            request
                .body(Body::from(json!({"secs":0,"private":false}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
async fn release_owner(state: Arc<server::http::AppState>, name: &str) {
    let response = server::http::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agent/owner")
                .header("x-phone-control", "1")
                .header("x-phone-owner", name)
                .header("content-type", "application/json")
                .body(Body::from(json!({"release":true}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
#[test]
fn queued_value_survives_renewal_but_not_owner_aba() {
    block(async {
        for aba in [false, true] {
            let runner = mock_runner(|r, _| {
                Some((
                    Duration::ZERO,
                    if r.contains("/element/E0/value") {
                        json!({"value":contract(8192)}).to_string()
                    } else {
                        base(r, 1, true)
                    },
                ))
            });
            let state = build_state_with_runner(runner.url());
            public_hold(state.clone(), Some("owner-a")).await;
            let reply = response(state.clone(), json!({"identifier":"field-a"})).await;
            assert_eq!(reply.status(), StatusCode::OK);
            if aba {
                release_owner(state.clone(), "owner-a").await;
                public_hold(state.clone(), Some("owner-b")).await;
                release_owner(state.clone(), "owner-b").await;
            }
            public_hold(state.clone(), Some("owner-a")).await;
            let (_, body) = decode(reply).await;
            if aba {
                assert_eq!(body["value_omitted"], true);
                assert!(body.get("value").is_none());
            } else {
                assert_eq!(body["value"], contract(8192)["value"]);
            }
        }
    });
}
#[test]
fn expiry_withholds_old_value_but_expired_owner_cleanup_preserves_new_read() {
    block(async {
        let runner = mock_runner(|r, _| {
            Some((
                Duration::ZERO,
                if r.contains("/element/E0/value") {
                    json!({"value":contract(8192)}).to_string()
                } else {
                    base(r, 1, true)
                },
            ))
        });
        let state = build_state_with_runner(runner.url());
        public_hold(state.clone(), Some("owner-a")).await;
        let old = response(state.clone(), json!({"identifier":"field-a"})).await;
        state.owner.lock().unwrap().as_mut().unwrap().last_seen =
            std::time::Instant::now() - Duration::from_secs(state.owner_lease_secs + 1);
        let (_, body) = decode(old).await;
        assert_eq!(body["value_omitted"], true);
        assert!(body.get("value").is_none());
        let fresh = response(state.clone(), json!({"identifier":"field-a"})).await;
        public_hold(state, None).await;
        let (_, body) = decode(fresh).await;
        assert_eq!(body["value"], contract(8192)["value"]);
    });
}
#[test]
fn explicit_secure_kind_is_refused_without_a_runner_request() {
    block(async {
        let runner = mock_runner(|_, _| panic!("secure kind must not query native data"));
        let (_, body) = decode(
            response(
                build_state_with_runner(runner.url()),
                json!({"identifier":"password","kind":"SecureTextField"}),
            )
            .await,
        )
        .await;
        assert_eq!(body["error"], "secure_value_read_requires_human");
        assert!(body.get("value").is_none());
    });
}

#[test]
fn transient_native_failure_is_retryable_instead_of_claiming_unsupported() {
    block(async {
        let runner = mock_runner(|r, _| {
            if r.starts_with("GET ") && r.contains("/element/E0/value") { return None; }
            Some((Duration::ZERO, base(r, 1, true)))
        });
        let (status, body) = decode(response(build_state_with_runner(runner.url()), json!({"identifier":"a"})).await).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["error"], "value_read_failed");
        assert_eq!(body["retry_safe"], true);
        assert!(body.get("value").is_none());
    });
}

#[test]
fn supported_reader_attribute_unavailability_does_not_prescribe_runner_upgrade() {
    block(async {
        let runner = mock_runner(|r, _| {
            let body = if r.starts_with("GET ") && r.contains("/element/E0/value") {
                let body=json!({"value":{"error":"unsupported operation","message":"full AX value attribute unavailable"}}).to_string();
                format!("HTTP/1.1 501 Not Implemented\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())
            } else {base(r,1,true)};
            Some((Duration::ZERO,body))
        });
        let state=build_state_with_runner(runner.url());
        let (status,body)=decode(response(state.clone(),json!({"identifier":"field-a"})).await).await;
        assert_eq!(status,StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["error"],"full_value_unavailable");
        assert_eq!(body["reason"],"ax_attribute_unavailable","{body}");
        assert!(!body["hint"].as_str().unwrap().contains("upgrade"),"{body}");
        assert!(body.get("value").is_none(),"unavailable is not empty or null");
        assert!(state.owner.lock().unwrap().is_none());
    });
}

#[test]
fn unavailable_value_reasons_preserve_capability_attribute_and_contract_boundaries() {
    block(async {
        for (supports,message,expected) in [
            (false,None,"runner_capability_missing"),
            (true,Some("full AX value read unavailable"),"ax_bridge_unavailable"),
            (true,Some("full AX value selector shape unavailable (return=@, arg2=@, arg3=@, arg4=^@)"),"ax_bridge_unavailable"),
            (true,Some("full AX value is not scalar"),"ax_value_not_scalar"),
            (true,Some("full AX value read failed"),"ax_read_failed"),
            (true,Some("unknown complete-value endpoint"),"runner_endpoint_unavailable"),
            (true,None,"invalid_contract"),
        ] {
            let reads=Arc::new(std::sync::atomic::AtomicUsize::new(0));let seen=reads.clone();
            let runner=mock_runner(move |r,_| {
                let body=if r.starts_with("GET ") && r.contains("/element/E0/value") {
                    seen.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
                    if let Some(message)=message {
                        let body=json!({"value":{"error":"unsupported operation","message":message}}).to_string();
                        format!("HTTP/1.1 501 Not Implemented\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())
                    } else {json!({"value":{"value":"PREVIEW","complete":true}}).to_string()}
                } else {base(r,1,supports)};
                Some((Duration::ZERO,body))
            });
            let (status,body)=decode(response(build_state_with_runner(runner.url()),json!({"identifier":"field-a"})).await).await;
            assert_eq!(status,StatusCode::NOT_IMPLEMENTED,"{body}");
            assert_eq!(body["error"],"full_value_unavailable");
            assert_eq!(body["reason"],expected,"{body}");
            assert!(body.get("value").is_none());
            assert!(!body.to_string().contains("PREVIEW"));
            assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed),usize::from(supports));
        }
    });
}

#[test]
fn an_explicit_complete_null_contract_remains_distinct_from_an_unavailable_value() {
    block(async {
        let runner=mock_runner(|r,_| Some((Duration::ZERO,
            if r.starts_with("GET ") && r.contains("/element/E0/value") {
                json!({"value":{"value":null,"source":"ax_value_attribute","utf16_length":0,
                    "returned_utf16_length":0,"limit_utf16":8192,"complete":true,"truncated":false}}).to_string()
            } else {base(r,1,true)})));
        let (status,body)=decode(response(build_state_with_runner(runner.url()),json!({"identifier":"field-a"})).await).await;
        assert_eq!(status,StatusCode::OK,"{body}");
        assert!(body.get("value").is_some_and(Value::is_null));
        assert_eq!(body["complete"],true);
        assert!(body.get("error").is_none());
        assert!(body.get("reason").is_none());
    });
}
