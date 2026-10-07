//! `POST /agent/login` moves credentials from the vault to the phone inside the
//! daemon. A sentinel password and username go in through a scripted `bwu`;
//! they must reach the phone (the runner's value writes) and nothing else:
//! not the HTTP responses, not an element read or diff, not the flow draft,
//! not the timing log, not a log line, not the `bwu` argv.

mod support;

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state_with_wda, mock_wda_with_apps};

const PASSWORD: &str = "SENTINEL_PASS_9zz";
const USERNAME: &str = "SENTINEL_USER_7fq@x.com";
const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;

fn login_tree() -> String {
    format!(
        r#"{{"value":{{"type":"XCUIElementTypeApplication","label":"Shop",
        "rect":{{"x":0,"y":0,"width":390,"height":844}},"children":[
          {{"type":"XCUIElementTypeTextField","label":"","placeholderValue":"Email",
            "rect":{{"x":20,"y":200,"width":350,"height":44}},"isEnabled":true,"children":[]}},
          {{"type":"XCUIElementTypeSecureTextField","label":"","placeholderValue":"Password",
            "value":"{PASSWORD}",
            "rect":{{"x":20,"y":260,"width":350,"height":44}},"isEnabled":true,"children":[]}},
          {{"type":"XCUIElementTypeButton","label":"Forgot password",
            "rect":{{"x":20,"y":320,"width":350,"height":44}},"isEnabled":true,"children":[]}},
          {{"type":"XCUIElementTypeButton","label":"Log in",
            "rect":{{"x":20,"y":380,"width":350,"height":44}},"isEnabled":true,"children":[]}}
        ]}}}}"#
    )
}

fn home_tree() -> String {
    r#"{"value":{"type":"XCUIElementTypeApplication","label":"Shop",
        "rect":{"x":0,"y":0,"width":390,"height":844},"children":[
          {"type":"XCUIElementTypeStaticText","label":"Welcome back",
           "rect":{"x":20,"y":120,"width":350,"height":30},"children":[]}
        ]}}"#
        .to_string()
}

fn rect(y: f64) -> String {
    format!(r#"{{"value":{{"x":20,"y":{y},"width":350,"height":44}}}}"#)
}

/// A captured log sink for the whole test binary.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn call(
    state: &Arc<server::http::AppState>,
    method: &str,
    uri: &str,
    body: &str,
) -> (StatusCode, String) {
    let response = server::http::router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-phone-control", "1")
                .header("x-phone-owner", "login-test")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[test]
fn credentials_reach_the_phone_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();

    // Every log line of this binary, at every level.
    let sink = Sink::default();
    let writer = sink.clone();
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .try_init();
    let timing_log = dir.path().join("agent-timing.jsonl");
    server::timing::set_log_path(timing_log.clone());

    // A scripted bitwarden-use that records its argv.
    let argv_log = dir.path().join("bwu-argv.log");
    let bwu = dir.path().join("bwu");
    let mut script = std::fs::File::create(&bwu).unwrap();
    write!(
        script,
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{argv}'
case "$1" in
  unlocked) exit 0 ;;
  list) printf '%s' '[{{"id":"e-1","name":"Shop","user":"{USERNAME}","uris":["https://login.example.com/"]}},{{"id":"e-2","name":"Bank","user":"x@y.z","uris":["https://bank.example.org"]}}]' ;;
  get) printf '%s' '{{"data":{{"username":"{USERNAME}","password":"{PASSWORD}","totp":null}},"id":"e-1","name":"Shop"}}' ;;
  *) exit 1 ;;
