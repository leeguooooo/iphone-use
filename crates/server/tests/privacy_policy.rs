//! Policy is process-global; this test binary has one test and sets it before spawning work.
mod support;
use axum::{body::Body, http::{Request, StatusCode}};
use http_body_util::BodyExt;
use serde_json::json;
use std::{sync::atomic::Ordering, time::Duration};
use support::{block,build_state_with_runner,mock_runner};
use tower::ServiceExt;

#[test]
fn authenticated_policy_refusal_is_fenced_after_owner_or_privacy_revocation() {
    std::env::set_var("IPHONE_USE_BLOCKED_APPS","com.private.foreground");
    block(async {
        for revoke in ["none","owner","privacy"] {
            let runner=mock_runner(|request,_| Some((Duration::ZERO,if request.starts_with("POST /session ") {
                json!({"value":{"sessionId":"SESSION"}}).to_string()
            } else if request.contains("/wda/apps/list") {
                json!({"value":[{"bundleId":"com.private.foreground"}]}).to_string()
            } else {json!({"value":null}).to_string()})));
            let state=build_state_with_runner(runner.url());
            let response=server::http::router(state.clone()).oneshot(Request::builder().method("POST")
                .uri("/agent/actions").header("x-phone-control","1").header("x-phone-owner","privacy-policy")
                .header("content-type","application/json")
                .body(Body::from(r#"{"steps":[{"kind":"action","action":{"type":"tap","x":1,"y":1}}],"observe":true}"#))
                .unwrap()).await.unwrap();
            assert_eq!(response.status(),StatusCode::FORBIDDEN);
            match revoke {
                "owner"=>{state.owner_epoch.fetch_add(1,Ordering::AcqRel);},
                "privacy"=>{state.privacy_epoch.fetch_add(1,Ordering::AcqRel);},
                _=>{}
            }
            let bytes=response.into_body().collect().await.unwrap().to_bytes();
            let text=String::from_utf8_lossy(&bytes);
            let value:serde_json::Value=serde_json::from_slice(&bytes).unwrap();
            if revoke=="none" {
                assert!(text.contains("com.private.foreground"),"test must exercise foreground-policy refusal");
            } else {
                assert!(!text.contains("com.private.foreground"),"policy refusal leaked after {revoke}: {text}");
                assert_eq!(value["observation_omitted"],true);
            }
            assert_eq!(value["applied_actions"],0);
        }
    });
}
