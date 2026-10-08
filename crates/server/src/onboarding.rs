//! First-run commands: `iphone-use setup | doctor | status | try | login`.
//!
//! They find the installed daemon of one instance (`--instance`, else
//! `PHONE_REMOTE_INSTANCE`, else the default) through its LaunchAgent, so a
//! person never has to know where `setup-wda.sh` lives, which port the daemon
//! took, or what the agent token is.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use server::instance::Instance;

const DEFAULT_PORT: u16 = 44321;
const TRY_OWNER: &str = "iphone-use-try";
/// A parked runner rebuilds from its cached product in ~15–25 s; a first
/// start after an upgrade can take a minute.
const START_WAIT: Duration = Duration::from_secs(150);

pub struct Target {
    pub instance: Instance,
    base: String,
    token: Option<String>,
}

impl Target {
    pub fn resolve(name: Option<&str>) -> Result<Target> {
        let name = name
            .map(str::to_string)
            .or_else(|| std::env::var("PHONE_REMOTE_INSTANCE").ok())
            .unwrap_or_default();
        let home = std::env::var("HOME").context("HOME is not set")?;
        let instance = Instance::derive(&name, home, None).map_err(|e| anyhow!(e))?;
        // The same order as the MCP server: explicit environment first, then
        // the daemon's own LaunchAgent.
        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
        let base = env("PHONE_REMOTE_URL")
            .map(|url| url.trim_end_matches('/').to_string())
            .unwrap_or_else(|| {
                let port = plist_env(&instance.daemon_label, "PHONE_REMOTE_PORT")
                    .and_then(|p| p.trim().parse::<u16>().ok())
                    .unwrap_or(DEFAULT_PORT);
                format!("http://127.0.0.1:{port}")
            });
        let token = env("PHONE_REMOTE_TOKEN").or_else(|| {
            plist_env(&instance.daemon_label, "PHONE_REMOTE_AGENT_TOKEN")
                .or_else(|| plist_env(&instance.daemon_label, "PHONE_REMOTE_PASSWORD"))
        });
        Ok(Target {
            instance,
            base,
            token,
        })
    }

    fn base(&self) -> String {
        self.base.clone()
    }

    fn setup_script(&self) -> std::path::PathBuf {
        self.instance.state_dir.join("setup-wda.sh")
    }

    fn named(&self) -> Option<&str> {
        (self.instance.name != server::instance::DEFAULT_NAME)
            .then_some(self.instance.name.as_str())
    }

    /// The command that reaches this same instance again, for messages.
    fn command(&self, sub: &str) -> String {
        match self.named() {
            Some(name) => format!("iphone-use {sub} --instance {name}"),
            None => format!("iphone-use {sub}"),
        }
    }
}

fn plist_env(label: &str, key: &str) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let plist = std::path::Path::new(&home).join(format!("Library/LaunchAgents/{label}.plist"));
    if !plist.is_file() {
        return None;
    }
    let out = std::process::Command::new("/usr/bin/plutil")
        .args([
            "-extract",
            &format!("EnvironmentVariables.{key}"),
            "raw",
            "-o",
            "-",
        ])
        .arg(&plist)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// test / schedule: iphone-use-mcp, pointed at this instance
// ---------------------------------------------------------------------------

/// Run `iphone-use-mcp <args…>` against this instance's daemon and return
/// its exit code. The token goes through the environment, never argv.
pub fn run_mcp(target: &Target, args: &[String]) -> i32 {
    let Some(mcp) = server::flows::mcp_binary() else {
        eprintln!("iphone-use-mcp is not installed next to iphone-use; re-run the installer");
        return 2;
    };
    let mut command = std::process::Command::new(mcp);
    command.args(args).env("PHONE_REMOTE_URL", target.base());
    match &target.token {
        Some(token) => command.env("PHONE_REMOTE_TOKEN", token),
        None => command.env_remove("PHONE_REMOTE_TOKEN"),
    };
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("could not run iphone-use-mcp: {error}");
            2
        }
    }
}

// ---------------------------------------------------------------------------
// setup / doctor: the instance's own setup-wda.sh
// ---------------------------------------------------------------------------

