//! `POST /agent/login` moves credentials from the vault to the phone inside the
//! daemon. A sentinel password and username go in through a scripted `bwu`;
//! they must reach the phone (the runner's value writes and key presses) and
//! nothing else: not the HTTP responses, not an element read or diff, not the
//! flow draft, not the timing log, not a log line, not the `bwu` argv.

mod support;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state_with_wda, mock_wda_with_apps};

const PASSWORD: &str = "SENTINEL_PASS_9zz";
const USERNAME: &str = "SENTINEL_USER_7fq@x.com";
const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;

/// Fields on the scripted login page: a search box at the top, then the form.
const SEARCH_Y: f64 = 60.0;
const ACCOUNT_Y: f64 = 200.0;
const PASSWORD_Y: f64 = 260.0;
const LOGIN_Y: f64 = 380.0;
const FIELD_H: f64 = 44.0;

#[derive(Clone, Copy, PartialEq)]
enum Runner {
    /// A native form: the direct element write lands.
    Native,
    /// A web form: the direct write is acknowledged and ignored; typing into
    /// the focused field lands.
    Web,
    /// A web form whose taps focus the search box instead of the field.
    Stray,
}

#[derive(Default)]
struct Phone {
    search: String,
    account: String,
    password: String,
    focus: Option<&'static str>,
    logged_in: bool,
    /// Element ids are handed out per lookup, as the runner does.
    lookups: usize,
    /// Every value write and key press, as the runner received it.
    received: Vec<String>,
}

impl Phone {
    fn field(&mut self, name: &str) -> &mut String {
        match name {
            "search" => &mut self.search,
            "acct" => &mut self.account,
            _ => &mut self.password,
        }
    }

    fn tree(&self) -> String {
        if self.logged_in {
            return r#"{"value":{"type":"XCUIElementTypeApplication","label":"Shop",
                "rect":{"x":0,"y":0,"width":390,"height":844},"children":[
                  {"type":"XCUIElementTypeStaticText","label":"Welcome back",
                   "rect":{"x":20,"y":120,"width":350,"height":30},"children":[]}
                ]}}"#
                .to_string();
        }
        let value = |text: &str| {
            if text.is_empty() {
                String::new()
            } else {
                format!(r#""value":{},"#, serde_json::to_string(text).unwrap())
            }
        };
        format!(
            r#"{{"value":{{"type":"XCUIElementTypeApplication","label":"Shop",
            "rect":{{"x":0,"y":0,"width":390,"height":844}},"children":[
              {{"type":"XCUIElementTypeTextField","label":"","placeholderValue":"Search",{search}
                "rect":{{"x":20,"y":{SEARCH_Y},"width":350,"height":{FIELD_H}}},"isEnabled":true,"children":[]}},
              {{"type":"XCUIElementTypeTextField","label":"","placeholderValue":"Email",{account}
                "rect":{{"x":20,"y":{ACCOUNT_Y},"width":350,"height":{FIELD_H}}},"isEnabled":true,"children":[]}},
              {{"type":"XCUIElementTypeSecureTextField","label":"","placeholderValue":"Password",{password}
                "rect":{{"x":20,"y":{PASSWORD_Y},"width":350,"height":{FIELD_H}}},"isEnabled":true,"children":[]}},
              {{"type":"XCUIElementTypeButton","label":"Forgot password",
                "rect":{{"x":20,"y":320,"width":350,"height":{FIELD_H}}},"isEnabled":true,"children":[]}},
              {{"type":"XCUIElementTypeButton","label":"Log in",
                "rect":{{"x":20,"y":{LOGIN_Y},"width":350,"height":{FIELD_H}}},"isEnabled":true,"children":[]}}
            ]}}}}"#,
            search = value(&self.search),
            account = value(&self.account),
            // The worst case: the runner reporting the password itself.
            password = value(&self.password),
        )
    }
}

