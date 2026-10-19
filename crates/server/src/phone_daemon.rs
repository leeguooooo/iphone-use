//! The daemon the device runner hosts on the phone (`crates/ios-embed`), as
//! the Mac sees it.
//!
//! - **Credentials.** Setup gives the phone's daemon an agent bearer token and
//!   a control password. They are generated once per instance and kept in the
//!   instance state dir (0600), so every relaunch of the runner hands the
//!   phone the same ones and an app paired with that daemon stays paired.
//!   Nothing logs them.
//! - **Status.** The runner's `GET /status` carries `daemon: {linked, state,
//!   port, error}`; the Mac daemon keeps the last one it saw and reports it as
//!   `phone_daemon` in `/agent/status`.
//! - **Pairing.** `POST /agent/phone-daemon/pair` asks the runner (signed with
//!   its launch token, through the same relay as every runner request) for a
//!   one-time code minted by the phone's daemon in-process, and answers with
//!   the `iphoneuse://pair` link (and its QR code) that pairs the iPhone app
//!   with the daemon on its own phone at `http://127.0.0.1:<port>`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

use crate::http::{authorize_extension, secured, AppState};

/// The agent bearer token file in the instance state dir.
pub const TOKEN_FILE: &str = "phone-daemon-token";
/// The control password file in the instance state dir.
pub const PASSWORD_FILE: &str = "phone-daemon-password";
/// The port the runner starts its daemon on unless told otherwise
/// (`RunnerDaemon.defaultPort`).
pub const DEFAULT_PORT: u16 = 8787;
/// xcodebuild hands `TEST_RUNNER_<NAME>` to the runner as `<NAME>`.
pub const XCODEBUILD_PREFIX: &str = "TEST_RUNNER_";
/// Where the phone's daemon listens (`127.0.0.1` when unset; `0.0.0.0` lets
/// other devices drive this phone through it).
pub const HOST_ENV: &str = "IPU_DAEMON_HOST";

/// What the phone's daemon is started with. `Debug` never shows the values.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub token: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials { token: <redacted>, password: <redacted> }")
    }
}

/// The stored credentials, created on first use. A missing or unreadable
/// file is regenerated; the other one is kept.
pub fn load_or_create(state_dir: &Path) -> Result<Credentials, String> {
    let token = match read_secret(&state_dir.join(TOKEN_FILE)) {
        Some(token) => token,
        None => {
            let token = core::runner_auth::new_token()?;
            write_secret(state_dir, TOKEN_FILE, &token)?;
            token
        }
    };
    let password = match read_secret(&state_dir.join(PASSWORD_FILE)) {
        Some(password) => password,
        None => {
            // 128 bits as 32 hex digits: typed by nobody (the QR pairs the app).
            let password = core::runner_auth::new_token()?[..32].to_string();
            write_secret(state_dir, PASSWORD_FILE, &password)?;
            password
        }
    };
    Ok(Credentials { token, password })
}

/// One line of lowercase hex from a plain file (never a symlink).
fn read_secret(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.len() > 1024 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let value = text.trim();
    core::runner_auth::valid_token(value).then(|| value.to_string())
}

/// Store `value` as `<state_dir>/<name>` (0600, atomically, never through a
/// symlink).
fn write_secret(state_dir: &Path, name: &str, value: &str) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    let path: PathBuf = state_dir.join(name);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("refusing to replace symlinked {}", path.display()));
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .tempfile_in(state_dir)
        .map_err(|e| format!("{}: {e}", state_dir.display()))?;
    std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    temporary
        .write_all(format!("{value}\n").as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    temporary
        .persist(&path)
        .map_err(|e| format!("{}: {}", path.display(), e.error))?;
    Ok(())
}