pub fn run_setup(target: &Target, args: &[String]) -> i32 {
    let script = target.setup_script();
    if !script.is_file() {
        eprintln!(
            "iphone-use is not installed for this {} ({} is missing). Install it with:\n  curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh",
            match target.named() {
                Some(name) => format!("instance ({name})"),
                None => "Mac".to_string(),
            },
            script.display()
        );
        return 2;
    }
    // `--force`: take the phone over even while another session holds it.
    // `--xcode <Xcode.app|system>`: this phone's own Xcode, persisted.
    let force = args.iter().any(|a| a == "--force");
    let mut xcode = None;
    let mut rest: Vec<&String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--force" {
            continue;
        }
        if arg == "--xcode" {
            xcode = iter.next().cloned();
            continue;
        }
        if let Some(value) = arg.strip_prefix("--xcode=") {
            xcode = Some(value.to_string());
            continue;
        }
        rest.push(arg);
    }
    let args = rest;
    let mut command = std::process::Command::new("/bin/bash");
    command.arg(&script).args(args);
    if force {
        command.env("IPHONE_USE_SETUP_FORCE", "1");
    }
    if let Some(xcode) = xcode {
        command.env("IPHONE_USE_XCODE", xcode);
    }
    if let Some(name) = target.named() {
        command.env("PHONE_REMOTE_INSTANCE", name);
    }
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("could not run {}: {error}", script.display());
            1
        }
    }
}

/// `iphone-use doctor`: the native preflight, for this instance.
pub fn run_doctor(target: &Target) -> i32 {
    // Before any thread exists: the setup engine resolves the instance (and
    // everything else) from the environment, exactly as the script does.
    std::env::set_var("PHONE_REMOTE_INSTANCE", &target.instance.name);
    server::setup::main(&["doctor".to_string()])
}

// ---------------------------------------------------------------------------
// a tiny blocking client for the daemon's agent API
// ---------------------------------------------------------------------------

struct Daemon<'a> {
    target: &'a Target,
    owner: String,
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

impl<'a> Daemon<'a> {
    fn new(target: &'a Target) -> Result<Self> {
        Ok(Daemon {
            target,
            owner: TRY_OWNER.to_string(),
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
            client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(60))
                .build()?,
        })
    }

    fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut request = self
            .client
            .request(method.clone(), format!("{}{path}", self.target.base()))
            .header("X-Phone-Owner", &self.owner);
        if let Some(token) = &self.target.token {
            request = request.bearer_auth(token);
        }
        if method != reqwest::Method::GET {
            request = request.header("X-Phone-Control", "1");
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        self.runtime.block_on(async {
            let response = request.send().await.map_err(|error| {
                anyhow!(
                    "the iphone-use daemon is not answering on {} ({error}); is it installed? run {}",
                    self.target.base(),
                    self.target.command("doctor")
                )
            })?;
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
            Ok((status, value))
        })
    }

    fn status(&self) -> Result<Value> {
        let (code, value) = self.call(reqwest::Method::GET, "/agent/status", None)?;
        if code == 401 {
            bail!("the daemon refused the token in its LaunchAgent (HTTP 401)");
        }
        Ok(value)
    }

    fn metrics(&self, owner: Option<&str>) -> Result<Value> {
        let path = match owner {
            Some(owner) => format!("/agent/metrics?owner={}", urlencode(owner)),
            None => "/agent/metrics".to_string(),
        };
        let (code, value) = self.call(reqwest::Method::GET, &path, None)?;
        if code == 401 {
            bail!("the daemon refused the token in its LaunchAgent (HTTP 401)");
        }
        if code == 404 {
            bail!("this daemon has no /agent/metrics yet; upgrade it (iphone-use upgrade)");
        }
        // Only a 2xx that says ok:true is a report; anything else is an
        // error, never rendered as "0 runs".
        if !(200..300).contains(&code) || value["ok"] != Value::Bool(true) {
            let reason = value["error"].as_str().unwrap_or("no report in the answer");
            bail!("the daemon could not report metrics (HTTP {code}: {reason})");
        }
        Ok(value)
    }

    fn input(&self, action: Value) -> Result<Value> {
        let (code, value) = self.call(reqwest::Method::POST, "/agent/input", Some(action))?;
        if code == 409 {
            bail!("{}", owned_message(&value));
        }
        if value.get("ok") != Some(&Value::Bool(true)) {
            bail!(
                "the phone did not accept the step: {}",
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            );
        }
        Ok(value)
    }

    fn release(&self) {
        let _ = self.call(
            reqwest::Method::POST,
            "/agent/owner",
            Some(json!({ "release": true })),
        );
    }
}

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        // Same reason as `update::fetch_latest_tag_blocking`: never wait on a
        // DNS/connect task that outlived its timeout.
        let runtime = std::mem::replace(
            &mut self.runtime,
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("an empty runtime"),
        );
        runtime.shutdown_background();
    }
}

