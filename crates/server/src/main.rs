//! `iphone-use` daemon CLI.
//!
//! ```text
//! iphone-use serve   # serve the phone over WebDriverAgent → axum
//! iphone-use stop    # best-effort: kill the recorded pid
//! iphone-use upgrade # install the latest release + refresh the skill (--check / --json)
//! ```
//!
//! WebDriverAgent on the phone does the input and serves the video, so the
//! daemon needs no iPhone Mirroring, no Mac TCC grant, and never touches the
//! Mac's screen or pointer.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use server::config::Config;
use server::http::{self, AppState};

/// PID file name inside the runtime dir.
const PID_FILE: &str = "iphone-use.pid";
/// Secret file name inside the runtime dir.
const SECRET_FILE: &str = "secret";
const PID_RECORD_VERSION: u8 = 1;
const STOP_WAIT_SECS: u64 = 5;
/// Seconds to sleep before exiting on an unattended startup failure, so a
/// launchd `KeepAlive` relaunch loop stays gentle instead of spinning (issue #28).
const STARTUP_BACKOFF_SECS: u64 = 30;

/// Is stderr a terminal? When false we're almost certainly running under launchd
/// (stderr redirected to the log file), where a fast crash-relaunch loop is
/// harmful — see the backoff in [`main`].
fn stderr_is_tty() -> bool {
    // SAFETY: `isatty` is a pure libc query on a fixed fd, no memory effects.
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}

fn endpoint_is_loopback(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn optional_env_bool(name: &str) -> Result<Option<bool>> {
    let Ok(value) = std::env::var(name) else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" | "" => Ok(Some(false)),
        _ => anyhow::bail!("{name} must be true/false or 1/0"),
    }
}

/// Return the host form accepted by `ToSocketAddrs`.
///
/// Operators commonly copy an IPv6 host either as `::1` (socket form) or
/// `[::1]` (URL authority form). Accept both, but never build a socket address
/// with string concatenation: `format!("{host}:{port}")` turns `::` into the
/// invalid `:::44321`.
fn socket_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host)
}

/// Format a configured host for an HTTP authority.
fn http_authority(host: &str, port: u16) -> String {
    let host = socket_host(host);
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn resolve_managed_wda(
    endpoints_are_local: bool,
    target_udid: Option<&str>,
    configured: Option<bool>,
) -> Result<bool> {
    // Auto-ownership is safe only when the daemon can restart the exact device
    // it stopped. A hand-run daemon with loopback defaults but no persisted
    // target may use an already-running WDA, but must never idle-stop it.
    let requested = configured.unwrap_or(endpoints_are_local && target_udid.is_some());
    if !requested {
        return Ok(false);
    }
    if !endpoints_are_local {
        anyhow::bail!(
            "PHONE_REMOTE_WDA_MANAGED=true requires loopback WDA and MJPEG endpoints; \
             remote device services are externally managed"
        );
    }
    // First install is allowed to start offline before a phone has been
    // selected. Treat an explicit managed=true as pending, not as ownership,
    // until setup persists the canonical target.
    Ok(target_udid.is_some())
}

fn wda_management_pending(
    endpoints_are_local: bool,
    target_udid: Option<&str>,
    configured: Option<bool>,
) -> bool {
    endpoints_are_local
        && target_udid.is_none()
        && configured != Some(false)
}

#[derive(Parser)]
// `version` spelled out: clap's bare `version` expands to `core::env!`, and
// this crate's dependency named `core` shadows the std one.
#[command(
    name = "iphone-use",
    about = "Direct iPhone remote-control daemon",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon and browser/API server.
    Serve,
    /// Stop a running daemon (best-effort; kills the recorded pid).
    Stop,
    /// Print which instance this environment would run as, as JSON, and exit.
    ///
    /// Read-only: derives the instance from PHONE_REMOTE_INSTANCE / HOME /
    /// PHONE_REMOTE_STATE_DIR (+ PHONE_REMOTE_UDID) without opening a socket,
    /// touching launchd, or starting WDA. Installers run this on a staged
    /// binary before switching services over: a binary that predates
    /// instances rejects the subcommand outright, so it can never be started
    /// against the wrong phone (#67).
    InstanceContext,
    /// Upgrade to the latest GitHub release (daemon app and agent skill).
    ///
    /// Runs the same install.sh the daemon was installed with, then refreshes
    /// any other copy of the skill (Claude Code plugin, git checkout). Exit 0
    /// on success or when already current, 2 when the check or download fails.
    Upgrade {
        /// Change nothing; print `iphone-use <current> -> <latest>` or
        /// `iphone-use <current> is up to date`.
        #[arg(long)]
        check: bool,
        /// Like --check, as JSON: name, current, latest, update_available, skills.
        #[arg(long)]
        json: bool,
    },
}

