//! Remote access from any network: Tailscale and Cloudflare Tunnel.
//!
//! The daemon serves plain HTTP on this Mac. To reach it from outside the
//! LAN it runs one of two helpers as a child process and keeps it running:
//!
//! - **Tailscale**: `iphone-use-tailnet`, a small Go program built on
//!   Tailscale's `tsnet` (the way Rex embeds it). It joins the user's tailnet
//!   as its own device (`iphone-use`, `iphone-use-i13`, …), serves HTTPS with
//!   the node's `*.ts.net` certificate plus plain HTTP on the daemon's port,
//!   admits only the Tailscale account that owns the node (unless told to
//!   admit every tailnet member), and reverse-proxies to the daemon. The
//!   Tailscale app does not need to be installed or signed in on this Mac.
//! - **Cloudflare**: `cloudflared`. With no token it opens a quick tunnel
//!   (a random `*.trycloudflare.com` URL, no account; Cloudflare buffers
//!   streamed responses there, so the live view falls back to stills); with
//!   a tunnel token it runs the user's named tunnel, whose public hostname is
//!   set up in the Cloudflare dashboard.
//!
//! Configuration lives in `<state dir>/remote/remote.json`, written by
//! `iphone-use remote …` and read by the daemon every few seconds, so it
//! survives reinstalls and the nightly upgrade (which rewrite the plist).
//! The daemon still asks for its password on every route: a tunnel only
//! carries traffic, and remote access is refused while no password is set.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How often the supervisor rereads the config and checks its children.
const TICK: Duration = Duration::from_secs(2);
/// A helper that died is restarted after this, doubling up to the cap.
const RESTART_BASE: Duration = Duration::from_secs(2);
const RESTART_MAX: Duration = Duration::from_secs(60);
/// The tailnet helper rewrites its status at least every 5 s; older than this
/// and the status no longer describes a live process.
const STATUS_STALE: Duration = Duration::from_secs(20);
/// Helper logs are cut back to nothing when they pass this at a (re)start.
const LOG_CAP_BYTES: u64 = 4 * 1024 * 1024;

pub const TAILNET_BIN: &str = "iphone-use-tailnet";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    pub tailscale: TailscaleConfig,
    pub cloudflare: CloudflareConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TailscaleConfig {
    pub enabled: bool,
    /// The tailnet device name; empty means [`default_hostname`].
    pub hostname: String,
    /// Admit every member of the tailnet, not only the node's owner.
    pub allow_tailnet_members: bool,
    /// Another control server, such as Headscale.
    pub control_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudflareConfig {
    pub enabled: bool,
    /// A file holding the named tunnel's token; none means a quick tunnel.
    pub token_file: Option<PathBuf>,
    /// The named tunnel's public URL (as set up in the Cloudflare dashboard).
    pub public_url: Option<String>,
}

impl CloudflareConfig {
    pub fn mode(&self) -> &'static str {
        if self.token_file.is_some() {
            "named"
        } else {
            "quick"
        }
    }
}

pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("remote")
}
fn config_path(state_dir: &Path) -> PathBuf {
    dir(state_dir).join("remote.json")
}
fn tailnet_state_dir(state_dir: &Path) -> PathBuf {
    dir(state_dir).join("tailscale")
}
fn tailnet_status_path(state_dir: &Path) -> PathBuf {
    dir(state_dir).join("tailscale-status.json")
}
fn cloudflare_status_path(state_dir: &Path) -> PathBuf {
    dir(state_dir).join("cloudflare-status.json")
}
/// A one-time Tailscale auth key handed to the helper on its first start.
pub fn tailnet_auth_key_path(state_dir: &Path) -> PathBuf {
    dir(state_dir).join("tailscale-authkey")
}
/// Where `iphone-use remote cloudflare on --token-file` keeps its copy.
pub fn cloudflare_token_path(state_dir: &Path) -> PathBuf {
    dir(state_dir).join("cloudflare-token")
}
/// Asking the running helper for a fresh sign-in link.
fn tailnet_login_request_path(state_dir: &Path) -> PathBuf {
    dir(state_dir).join("tailscale-login-request")
}