fn owned_message(value: &Value) -> String {
    let owner = value
        .get("owner")
        .and_then(Value::as_str)
        .unwrap_or("another session");
    format!("the phone is being driven by {owner}; try again when it is done")
}

/// What the person should do next, in their language when we have both.
fn next_step(status: &Value) -> Option<String> {
    let step = status.get("next_step")?;
    let zh = step.get("zh").and_then(Value::as_str);
    let en = step.get("en").and_then(Value::as_str);
    let chinese = std::env::var("LANG")
        .map(|lang| lang.starts_with("zh"))
        .unwrap_or(false);
    match (zh, en) {
        (Some(zh), _) if chinese => Some(zh.to_string()),
        (_, Some(en)) => Some(en.to_string()),
        (Some(zh), None) => Some(zh.to_string()),
        _ => None,
    }
}

fn hint(status: &Value) -> Option<String> {
    next_step(status).or_else(|| {
        status
            .get("hint")
            .and_then(Value::as_str)
            .filter(|h| !h.is_empty())
            .map(str::to_string)
    })
}

// ---------------------------------------------------------------------------
// auth: log the app on the phone in from the password vault
// ---------------------------------------------------------------------------

/// `iphone-use auth login --bwu` / `auth code`: one call to the daemon, which
/// reads the vault itself. Prints the daemon's answer; the exit code is 0 only
/// when it says ok.
pub fn run_auth(target: &Target, path: &str, body: Value) -> Result<i32> {
    let mut daemon = Daemon::new(target)?;
    daemon.owner = std::env::var("PHONE_REMOTE_OWNER")
        .ok()
        .filter(|owner| !owner.trim().is_empty())
        .unwrap_or_else(|| "iphone-use-auth".to_string());
    let (code, value) = daemon.call(reqwest::Method::POST, path, Some(body))?;
    if code == 409 && value.get("error").and_then(Value::as_str) == Some("phone_owned") {
        bail!("{}", owned_message(&value));
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    // Sent is not done: a form still on screen after the tap is a failure.
    let still = |key: &str| value.get(key) == Some(&Value::Bool(true));
    Ok(
        if value.get("ok") == Some(&Value::Bool(true))
            && !still("login_form_still_visible")
            && !still("code_field_still_visible")
        {
            0
        } else {
            1
        },
    )
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `iphone-use metrics`: per-run task metrics from the daemon.
pub fn run_metrics(target: &Target, owner: Option<&str>, as_json: bool) -> Result<i32> {
    let daemon = Daemon::new(target)?;
    let report = daemon.metrics(owner)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(0);
    }
    print!("{}", format_metrics(&report));
    Ok(0)
}

/// A short human view of `/agent/metrics`: one line per run.
fn format_metrics(report: &Value) -> String {
    let mut out = String::new();
    let line = |run: &Value| {
        let owner = run["key"]["owner"].as_str().unwrap_or("?");
        let id = run["key"]["run_id"].as_str().unwrap_or("(inferred)");
        let state = run["closed"].as_str().unwrap_or("unknown");
        let ms = |key: &str| {
            run[key]
                .as_u64()
                .map_or("-".to_string(), |v| format!("{v}ms"))
        };
        format!(
            "  {owner} {id} [state: {state}]: {} calls ({} batch, {} observed, {} flow), p50 {} p95 {}, \
             stale {}, unknown {}, model turns {}{}\n",
            run["tool_calls"],
            run["batch_calls"],
            run["observed_calls"],
            run["flow_calls"],
            ms("call_p50_ms"),
            ms("call_p95_ms"),
            run["stale"],
            run["outcome_unknown"],
            run["model_round_trips"]
                .as_u64()
                .map_or("n/a".to_string(), |v| v.to_string()),
            if run["incomplete"] == Value::Bool(true) {
                " — incomplete"
            } else {
                ""
            },
        )
    };
    for (title, key) in [("open runs", "open"), ("recent runs", "recent")] {
        let runs = report[key].as_array().cloned().unwrap_or_default();
        out.push_str(&format!("{title}: {}\n", runs.len()));
        for run in &runs {
            out.push_str(&line(run));
        }
    }
    out.push_str("calls are HTTP calls to this daemon, not model turns\n");
    out
}

pub fn run_status(target: &Target, as_json: bool) -> Result<i32> {
    let daemon = Daemon::new(target)?;
    let status = daemon.status()?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_status(target, &status);
    }
    Ok(if status["drivable"] == Value::Bool(true) {
        0
    } else {
        1
    })
}