fn wda_value(text: &str) -> String {
    format!(r#"{{"value":{}}}"#, serde_json::to_string(text).unwrap())
}

fn rect(y: f64) -> String {
    format!(r#"{{"value":{{"x":20,"y":{y},"width":350,"height":{FIELD_H}}}}}"#)
}

fn tapped_y(body: &str) -> Option<f64> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    json["actions"][0]["actions"]
        .as_array()?
        .iter()
        .find(|step| step["type"] == "pointerMove")
        .and_then(|step| step["y"].as_f64())
}

fn scripted_runner(mode: Runner, phone: Arc<Mutex<Phone>>) -> support::MockWda {
    mock_wda_with_apps(r#"[{"bundleId":"com.example.shop"}]"#, move |request, _| {
        let line = request.lines().next().unwrap_or("");
        let body = request.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        let reply = |s: String| Some((Duration::ZERO, s));
        let mut phone = phone.lock().unwrap();
        if line.starts_with("POST /session ") {
            return reply(SESSION.to_string());
        }
        if line.contains("/source") {
            return reply(phone.tree());
        }
        if line.starts_with("POST /session/SESSION/elements ") {
            phone.lookups += 1;
            let n = phone.lookups;
            if body.contains("SecureTextField") {
                return reply(format!(r#"{{"value":[{{"ELEMENT":"pw-{n}"}}]}}"#));
            }
            if body.contains("TextField") {
                return reply(format!(
                    r#"{{"value":[{{"ELEMENT":"search-{n}"}},{{"ELEMENT":"acct-{n}"}}]}}"#
                ));
            }
            if body.contains("Button") {
                return reply(format!(
                    r#"{{"value":[{{"ELEMENT":"forgot-{n}"}},{{"ELEMENT":"login-{n}"}}]}}"#
                ));
            }
            return reply(r#"{"value":[]}"#.to_string());
        }
        // The buttons: Forgot password, then Log in.
        for (id, y) in [("forgot", 320.0), ("login", LOGIN_Y)] {
            if !line.contains(&format!("/session/SESSION/element/{id}-")) {
                continue;
            }
            if line.contains("/rect") {
                return reply(rect(y));
            }
            if line.contains("/click") && id == "login" {
                phone.logged_in = phone.account == USERNAME && phone.password == PASSWORD;
            }
            return reply(r#"{"value":null}"#.to_string());
        }
        for (id, y, placeholder) in [
            ("search", SEARCH_Y, "Search"),
            ("acct", ACCOUNT_Y, "Email"),
            ("pw", PASSWORD_Y, "Password"),
        ] {
            let element = format!("/session/SESSION/element/{id}-");
            if !line.contains(&element) {
                continue;
            }
            if line.ends_with("/rect HTTP/1.1") || line.contains("/rect ") {
                return reply(rect(y));
            }
            if line.contains("/attribute/value") {
                // Web inputs read back their placeholder while empty; a
                // password field reads back bullets.
                let now = phone.field(id).clone();
                return reply(wda_value(match (now.is_empty(), id) {
                    (true, _) => placeholder,
                    (false, "pw") => "•••••••••••••••••",
                    (false, _) => &now,
                }));
            }
            if line.contains("/clear") {
                phone.field(id).clear();
            }
            if line.contains("/click") {
                phone.focus = Some(if mode == Runner::Stray { "search" } else { id });
            }
            if line.contains("/value ") {
                phone.received.push(body.clone());
                if mode == Runner::Native {
                    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
                    *phone.field(id) = json["text"].as_str().unwrap_or_default().to_string();
                }
            }
            return reply(r#"{"value":null}"#.to_string());
        }
        if line.contains("/wda/keys") {
            phone.received.push(body.clone());
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            let text: String = json["value"]
                .as_array()
                .map(|parts| parts.iter().filter_map(|p| p.as_str()).collect())
                .unwrap_or_default();
            if let Some(focus) = phone.focus {
                phone.field(focus).push_str(&text);
            }
        }
        if line.contains("/actions") {
            if let Some(y) = tapped_y(&body) {
                let hit = |top: f64| y >= top && y <= top + FIELD_H;
                if hit(LOGIN_Y) {
                    phone.logged_in = phone.account == USERNAME && phone.password == PASSWORD;
                } else if hit(ACCOUNT_Y) || hit(PASSWORD_Y) {
                    phone.focus = Some(match mode {
                        Runner::Stray => "search",
                        _ if hit(ACCOUNT_Y) => "acct",
                        _ => "pw",
                    });
                } else if hit(SEARCH_Y) {
                    phone.focus = Some("search");
                }
            }
        }
        reply(r#"{"value":null}"#.to_string())
    })
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

struct Harness {
    _dir: tempfile::TempDir,
    logs: Sink,
    timing: PathBuf,
    argv: PathBuf,
}

/// One scripted `bwu`, one log sink and one timing log for the whole binary
/// (the env var and the subscriber are process-wide).
fn harness() -> &'static Harness {
    static HARNESS: OnceLock<Harness> = OnceLock::new();
    HARNESS.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let logs = Sink::default();
        let writer = logs.clone();
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .try_init();
        let timing = dir.path().join("agent-timing.jsonl");
        server::timing::set_log_path(timing.clone());
        let argv = dir.path().join("bwu-argv.log");
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
            argv = argv.display()
        )
        .unwrap();
        drop(script);
        std::fs::set_permissions(&bwu, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        std::env::set_var("IPHONE_USE_BWU", &bwu);
        Harness {
            _dir: dir,
            logs,
            timing,
            argv,
        }
    })
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

fn assert_clean(what: &str, text: &str) {
    assert!(
        !text.contains(PASSWORD),
        "{what} leaked the password: {text}"
    );
    assert!(
        !text.contains(USERNAME),
        "{what} leaked the username: {text}"
    );
}

/// Logs, timing records and the bwu argv, checked after every scenario.
fn assert_side_channels_clean() {
    let harness = harness();
    let logs = String::from_utf8_lossy(&harness.logs.0.lock().unwrap()).into_owned();
    let timing = std::fs::read_to_string(&harness.timing).unwrap_or_default();
    let argv = std::fs::read_to_string(&harness.argv).unwrap_or_default();
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
        "the timing log was written, so the check means something"
    );
}

#[test]
fn credentials_reach_the_phone_and_nothing_else() {
    harness();
    let phone = Arc::new(Mutex::new(Phone::default()));
    let wda = scripted_runner(Runner::Native, phone.clone());
    let state = build_state_with_wda(wda.url());
    // The worst case for a read: the runner reporting a password field's
    // contents verbatim.
    phone.lock().unwrap().password = PASSWORD.to_string();

    block(async {
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
        phone.lock().unwrap().password.clear();

        let (status, login) = call(&state, "POST", "/agent/login", "{}").await;
        assert_eq!(status, StatusCode::OK, "{login}");
        let json: serde_json::Value = serde_json::from_str(&login).unwrap();
        assert_eq!(json["ok"], true, "{login}");
        assert_eq!(json["entry"], "Shop");
        assert_eq!(json["account"], "SE***@x.com");
        assert_eq!(json["filled"], serde_json::json!(["account", "password"]));
        assert_eq!(json["submitted"], true, "{login}");
        assert!(
            phone.lock().unwrap().logged_in,
            "the scripted app accepted the login"
        );

        let (_, diff) = call(
            &state,
            "GET",
            &format!("/agent/elements?since={snapshot}"),
            "",
        )
        .await;
        let (_, draft) = call(&state, "GET", "/agent/flow/draft", "").await;
        let (_, missing) = call(&state, "POST", "/agent/login", r#"{"item":"nope"}"#).await;
        let (_, bad) = call(&state, "POST", "/agent/login", r#"{"password":"x"}"#).await;
        for (what, text) in [
            ("login response", &login),
            ("element diff", &diff),
            ("flow draft", &draft),
            ("no-entry response", &missing),
            ("invalid request", &bad),
        ] {
            assert_clean(what, text);
        }
        assert!(
            !missing.contains("--"),
            "a refusal names no bypass flag: {missing}"
        );
    });

    let received = phone.lock().unwrap().received.join("\n");
    assert!(
        received.contains(PASSWORD),
        "password never reached the phone"
    );
    assert!(
        received.contains(USERNAME),
        "username never reached the phone"
    );
    assert_side_channels_clean();
}

/// A web form ignores the direct write (hardware: Safari, iOS 27): the login
/// focuses each field and types into it instead.
#[test]
fn a_web_form_is_filled_by_typing_into_the_focused_field() {
    harness();
    let phone = Arc::new(Mutex::new(Phone::default()));
    let wda = scripted_runner(Runner::Web, phone.clone());
    let state = build_state_with_wda(wda.url());
    block(async {
        let (status, login) = call(&state, "POST", "/agent/login", "{}").await;
        assert_eq!(status, StatusCode::OK, "{login}");
        assert_clean("login response", &login);
        let json: serde_json::Value = serde_json::from_str(&login).unwrap();
        assert_eq!(
            json["filled"],
            serde_json::json!(["account", "password"]),
            "{login}"
        );
        assert!(
            phone.lock().unwrap().logged_in,
            "the scripted app accepted the login"
        );
    });
    assert_side_channels_clean();
}

/// When typing into the focus puts the value somewhere else, that field is
/// cleared at once and the login stops: a password must not sit in a plain
/// field on screen.
#[test]
fn a_value_that_lands_in_another_field_is_cleared() {
    harness();
    let phone = Arc::new(Mutex::new(Phone::default()));
    let wda = scripted_runner(Runner::Stray, phone.clone());
    let state = build_state_with_wda(wda.url());
    block(async {
        let (status, login) = call(&state, "POST", "/agent/login", "{}").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{login}");
        assert_clean("login response", &login);
        let json: serde_json::Value = serde_json::from_str(&login).unwrap();
        assert_eq!(json["error"], "value_landed_elsewhere", "{login}");
    });
    let phone = phone.lock().unwrap();
    assert!(
        !phone.search.contains(USERNAME),
        "the stray value was cleared"
    );
    assert!(!phone.logged_in);
    drop(phone);
    assert_side_channels_clean();
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
