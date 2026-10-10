//! A daemon that outlives runner relaunches: every launch gets a fresh token,
//! setup hands the new runner over, and the daemon must reach it without a
//! restart.
//!
//! The real router is served over TCP against a fake device runner that signs
//! like the real one: it refuses any request not signed with its current
//! launch's token, and a relaunch starts a new token and a new session
//! namespace. Reconnects use a fake supervisor bootstrap (no launchd) that
//! relaunches that runner the way setup does: rotate `runner-token`, then
//! serve with it.
//!
//! The field incident these pin down (v0.17.14): a reconnect was begun by a
//! `POST /agent/mode` whose request did not stay to the end. The handler
//! awaited the supervisor bootstrap itself, so cancelling the request
//! cancelled the round half-way: it had taken the lifecycle (`reconnecting`)
//! but never spawned the readiness wait that ends it. While reconnecting,
//! status answers from a health cache only that wait refreshes, so every
//! runner setup brought up afterwards was reported `wda:false`, setup failed
//! each verified handoff with `daemon-fail`, and only restarting the daemon
//! ended the loop.
//!
//! Hand-built runtimes instead of `#[tokio::test]`: the local crate named
//! `core` would shadow the std `core` the macro expands to.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;

use server::http::AppState;

use server as srv;
include!("fixtures/app_state.rs");

const UDID: &str = "00008150-000A60EC1A02401C";

/// Every test here shares the process-wide instance (its state dir holds the
/// one `runner-token`), so they run one at a time.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The instance state dir: a temp dir pinned through `PHONE_REMOTE_STATE_DIR`
/// before anything resolves the instance, holding the `setup-wda.sh` the
/// reconnect handler insists on (never run: the bootstrap is faked).
fn state_dir() -> &'static std::path::Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("setup-wda.sh"), "#!/bin/bash\nexit 0\n").unwrap();
        std::env::set_var("PHONE_REMOTE_STATE_DIR", dir.path());
        std::env::remove_var("PHONE_REMOTE_INSTANCE");
        assert_eq!(server::instance::current().state_dir, dir.path());
        dir
    })
    .path()
}

/// One launch of the fake runner.
#[derive(Clone, Default)]
struct Launch {
    token: String,
    number: usize,
}

/// A device runner that verifies request signatures like the real one.
struct FakeRunner {
    url: String,
    launch: Mutex<Launch>,
    /// Requests refused for a bad or missing signature.
    refused: std::sync::atomic::AtomicUsize,
}

impl FakeRunner {
    /// Relaunch: a new token, written to the state dir first (as setup does
    /// right before it starts the runner), and a new session namespace.
    fn relaunch(&self) -> String {
        let token = server::runner_token::rotate(state_dir()).unwrap();
        self.serve_with(&token);
        token
    }

    /// Serve with `token` without touching the state dir.
    fn serve_with(&self, token: &str) {
        let mut launch = self.launch.lock().unwrap();
        launch.number += 1;
        launch.token = token.to_string();
    }
}

fn wda(body: &str) -> axum::response::Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn wda_error(status: StatusCode, code: &str, message: &str) -> axum::response::Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"value": {"error": code, "message": message}}).to_string(),
    )
        .into_response()
}