/// Once-a-day "new version available" line on stderr for one-shot commands.
/// Cached in `${XDG_CACHE_HOME:-~/.cache}/iphone-use/update-check.json`;
/// a stale cache costs at most a 2 s lookup. Silent on any failure.
fn update_notice() {
    use server::update;
    let token = std::env::var("GITHUB_TOKEN").ok();
    let mut fetch = || {
        update::fetch_latest_tag_blocking(
            &update::Endpoints::github(),
            update::NOTICE_TIMEOUT,
            token.as_deref(),
        )
        .ok()
    };
    update::maybe_notice(
        &update::process_env,
        update::unix_now(),
        env!("CARGO_PKG_VERSION"),
        &mut fetch,
        &mut std::io::stderr(),
    );
}

/// Is `program` an executable somewhere on `$PATH`?
fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(program))
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
    })
}

/// Download install.sh into a private temp dir, then run it the way the
/// documented `curl … | sh` does (script on stdin, so it behaves as a piped
/// install rather than a local checkout). Downloading first means a failed
/// fetch is reported, not silently piped into `sh` as an empty script.
fn run_installer() -> std::result::Result<(), String> {
    use server::update;
    let dir = tempfile::tempdir().map_err(|e| format!("could not create a temp dir: {e}"))?;
    let script = dir.path().join("install.sh");
    let fetched = std::process::Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "-o"])
        .arg(&script)
        .arg(update::INSTALL_SCRIPT_URL)
        .status()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !fetched.success() {
        return Err(format!(
            "downloading {} failed ({fetched}); nothing was changed",
            update::INSTALL_SCRIPT_URL
        ));
    }
    let stdin = std::fs::File::open(&script)
        .map_err(|e| format!("could not open the downloaded installer: {e}"))?;
    let status = std::process::Command::new("/bin/sh")
        .stdin(stdin)
        .status()
        .map_err(|e| format!("could not run install.sh: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "install.sh failed ({status}); it restores the previous install on failure"
        ))
    }
}

/// `iphone-use upgrade [--check] [--json]`. Returns the process exit code.
fn upgrade(check_only: bool, json: bool) -> i32 {
    use server::update;
    let current = env!("CARGO_PKG_VERSION");
    let token = std::env::var("GITHUB_TOKEN").ok();
    let fetched = update::fetch_latest_tag_blocking(
        &update::Endpoints::github(),
        update::CHECK_TIMEOUT,
        token.as_deref(),
    );
    // An explicit check refreshes the notice cache too.
    if let Some(path) = update::cache_path(&update::process_env) {
        update::record_check(&path, update::unix_now(), fetched.as_deref().ok());
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let skills = home
        .as_deref()
        .map(update::find_skills)
        .unwrap_or_default();
    let report = update::UpgradeReport::new(current, &fetched, skills);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string())
        );
        return report.exit_code();
    }
    if let Some(error) = &report.error {
        eprintln!("iphone-use: could not check for a new release: {error}");
        return 2;
    }
    if check_only {
        let _ = report.write_text(&mut std::io::stdout());
        return 0;
    }

    let mut installer_ran = false;
    if report.update_available {
        let exe = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .unwrap_or_default();
        if !update::installed_by_installer(&exe) {
            // A cargo build or other copy: never replace ~/Applications behind
            // its back. Say how instead.
            println!("{}", report.summary());
            println!(
                "this binary ({}) was not installed by install.sh; rebuild it from source, \
                 or install the release with:\n  {}",
                exe.display(),
                update::INSTALL_COMMAND
            );
            return 0;
        }
        println!(
            "iphone-use: upgrading {} -> {} with install.sh (the daemon restarts)",
            report.current,
            report.latest.as_deref().unwrap_or("?")
        );
        if let Err(error) = run_installer() {
            eprintln!("iphone-use: {error}");
            return 2;
        }
        installer_ran = true;
        println!(
            "iphone-use upgraded {} -> {}",
            report.current,
            report.latest.as_deref().unwrap_or("?")
        );
    } else {
        println!("{}", report.summary());
    }

    let mut run = |program: &str, args: &[String]| {
        std::process::Command::new(program)
            .args(args)
            .status()
            .map(|status| status.success())
    };
    let _ = update::refresh_skills(
        &report.skills,
        installer_ran,
        on_path("claude"),
        &mut run,
        &mut std::io::stdout(),
    );
    0
}