pub fn load(state_dir: &Path) -> RemoteConfig {
    std::fs::read(config_path(state_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(state_dir: &Path, cfg: &RemoteConfig) -> std::io::Result<()> {
    let path = config_path(state_dir);
    let mut bytes = serde_json::to_vec_pretty(cfg).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    write_private(&path, &bytes)
}

/// Write `bytes` to `path` (mode 0600) by rename, creating the directory.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// `iphone-use` for the default phone, `iphone-use-<instance>` otherwise.
pub fn default_hostname(instance: &str) -> String {
    if instance.is_empty() || instance == crate::instance::DEFAULT_NAME {
        "iphone-use".to_string()
    } else {
        format!("iphone-use-{instance}")
    }
}

/// One DNS label: 1–63 ASCII letters, digits or hyphens, no hyphen at
/// either end.
pub fn valid_hostname(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// A public URL a user typed: `https://host[/path]`, no query or fragment.
pub fn normalize_public_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = if url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    let rest = url.strip_prefix("https://")?;
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty()
        || url.contains('?')
        || url.contains('#')
        || url.contains('@')
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b':')
    {
        return None;
    }
    Some(url)
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// What `iphone-use-tailnet` writes to its status file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TailnetStatus {
    pub pid: Option<u32>,
    pub state: String,
    pub backend_state: Option<String>,
    pub login_url: Option<String>,
    pub dns_name: Option<String>,
    pub tailnet_ips: Vec<String>,
    pub https: bool,
    pub https_pending: bool,
    pub magic_dns: bool,
    pub owner: Option<String>,
    pub hint: Option<String>,
    pub error: Option<String>,
}

/// What the supervisor records about `cloudflared`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudflareStatus {
    pub pid: Option<u32>,
    pub state: String,
    pub url: Option<String>,
    pub connections: u32,
    pub error: Option<String>,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<(T, SystemTime)> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let bytes = std::fs::read(path).ok()?;
    Some((serde_json::from_slice(&bytes).ok()?, modified))
}

fn fresh(modified: SystemTime) -> bool {
    modified
        .elapsed()
        .map(|age| age < STATUS_STALE)
        .unwrap_or(true)
}

/// The daemon's view of remote access, for `/agent/status` and the CLI.
pub fn status(state_dir: &Path, password_set: bool, port: u16) -> Value {
    let cfg = load(state_dir);
    json!({
        "tailscale": tailscale_status(state_dir, &cfg.tailscale, password_set, port),
        "cloudflare": cloudflare_status(state_dir, &cfg.cloudflare, password_set),
    })
}

fn requires_password() -> Value {
    json!("remote access needs a password: re-run install.sh, or set IPHONE_USE_PASSWORD, then turn it on again")
}

fn tailscale_status(
    state_dir: &Path,
    cfg: &TailscaleConfig,
    password_set: bool,
    port: u16,
) -> Value {
    if !cfg.enabled {
        return json!({"enabled": false, "state": "off"});
    }
    if !password_set {
        return json!({"enabled": true, "state": "error", "error": "password_required", "hint": requires_password()});
    }
    let Some((st, modified)) = read_json::<TailnetStatus>(&tailnet_status_path(state_dir)) else {
        return json!({"enabled": true, "state": "starting"});
    };
    let state = if fresh(modified) || st.state == "stopped" {
        st.state.clone()
    } else {
        "stale".to_string()
    };
    let (url, plain_url) = tailnet_urls(&st, port);
    json!({
        "enabled": true,
        "state": state,
        "hostname": cfg.hostname,
        "dns_name": st.dns_name,
        "url": if state == "running" { url } else { None },
        "plain_url": if state == "running" { plain_url } else { None },
        "login_url": if state == "needs_login" { st.login_url.clone() } else { None },
        "https": st.https,
        "https_pending": st.https_pending,
        "magic_dns": st.magic_dns,
        "owner": st.owner,
        "allow_tailnet_members": cfg.allow_tailnet_members,
        "hint": st.hint,
        "error": st.error,
    })
}

/// The HTTPS URL (MagicDNS name, when the tailnet issues certificates) and
/// the plain one on the tailnet IPv4 address and the daemon's port.
fn tailnet_urls(st: &TailnetStatus, port: u16) -> (Option<String>, Option<String>) {
    let https = st
        .dns_name
        .as_deref()
        .filter(|_| st.https)
        .map(|name| format!("https://{}", name.trim_end_matches('.')));
    let plain = st
        .tailnet_ips
        .iter()
        .find(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
        .map(|ip| format!("http://{ip}:{port}"));
    (https, plain)
}

fn cloudflare_status(state_dir: &Path, cfg: &CloudflareConfig, password_set: bool) -> Value {
    if !cfg.enabled {
        return json!({"enabled": false, "state": "off"});
    }
    if !password_set {
        return json!({"enabled": true, "mode": cfg.mode(), "state": "error", "error": "password_required", "hint": requires_password()});
    }
    let st = read_json::<CloudflareStatus>(&cloudflare_status_path(state_dir))
        .map(|(st, _)| st)
        .unwrap_or_else(|| CloudflareStatus {
            state: "starting".into(),
            ..Default::default()
        });
    let url = match cfg.mode() {
        "named" => cfg.public_url.clone(),
        _ => st.url.clone(),
    };
    let running = st.state == "running";
    json!({
        "enabled": true,
        "mode": cfg.mode(),
        "state": st.state,
        "url": if running { url } else { None },
        "connections": st.connections,
        // Quick tunnels hold a streamed response until it ends, so the live
        // H.264/MJPEG view never arrives there; clients poll stills instead.
        "streams_buffered": cfg.mode() == "quick",
        "error": st.error,
        "hint": if cfg.mode() == "named" && cfg.public_url.is_none() {
            Some("set the tunnel's public hostname with: iphone-use remote cloudflare on --token-file FILE --url https://phone.example.com")
        } else { None },
    })
}

/// The URLs a phone or browser elsewhere can use right now, best first:
/// Tailscale HTTPS, a named Cloudflare tunnel, a quick tunnel, plain tailnet.
pub fn public_urls(state_dir: &Path, password_set: bool, port: u16) -> Vec<String> {
    let st = status(state_dir, password_set, port);
    let mut urls = Vec::new();
    let ts = &st["tailscale"];
    let cf = &st["cloudflare"];
    if let Some(url) = ts["url"].as_str() {
        urls.push(url.to_string());
    }
    if let Some(url) = cf["url"].as_str() {
        if cf["mode"] == "named" {
            urls.insert(urls.len().min(1), url.to_string());
        } else {
            urls.push(url.to_string());
        }
    }
    if let Some(url) = ts["plain_url"].as_str() {
        urls.push(url.to_string());
    }
    urls.dedup();
    urls
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// Where the helpers are and what they connect to.
#[derive(Clone)]
pub struct SupervisorInputs {
    pub state_dir: PathBuf,
    pub instance: String,
    /// The daemon as the helpers reach it, e.g. `http://127.0.0.1:44321`.
    pub upstream: String,
    pub port: u16,
    pub password_set: bool,
}

/// Keep the configured helpers running for the life of the daemon.
pub fn spawn_supervisor(inputs: SupervisorInputs) {
    let _ = std::thread::Builder::new()
        .name("remote-supervisor".into())
        .spawn(move || {
            let mut tailnet = Helper::new("tailnet");
            let mut cloudflare = Helper::new("cloudflared");
            let cf_status = Arc::new(Mutex::new(CloudflareStatus::default()));
            reap_orphans(&inputs.state_dir);
            loop {
                let cfg = load(&inputs.state_dir);
                let allowed = inputs.password_set;
                let ts_want = (cfg.tailscale.enabled && allowed)
                    .then(|| tailnet_command(&inputs, &cfg.tailscale));
                tailnet.reconcile(&inputs.state_dir, ts_want, None, |_| {});
                if tailnet.child.is_some() {
                    forward_login_request(&inputs.state_dir, &tailnet);
                }

                let cf_want = (cfg.cloudflare.enabled && allowed)
                    .then(|| cloudflared_command(&inputs, &cfg.cloudflare));
                cloudflare.reconcile(
                    &inputs.state_dir,
                    cf_want,
                    Some(cloudflared_line_hook(
                        cf_status.clone(),
                        inputs.state_dir.clone(),
                    )),
                    // Before its log reader starts, so no line of the new
                    // process is applied to the old one's status.
                    |pid| {
                        let mut st = cf_status.lock().unwrap();
                        *st = CloudflareStatus {
                            pid: Some(pid),
                            state: "starting".into(),
                            ..Default::default()
                        };
                        write_cloudflare_status(&inputs.state_dir, &st);
                    },
                );
                if !cfg.cloudflare.enabled || !allowed {
                    let _ = std::fs::remove_file(cloudflare_status_path(&inputs.state_dir));
                } else if let Some(error) = cloudflare.last_error.take() {
                    let mut st = cf_status.lock().unwrap();
                    st.state = "error".into();
                    st.error = Some(error);
                    write_cloudflare_status(&inputs.state_dir, &st);
                } else if cloudflare.child.is_none() {
                    // cloudflared exited and waits for its restart: its URL no
                    // longer answers (a quick tunnel gets a new one).
                    let mut st = cf_status.lock().unwrap();
                    if mark_cloudflared_down(&mut st) {
                        write_cloudflare_status(&inputs.state_dir, &st);
                    }
                }
                std::thread::sleep(TICK);
            }
        });
}

/// A helper the supervisor runs: the command it was started with, so a
/// config change restarts it, and the restart backoff.
struct Helper {
    name: &'static str,
    child: Option<Child>,
    running: Option<Vec<String>>,
    next_start: Instant,
    backoff: Duration,
    last_error: Option<String>,
}

impl Helper {
    fn new(name: &'static str) -> Helper {
        Helper {
            name,
            child: None,
            running: None,
            next_start: Instant::now(),
            backoff: RESTART_BASE,
            last_error: None,
        }
    }

    /// Bring the helper in line with `want`. Returns true when it started.
    fn reconcile(
        &mut self,
        state_dir: &Path,
        want: Option<Result<Vec<String>, String>>,
        on_line: Option<LineHook>,
        on_spawn: impl FnOnce(u32),
    ) -> bool {
        // Notice an exit first.
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(exit)) = child.try_wait() {
                tracing::warn!("remote access: {} exited ({exit})", self.name);
                self.child = None;
                self.running = None;
                self.next_start = Instant::now() + self.backoff;
                self.backoff = (self.backoff * 2).min(RESTART_MAX);
                let _ = std::fs::remove_file(pid_path(state_dir, self.name));
            }
        }
        let want = match want {
            None => {
                self.stop(state_dir);
                return false;
            }
            Some(Err(error)) => {
                self.stop(state_dir);
                if self.last_error.as_deref() != Some(&error) {
                    tracing::warn!("remote access: {}: {error}", self.name);
                }
                self.last_error = Some(error);
                return false;
            }
            Some(Ok(argv)) => argv,
        };
        if self.running.as_ref() == Some(&want) {
            // Healthy for a while: forget earlier crashes.
            if self.child.is_some() && Instant::now() > self.next_start + RESTART_MAX {
                self.backoff = RESTART_BASE;
            }
            return false;
        }
        self.stop(state_dir);
        if Instant::now() < self.next_start {
            return false;
        }
        let log = dir(state_dir).join(format!("{}.log", self.name));
        let Ok(log_file) = open_log(&log) else {
            return false;
        };
        let mut cmd = Command::new(&want[0]);
        cmd.args(&want[1..])
            .stdin(Stdio::null())
            .stdout(
                log_file
                    .try_clone()
                    .map(Stdio::from)
                    .unwrap_or(Stdio::null()),
            )
            .stderr(Stdio::piped())
            // Never let the helper find the daemon's secrets in its env.
            .env_remove("IPHONE_USE_PASSWORD")
            .env_remove("IPHONE_USE_AGENT_TOKEN")
            .env_remove("IPHONE_USE_SECRET")
            .env_remove("PHONE_REMOTE_PASSWORD")
            .env_remove("PHONE_REMOTE_AGENT_TOKEN")
            .env_remove("PHONE_REMOTE_SECRET")
            .env_remove("TUNNEL_TOKEN")
            // tsnet uploads its logs to Tailscale unless told not to; keep
            // the daemon's no-telemetry default (docs/privacy.md).
            .env("TS_NO_LOGS_NO_SUPPORT", "true");
        match cmd.spawn() {
            Ok(mut child) => {
                tracing::info!("remote access: started {} (pid {})", self.name, child.id());
                let _ = std::fs::write(pid_path(state_dir, self.name), child.id().to_string());
                on_spawn(child.id());
                if let Some(stderr) = child.stderr.take() {
                    let mut log_file = log_file;
                    let mut on_line = on_line;
                    let pid = child.id();
                    let _ = std::thread::Builder::new()
                        .name(format!("{}-log", self.name))
                        .spawn(move || {
                            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                                let _ = writeln!(log_file, "{line}");
                                if let Some(hook) = on_line.as_mut() {
                                    hook(pid, &line);
                                }
                            }
                        });
                }
                self.child = Some(child);
                self.running = Some(want);
                self.last_error = None;
                true
            }
            Err(e) => {
                let error = format!("could not start {}: {e}", want[0]);
                tracing::warn!("remote access: {error}");
                self.last_error = Some(error);
                self.next_start = Instant::now() + self.backoff;
                self.backoff = (self.backoff * 2).min(RESTART_MAX);
                false
            }
        }
    }

    fn stop(&mut self, state_dir: &Path) {
        if let Some(mut child) = self.child.take() {
            tracing::info!("remote access: stopping {}", self.name);
            terminate(&mut child);
            let _ = std::fs::remove_file(pid_path(state_dir, self.name));
        }
        self.running = None;
    }
}