esac
"#,
        argv = argv_log.display()
    )
    .unwrap();
    drop(script);
    std::fs::set_permissions(&bwu, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::env::set_var("IPHONE_USE_BWU", &bwu);

    // The runner: a login form until the Log in tap, then the signed-in page.
    let written: Arc<Mutex<Vec<String>>> = Arc::default();
    let logged_in = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (seen, done) = (written.clone(), logged_in.clone());
    let wda = mock_wda_with_apps(r#"[{"bundleId":"com.example.shop"}]"#, move |request, _| {
        let line = request.lines().next().unwrap_or("");
        let body = request.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        let reply = |s: String| Some((Duration::ZERO, s));
        if line.starts_with("POST /session ") {
            return reply(SESSION.to_string());
        }
        if line.contains("/source") {
            return reply(if done.load(std::sync::atomic::Ordering::Acquire) {
                home_tree()
            } else {
                login_tree()
            });
        }
        if line.starts_with("POST /session/SESSION/elements ") {
            if body.contains("SecureTextField") {
                return reply(r#"{"value":[{"ELEMENT":"pw"}]}"#.to_string());
            }
            if body.contains("TextField") {
                return reply(r#"{"value":[{"ELEMENT":"acct"}]}"#.to_string());
            }
            return reply(r#"{"value":[]}"#.to_string());
        }
        if line.starts_with("GET /session/SESSION/element/acct/rect") {
            return reply(rect(200.0));
        }
        if line.starts_with("GET /session/SESSION/element/pw/rect") {
            return reply(rect(260.0));
        }
        if line.starts_with("GET /session/SESSION/element/acct/attribute/value") {
            return reply(format!(r#"{{"value":"{USERNAME}"}}"#));
        }
        if line.contains("/value") {
            seen.lock().unwrap().push(format!("{line} {body}"));
        }
        if line.contains("/actions") && body.contains("pointerDown") {
            // Only the Log in button sits at y≈402.
            if body.contains("402") {
                done.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        reply(r#"{"value":null}"#.to_string())
    });
    let state = build_state_with_wda(wda.url());

    block(async {
        // A read before the login: the password field's contents are masked.
        let (status, before) = call(&state, "GET", "/agent/elements", "").await;
        assert_eq!(status, StatusCode::OK, "{before}");
        assert!(
            !before.contains(PASSWORD),
            "element read leaked the password: {before}"
        );
        assert!(
            before.contains("••••••••"),
            "the password field reads as filled: {before}"
        );
        let snapshot: serde_json::Value = serde_json::from_str(&before).unwrap();
        let snapshot = snapshot["snapshot"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let (status, login) = call(&state, "POST", "/agent/login", "{}").await;
        assert_eq!(status, StatusCode::OK, "{login}");
        let json: serde_json::Value = serde_json::from_str(&login).unwrap();
        assert_eq!(json["ok"], true, "{login}");
        assert_eq!(json["entry"], "Shop");
        assert_eq!(json["account"], "SE***@x.com");
        assert_eq!(json["filled"], serde_json::json!(["account", "password"]));
        assert_eq!(json["submitted"], true, "{login}");

        let (_, diff) = call(
            &state,
            "GET",
            &format!("/agent/elements?since={snapshot}"),
            "",
        )
        .await;
        let (_, draft) = call(&state, "GET", "/agent/flow/draft", "").await;
        let (_, ambiguous) = call(&state, "POST", "/agent/login", r#"{"item":"nope"}"#).await;
        let (_, bad) = call(&state, "POST", "/agent/login", r#"{"password":"x"}"#).await;

        for (what, text) in [
            ("login response", &login),
            ("element diff", &diff),
            ("flow draft", &draft),
            ("no-entry response", &ambiguous),
            ("invalid request", &bad),
        ] {
            assert!(
                !text.contains(PASSWORD),
                "{what} leaked the password: {text}"
            );
            assert!(
                !text.contains(USERNAME),
                "{what} leaked the username: {text}"
            );
        }
        assert!(
            !ambiguous.contains("--"),
            "a refusal names no bypass flag: {ambiguous}"
        );
    });

    // The phone got both values, through the runner's value writes.
    let written = written.lock().unwrap().join("\n");
    assert!(
        written.contains(PASSWORD),
        "password never reached the phone: {written}"
    );
    assert!(
        written.contains(USERNAME),
        "username never reached the phone: {written}"
    );

    // Nothing else did.
    let logs = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
    let timing = std::fs::read_to_string(&timing_log).unwrap_or_default();
    let argv = std::fs::read_to_string(&argv_log).unwrap();
    for (what, text) in [
        ("daemon log", &logs),
        ("timing log", &timing),
        ("bwu argv", &argv),
    ] {
        assert!(!text.contains(PASSWORD), "{what} leaked the password");
        assert!(!text.contains(USERNAME), "{what} leaked the username");
    }
    assert!(
        argv.contains("get --raw --reveal e-1"),
        "bwu was asked by id: {argv}"
    );
    assert!(
        !timing.is_empty(),
        "the timing log was written, so the check above means something"
    );
}

/// A refusal names no way around itself (chrome-use #433: weaker models copy a
/// bypass straight out of an error message). The takeover header stays in the
/// docs for a person; the `phone_owned` answer does not suggest it.
#[test]
fn a_phone_owned_refusal_suggests_no_takeover() {
    let wda = support::mock_wda(|_, _| Some((Duration::ZERO, SESSION.to_string())));
    let state = build_state_with_wda(wda.url());
    block(async {
        let claim = |owner: &'static str| {
            Request::builder()
                .method("POST")
                .uri("/agent/hold")
                .header("x-phone-control", "1")
                .header("x-phone-owner", owner)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"secs":0}"#))
                .unwrap()
        };
        let first = server::http::router(state.clone())
            .oneshot(claim("first"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let second = server::http::router(state.clone())
            .oneshot(claim("second"))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT);
        let body = second.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("phone_owned"), "{body}");
        assert!(!body.to_lowercase().contains("takeover"), "{body}");
    });
}