/// `iphone-use instance-context`: the derived instance as one JSON object.
fn instance_context() -> Result<()> {
    let instance = server::instance::Instance::from_env().map_err(|error| anyhow::anyhow!(error))?;
    let udid = std::env::var("PHONE_REMOTE_UDID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let state_dir_ok = instance.verify_on_disk().map_err(|error| error.to_string());
    let body = serde_json::json!({
        "schema_version": 1,
        "name": instance.name,
        "state_dir": instance.state_dir,
        "daemon_label": instance.daemon_label,
        "wda_label": instance.wda_label,
        "wda_plist": instance.wda_plist(),
        "udid": udid,
        "state_dir_ok": state_dir_ok.is_ok(),
        "state_dir_error": state_dir_ok.err(),
    });
    println!("{body}");
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    // Only the long-running service earns the unattended-relaunch backoff
    // below; a one-shot query must report and exit at once.
    let unattended_service = matches!(cli.command, Command::Serve);
    let result = match cli.command {
        Command::Serve => serve(),
        Command::Stop => {
            let result = stop();
            // The daily "new version" line (stderr only). `serve` gets the same
            // line from its own background check; `instance-context` is an
            // installer probe and `upgrade` reports versions itself.
            update_notice();
            result
        }
        Command::InstanceContext => instance_context(),
        Command::Upgrade { check, json } => std::process::exit(upgrade(check || json, json)),
    };

    // Issue #28: under launchd `KeepAlive=true`, a startup that fails fast
    // (missing TCC grant, port already in use) is relaunched instantly — tens
    // of thousands of times — pegging `launchservicesd` at ~100% CPU and growing
    // the stderr log to hundreds of MB. When we're running UNATTENDED (stderr is
    // a file launchd redirected, not a TTY), back off before exiting so the
    // relaunch cadence is gentle and self-recovers once the user grants
    // permissions / frees the port. (The plist also carries a ThrottleInterval;
    // this is belt-and-suspenders and protects even a hand-written plist.) A
    // human running `iphone-use serve` in a terminal exits immediately as before.
    if result.is_err() && unattended_service && !stderr_is_tty() {
        if let Err(e) = &result {
            tracing::error!(
                "startup failed: {e:#} — backing off {STARTUP_BACKOFF_SECS}s before exit \
                 (launchd will relaunch; fix the reported prerequisite or free the port \
                 to recover)"
            );
        }
        std::thread::sleep(std::time::Duration::from_secs(STARTUP_BACKOFF_SECS));
    }
    result
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

fn serve() -> Result<()> {
    // 1. Resolve the backend before touching any Mac capture/input API. Direct is
    //    the default and must start without Screen Recording or Accessibility.
    let cfg = Config::from_env();
    // Which daemon this is (#67): pin the instance before anything derives a
    // path or a launchd label from it.
    let instance = server::instance::install(
        server::instance::Instance::from_env().map_err(|error| anyhow::anyhow!(error))?,
    );
    instance
        .verify_on_disk()
        .map_err(|error| anyhow::anyhow!(error))?;
    tracing::info!(
        "instance {} (state_dir={}, wda_label={})",
        instance.name,
        instance.state_dir.display(),
        instance.wda_label
    );

    // The iPhone Mirroring backend was removed in v0.9. An old plist may still
    // ask for it; say so once and serve the phone over WDA as usual.
    if std::env::var("PHONE_REMOTE_BACKEND")
        .is_ok_and(|v| matches!(v.trim(), "mirror" | "legacy-mirror"))
    {
        tracing::warn!(
            "PHONE_REMOTE_BACKEND=mirror is no longer supported (iPhone Mirroring was removed in v0.9); serving over WDA"
        );
    }

    // 3. Runtime dir + signing secret. The pid record is written only after the
    //    listener binds, so a failed second start cannot replace the live
    //    daemon's identity.
    let dir = server::runtime_dir::runtime_dir().context("create runtime dir")?;
    let secret = load_or_make_secret(&dir, &cfg)?;

    // Direct mode has stable localhost relay defaults. The setup script creates
    // these relays; keeping the client configured while WDA is down lets the web
    // UI report setup/recovery state instead of crash-looping the daemon.
    let configured_wda_url = std::env::var("PHONE_REMOTE_WDA_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let wda_url = configured_wda_url.unwrap_or_else(|| "http://127.0.0.1:8100".to_string());
    let mjpeg_url = std::env::var("PHONE_REMOTE_WDA_MJPEG_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:9100".to_string());
    let endpoints_are_local = endpoint_is_loopback(&wda_url) && endpoint_is_loopback(&mjpeg_url);
    let managed_setting = optional_env_bool("PHONE_REMOTE_WDA_MANAGED")?;
    let managed_wda_pending = wda_management_pending(
        endpoints_are_local,
        cfg.device_udid.as_deref(),
        managed_setting,
    );
    let managed_wda = resolve_managed_wda(
        endpoints_are_local,
        cfg.device_udid.as_deref(),
        managed_setting,
    )?;

    // The daemon always serves plain HTTP; whether the cookie is marked `Secure`
    // is decided per-request from `X-Forwarded-Proto` (the Cloudflare tunnel sets
    // it to https). Binding to a LAN IP (0.0.0.0) is still plain HTTP, so forcing
    // Secure here would make browsers reject the cookie over LAN. Keep this
    // `false`; per-request HTTPS is detected in http.rs.
    let cookie_secure = false;
    let state = Arc::new(AppState {
        password: cfg.password.clone(),
        secret,
        session_ttl_secs: cfg.session_ttl_secs,
        cookie_secure,
        auth_limiter: Arc::new(Mutex::new(http::AuthLimiter::new())),
        agent_token: cfg.agent_token.clone(),
        device_udid: cfg.device_udid.clone(),
        inbox: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        wda: match server::wda::WdaClient::new(&wda_url) {
            Ok(c) => {
                tracing::info!("device control via WDA at {wda_url}");
                Some(Arc::new(tokio::sync::Mutex::new(c)))
            }
            Err(e) => {
                tracing::warn!("PHONE_REMOTE_WDA_URL set but client failed: {e:#}");
                None
            }
        },
        managed_wda,
        managed_wda_pending,
        latest_release: Arc::new(Mutex::new(None)),
        // WDA's MJPEG stream (see /agent/mjpeg); the relay forwards it to
        // 127.0.0.1:9100 (override with PHONE_REMOTE_WDA_MJPEG_URL).
        video: server::video::VideoHub::supported()
            .then(|| server::video::VideoHub::new(mjpeg_url.clone())),
        mjpeg_url: Some(mjpeg_url),
        wda_actionable: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pairing: Arc::new(server::pairing::Pairing::new(
            !server::pairing::is_loopback_host(&cfg.host),
        )),
        wda_health: Arc::new(Mutex::new(server::wda::WdaHealth::down())),
        wda_death: Arc::new(Mutex::new(Default::default())),
        wda_health_probe: Arc::new(Mutex::new(None)),
        wda_control_pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        // Idle auto-release: start the clock now so a daemon that boots with no
        // one driving releases the phone after the first idle window.
        last_activity: Arc::new(Mutex::new(std::time::Instant::now())),
        released: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        wda_lifecycle: Arc::new(http::WdaLifecycle::new()),
        live_streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        mjpeg_stream_activity: Arc::new(Mutex::new(std::collections::HashMap::new())),
        element_snapshots: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        hold_until: Arc::new(Mutex::new(None)),
        owner: Arc::new(Mutex::new(None)),
        owner_lease_secs: cfg.owner_lease_secs,
    });

    run_server(cfg, state, dir)
}

/// Background update check: resolve the repo's latest release tag every 24h
/// and stash it in `AppState.latest_release` for `/agent/status` to report.
///
/// Same lookup as `iphone-use upgrade` (`server::update`): the GitHub API
/// (honouring `GITHUB_TOKEN`), falling back to the `releases/latest` redirect,
/// which has no anonymous rate limit. Each result also refreshes the CLI's
/// notice cache. Disabled by `PHONE_REMOTE_NO_UPDATE_CHECK`,
/// `IPHONE_USE_NO_UPDATE_CHECK`, `USE_NO_UPDATE_CHECK` or `CI`.
fn spawn_update_check(state: Arc<AppState>) {
    use server::update;
    if update::update_check_disabled(&update::process_env) {
        tracing::info!("update check disabled (opt-out environment variable set)");
        return;
    }
    let token = std::env::var("GITHUB_TOKEN").ok();
    tokio::spawn(async move {
        let endpoints = update::Endpoints::github();
        loop {
            match update::fetch_latest_tag(
                &endpoints,
                std::time::Duration::from_secs(15),
                token.as_deref(),
            )
            .await
            {
                Ok(tag) => {
                    let current = env!("CARGO_PKG_VERSION");
                    if let Some(path) = update::cache_path(&update::process_env) {
                        update::record_check(&path, update::unix_now(), Some(&tag));
                    }
                    if update::is_newer(&tag, current) {
                        // The family's notice line, verbatim, on stderr (the
                        // daemon log under launchd).
                        eprintln!("{}", update::notice_line(&tag, current));
                    }
                    *state
                        .latest_release
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = Some(tag);
                }
                Err(e) => tracing::debug!("update check failed (will retry): {e:#}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
        }
    });
}

/// Build the tokio runtime and serve.
fn run_server(cfg: Config, state: Arc<AppState>, runtime_dir: std::path::PathBuf) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    rt.block_on(async move {
        let authority = http_authority(&cfg.host, cfg.port);
        let listener = tokio::net::TcpListener::bind((socket_host(&cfg.host), cfg.port))
            .await
            .with_context(|| format!("bind {authority}"))?;
        let pid_record = write_pid(&runtime_dir)?;

        print_startup_banner(&cfg);

        // Daily release check → /agent/status {version, latest, update_available}.
        spawn_update_check(state.clone());

        // Idle auto-release owns only the local supervisor. Remote WDA
        // endpoints are externally managed and must never trigger local
        // launchctl/setup commands.
        if state.managed_wda {
            http::spawn_idle_release_watchdog(state.clone());
        }

        let app = http::router(state);
        let serve_result = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("axum serve");
        if let Err(error) = remove_pid_record_if_unchanged(&runtime_dir.join(PID_FILE), &pid_record)
        {
            tracing::warn!("could not remove pid record after server exit: {error:#}");
        }
        serve_result?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Print the local URL + password so the operator can open the client.
fn print_startup_banner(cfg: &Config) {
    let url = format!("http://{}/phone", http_authority(&cfg.host, cfg.port));
    eprintln!("──────────────────────────────────────────────");
    eprintln!(" iphone-use serving");
    eprintln!("   url:      {url}");
    match &cfg.password {
        Some(_) => eprintln!("   password: (set via PHONE_REMOTE_PASSWORD)"),
        None => eprintln!("   password: (none — open LAN mode)"),
    }
    match &cfg.agent_token {
        Some(_) => eprintln!("   agent:    (dedicated token set via PHONE_REMOTE_AGENT_TOKEN)"),
        None => eprintln!("   agent:    (no dedicated token — uses password for bearer auth)"),
    }
    if cfg.host == "127.0.0.1" {
        eprintln!("   note:     bound to 127.0.0.1; set PHONE_REMOTE_HOST=0.0.0.0 for LAN access");
    }
    eprintln!("──────────────────────────────────────────────");
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}

// ---------------------------------------------------------------------------
// TCC preflight
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// secret + pid management
// ---------------------------------------------------------------------------

/// Load the signing secret, preferring (in order): the configured
/// `PHONE_REMOTE_SECRET`, an existing secret file, or a freshly generated one.
fn load_or_make_secret(dir: &std::path::Path, cfg: &Config) -> Result<Vec<u8>> {
    if let Some(s) = &cfg.secret {
        return Ok(s.clone().into_bytes());
    }

    match server::runtime_dir::read_secret(dir, SECRET_FILE) {
        Ok(bytes) if !bytes.is_empty() => return Ok(bytes),
        Ok(_) => anyhow::bail!(
            "persisted session secret is empty; remove it only after stopping every daemon"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("read persisted session secret"),
    }

    persist_generated_secret(dir, gen_secret()?)
}

/// Generate the session-cookie HMAC key directly from the operating system's
/// cryptographically secure random source. There is intentionally no weak
/// fallback: starting without a trustworthy signing key would be worse than
/// refusing to start.
fn gen_secret() -> Result<Vec<u8>> {
    let mut secret = vec![0_u8; 32];
    getrandom::fill(&mut secret)
        .map_err(|error| anyhow::anyhow!("operating-system CSPRNG failed: {error}"))?;
    Ok(secret)
}

/// Persist a freshly generated secret. If another process wins the exclusive
/// create race, use that process's complete secret so both daemons agree.
fn persist_generated_secret(dir: &std::path::Path, secret: Vec<u8>) -> Result<Vec<u8>> {
    match server::runtime_dir::write_secret(dir, SECRET_FILE, &secret) {
        Ok(()) => Ok(secret),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // `write_secret` uses O_EXCL, so the losing process can observe the
            // winner's file in the tiny interval between create(2) and write(2).
            // Wait briefly for non-empty contents; never fall back to our
            // different key or accept an empty/partial startup state.
            for attempt in 0..50 {
                match server::runtime_dir::read_secret(dir, SECRET_FILE) {
                    Ok(winner) if winner.len() == 32 => return Ok(winner),
                    Ok(winner) if winner.len() < 32 && attempt < 49 => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Ok(winner) => anyhow::bail!(
                        "concurrent daemon persisted an invalid {}-byte session secret",
                        winner.len()
                    ),
                    Err(read_error)
                        if read_error.kind() == std::io::ErrorKind::NotFound && attempt < 49 =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(read_error) => {
                        return Err(read_error)
                            .context("read session secret created by concurrent daemon");
                    }
                }
            }
            unreachable!("bounded secret-read loop always returns")
        }
        Err(error) => Err(error).context("persist generated session secret"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessIdentity {
    euid: u32,
    started_at: String,
    executable: String,
    argv: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PidRecord {
    version: u8,
    pid: i32,
    identity: ProcessIdentity,
}

enum ParsedPidRecord {
    Structured(PidRecord),
    Legacy(i32),
}

struct PidFileContents {
    bytes: Vec<u8>,
    mode: u32,
}

fn current_euid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    unsafe { libc::geteuid() }
}

/// Read one `ps` field. A missing process is represented as `None`; inability
/// to execute or decode `/bin/ps` is an error so callers fail closed.
fn ps_field(pid: i32, field: &str) -> Result<Option<String>> {
    if pid <= 1 {
        anyhow::bail!("refusing unsafe pid {pid}");
    }
    let output = std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", &format!("{field}=")])
        .output()
        .with_context(|| format!("query pid {pid} field {field} with /bin/ps"))?;
    let stdout = output.stdout;
    let value = String::from_utf8(stdout)
        .with_context(|| format!("/bin/ps returned non-UTF-8 {field} for pid {pid}"))?
        .trim()
        .to_owned();
    if !output.status.success() {
        if value.is_empty() && output.stderr.is_empty() {
            return Ok(None);
        }
        let detail = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "/bin/ps could not query {field} for pid {pid}: {}",
            detail.trim()
        );
    }
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(value))
}

/// Snapshot the fields used to distinguish the daemon from a reused or
/// attacker-selected pid. Reading `lstart` both before and after the other
/// fields avoids accepting a mixed snapshot if the pid changes mid-query.
fn read_process_identity(pid: i32) -> Result<Option<ProcessIdentity>> {
    let Some(started_at) = ps_field(pid, "lstart")? else {
        return Ok(None);
    };
    let Some(uid) = ps_field(pid, "uid")? else {
        return Ok(None);
    };
    let Some(executable) = ps_field(pid, "comm")? else {
        return Ok(None);
    };
    let Some(argv) = ps_field(pid, "command")? else {
        return Ok(None);
    };
    let Some(started_at_after) = ps_field(pid, "lstart")? else {
        return Ok(None);
    };
    if started_at != started_at_after {
        return Ok(None);
    }
    let euid = uid
        .parse::<u32>()
        .with_context(|| format!("/bin/ps returned invalid uid for pid {pid}"))?;
    Ok(Some(ProcessIdentity {
        euid,
        started_at,
        executable,
        argv,
    }))
}

fn parse_pid_record(bytes: &[u8]) -> Result<ParsedPidRecord> {
    if let Ok(record) = serde_json::from_slice::<PidRecord>(bytes) {
        if record.version != PID_RECORD_VERSION {
            anyhow::bail!("unsupported pid record version {}", record.version);
        }
        if record.pid <= 1 {
            anyhow::bail!("pid record contains unsafe pid {}", record.pid);
        }
        return Ok(ParsedPidRecord::Structured(record));
    }
    let text = std::str::from_utf8(bytes).context("pid record is not UTF-8")?;
    let pid = text
        .trim()
        .parse::<i32>()
        .context("pid record is neither versioned JSON nor a legacy pid")?;
    if pid <= 1 {
        anyhow::bail!("legacy pid record contains unsafe pid {pid}");
    }
    Ok(ParsedPidRecord::Legacy(pid))
}

fn validate_pid_identity(record: &PidRecord, observed: &ProcessIdentity) -> Result<()> {
    let euid = current_euid();
    if record.identity.euid != euid {
        anyhow::bail!(
            "pid record uid {} does not match current uid {euid}",
            record.identity.euid
        );
    }
    if observed.euid != euid {
        anyhow::bail!(
            "pid {} belongs to uid {}, not current uid {euid}",
            record.pid,
            observed.euid
        );
    }
    if &record.identity != observed {
        anyhow::bail!(
            "pid {} identity no longer matches its recorded start time and command",
            record.pid
        );
    }
    Ok(())
}

fn read_pid_file(path: &std::path::Path) -> std::io::Result<PidFileContents> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.uid() != current_euid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "pid record must be a regular file owned by the current uid",
        ));
    }
    let mut bytes = Vec::new();
    file.by_ref().take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "pid record exceeds 64 KiB",
        ));
    }
    Ok(PidFileContents {
        bytes,
        mode: metadata.mode() & 0o777,
    })
}

fn atomic_replace_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("pid record has no parent directory"))?;
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| std::io::Error::other("pid record has no UTF-8 file name"))?;

    let mut staged = None;
    for _ in 0..32 {
        let nonce = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), nonce));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate);
        match file {
            Ok(file) => {
                staged = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let Some((staged_path, mut file)) = staged else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique pid staging file",
        ));
    };

    let result = (|| {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&staged_path, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged_path);
    }
    result
}

