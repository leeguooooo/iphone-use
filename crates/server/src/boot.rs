//! Building and running the daemon: the shared `AppState` both entry points
//! use, and the embedded serve loop the device runner starts on the phone
//! itself (see `crates/ios-embed`).

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::config::Config;
use crate::http::{self, AppState};

/// What `app_state` needs beyond the config.
pub struct StateInputs<'a> {
    pub cfg: &'a Config,
    pub secret: Vec<u8>,
    pub runner_url: &'a str,
    pub mjpeg_url: String,
    pub managed_runner: bool,
    pub managed_runner_pending: bool,
    /// `IPHONE_USE_SIMULATOR`: the simulator this daemon drives (preview).
    pub simulator: Option<Arc<crate::simulator::Simulator>>,
}

/// The daemon's state, as `iphone-use serve` and the embedded daemon build it.
pub fn app_state(inputs: StateInputs<'_>) -> Arc<AppState> {
    Arc::new(AppState {
        password: inputs.cfg.password.clone(),
        secret: inputs.secret,
        session_ttl_secs: inputs.cfg.session_ttl_secs,
        // The daemon always serves plain HTTP; whether the cookie is marked
        // `Secure` is decided per-request from `X-Forwarded-Proto` (the
        // Cloudflare tunnel sets it to https). Binding to a LAN IP (0.0.0.0) is
        // still plain HTTP, so forcing Secure here would make browsers reject
        // the cookie over LAN. Keep this `false`; per-request HTTPS is detected
        // in http.rs.
        cookie_secure: false,
        base_path: inputs.cfg.base_path.clone(),
        auth_limiter: Arc::new(Mutex::new(http::AuthLimiter::new())),
        agent_token: inputs.cfg.agent_token.clone(),
        device_udid: inputs.cfg.device_udid.clone(),
        simulator: inputs.simulator,
        inbox: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        runner: match crate::runner::RunnerClient::new(inputs.runner_url) {
            Ok(c) => {
                tracing::info!("device control via runner at {}", inputs.runner_url);
                Some(Arc::new(tokio::sync::Mutex::new(c)))
            }
            Err(e) => {
                tracing::warn!("IPHONE_USE_RUNNER_URL set but client failed: {e:#}");
                None
            }
        },
        managed_runner: inputs.managed_runner,
        managed_runner_pending: inputs.managed_runner_pending,
        latest_release: Arc::new(Mutex::new(None)),
        // runner's MJPEG stream (see /agent/mjpeg); the relay forwards it to
        // 127.0.0.1:9100 (override with IPHONE_USE_RUNNER_MJPEG_URL).
        video: crate::video::VideoHub::supported()
            .then(|| crate::video::VideoHub::new(inputs.mjpeg_url.clone())),
        mjpeg_url: Some(inputs.mjpeg_url),
        runner_actionable: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        runner_read_degraded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pairing: Arc::new(
            crate::pairing::Pairing::new(!crate::pairing::is_loopback_host(&inputs.cfg.host))
                .with_port(inputs.cfg.port),
        ),
        runner_health: Arc::new(Mutex::new(crate::runner::RunnerHealth::down())),
        runner_death: Arc::new(Mutex::new(Default::default())),
        runner_health_probe: Arc::new(Mutex::new(None)),
        settled_frame_prefetch: Arc::new(Mutex::new(None)),
        runner_control_pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        // Idle auto-release: start the clock now so a daemon that boots with no
        // one driving releases the phone after the first idle window.
        last_activity: Arc::new(Mutex::new(std::time::Instant::now())),
        released: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        runner_lifecycle: Arc::new(http::RunnerLifecycle::new()),
        live_streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        mjpeg_stream_activity: Arc::new(Mutex::new(std::collections::HashMap::new())),
        element_snapshots: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        owner_snapshots: Arc::new(Mutex::new(std::collections::HashMap::new())),
        flow_trail: Arc::new(Mutex::new(crate::flows::FlowTrail::default())),
        agent_focus: Arc::new(Mutex::new(crate::focus::AgentFocus::load(
            crate::focus::state_path(),
        ))),
        hold_until: Arc::new(Mutex::new(None)),
        private_until: Arc::new(Mutex::new(None)),
        owner: Arc::new(Mutex::new(None)),
        owner_lease_secs: inputs.cfg.owner_lease_secs,
        runner_bootstrap: crate::http::write_and_bootstrap_runner_agent,
    })
}