/// SIGTERM, then SIGKILL after 5 s.
fn terminate(child: &mut Child) {
    // SAFETY: signalling our own child by pid.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn pid_path(state_dir: &Path, name: &str) -> PathBuf {
    dir(state_dir).join(format!("{name}.pid"))
}

/// A helper left behind by a daemon that crashed would hold the tailnet
/// state directory; stop it if its command line still names our state dir.
fn reap_orphans(state_dir: &Path) {
    for name in ["tailnet", "cloudflared"] {
        let path = pid_path(state_dir, name);
        let Some(pid) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        else {
            continue;
        };
        let _ = std::fs::remove_file(&path);
        let command = Command::new("/bin/ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        if command.contains(&*dir(state_dir).to_string_lossy()) {
            tracing::info!(
                "remote access: stopping {name} left over from an earlier daemon (pid {pid})"
            );
            // SAFETY: a pid we started earlier, checked by its command line.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let truncate = std::fs::metadata(path)
        .map(|m| m.len() > LOG_CAP_BYTES)
        .unwrap_or(false);
    std::fs::OpenOptions::new()
        .create(true)
        .append(!truncate)
        .write(true)
        .truncate(truncate)
        .open(path)
}

/// The tailnet helper: next to this executable, or `IPHONE_USE_TAILNET_BIN`.
pub fn tailnet_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("IPHONE_USE_TAILNET_BIN") {
        return Some(PathBuf::from(path));
    }
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join(TAILNET_BIN);
    path.is_file().then_some(path)
}

/// `cloudflared`: `IPHONE_USE_CLOUDFLARED`, then PATH, then Homebrew.
/// launchd gives the daemon a minimal PATH, so the usual install places are
/// checked by name.
pub fn cloudflared_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("IPHONE_USE_CLOUDFLARED") {
        return Some(PathBuf::from(path));
    }
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("cloudflared"))
                .collect()
        })
        .unwrap_or_default();
    for fixed in [
        "/opt/homebrew/bin/cloudflared",
        "/usr/local/bin/cloudflared",
    ] {
        candidates.push(PathBuf::from(fixed));
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn tailnet_command(
    inputs: &SupervisorInputs,
    cfg: &TailscaleConfig,
) -> Result<Vec<String>, String> {
    let bin = tailnet_binary().ok_or_else(|| {
        format!("{TAILNET_BIN} is missing next to iphone-use; reinstall or upgrade iphone-use")
    })?;
    let hostname = if cfg.hostname.is_empty() {
        default_hostname(&inputs.instance)
    } else {
        cfg.hostname.clone()
    };
    let mut argv = vec![
        bin.to_string_lossy().into_owned(),
        "serve".into(),
        "--state-dir".into(),
        tailnet_state_dir(&inputs.state_dir)
            .to_string_lossy()
            .into_owned(),
        "--hostname".into(),
        hostname,
        "--upstream".into(),
        inputs.upstream.clone(),
        "--status-file".into(),
        tailnet_status_path(&inputs.state_dir)
            .to_string_lossy()
            .into_owned(),
        "--plain-port".into(),
        inputs.port.to_string(),
    ];
    if cfg.allow_tailnet_members {
        argv.push("--allow-tailnet-members".into());
    }
    let key = tailnet_auth_key_path(&inputs.state_dir);
    if key.is_file() {
        argv.push("--auth-key-file".into());
        argv.push(key.to_string_lossy().into_owned());
    }
    if let Some(url) = cfg.control_url.as_deref().filter(|u| !u.is_empty()) {
        argv.push("--control-url".into());
        argv.push(url.to_string());
    }
    Ok(argv)
}

/// `iphone-use remote tailscale login` drops a request file; pass it on as
/// SIGUSR1, which makes the helper ask Tailscale for a new sign-in link.
fn forward_login_request(state_dir: &Path, helper: &Helper) {
    let path = tailnet_login_request_path(state_dir);
    if path.exists() {
        let _ = std::fs::remove_file(&path);
        if let Some(child) = helper.child.as_ref() {
            // SAFETY: signalling our own child by pid.
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGUSR1);
            }
        }
    }
}