fn print_status(target: &Target, status: &Value) {
    let version = status["version"].as_str().unwrap_or("?");
    let instance = target
        .named()
        .map(|n| format!(" ({n})"))
        .unwrap_or_default();
    if status["drivable"] == Value::Bool(true) {
        println!("✓ iphone-use {version}{instance}: the iPhone is ready for agents");
    } else {
        let mode = status["mode"].as_str().unwrap_or("?");
        let blocker = status["setup_blocked_on"].as_str().unwrap_or("");
        let state = match (mode, blocker) {
            (_, b) if !b.is_empty() => format!("blocked ({b})"),
            ("offline", _) if status["released"] == Value::Bool(true) => {
                "parked while idle; the next agent request starts it".to_string()
            }
            (m, _) => m.to_string(),
        };
        println!("✗ iphone-use {version}{instance}: not ready — {state}");
        if let Some(hint) = hint(status) {
            println!("  {hint}");
        }
    }
    if let Some(owner) = status["owner"].as_str() {
        println!("  in use by: {owner}");
    }
}

// ---------------------------------------------------------------------------
// try: a harmless first run
// ---------------------------------------------------------------------------

pub fn run_try(target: &Target) -> Result<i32> {
    let daemon = Daemon::new(target)?;
    let result = try_steps(&daemon);
    daemon.release();
    match result {
        Ok(()) => Ok(0),
        Err(error) => {
            eprintln!("✗ {error:#}");
            Ok(1)
        }
    }
}

fn try_steps(daemon: &Daemon) -> Result<()> {
    let mut status = daemon.status()?;
    if let Some(owner) = status["owner"].as_str() {
        if owner != TRY_OWNER {
            bail!("{}", owned_message(&status));
        }
    }
    if status["drivable"] != Value::Bool(true) {
        let blocker = status["setup_blocked_on"].as_str().unwrap_or("");
        if !blocker.is_empty() {
            bail!(
                "the iPhone is not ready: {}\n  then run {}",
                hint(&status).unwrap_or_else(|| blocker.to_string()),
                daemon.target.command("try")
            );
        }
        println!("Starting the device runner on the iPhone (keep it unlocked)…");
        let (code, value) = daemon.call(
            reqwest::Method::POST,
            "/agent/mode",
            Some(json!({ "mode": "agent" })),
        )?;
        if code == 409 {
            bail!("{}", owned_message(&value));
        }
        let deadline = Instant::now() + START_WAIT;
        loop {
            std::thread::sleep(Duration::from_secs(3));
            status = daemon.status()?;
            if status["drivable"] == Value::Bool(true) {
                break;
            }
            let blocker = status["setup_blocked_on"].as_str().unwrap_or("");
            if !blocker.is_empty() || Instant::now() > deadline {
                bail!(
                    "the iPhone did not become ready: {}\n  check with {}",
                    hint(&status).unwrap_or_else(|| "no reason reported".to_string()),
                    daemon.target.command("doctor")
                );
            }
        }
    }

    println!("Opening Settings on the iPhone…");
    daemon.input(json!({ "type": "launch_app", "bundle": "com.apple.Preferences" }))?;
    std::thread::sleep(Duration::from_millis(800));
    let started = Instant::now();
    let (code, screen) = daemon.call(reqwest::Method::GET, "/agent/elements", None)?;
    if code != 200 {
        bail!("could not read the screen (HTTP {code})");
    }
    let seconds = started.elapsed().as_secs_f32();
    let labels = visible_labels(&screen, 8);
    if labels.is_empty() {
        println!("Read the screen in {seconds:.1} s, but it had no labelled controls.");
    } else {
        println!(
            "✓ Read the screen in {seconds:.1} s. I can see: {}",
            labels.join(", ")
        );
    }
    daemon.input(json!({ "type": "shortcut", "name": "home" }))?;
    println!("✓ Back on the Home Screen. An agent can now drive this iPhone the same way.");
    println!(
        "  Connect one: the iphone-use MCP server, or the HTTP API on {}",
        daemon.target.base()
    );
    Ok(())
}

