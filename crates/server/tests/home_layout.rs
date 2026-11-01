//! Discovery never chooses a phone implicitly or leaks through a private handoff.
mod support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use support::{block, build_state};
use tower::ServiceExt;
async fn get(
    state: Arc<server::http::AppState>,
    token: Option<&str>,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = server::http::router(state)
        .oneshot(request.body(Body::empty()).unwrap())
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
fn auth_precedes_target_discovery() {
    block(async {
        let state = build_state(Some("secret"));
        assert_eq!(
            get(state.clone(), None, "/agent/home-layout").await.0,
            StatusCode::UNAUTHORIZED
        );
        let (status, body) = get(state, Some("secret"), "/agent/home-layout").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "target_required");
    });
}
#[test]
fn private_handoff_precedes_any_device_read() {
    block(async {
        let state = build_state(None);
        for duration in [Duration::from_secs(30), Duration::from_millis(900)] {
            *state.private_until.lock().unwrap() = Some(Instant::now() + duration);
            let (status, body) = get(state.clone(), None, "/agent/home-layout").await;
            assert_eq!(status, StatusCode::LOCKED);
            assert_eq!(body["error"], "screen_private");
            assert!(body["screen_private_secs"].as_u64().unwrap() >= 1);
        }
    });
}
#[test]
fn read_does_not_claim_an_owner_and_a_bare_write_is_refused() {
    block(async {
        let state = build_state(None);
        let _ = get(state.clone(), None, "/agent/home-layout").await;
        assert!(state.owner.lock().unwrap().is_none());
        let response = server::http::router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/home-layout")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Writing exists (#383) but needs auth and X-Phone-Control: a bare
        // POST is refused before any owner is claimed or the phone is read.
        assert!(response.status().is_client_error(), "{}", response.status());
        assert!(state.owner.lock().unwrap().is_none());
    });
}
#[test]
fn malformed_query_is_refused_without_a_target() {
    block(async {
        for query in ["page=-1", "unknown=1"] {
            let (status, _) = get(
                build_state(None),
                None,
                &format!("/agent/home-layout?{query}"),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
    });
}