pub fn request_tailnet_login(state_dir: &Path) -> std::io::Result<()> {
    write_private(&tailnet_login_request_path(state_dir), b"1\n")
}

fn cloudflared_command(
    inputs: &SupervisorInputs,
    cfg: &CloudflareConfig,
) -> Result<Vec<String>, String> {
    let bin = cloudflared_binary().ok_or_else(|| {
        "cloudflared is not installed; install it with: brew install cloudflared".to_string()
    })?;
    // An empty config of our own: the user's ~/.cloudflared/config.yml may
    // hold ingress rules for another tunnel, and those override `--url`
    // (every request then gets the catch-all 404).
    let empty_config = dir(&inputs.state_dir).join("cloudflared.yml");
    if !empty_config.is_file() {
        write_private(
            &empty_config,
            b"# iphone-use: intentionally empty, see remote.rs\n",
        )
        .map_err(|e| format!("write {}: {e}", empty_config.display()))?;
    }
    let mut argv = vec![
        bin.to_string_lossy().into_owned(),
        "tunnel".into(),
        "--config".into(),
        empty_config.to_string_lossy().into_owned(),
        "--no-autoupdate".into(),
        "--metrics".into(),
        "127.0.0.1:0".into(),
    ];
    match cfg.token_file.as_ref() {
        Some(token) => {
            argv.extend([
                "run".into(),
                "--token-file".into(),
                token.to_string_lossy().into_owned(),
            ]);
        }
        None => {
            argv.extend(["--url".into(), inputs.upstream.clone()]);
        }
    }
    Ok(argv)
}

