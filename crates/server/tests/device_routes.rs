//! Device diagnostics and recording over HTTP: `/agent/logs`,
//! `/agent/crashes`, `/agent/location` and `/agent/record`. No phone here,
//! so these pin what the HTTP layer owns — auth, the control header,
//! validation, the configured-device requirement, error shapes — and a
//! recording against a video hub that never produces a frame.

mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use server::http::AppState;
use support::{block, build_state};

/// One state dir for this binary (recordings land in it).
fn state_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap()).path();
    std::env::set_var("IPHONE_USE_STATE_DIR", dir);
    dir
}

fn state_with(edit: impl FnOnce(&mut AppState)) -> Arc<AppState> {
    let Ok(mut state) = Arc::try_unwrap(build_state(None)) else {
        panic!("test state unexpectedly shared")
    };
    edit(&mut state);
    Arc::new(state)
}

async fn call(
    state: &Arc<AppState>,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = server::http::router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

const CONTROL: &[(&str, &str)] = &[("x-phone-control", "1")];

#[test]
fn routes_need_auth_and_mutations_need_the_control_header() {
    state_dir();
    block(async {
        let state = build_state(Some("pw"));
        for (method, uri) in [
            ("GET", "/agent/logs"),
            ("POST", "/agent/logs"),
            ("GET", "/agent/crashes"),
            ("GET", "/agent/crashes/App-2026-10-11-070102.ips"),
            ("POST", "/agent/location"),
            ("GET", "/agent/record"),
            ("POST", "/agent/record"),
        ] {
            let (status, _) = call(&state, method, uri, CONTROL, "{}").await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        }
        let bearer = [("authorization", "Bearer pw")];
        for uri in ["/agent/logs", "/agent/location", "/agent/record"] {
            let (status, json) = call(&state, "POST", uri, &bearer, r#"{"action":"stop"}"#).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {json}");
            assert_eq!(json["error"], "missing_control_header", "{uri}");
        }
        let (status, json) = call(&state, "GET", "/agent/logs", &bearer, "").await;
        assert_eq!(status, StatusCode::OK, "{json}");
    });
}

#[test]
fn logs_read_validate_and_start_needs_a_configured_phone() {
    state_dir();
    block(async {
        let state = build_state(None);
        let (status, json) = call(&state, "GET", "/agent/logs", &[], "").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(json["items"], serde_json::json!([]));
        assert_eq!(json["capturing"], false);
        assert!(json["next"].is_u64(), "{json}");
        assert!(json["hint"].as_str().unwrap().contains("start"), "{json}");

        for bad in ["level=loud", "limit=0", "since=x", "limit=99999"] {
            let (status, json) = call(&state, "GET", &format!("/agent/logs?{bad}"), &[], "").await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {json}");
            assert_eq!(json["error"], "invalid_query", "{bad}");
            assert!(json["hint"].is_string());
        }
        let (status, json) =
            call(&state, "GET", "/agent/logs?level=error&process=SpringBoard&limit=5", &[], "").await;
        assert_eq!(status, StatusCode::OK, "{json}");

        let (status, json) = call(&state, "POST", "/agent/logs", CONTROL, r#"{"action":"tail"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(json["error"], "invalid_logs_action");
        let (status, json) = call(&state, "POST", "/agent/logs", CONTROL, r#"{"action":"start"}"#).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
        assert_eq!(json["error"], "device_not_configured");
        for action in ["stop", "clear"] {
            let body = format!(r#"{{"action":"{action}"}}"#);
            let (status, json) = call(&state, "POST", "/agent/logs", CONTROL, &body).await;
            assert_eq!(status, StatusCode::OK, "{action}: {json}");
            assert_eq!(json, serde_json::json!({"ok": true, "capturing": false}));
        }
    });
}

#[test]
fn crashes_and_location_validate_before_touching_the_phone() {
    state_dir();
    block(async {
        let state = build_state(None);
        for bad in ["..%2Fsecret.ips", "notes.txt", ".hidden.ips"] {
            let (status, json) = call(&state, "GET", &format!("/agent/crashes/{bad}"), &[], "").await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {json}");
            assert_eq!(json["error"], "invalid_name", "{bad}");
        }
        let (status, json) = call(&state, "GET", "/agent/crashes?limit=0", &[], "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        for uri in ["/agent/crashes", "/agent/crashes/App-2026-10-11-070102.ips"] {
            let (status, json) = call(&state, "GET", uri, &[], "").await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}: {json}");
            assert_eq!(json["error"], "device_not_configured");
            assert!(json["hint"].as_str().unwrap().contains("IPHONE_USE_UDID"));
        }

        for bad in [r#"{"lat":95,"lon":0}"#, r#"{"lat":1}"#, "nope", r#"{"clear":1}"#] {
            let (status, json) = call(&state, "POST", "/agent/location", CONTROL, bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {json}");
            assert_eq!(json["error"], "invalid_location", "{bad}");
        }
        let (status, json) =
            call(&state, "POST", "/agent/location", CONTROL, r#"{"lat":35.68,"lon":139.76}"#).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
        assert_eq!(json["error"], "device_not_configured");

        // A configured phone that is not reachable: a device error with a
        // next step, never a 500 or a hang.
        let state = state_with(|state| state.device_udid = Some("00008110-000A1B2C3D4E5F60".into()));
        for (method, uri, body) in [
            ("POST", "/agent/location", r#"{"clear":true}"#),
            ("GET", "/agent/crashes", ""),
            ("POST", "/agent/logs", r#"{"action":"start"}"#),
        ] {
            let (status, json) = call(&state, method, uri, CONTROL, body).await;
            assert!(
                status == StatusCode::BAD_GATEWAY || status == StatusCode::GATEWAY_TIMEOUT,
                "{uri}: {status} {json}"
            );
            assert!(
                ["device_service_failed", "device_timeout"].contains(&json["error"].as_str().unwrap()),
                "{uri}: {json}"
            );
            assert!(json["hint"].is_string(), "{uri}: {json}");
        }
        let (_, activity) = call(&state, "GET", "/agent/activity", &[], "").await;
        assert!(
            !activity.to_string().contains("simulated location"),
            "a failed call is not on the feed: {activity}"
        );
    });
}

#[test]
fn recording_starts_once_and_stops_with_a_clear_answer() {
    let dir = state_dir().join("runs").join("recordings");
    block(async {
        // No video pipeline: nothing to record.
        let state = build_state(None);
        let (status, json) = call(&state, "POST", "/agent/record", CONTROL, r#"{"action":"start"}"#).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
        assert_eq!(json["error"], "video_unavailable");
        let (status, json) = call(&state, "POST", "/agent/record", CONTROL, r#"{"action":"stop"}"#).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json, serde_json::json!({"ok": true, "summary": null}));
        let (status, json) = call(&state, "GET", "/agent/record", &[], "").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json, serde_json::json!({"ok": true, "recording": null}));
        let (status, json) = call(&state, "POST", "/agent/record", CONTROL, r#"{"action":"pause"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");

        // A hub whose stream never yields a frame.
        let state = state_with(|state| {
            state.video = Some(server::video::VideoHub::new("http://127.0.0.1:9".into()));
        });
        let (status, json) = call(&state, "POST", "/agent/record", CONTROL, r#"{"action":"start"}"#).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let path = json["recording"]["path"].as_str().unwrap().to_string();
        assert!(std::path::Path::new(&path).starts_with(&dir), "{path}");
        assert!(path.ends_with(".mp4"), "{path}");
        assert_eq!(json["recording"]["frames"], 0);

        let (_, json) = call(&state, "GET", "/agent/record", &[], "").await;
        assert_eq!(json["recording"]["path"], path.as_str(), "{json}");
        let (status, json) = call(&state, "POST", "/agent/record", CONTROL, r#"{"action":"start"}"#).await;
        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert_eq!(json["error"], "already_recording");

        let (status, json) = call(&state, "POST", "/agent/record", CONTROL, r#"{"action":"stop"}"#).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{json}");
        assert_eq!(json["error"], "recording_empty", "{json}");
        assert!(!std::path::Path::new(&path).exists(), "no file without frames");
        let (_, json) = call(&state, "GET", "/agent/record", &[], "").await;
        assert_eq!(json["recording"], serde_json::Value::Null, "{json}");

        let (_, activity) = call(&state, "GET", "/agent/activity", &[], "").await;
        let texts: Vec<&str> = activity["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect();
        assert!(texts.contains(&"Started recording"), "{activity}");
    });
}