/// The first `limit` distinct labels of on-screen controls, in tree order.
fn visible_labels(screen: &Value, limit: usize) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    let rows = screen["elements"].as_array().cloned().unwrap_or_default();
    for row in rows {
        if row["visible"] == Value::Bool(false) {
            continue;
        }
        if !matches!(
            row["kind"].as_str(),
            Some("Button" | "Cell" | "Switch" | "Link")
        ) {
            continue;
        }
        let Some(label) = row["label"].as_str().map(str::trim) else {
            continue;
        };
        // Skip empty labels and reverse-DNS identifiers that leaked into labels.
        if label.is_empty() || (label.contains('.') && !label.contains(' ')) {
            continue;
        }
        if !labels.iter().any(|l| l == label) {
            labels.push(label.to_string());
        }
        if labels.len() == limit {
            break;
        }
    }
    labels
}

// ---------------------------------------------------------------------------
// login: sign this Mac's browser in once, show a QR for the phone
// ---------------------------------------------------------------------------

pub fn run_login(target: &Target, open: bool) -> Result<i32> {
    let daemon = Daemon::new(target)?;
    let (code, link) = daemon.call(reqwest::Method::POST, "/agent/login-link", None)?;
    if code != 200 {
        bail!("the daemon could not make a sign-in link (HTTP {code})");
    }
    let url = link["url"]
        .as_str()
        .context("no sign-in link in the answer")?;
    let minutes = link["expires_in_secs"].as_u64().unwrap_or(300) / 60;
    if open && can_open_browser() {
        let opened = std::process::Command::new("/usr/bin/open")
            .arg(url)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if opened {
            println!("✓ Opened the control page in your browser (signed in with a one-time link).");
        } else {
            println!("Open this one-time sign-in link in a browser on this Mac ({minutes} min):\n  {url}");
        }
    } else {
        println!(
            "Open this one-time sign-in link in a browser on this Mac ({minutes} min):\n  {url}"
        );
    }
    if let Some(lan_url) = link["lan_url"].as_str() {
        if let Some(qr) = terminal_qr(lan_url) {
            println!("\nOn the iPhone, scan this with the Camera to control it from Safari or the Phone Use Remote app ({minutes} min, one use):\n");
            println!("{qr}");
        }
    }
    Ok(0)
}

/// `open` only makes sense in the logged-in desktop session, not over SSH.
fn can_open_browser() -> bool {
    std::env::var_os("SSH_CONNECTION").is_none()
        && std::env::var_os("SSH_TTY").is_none()
        && std::path::Path::new("/usr/bin/open").exists()
}

fn terminal_qr(text: &str) -> Option<String> {
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::L).ok()?;
    Some(
        code.render::<qrcode::render::unicode::Dense1x2>()
            .dark_color(qrcode::render::unicode::Dense1x2::Light)
            .light_color(qrcode::render::unicode::Dense1x2::Dark)
            .quiet_zone(true)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_lists_the_visible_controls_once_in_order() {
        let screen = json!({"elements": [
            {"kind": "NavigationBar", "label": "设置"},
            {"kind": "Button", "label": "通用"},
            {"kind": "StaticText", "label": "通用"},
            {"kind": "Button", "label": "通用"},
            {"kind": "Button", "label": "com.apple.settings.general"},
            {"kind": "Button", "label": ""},
            {"kind": "Button", "label": "蓝牙", "visible": false},
            {"kind": "Cell", "label": "无障碍"},
            {"kind": "Switch", "label": "飞行模式"},
        ]});
        assert_eq!(
            visible_labels(&screen, 8),
            vec!["通用", "无障碍", "飞行模式"]
        );
        assert_eq!(visible_labels(&screen, 1), vec!["通用"]);
    }

    #[test]
    fn next_step_prefers_the_language_it_has() {
        let status = json!({"next_step": {"en": "Unlock the iPhone"}, "hint": "raw"});
        assert_eq!(next_step(&status).as_deref(), Some("Unlock the iPhone"));
        assert_eq!(hint(&json!({"hint": "raw"})).as_deref(), Some("raw"));
        assert_eq!(hint(&json!({"hint": ""})), None);
    }

    #[test]
    fn the_phone_qr_renders_as_text() {
        let qr = terminal_qr("http://192.168.0.10:44321/pair?c=abc").unwrap();
        assert!(qr.lines().count() > 10);
    }
}