/// Called with each stderr line of a helper (and its pid).
type LineHook = Box<dyn FnMut(u32, &str) + Send>;

/// Follow cloudflared's log for the quick-tunnel URL and its connections.
fn cloudflared_line_hook(status: Arc<Mutex<CloudflareStatus>>, state_dir: PathBuf) -> LineHook {
    Box::new(move |pid, line| {
        let mut st = status.lock().unwrap();
        if st.pid != Some(pid) {
            return; // an older cloudflared still flushing its last lines
        }
        if apply_cloudflared_line(&mut st, line) {
            write_cloudflare_status(&state_dir, &st);
        }
    })
}

/// cloudflared is not running: nothing answers at its URL. True when this
/// changed `st`.
fn mark_cloudflared_down(st: &mut CloudflareStatus) -> bool {
    if st.state == "running" || st.url.is_some() || st.connections > 0 {
        st.state = "starting".into();
        st.url = None;
        st.connections = 0;
        st.pid = None;
        return true;
    }
    false
}

/// Update `st` from one cloudflared log line; true when it changed.
pub fn apply_cloudflared_line(st: &mut CloudflareStatus, line: &str) -> bool {
    let before = st.clone();
    if let Some(url) = quick_tunnel_url(line) {
        st.url = Some(url);
    }
    if line.contains("Registered tunnel connection") {
        st.connections += 1;
        st.state = "running".into();
        st.error = None;
    } else if line.contains("Unregistered tunnel connection")
        || line.contains("Connection terminated")
    {
        st.connections = st.connections.saturating_sub(1);
        if st.connections == 0 {
            st.state = "starting".into();
        }
    } else if line.contains(" ERR ") && st.state != "running" {
        let message = line.split(" ERR ").nth(1).unwrap_or(line).trim();
        st.error = Some(message.chars().take(300).collect());
    }
    *st != before
}