fn remove_pid_record_if_unchanged(path: &std::path::Path, expected: &[u8]) -> Result<()> {
    match read_pid_file(path) {
        Ok(current) if current.bytes == expected => {
            std::fs::remove_file(path).with_context(|| format!("remove pid record {path:?}"))
        }
        Ok(_) => anyhow::bail!("pid record changed; preserving the newer record"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("re-read pid record {path:?}")),
    }
}

/// Write a versioned, private pid record after refusing to replace a live
/// daemon record. Returns the exact bytes so shutdown can avoid deleting a
/// newer daemon's record.
fn write_pid(dir: &std::path::Path) -> Result<Vec<u8>> {
    let path = dir.join(PID_FILE);
    match read_pid_file(&path) {
        Ok(existing) => match parse_pid_record(&existing.bytes)
            .with_context(|| format!("parse existing pid record {path:?}"))?
        {
            ParsedPidRecord::Structured(record) => {
                if existing.mode != 0o600 {
                    anyhow::bail!(
                        "existing structured pid record has mode {:03o}, expected 600",
                        existing.mode
                    );
                }
                if let Some(observed) = read_process_identity(record.pid)? {
                    if validate_pid_identity(&record, &observed).is_ok() {
                        anyhow::bail!("iphone-use daemon already running as pid {}", record.pid);
                    }
                    tracing::warn!(
                        "replacing stale pid record for reused/mismatched pid {}",
                        record.pid
                    );
                }
            }
            ParsedPidRecord::Legacy(pid) => {
                if read_process_identity(pid)?.is_some() {
                    anyhow::bail!(
                        "legacy pid record points to a live process {pid}; refusing to replace \
                         unverifiable record"
                    );
                }
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("read existing pid record {path:?}"));
        }
    }

    let pid = i32::try_from(std::process::id()).context("current pid does not fit i32")?;
    let identity = read_process_identity(pid)?
        .ok_or_else(|| anyhow::anyhow!("current process disappeared while recording pid"))?;
    if identity.euid != current_euid() {
        anyhow::bail!("current process uid changed while recording pid");
    }
    let record = PidRecord {
        version: PID_RECORD_VERSION,
        pid,
        identity,
    };
    let mut bytes = serde_json::to_vec(&record).context("serialize pid record")?;
    bytes.push(b'\n');
    atomic_replace_private(&path, &bytes)
        .with_context(|| format!("atomically write pid record {path:?}"))?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

fn stop() -> Result<()> {
    let dir = server::runtime_dir::runtime_dir().context("locate runtime dir")?;
    let path = dir.join(PID_FILE);
    let contents = match read_pid_file(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("no running daemon (no pid file at {path:?})");
            return Ok(());
        }
        Err(error) => return Err(error).with_context(|| format!("read pid record {path:?}")),
    };
    let record = match parse_pid_record(&contents.bytes)
        .with_context(|| format!("parse pid record {path:?}"))?
    {
        ParsedPidRecord::Legacy(pid) => {
            if read_process_identity(pid)?.is_some() {
                anyhow::bail!(
                    "legacy pid-only record points to live process {pid}; refusing to signal \
                     without start-time and command identity"
                );
            }
            remove_pid_record_if_unchanged(&path, &contents.bytes)?;
            eprintln!("removed stale legacy pid record for dead pid {pid}");
            return Ok(());
        }
        ParsedPidRecord::Structured(record) => record,
    };
    if contents.mode != 0o600 {
        anyhow::bail!(
            "pid record has mode {:03o}, expected 600; preserving it without signaling",
            contents.mode
        );
    }

    let Some(observed) = read_process_identity(record.pid)? else {
        remove_pid_record_if_unchanged(&path, &contents.bytes)?;
        eprintln!("removed stale pid record for dead pid {}", record.pid);
        return Ok(());
    };
    validate_pid_identity(&record, &observed)
        .with_context(|| format!("refusing to signal pid {}", record.pid))?;

    // SAFETY: kill is a simple signal send; we send SIGTERM for a graceful stop.
    let rc = unsafe { libc::kill(record.pid, libc::SIGTERM) };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).with_context(|| format!("send SIGTERM to pid {}", record.pid));
        }
    }
    eprintln!("sent SIGTERM to verified pid {}", record.pid);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(STOP_WAIT_SECS);
    loop {
        match read_process_identity(record.pid)? {
            None => {
                remove_pid_record_if_unchanged(&path, &contents.bytes)?;
                return Ok(());
            }
            Some(current) if current != record.identity => {
                // The recorded process exited and the kernel reused its pid. Do
                // not touch the replacement; only clear our now-stale record.
                remove_pid_record_if_unchanged(&path, &contents.bytes)?;
                return Ok(());
            }
            Some(_) if std::time::Instant::now() >= deadline => {
                anyhow::bail!(
                    "pid {} did not exit within {STOP_WAIT_SECS}s; preserving pid record",
                    record.pid
                );
            }
            Some(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        atomic_replace_private, current_euid, endpoint_is_loopback, gen_secret, http_authority,
        load_or_make_secret, persist_generated_secret, read_process_identity, resolve_managed_wda,
        socket_host, validate_pid_identity, wda_management_pending, Config, PidRecord,
        ProcessIdentity, SECRET_FILE,
    };

    fn test_config() -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: 44321,
            password: None,
            secret: None,
            session_ttl_secs: 28_800,
            state_dir: None,
            owner_lease_secs: 300,
            agent_token: None,
            device_udid: None,
        }
    }

    #[test]
    fn managed_wda_endpoint_must_be_loopback() {
        assert!(endpoint_is_loopback("http://127.0.0.1:8100"));
        assert!(endpoint_is_loopback("http://localhost:9100"));
        assert!(endpoint_is_loopback("http://[::1]:8100"));
        assert!(!endpoint_is_loopback("http://192.168.1.20:8100"));
        assert!(!endpoint_is_loopback("https://wda.example.com"));
        assert!(!endpoint_is_loopback("ftp://127.0.0.1:8100"));
        assert!(!endpoint_is_loopback("http://127.0.0.1"));
        assert!(!endpoint_is_loopback("http://user@127.0.0.1:8100"));
        assert!(!endpoint_is_loopback("http://127.0.0.1:8100/?mode=test"));
        assert!(!endpoint_is_loopback("http://127.0.0.1:8100/#fragment"));
        assert!(!endpoint_is_loopback("not a url"));
    }

    #[test]
    fn ipv6_hosts_use_socket_and_url_forms_consistently() {
        assert_eq!(socket_host("::"), "::");
        assert_eq!(socket_host("[::1]"), "::1");
        assert_eq!(http_authority("::", 44321), "[::]:44321");
        assert_eq!(http_authority("[::1]", 44321), "[::1]:44321");
        assert_eq!(
            http_authority("control.example.test", 44321),
            "control.example.test:44321"
        );
        assert_eq!(http_authority("127.0.0.1", 44321), "127.0.0.1:44321");
    }

    #[test]
    fn managed_wda_requires_both_loopback_relays_and_a_target() {
        let udid = Some("00008110-001234567890001E");
        assert!(resolve_managed_wda(true, udid, None).unwrap());
        assert!(!resolve_managed_wda(true, None, None).unwrap());
        assert!(!resolve_managed_wda(true, udid, Some(false)).unwrap());
        assert!(resolve_managed_wda(false, udid, Some(true)).is_err());
        assert!(!resolve_managed_wda(true, None, Some(true)).unwrap());

        assert!(wda_management_pending(true, None, None));
        assert!(wda_management_pending(true, None, Some(true)));
        assert!(!wda_management_pending(true, None, Some(false)));
        assert!(!wda_management_pending(false, None, Some(true)));
    }

    #[test]
    fn generated_session_secrets_are_csprng_sized_and_not_fixed() {
        let first = gen_secret().unwrap();
        let second = gen_secret().unwrap();
        assert_eq!(first.len(), 32);
        assert_eq!(second.len(), 32);
        assert_ne!(first, second);
    }

    #[test]
    fn generated_session_secret_persists_and_is_reused() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::TempDir::new().unwrap();
        let cfg = test_config();
        let first = load_or_make_secret(dir.path(), &cfg).unwrap();
        let second = load_or_make_secret(dir.path(), &cfg).unwrap();

        assert_eq!(first.len(), 32);
        assert_eq!(first, second);
        let mode = std::fs::metadata(dir.path().join(SECRET_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn secret_create_race_reads_the_winner() {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let dir = tempfile::TempDir::new().unwrap();
        let winner = vec![0x5a; 32];
        let path = dir.path().join(SECRET_FILE);
        let peer_winner = winner.clone();
        let (created_tx, created_rx) = std::sync::mpsc::sync_channel(0);
        let peer = std::thread::spawn(move || {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .unwrap();
            file.write_all(&peer_winner[..8]).unwrap();
            file.sync_all().unwrap();
            created_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(30));
            file.write_all(&peer_winner[8..]).unwrap();
            file.sync_all().unwrap();
        });
        created_rx.recv().unwrap();

        let result = persist_generated_secret(dir.path(), gen_secret().unwrap()).unwrap();
        peer.join().unwrap();
        assert_eq!(result, winner);
    }

    fn sample_pid_record() -> PidRecord {
        PidRecord {
            version: 1,
            pid: 4242,
            identity: ProcessIdentity {
                euid: current_euid(),
                started_at: "Tue Jul 28 12:00:00 2026".to_owned(),
                executable: "/usr/local/bin/iphone-use".to_owned(),
                argv: "/usr/local/bin/iphone-use serve".to_owned(),
            },
        }
    }

    #[test]
    fn pid_reuse_start_time_mismatch_is_rejected() {
        let record = sample_pid_record();
        let mut reused = record.identity.clone();
        reused.started_at = "Tue Jul 28 12:00:01 2026".to_owned();
        assert!(validate_pid_identity(&record, &reused).is_err());
    }

    #[test]
    fn pid_command_mismatch_is_rejected() {
        let record = sample_pid_record();
        let mut other = record.identity.clone();
        other.argv = "/usr/bin/unrelated-process --serve".to_owned();
        assert!(validate_pid_identity(&record, &other).is_err());
    }

    #[test]
    fn ps_identity_snapshot_captures_current_process() {
        let identity = read_process_identity(std::process::id() as i32)
            .unwrap()
            .expect("current process must be visible to /bin/ps");
        assert_eq!(identity.euid, current_euid());
        assert!(!identity.started_at.is_empty());
        assert!(!identity.executable.is_empty());
        assert!(!identity.argv.is_empty());
    }

    #[test]
    fn atomic_pid_record_is_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("daemon.pid");
        atomic_replace_private(&path, b"{\"version\":1}\n").unwrap();
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