/// How the device runner configures the daemon it hosts on the phone.
pub struct EmbeddedOptions {
    /// Address the daemon binds: the phone's own 127.0.0.1 unless a developer
    /// opens it up to reach it from a Mac (`/agent/*` still needs the token).
    pub host: String,
    pub port: u16,
    /// The runner's command and MJPEG ports, on the same phone.
    pub runner_port: u16,
    pub runner_mjpeg_port: u16,
    /// The runner's per-launch request-signing token, written where `RunnerClient`
    /// reads it so the daemon's calls to its own runner are signed.
    pub runner_token: Option<String>,
    /// Bearer token for `/agent/*` (the on-phone app sends it).
    pub agent_token: Option<String>,
    /// Control password for the web page and `/login`.
    pub password: Option<String>,
}

/// The daemon as the device runner hosts it on the phone: same router and
/// state as `iphone-use serve`, bound to loopback, pointed at the runner it
/// lives in. Never manages that runner (no launchctl, no idle release: the
/// runner is the process this runs in). Blocks until the listener fails; call
/// it from a thread of its own, never the runner's main thread, which serves
/// every runner command the daemon will send.
pub fn serve_embedded(options: EmbeddedOptions) -> Result<()> {
    let port = options.port.to_string();
    let host = options.host.clone();
    let password = options.password.clone();
    let agent_token = options.agent_token.clone();
    let cfg = Config::from_map(|key| match key {
        "IPHONE_USE_HOST" => Some(host.clone()),
        "IPHONE_USE_PORT" => Some(port.clone()),
        "IPHONE_USE_PASSWORD" => password.clone(),
        "IPHONE_USE_AGENT_TOKEN" => agent_token.clone(),
        _ => std::env::var(key).ok(),
    });
    sandbox_dirs()?;
    let instance = crate::instance::install(
        crate::instance::Instance::from_env().map_err(|error| anyhow::anyhow!(error))?,
    );
    std::fs::create_dir_all(&instance.state_dir)
        .with_context(|| format!("create {}", instance.state_dir.display()))?;
    instance
        .verify_on_disk()
        .map_err(|error| anyhow::anyhow!(error))?;
    if let Some(token) = &options.runner_token {
        crate::runner_token::write(&instance.state_dir, token)
            .map_err(|error| anyhow::anyhow!(error))?;
    }
    crate::timing::set_log_path(instance.state_dir.join("agent-timing.jsonl"));
    crate::metrics::set_runs_log(instance.state_dir.join("agent-runs.jsonl"));

    let secret = embedded_secret(&instance.state_dir)?;
    let runner_url = format!("http://127.0.0.1:{}", options.runner_port);
    let state = app_state(StateInputs {
        cfg: &cfg,
        secret,
        runner_url: &runner_url,
        mjpeg_url: format!("http://127.0.0.1:{}", options.runner_mjpeg_port),
        managed_runner: false,
        managed_runner_pending: false,
        simulator: None,
    });

    // The runner mints pairing codes for the iPhone app on this same phone
    // through `embedded_pair_code`, in-process: the app is a loopback client,
    // so this works although the daemon is not LAN-reachable.
    let _ = EMBEDDED_PAIRING.set(state.pairing.clone());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), cfg.port))
            .await
            .with_context(|| format!("bind {}:{}", cfg.host, cfg.port))?;
        tracing::info!(
            "embedded daemon on {}:{} → runner {runner_url}",
            cfg.host,
            cfg.port
        );
        crate::schedules::start(
            "127.0.0.1",
            cfg.port,
            cfg.agent_token.clone().or_else(|| cfg.password.clone()),
            &instance.state_dir,
            true,
        );
        crate::keep_awake::spawn(state.clone());
        crate::lock_readiness::spawn(state.clone());
        spawn_registry_refresh_in_process();
        axum::serve(listener, http::router(state))
            .await
            .context("axum serve")
    })
}