fn quick_tunnel_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|' || c == '"')
        .unwrap_or(rest.len());
    let url = &rest[..end];
    url.ends_with(".trycloudflare.com").then(|| url.to_string())
}

fn write_cloudflare_status(state_dir: &Path, st: &CloudflareStatus) {
    if let Ok(bytes) = serde_json::to_vec(st) {
        let _ = write_private(&cloudflare_status_path(state_dir), &bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames_follow_the_instance_and_dns_rules() {
        assert_eq!(default_hostname("default"), "iphone-use");
        assert_eq!(default_hostname(""), "iphone-use");
        assert_eq!(default_hostname("i13"), "iphone-use-i13");
        assert!(valid_hostname("iphone-use-i13"));
        assert!(!valid_hostname("-x"));
        assert!(!valid_hostname("a.b"));
        assert!(!valid_hostname(&"a".repeat(64)));
    }

    #[test]
    fn public_urls_must_be_plain_https() {
        assert_eq!(
            normalize_public_url("phone.example.com/").as_deref(),
            Some("https://phone.example.com")
        );
        assert_eq!(
            normalize_public_url("https://remote.example.com/ix").as_deref(),
            Some("https://remote.example.com/ix")
        );
        assert_eq!(normalize_public_url("http://phone.example.com"), None);
        assert_eq!(normalize_public_url("https://a@b.com"), None);
        assert_eq!(normalize_public_url("https://b.com/?x"), None);
    }

    #[test]
    fn cloudflared_lines_give_the_url_and_the_connection_state() {
        let mut st = CloudflareStatus {
            state: "starting".into(),
            ..Default::default()
        };
        assert!(apply_cloudflared_line(
            &mut st,
            "2026-11-07T04:10:30Z INF |  https://registrar-email-exploring-los.trycloudflare.com                                  |"
        ));
        assert_eq!(
            st.url.as_deref(),
            Some("https://registrar-email-exploring-los.trycloudflare.com")
        );
        assert_eq!(st.state, "starting");
        // The banner also names the terms-of-service page; not a tunnel URL.
        assert!(!apply_cloudflared_line(
            &mut st,
            "INF Thank you for trying Cloudflare Tunnel. https://www.cloudflare.com/website-terms/"
        ));
        assert!(apply_cloudflared_line(
            &mut st,
            "2026-11-07T04:10:36Z INF Registered tunnel connection connIndex=0 connection=5b00 event=0 ip=2606:4700:a8::6 location=nrt17 protocol=http2"
        ));
        assert_eq!((st.state.as_str(), st.connections), ("running", 1));
        assert!(apply_cloudflared_line(
            &mut st,
            "INF Unregistered tunnel connection connIndex=0"
        ));
        assert_eq!((st.state.as_str(), st.connections), ("starting", 0));
        assert!(apply_cloudflared_line(
            &mut st,
            "2026-11-07T04:11:00Z ERR Couldn't start tunnel error=\"Unauthorized: Invalid tunnel secret\""
        ));
        assert!(st
            .error
            .as_deref()
            .unwrap()
            .contains("Invalid tunnel secret"));
    }

    #[test]
    fn an_exited_cloudflared_no_longer_reports_its_url() {
        let mut st = CloudflareStatus {
            pid: Some(7),
            state: "running".into(),
            url: Some("https://a-b.trycloudflare.com".into()),
            connections: 4,
            error: None,
        };
        assert!(mark_cloudflared_down(&mut st));
        assert_eq!(
            (st.state.as_str(), st.url.as_deref(), st.connections),
            ("starting", None, 0)
        );
        assert!(!mark_cloudflared_down(&mut st));
    }

    #[test]
    fn tailnet_urls_prefer_https_and_use_the_ipv4_address_for_plain() {
        let st = TailnetStatus {
            dns_name: Some("iphone-use.tail1eab92.ts.net.".into()),
            tailnet_ips: vec!["fd7a:115c:a1e0::1".into(), "100.101.102.103".into()],
            https: true,
            ..Default::default()
        };
        assert_eq!(
            tailnet_urls(&st, 44321),
            (
                Some("https://iphone-use.tail1eab92.ts.net".into()),
                Some("http://100.101.102.103:44321".into())
            )
        );
        let no_certs = TailnetStatus { https: false, ..st };
        assert_eq!(tailnet_urls(&no_certs, 44321).0, None);
    }

    #[test]
    fn status_reports_off_and_refuses_without_a_password() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path();
        let off = status(state_dir, true, 44321);
        assert_eq!(off["tailscale"]["state"], "off");
        assert_eq!(off["cloudflare"]["state"], "off");
        let mut cfg = RemoteConfig::default();
        cfg.tailscale.enabled = true;
        cfg.cloudflare.enabled = true;
        save(state_dir, &cfg).unwrap();
        assert_eq!(load(state_dir), cfg);
        let open = status(state_dir, false, 44321);
        assert_eq!(open["tailscale"]["error"], "password_required");
        assert_eq!(open["cloudflare"]["error"], "password_required");
        assert!(public_urls(state_dir, false, 44321).is_empty());
        let starting = status(state_dir, true, 44321);
        assert_eq!(starting["tailscale"]["state"], "starting");
        assert_eq!(starting["cloudflare"]["streams_buffered"], true);
    }

    #[test]
    fn running_helpers_publish_their_urls_best_first() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path();
        let mut cfg = RemoteConfig::default();
        cfg.tailscale.enabled = true;
        cfg.cloudflare.enabled = true;
        save(state_dir, &cfg).unwrap();
        write_private(
            &tailnet_status_path(state_dir),
            br#"{"version":1,"state":"running","dns_name":"iphone-use.tail1.ts.net","tailnet_ips":["100.1.2.3"],"https":true}"#,
        )
        .unwrap();
        write_cloudflare_status(
            state_dir,
            &CloudflareStatus {
                state: "running".into(),
                url: Some("https://a-b.trycloudflare.com".into()),
                connections: 1,
                ..Default::default()
            },
        );
        let port = 44321;
        assert_eq!(
            public_urls(state_dir, true, port),
            vec![
                "https://iphone-use.tail1.ts.net".to_string(),
                "https://a-b.trycloudflare.com".to_string(),
                format!("http://100.1.2.3:{port}"),
            ]
        );
    }
}