/// A usable `IPU_DAEMON_HOST`: an IP address literal (`0.0.0.0`, a LAN
/// address, `127.0.0.1`).
pub fn valid_host(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// The environment that starts the phone's daemon with `credentials`, each
/// name behind `prefix` (`TEST_RUNNER_` for xcodebuild, empty for the iOS
/// 15/16 launcher, which puts the names straight into the test environment).
/// `host` only when set: the runner defaults to loopback.
pub fn runner_env(
    credentials: &Credentials,
    host: Option<&str>,
    prefix: &str,
) -> Vec<(String, String)> {
    let mut env = vec![
        (
            format!("{prefix}IPU_DAEMON_TOKEN"),
            credentials.token.clone(),
        ),
        (
            format!("{prefix}IPU_DAEMON_PASSWORD"),
            credentials.password.clone(),
        ),
    ];
    if let Some(host) = host.map(str::trim).filter(|h| !h.is_empty()) {
        env.push((format!("{prefix}{HOST_ENV}"), host.to_string()));
    }
    env
}

// ── status ──────────────────────────────────────────────────────────────────

/// The runner's last `/status` `daemon` object (one runner per daemon process).
static LAST_STATUS: Mutex<Option<serde_json::Value>> = Mutex::new(None);

/// Record what a successful runner `/status` said about its daemon. A runner
/// that says nothing (WebDriverAgent, an older runner) clears it.
pub fn note_runner_status(body: &serde_json::Value) {
    *LAST_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = daemon_of(body);
}

/// `value.daemon` of a runner `/status` answer, when it is an object.
pub fn daemon_of(body: &serde_json::Value) -> Option<serde_json::Value> {
    body.get("value")
        .and_then(|value| value.get("daemon"))
        .filter(|daemon| daemon.is_object())
        .cloned()
}

/// The last seen `daemon` object.
pub fn last_status() -> Option<serde_json::Value> {
    LAST_STATUS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// `phone_daemon` in `/agent/status`: the last runner report while the runner
/// answers, else `null`.
pub fn status_json(runner_up: bool) -> String {
    runner_up
        .then(last_status)
        .flatten()
        .map_or_else(|| "null".to_string(), |value| value.to_string())
}

// ── pairing ─────────────────────────────────────────────────────────────────

/// The runner's address and signing, detached from the `RunnerClient` lock (see
/// [`crate::runner::RunnerClient::phone_daemon_endpoint`]).
pub struct Endpoint {
    pub(crate) base: String,
    pub(crate) http: reqwest::Client,
    pub(crate) auth: Arc<crate::runner_token::TokenSource>,
}

/// Why no pairing code came back.
#[derive(Debug)]
pub enum PairError {
    /// The runner is unreachable.
    Unreachable(String),
    /// The runner has no `/daemon/pair-code` (older runner, WebDriverAgent).
    Unsupported,
    /// The runner answered but its daemon cannot mint a code (not linked,
    /// not serving).
    Unavailable(String),
}

impl Endpoint {
    /// `POST /daemon/pair-code` → (code, port of the phone's daemon).
    pub async fn pair_code(&self) -> Result<(String, u16), PairError> {
        let request = self
            .http
            .post(format!("{}/daemon/pair-code", self.base))
            .json(&serde_json::json!({}))
            .timeout(Duration::from_secs(5));
        let response = crate::runner_token::send(request, &self.auth)
            .await
            .map_err(|error| PairError::Unreachable(format!("{error}")))?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(PairError::Unsupported);
        }
        let value = body.get("value").cloned().unwrap_or_default();
        if !status.is_success() {
            let message = value
                .get("message")
                .or_else(|| value.get("error"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("the runner refused")
                .to_string();
            return Err(PairError::Unavailable(format!(
                "HTTP {}: {message}",
                status.as_u16()
            )));
        }
        parse_pair_code(&value).ok_or_else(|| {
            PairError::Unavailable("the runner answered without a pairing code".into())
        })
    }
}

/// `{"code": "<base64url>", "port": 8787}` from the runner.
pub fn parse_pair_code(value: &serde_json::Value) -> Option<(String, u16)> {
    let code = value.get("code")?.as_str()?;
    let shape_ok = !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !shape_ok {
        return None;
    }
    let port = value
        .get("port")
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p > 0)
        .unwrap_or(DEFAULT_PORT);
    Some((code.to_string(), port))
}

/// The link that opens the iPhone app and pairs it with the daemon on its
/// own phone.
pub fn app_link(port: u16, code: &str) -> String {
    format!(
        "iphoneuse://pair?u={}&c={code}",
        percent_encode(&format!("http://127.0.0.1:{port}"))
    )
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn json(status: StatusCode, value: serde_json::Value) -> Response {
    secured(
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

fn refusal(status: StatusCode, error: &str, hint: &str) -> Response {
    json(
        status,
        serde_json::json!({"ok": false, "error": error, "hint": hint}),
    )
}

/// `POST /agent/phone-daemon/pair` — a one-time link (and QR code) that pairs
/// the iPhone app with the daemon running inside this phone's runner. Agent
/// bearer or a signed-in browser, plus `X-Phone-Control: 1`.
pub async fn pair(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_extension(&state, &headers, true) {
        return response;
    }
    let Some(runner) = state.runner.clone() else {
        return refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_runner",
            "this daemon has no device runner configured",
        );
    };
    let endpoint = match tokio::time::timeout(Duration::from_secs(5), runner.lock()).await {
        Ok(client) => client.phone_daemon_endpoint(),
        Err(_) => {
            return refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "runner_busy",
                "the device runner is busy with another command; try again in a moment",
            )
        }
    };
    match endpoint.pair_code().await {
        Ok((code, port)) => {
            let link = app_link(port, &code);
            json(
                StatusCode::OK,
                serde_json::json!({
                    "ok": true,
                    "link": link,
                    "port": port,
                    "expires_in_secs": crate::pairing::CODE_TTL.as_secs(),
                    "svg": crate::pairing::qr_svg(&link),
                }),
            )
        }
        Err(PairError::Unreachable(error)) => {
            tracing::warn!("phone daemon pairing: runner unreachable: {error}");
            refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "runner_unreachable",
                "the device runner did not answer; reconnect the phone and try again",
            )
        }
        Err(PairError::Unsupported) => refusal(
            StatusCode::CONFLICT,
            "runner_too_old",
            "this phone's runner predates the on-phone daemon; rerun setup to rebuild it",
        ),
        Err(PairError::Unavailable(error)) => {
            tracing::warn!("phone daemon pairing: {error}");
            refusal(
                StatusCode::CONFLICT,
                "phone_daemon_unavailable",
                "the runner on this phone has no daemon running (built without the embed library, or it failed to start; see phone_daemon in /agent/status)",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn credentials_are_generated_once_private_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create(dir.path()).unwrap();
        assert_eq!(first.token.len(), 64);
        assert!(core::runner_auth::valid_token(&first.token));
        assert_eq!(first.password.len(), 32);
        assert_ne!(first.token[..32], first.password);
        for name in [TOKEN_FILE, PASSWORD_FILE] {
            let mode = std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
        let second = load_or_create(dir.path()).unwrap();
        assert_eq!(first, second, "a second run must reuse the stored values");
        assert!(!format!("{first:?}").contains(&first.token));
        assert!(!format!("{first:?}").contains(&first.password));
    }

    #[test]
    fn a_damaged_credential_is_replaced_and_the_other_kept() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create(dir.path()).unwrap();
        std::fs::write(dir.path().join(TOKEN_FILE), "not hex\n").unwrap();
        let second = load_or_create(dir.path()).unwrap();
        assert_ne!(first.token, second.token);
        assert_eq!(first.password, second.password);
    }

    #[test]
    fn a_symlinked_credential_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, "a".repeat(64)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join(TOKEN_FILE)).unwrap();
        assert!(load_or_create(dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "a".repeat(64));
    }

    #[test]
    fn runner_env_names_the_daemon_settings_for_xcodebuild() {
        let credentials = Credentials {
            token: "a".repeat(64),
            password: "b".repeat(32),
        };
        let env = runner_env(&credentials, None, XCODEBUILD_PREFIX);
        assert_eq!(
            env,
            vec![
                ("TEST_RUNNER_IPU_DAEMON_TOKEN".to_string(), "a".repeat(64)),
                (
                    "TEST_RUNNER_IPU_DAEMON_PASSWORD".to_string(),
                    "b".repeat(32)
                ),
            ]
        );
        let env = runner_env(&credentials, Some("0.0.0.0"), XCODEBUILD_PREFIX);
        assert_eq!(
            env.last().unwrap(),
            &(
                "TEST_RUNNER_IPU_DAEMON_HOST".to_string(),
                "0.0.0.0".to_string()
            )
        );
        let legacy = runner_env(&credentials, Some(" "), "");
        assert_eq!(
            legacy.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["IPU_DAEMON_TOKEN", "IPU_DAEMON_PASSWORD"]
        );
        assert!(valid_host("0.0.0.0") && valid_host("127.0.0.1") && valid_host("::"));
        assert!(!valid_host("phone.local") && !valid_host("0.0.0.0 -x"));
    }

    #[test]
    fn status_reports_the_runner_daemon_object_or_null() {
        let body = serde_json::json!({"value": {"ready": true, "daemon": {"linked": true, "state": "serving", "port": 8787}}});
        assert_eq!(
            daemon_of(&body),
            Some(serde_json::json!({"linked": true, "state": "serving", "port": 8787}))
        );
        assert_eq!(
            daemon_of(&serde_json::json!({"value": {"ready": true}})),
            None
        );
        assert_eq!(
            daemon_of(&serde_json::json!({"value": {"daemon": "x"}})),
            None
        );
        assert_eq!(status_json(false), "null");
    }

    #[test]
    fn the_link_points_the_app_at_its_own_phone() {
        assert_eq!(
            app_link(8787, "abc_DEF-1"),
            "iphoneuse://pair?u=http%3A%2F%2F127.0.0.1%3A8787&c=abc_DEF-1"
        );
        assert_eq!(
            parse_pair_code(&serde_json::json!({"code": "abc", "port": 9000})),
            Some(("abc".to_string(), 9000))
        );
        assert_eq!(
            parse_pair_code(&serde_json::json!({"code": "abc"})),
            Some(("abc".to_string(), DEFAULT_PORT))
        );
        assert_eq!(parse_pair_code(&serde_json::json!({"code": "a&b"})), None);
        assert_eq!(parse_pair_code(&serde_json::json!({})), None);
    }

    mod fixture {
        use crate as srv;
        use crate::http::AppState;
        use std::sync::{Arc, Mutex};
        include!("../tests/fixtures/app_state.rs");
        pub fn state(password: Option<&str>) -> Arc<AppState> {
            fixture_app_state(password)
        }
    }

    fn block<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn pairing_refuses_without_auth_or_control_header() {
        let state = fixture::state(Some("secret-password"));
        let response = block(pair(State(state.clone()), HeaderMap::new()));
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mut wrong = HeaderMap::new();
        wrong.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        wrong.insert("x-phone-control", "1".parse().unwrap());
        let response = block(pair(State(state.clone()), wrong));
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mut no_header = HeaderMap::new();
        no_header.insert(
            header::AUTHORIZATION,
            "Bearer secret-password".parse().unwrap(),
        );
        let response = block(pair(State(state.clone()), no_header));
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        // Authorized, but no runner configured in the fixture.
        let mut ok = HeaderMap::new();
        ok.insert(
            header::AUTHORIZATION,
            "Bearer secret-password".parse().unwrap(),
        );
        ok.insert("x-phone-control", "1".parse().unwrap());
        let response = block(pair(State(state), ok));
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