/// The embedded daemon's pairing, once it is built.
static EMBEDDED_PAIRING: std::sync::OnceLock<Arc<crate::pairing::Pairing>> =
    std::sync::OnceLock::new();

/// A fresh one-time pairing code from the daemon the runner hosts, for the
/// iPhone app on the same phone (`iphoneuse://pair?u=http://127.0.0.1:<port>&c=<code>`).
/// `None` until [`serve_embedded`] has built its state.
pub fn embedded_pair_code() -> Option<String> {
    EMBEDDED_PAIRING.get().map(|pairing| pairing.issue())
}

/// An iOS app container's root is not writable, so `~/.iphone-use` cannot be
/// created there: the state and the flow store go under the container's
/// `Library/Application Support` instead, unless already set.
fn sandbox_dirs() -> Result<()> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let support = std::path::Path::new(&home).join("Library/Application Support/iphone-use");
    for (key, dir) in [
        ("IPHONE_USE_STATE_DIR", support.join("state")),
        ("IPHONE_USE_FLOWS_DIR", support.join("flows")),
    ] {
        if std::env::var_os(key).is_some_and(|v| !v.is_empty()) {
            continue;
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        // Set once, on the daemon's thread before it starts any other.
        std::env::set_var(key, &dir);
    }
    Ok(())
}

/// The flow registry on the phone: fetched in this process (an app cannot run
/// `iphone-use-mcp flow update`), soon after start and then whenever the store
/// is older than a day. Opt out with `IPHONE_USE_FLOWS_NO_AUTO_UPDATE=1`.
fn spawn_registry_refresh_in_process() {
    if crate::flows::auto_update_disabled() {
        tracing::info!("flow registry auto-update disabled");
        return;
    }
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        loop {
            if crate::flows::store_is_stale() {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(120),
                    iphone_use_mcp::registry::update(),
                )
                .await
                {
                    Ok(Ok(_)) => tracing::info!("flow registry refreshed"),
                    Ok(Err(error)) => tracing::warn!("flow registry refresh failed: {error:#}"),
                    Err(_) => tracing::warn!("flow registry refresh timed out"),
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
}

/// The session secret the embedded daemon signs cookies and device tokens
/// with, kept in its state dir (0600). A new secret on every runner start
/// would silently unpair every app that paired with this phone's daemon.
fn embedded_secret(state_dir: &std::path::Path) -> Result<Vec<u8>> {
    const FILE: &str = "session-secret";
    match crate::runtime_dir::read_secret(state_dir, FILE) {
        Ok(secret) if secret.len() == 32 => return Ok(secret),
        Ok(secret) => anyhow::bail!(
            "{}/{FILE} holds {} bytes, not 32; remove it to pair again",
            state_dir.display(),
            secret.len()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("read the session secret"),
    }
    let mut secret = vec![0_u8; 32];
    getrandom::fill(&mut secret).map_err(|error| anyhow::anyhow!("CSPRNG failed: {error}"))?;
    crate::runtime_dir::write_secret(state_dir, FILE, &secret)
        .context("save the session secret")?;
    Ok(secret)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_embedded_secret_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let first = super::embedded_secret(dir.path()).unwrap();
        assert_eq!(first.len(), 32);
        assert_eq!(
            super::embedded_secret(dir.path()).unwrap(),
            first,
            "same secret after a restart"
        );
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir.path().join("session-secret"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
