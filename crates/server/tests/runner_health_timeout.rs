//! A timed-out refresh must not advertise old actionability indefinitely.
mod support;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;

#[test]
fn timed_out_health_probe_revokes_readiness_without_releasing_the_runner() {
    block(async {
        let runner = mock_runner(|request, _| {
            assert!(
                request.starts_with("GET /status "),
                "no restart or mutation: {request}"
            );
            Some((
                Duration::from_secs(30),
                r#"{"value":{"ready":true}}"#.into(),
            ))
        });
        let state = build_state_with_runner(runner.url());
        *state.runner_health.lock().unwrap() = server::runner::RunnerHealth {
            up: true,
            actionable: true,
            locked: Some(false),
        };
        state.runner_actionable.store(true, Ordering::Release);
        let app = server::http::router(state.clone());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_success());
        tokio::time::sleep(Duration::from_secs(16)).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["drivable"], false, "{json}");
        assert_eq!(json["runner_actionable"], false);
        assert_eq!(json["runner"], true, "timeout alone does not prove down");
        assert_eq!(json["released"], false);
        assert!(!state.runner_actionable.load(Ordering::Acquire));
    });
}