async fn runner_answer(
    runner: Arc<FakeRunner>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let launch = runner.launch.lock().unwrap().clone();
    let target = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if launch.token.is_empty()
        || server::core_crate::runner_auth::verify(
            &launch.token,
            method.as_str(),
            &target,
            &body,
            authorization,
            now,
        )
        .is_err()
    {
        runner
            .refused
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        return wda_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "request not signed with this launch's token",
        );
    }
    let session = format!("L{}", launch.number);
    let path = uri.path();
    match (method.as_str(), path) {
        ("GET", "/status") => wda(r#"{"value":{"ready":true}}"#),
        ("GET", "/wda/locked") => wda(r#"{"value":false}"#),
        ("POST", "/session") => wda(&format!(
            r#"{{"value":{{"sessionId":"{session}"}},"sessionId":"{session}"}}"#
        )),
        _ => {
            let Some(rest) = path.strip_prefix("/session/") else {
                return wda_error(StatusCode::NOT_FOUND, "unknown command", path);
            };
            let (sid, route) = rest.split_once('/').unwrap_or((rest, ""));
            if sid != session {
                // A relaunched runner knows nothing of the last one's session.
                return wda_error(StatusCode::NOT_FOUND, "invalid session id", sid);
            }
            match route {
                "appium/settings" => wda(r#"{"value":{}}"#),
                "wda/locked" => wda(r#"{"value":false}"#),
                "wda/apps/list" => {
                    wda(r#"{"value":[{"pid":1,"bundleId":"com.apple.springboard"}]}"#)
                }
                _ => wda_error(StatusCode::NOT_FOUND, "unknown command", route),
            }
        }
    }
}

/// Serve `router` on a fresh loopback port; returns its base URL.
async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{address}")
}

/// The runner the fake bootstrap relaunches (a `fn` pointer captures nothing).
static RUNNER: Mutex<Option<Arc<FakeRunner>>> = Mutex::new(None);

async fn start_runner() -> Arc<FakeRunner> {
    let shared: Arc<Mutex<Option<Arc<FakeRunner>>>> = Arc::new(Mutex::new(None));
    let lookup = shared.clone();
    let router = axum::Router::new().fallback(
        move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let runner = lookup.lock().unwrap().clone().unwrap();
            runner_answer(runner, method, uri, headers, body)
        },
    );
    let url = serve(router).await;
    let runner = Arc::new(FakeRunner {
        url,
        launch: Mutex::new(Launch::default()),
        refused: Default::default(),
    });
    *shared.lock().unwrap() = Some(runner.clone());
    *RUNNER.lock().unwrap() = Some(runner.clone());
    runner
}

/// Setup's part of a reconnect, as the supervisor does it: takes a moment
/// (here 1.5 s; launchd waits for the previous setup run to exit), then
/// relaunches the runner with a fresh token.
fn slow_bootstrap(_setup_sh: &str, _log: &str, udid: &str) -> bool {
    assert_eq!(udid, UDID);
    std::thread::sleep(Duration::from_millis(1500));
    let runner = RUNNER.lock().unwrap().clone().unwrap();
    runner.relaunch();
    true
}

/// A daemon managing the fake runner, released (idle) like the incident's.
fn daemon_state(runner: &FakeRunner) -> Arc<AppState> {
    let mut state = match Arc::try_unwrap(fixture_app_state(None)) {
        Ok(state) => state,
        Err(_) => unreachable!("fresh fixture"),
    };
    state.managed_wda = true;
    state.device_udid = Some(UDID.to_string());
    // The instance's token source: the daemon reads `<state dir>/runner-token`.
    state.wda = Some(Arc::new(tokio::sync::Mutex::new(
        server::wda::WdaClient::new(&runner.url).unwrap(),
    )));
    state.wda_bootstrap = slow_bootstrap;
    Arc::new(state)
}

async fn status(base: &str) -> serde_json::Value {
    let response = reqwest::get(format!("{base}/agent/status")).await.unwrap();
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

/// Poll status until `drivable:true`, or fail with the last answer.
async fn wait_drivable(base: &str, within: Duration) -> serde_json::Value {
    let deadline = Instant::now() + within;
    loop {
        let answer = status(base).await;
        if answer["drivable"] == true && answer["wda"] == true {
            return answer;
        }
        if Instant::now() >= deadline {
            panic!(
                "the daemon never reached the relaunched runner within {within:?}: wda={} reconnecting={} released={} drivable={}",
                answer["wda"], answer["reconnecting"], answer["released"], answer["drivable"]
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_reconnect_whose_request_is_cancelled_still_reaches_the_relaunched_runner() {
    let _serial = serial();
    state_dir();
    runtime().block_on(async {
        let runner = start_runner().await;
        // Launch 1 served before the daemon was idle-released.
        runner.relaunch();
        let state = daemon_state(&runner);
        state
            .released
            .store(true, std::sync::atomic::Ordering::Release);
        let daemon = serve(server::http::router(state.clone())).await;

        // A client asks for the phone, then goes away while the supervisor
        // is still starting (a client timeout, a closed browser tab).
        let address = daemon.trim_start_matches("http://").to_string();
        let body = r#"{"mode":"agent"}"#;
        let request = format!(
            "POST /agent/mode HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nX-Phone-Control: 1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        tokio::task::spawn_blocking(move || {
            let mut stream = std::net::TcpStream::connect(&address).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            stream.shutdown(std::net::Shutdown::Both).unwrap();
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
        })
        .await
        .unwrap();
        assert_eq!(
            status(&daemon).await["reconnecting"],
            true,
            "the reconnect began before the client left"
        );

        // The runner the bootstrap relaunched (launch 2, a new token) must be
        // reached without restarting the daemon.
        let answer = wait_drivable(&daemon, Duration::from_secs(20)).await;
        assert_eq!(answer["reconnecting"], false);
        assert_eq!(answer["released"], false);
        assert_eq!(runner.launch.lock().unwrap().number, 2);
    });
}

#[test]
fn a_verified_handoff_reaches_each_relaunch_and_names_why_it_cannot() {
    let _serial = serial();
    state_dir();
    runtime().block_on(async {
        let runner = start_runner().await;
        runner.relaunch();
        let state = daemon_state(&runner);
        let daemon = serve(server::http::router(state.clone())).await;
        let client = reqwest::Client::new();
        let handoff = || async {
            let response = client
                .post(format!("{daemon}/agent/runner-handoff"))
                .header("x-phone-control", "1")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            response.json::<serde_json::Value>().await.unwrap()
        };

        // Launch 1 is reached, and a session is cached on it.
        wait_drivable(&daemon, Duration::from_secs(10)).await;

        // Relaunch twice (each launch rotates the token and forgets its
        // sessions): a verified handoff reaches every new launch.
        for launch in 2..=3 {
            runner.relaunch();
            let answer = handoff().await;
            assert_eq!(answer["up"], true, "launch {launch}: {answer}");
            assert_eq!(answer["actionable"], true, "launch {launch}: {answer}");
            assert_eq!(answer["error"], serde_json::Value::Null);
            wait_drivable(&daemon, Duration::from_secs(10)).await;
        }

        // A runner serving a token the daemon cannot read (the launch's token
        // never reached the state dir): the handoff retries once and names
        // the cause instead of leaving setup with a bare daemon-fail.
        let refused_before = runner.refused.load(std::sync::atomic::Ordering::SeqCst);
        runner.serve_with(&server::core_crate::runner_auth::new_token().unwrap());
        let answer = handoff().await;
        assert_eq!(answer["up"], false, "{answer}");
        assert_eq!(answer["retried"], true, "{answer}");
        let error = answer["error"].as_str().unwrap_or_default();
        assert!(error.contains("HTTP 401"), "{error}");
        assert!(
            runner.refused.load(std::sync::atomic::Ordering::SeqCst) >= refused_before + 2,
            "probed, reset, probed again"
        );
        assert_eq!(status(&daemon).await["wda"], false);

        // Setup writes the token it launched with: the next handoff reaches it.
        runner.relaunch();
        let answer = handoff().await;
        assert_eq!(answer["up"], true, "{answer}");
        assert_eq!(answer["retried"], false, "{answer}");
        wait_drivable(&daemon, Duration::from_secs(10)).await;
    });
}
