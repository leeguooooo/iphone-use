//! `iphone-use remote …`: turn Tailscale / Cloudflare Tunnel access on and
//! off, and show how to connect from another network. The daemon does the
//! work (see `server::remote`); this writes its config and waits for it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde_json::Value;
use server::remote::{self, RemoteConfig};

use crate::onboarding::{terminal_qr, Target};

#[derive(Subcommand, Debug)]
pub enum RemoteAction {
    /// Show what is on and the addresses to connect from elsewhere.
    Status {
        /// Print the status as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Join your tailnet as its own device (no Tailscale app needed here).
    Tailscale {
        #[command(subcommand)]
        action: TailscaleAction,
    },
    /// Reach the phone through a Cloudflare Tunnel.
    Cloudflare {
        #[command(subcommand)]
        action: CloudflareAction,
    },
    /// Show a one-time QR code to connect a phone or browser elsewhere.
    Pair,
}

#[derive(Subcommand, Debug)]
pub enum TailscaleAction {
    /// Serve iphone-use on your tailnet. The first time, sign in with the
    /// link it prints (or pass a one-time auth key).
    On {
        /// The tailnet device name (default: iphone-use, iphone-use-<instance>).
        #[arg(long)]
        hostname: Option<String>,
        /// Let every member of the tailnet in, not only your own devices.
        #[arg(long)]
        allow_tailnet_members: bool,
        /// Enroll with the one-time auth key in FILE ('-' reads stdin)
        /// instead of the sign-in link. Never on the command line.
        #[arg(long, value_name = "FILE")]
        auth_key_file: Option<String>,
        /// Register with another control server, such as Headscale.
        #[arg(long, value_name = "URL")]
        control_url: Option<String>,
    },
    /// Stop serving on the tailnet; stays signed in for the next `on`.
    Off,
    /// Print a fresh sign-in link.
    Login,
    /// Sign this device out of the tailnet and stop serving.
    Logout,
    /// Rename the tailnet device.
    Rename { name: String },
}

#[derive(Subcommand, Debug)]
pub enum CloudflareAction {
    /// Open a tunnel. Without a token: a quick tunnel (random
    /// *.trycloudflare.com address, no account, changes on every restart,
    /// live video falls back to stills). With a token: your named tunnel.
    On {
        /// The named tunnel's token in FILE ('-' reads stdin), from
        /// Cloudflare Zero Trust › Networks › Tunnels.
        #[arg(long, value_name = "FILE")]
        token_file: Option<String>,
        /// The public hostname you routed to http://localhost:<port> for
        /// that tunnel, e.g. https://phone.example.com.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
    },
    /// Close the tunnel.
    Off,
}

pub fn run(target: &Target, action: Option<RemoteAction>) -> Result<i32> {
    let state_dir = target.instance.state_dir.clone();
    match action.unwrap_or(RemoteAction::Status { json: false }) {
        RemoteAction::Status { json } => {
            let st = remote::status(&state_dir, target.has_password(), target.port());
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                print_status(target, &st);
            }
            Ok(0)
        }
        RemoteAction::Tailscale { action } => tailscale(target, &state_dir, action),
        RemoteAction::Cloudflare { action } => cloudflare(target, &state_dir, action),
        RemoteAction::Pair => pair(target),
    }
}

fn require_password(target: &Target) -> Result<()> {
    if !target.has_password() {
        bail!(
            "this iphone-use has no password, so anyone who reaches it could control the phone; \
             re-run install.sh (it sets one) and try again"
        );
    }
    Ok(())
}

fn update(state_dir: &Path, change: impl FnOnce(&mut RemoteConfig)) -> Result<RemoteConfig> {
    let mut cfg = remote::load(state_dir);
    change(&mut cfg);
    remote::save(state_dir, &cfg).context("save the remote access settings")?;
    Ok(cfg)
}

/// Copy a secret from FILE or stdin into `dest` (0600).
fn store_secret(source: &str, dest: &Path) -> Result<()> {
    let text = if source == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        text
    } else {
        std::fs::read_to_string(source).with_context(|| format!("read {source}"))?
    };
    let text = text.trim();
    if text.is_empty() {
        bail!("{source} is empty");
    }
    remote::write_private(dest, format!("{text}\n").as_bytes())
        .with_context(|| format!("write {}", dest.display()))
}

fn tailscale(target: &Target, state_dir: &Path, action: TailscaleAction) -> Result<i32> {
    match action {
        TailscaleAction::On {
            hostname,
            allow_tailnet_members,
            auth_key_file,
            control_url,
        } => {
            require_password(target)?;
            if remote::tailnet_binary().is_none() {
                bail!(
                    "{} is missing next to iphone-use; upgrade iphone-use (`iphone-use upgrade`)",
                    remote::TAILNET_BIN
                );
            }
            if let Some(name) = hostname.as_deref() {
                if !remote::valid_hostname(name) {
                    bail!("{name:?} is not a device name: 1–63 letters, digits or hyphens");
                }
            }
            if let Some(source) = auth_key_file.as_deref() {
                store_secret(source, &remote::tailnet_auth_key_path(state_dir))?;
            }
            update(state_dir, |cfg| {
                cfg.tailscale.enabled = true;
                if let Some(name) = hostname {
                    cfg.tailscale.hostname = name;
                }
                cfg.tailscale.allow_tailnet_members = allow_tailnet_members;
                if control_url.is_some() {
                    cfg.tailscale.control_url = control_url;
                }
            })?;
            wait_tailscale(target, state_dir)
        }
        TailscaleAction::Off => {
            update(state_dir, |cfg| cfg.tailscale.enabled = false)?;
            println!("✓ Tailscale access is off (still signed in; `iphone-use remote tailscale on` reconnects).");
            Ok(0)
        }
        TailscaleAction::Login => {
            let cfg = remote::load(state_dir);
            if !cfg.tailscale.enabled {
                bail!("Tailscale access is off; turn it on with `iphone-use remote tailscale on`");
            }
            remote::request_tailnet_login(state_dir)?;
            wait_tailscale(target, state_dir)
        }
        TailscaleAction::Logout => {
            let cfg = update(state_dir, |cfg| cfg.tailscale.enabled = false)?;
            wait_helper_stopped(state_dir, "tailnet");
            let bin = remote::tailnet_binary()
                .with_context(|| format!("{} is missing", remote::TAILNET_BIN))?;
            let mut cmd = std::process::Command::new(bin);
            cmd.args(["logout", "--state-dir"])
                .arg(remote::dir(state_dir).join("tailscale"))
                .env("TS_NO_LOGS_NO_SUPPORT", "true");
            if let Some(url) = cfg.tailscale.control_url.as_deref() {
                cmd.args(["--control-url", url]);
            }
            let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
            let _ = std::fs::remove_file(remote::dir(state_dir).join("tailscale-status.json"));
            if !ok {
                bail!(
                    "could not sign out; remove the device in the Tailscale admin console instead"
                );
            }
            println!("✓ Signed out of Tailscale; the next `on` signs in again.");
            Ok(0)
        }
        TailscaleAction::Rename { name } => {
            if !remote::valid_hostname(&name) {
                bail!("{name:?} is not a device name: 1–63 letters, digits or hyphens");
            }
            let cfg = update(state_dir, |cfg| cfg.tailscale.hostname = name.clone())?;
            if !cfg.tailscale.enabled {
                println!("✓ Saved; the device will be called {name} when Tailscale access is on.");
                return Ok(0);
            }
            println!("Renaming to {name} (the address may take a minute to change)…");
            wait_tailscale(target, state_dir)
        }
    }
}

/// Wait for the tailnet helper to come up, showing the sign-in link if it
/// needs one, then print how to connect.
fn wait_tailscale(target: &Target, state_dir: &Path) -> Result<i32> {
    let started = Instant::now();
    let mut shown_login: Option<String> = None;
    let mut warned_daemon = false;
    loop {
        let st = remote::status(state_dir, target.has_password(), target.port());
        let ts = &st["tailscale"];
        match ts["state"].as_str().unwrap_or("") {
            "running" => {
                let _ = std::fs::remove_file(remote::tailnet_auth_key_path(state_dir));
                println!(
                    "✓ On your tailnet as {}",
                    ts["dns_name"].as_str().unwrap_or("?")
                );
                print_tailscale(target, ts);
                return Ok(0);
            }
            "needs_login" => {
                if let Some(url) = ts["login_url"].as_str() {
                    if shown_login.as_deref() != Some(url) {
                        println!(
                            "\nSign in to Tailscale to add this iPhone to your tailnet:\n  {url}"
                        );
                        if let Some(qr) = terminal_qr(url) {
                            println!("\n(or scan it with a phone)\n{qr}");
                        }
                        println!("Waiting for the sign-in…");
                        shown_login = Some(url.to_string());
                    }
                }
            }
            "needs_machine_auth" => {
                if shown_login.as_deref() != Some("machine") {
                    println!("Signed in; an admin must approve the device in the Tailscale admin console. Waiting…");
                    shown_login = Some("machine".into());
                }
            }
            "error" => {
                bail!(
                    "{}",
                    ts["hint"].as_str().or(ts["error"].as_str()).unwrap_or(
                        "Tailscale could not start; see ~/.iphone-use/remote/tailnet.log"
                    )
                );
            }
            _ => {
                if !warned_daemon && started.elapsed() > Duration::from_secs(15) {
                    warned_daemon = true;
                    if target.daemon_status().is_none() {
                        println!("The iphone-use daemon is not answering; it starts the Tailscale helper. Check `iphone-use status`.");
                    } else if ts["state"] == "starting" {
                        println!(
                            "Still starting; the daemon checks the settings every few seconds…"
                        );
                    }
                }
            }
        }
        let limit = if shown_login.is_some() { 600 } else { 90 };
        if started.elapsed() > Duration::from_secs(limit) {
            println!("Still not connected; it keeps trying in the background. Check `iphone-use remote status`.");
            return Ok(1);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn wait_helper_stopped(state_dir: &Path, name: &str) {
    let pid = remote::dir(state_dir).join(format!("{name}.pid"));
    let deadline = Instant::now() + Duration::from_secs(15);
    while pid.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn cloudflare(target: &Target, state_dir: &Path, action: CloudflareAction) -> Result<i32> {
    match action {
        CloudflareAction::On { token_file, url } => {
            require_password(target)?;
            if remote::cloudflared_binary().is_none() {
                bail!("cloudflared is not installed; install it with `brew install cloudflared` and try again");
            }
            let public_url = match url.as_deref() {
                Some(url) => Some(
                    remote::normalize_public_url(url)
                        .with_context(|| format!("{url:?} is not an https:// address"))?,
                ),
                None => None,
            };
            let token_path: Option<PathBuf> = match token_file.as_deref() {
                Some(source) => {
                    let dest = remote::cloudflare_token_path(state_dir);
                    store_secret(source, &dest)?;
                    Some(dest)
                }
                None => None,
            };
            if token_path.is_some() && public_url.is_none() {
                println!("Note: pass --url https://<your public hostname> so iphone-use can show it in QR codes.");
            }
            if token_path.is_none() && public_url.is_some() {
                bail!("--url belongs to a named tunnel; pass its --token-file too");
            }
            update(state_dir, |cfg| {
                cfg.cloudflare.enabled = true;
                cfg.cloudflare.token_file = token_path;
                cfg.cloudflare.public_url = public_url;
            })?;
            wait_cloudflare(target, state_dir)
        }
        CloudflareAction::Off => {
            update(state_dir, |cfg| cfg.cloudflare.enabled = false)?;
            println!("✓ Cloudflare Tunnel is off.");
            Ok(0)
        }
    }
}

fn wait_cloudflare(target: &Target, state_dir: &Path) -> Result<i32> {
    let started = Instant::now();
    let mut warned = false;
    loop {
        let st = remote::status(state_dir, target.has_password(), target.port());
        let cf = &st["cloudflare"];
        match cf["state"].as_str().unwrap_or("") {
            "running" if cf["mode"] == "named" || cf["url"].is_string() => {
                println!("✓ Cloudflare Tunnel is up");
                print_cloudflare(target, cf);
                return Ok(0);
            }
            "error" => {
                bail!(
                    "{}",
                    cf["hint"].as_str().or(cf["error"].as_str()).unwrap_or(
                        "cloudflared could not start; see ~/.iphone-use/remote/cloudflared.log"
                    )
                );
            }
            _ => {}
        }
        if let Some(error) = cf["error"].as_str() {
            if !warned && started.elapsed() > Duration::from_secs(10) {
                println!("cloudflared: {error}");
                warned = true;
            }
        }
        if !warned
            && started.elapsed() > Duration::from_secs(20)
            && target.daemon_status().is_none()
        {
            println!("The iphone-use daemon is not answering; it starts cloudflared. Check `iphone-use status`.");
            warned = true;
        }
        if started.elapsed() > Duration::from_secs(90) {
            println!("Still not connected; it keeps trying in the background. Check `iphone-use remote status`.");
            return Ok(1);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn print_status(target: &Target, st: &Value) {
    let ts = &st["tailscale"];
    let cf = &st["cloudflare"];
    match ts["state"].as_str().unwrap_or("off") {
        "off" => println!("Tailscale: off  (turn on: iphone-use remote tailscale on)"),
        "running" => {
            println!("Tailscale: on, {}", ts["dns_name"].as_str().unwrap_or("?"));
            print_tailscale(target, ts);
        }
        "needs_login" => println!(
            "Tailscale: waiting for sign-in: {}",
            ts["login_url"]
                .as_str()
                .unwrap_or("run `iphone-use remote tailscale login`")
        ),
        other => println!(
            "Tailscale: {other}{}",
            ts["hint"]
                .as_str()
                .or(ts["error"].as_str())
                .map(|h| format!(" ({h})"))
                .unwrap_or_default()
        ),
    }
    match cf["state"].as_str().unwrap_or("off") {
        "off" => println!("Cloudflare Tunnel: off  (turn on: iphone-use remote cloudflare on)"),
        "running" => {
            println!(
                "Cloudflare Tunnel: on ({})",
                cf["mode"].as_str().unwrap_or("?")
            );
            print_cloudflare(target, cf);
        }
        other => println!(
            "Cloudflare Tunnel: {other}{}",
            cf["hint"]
                .as_str()
                .or(cf["error"].as_str())
                .map(|h| format!(" ({h})"))
                .unwrap_or_default()
        ),
    }
}

fn print_tailscale(target: &Target, ts: &Value) {
    let url = ts["url"].as_str();
    let plain = ts["plain_url"].as_str();
    if let Some(url) = url {
        println!("  Browser or Phone Use Remote app, on any device signed in to your Tailscale:\n    {url}");
    }
    if let Some(plain) = plain {
        println!("  Without HTTPS (no H.264 video in the browser):\n    {plain}");
    }
    if ts["https"] == false {
        if let Some(hint) = ts["hint"].as_str() {
            println!("  Note: {hint}");
        }
    }
    if ts["allow_tailnet_members"] == true {
        println!("  Every member of the tailnet may connect (they still need the password).");
    } else if let Some(owner) = ts["owner"].as_str() {
        println!("  Only devices of {owner} may connect.");
    }
    if let Some(url) = url.or(plain) {
        print_agent_hint(target, url);
    }
}

fn print_cloudflare(target: &Target, cf: &Value) {
    if let Some(url) = cf["url"].as_str() {
        println!("  From any network:\n    {url}");
        if cf["streams_buffered"] == true {
            println!("  Quick tunnel: the address changes when the daemon restarts, and Cloudflare holds");
            println!("  streamed video there, so the live view shows stills. For smooth video use");
            println!("  Tailscale or a named tunnel (`--token-file … --url …`).");
        }
        print_agent_hint(target, url);
    }
}

fn print_agent_hint(target: &Target, url: &str) {
    let instance = match target.instance.name.as_str() {
        server::instance::DEFAULT_NAME => String::new(),
        name => format!(" --instance {name}"),
    };
    println!("  Agents on another computer: IPHONE_USE_URL={url} IPHONE_USE_TOKEN=<this daemon's password>");
    println!("  Pair a phone elsewhere: iphone-use remote pair{instance}");
}

fn pair(target: &Target) -> Result<i32> {
    let link = target.login_link()?;
    let Some(url) = link["remote_url"].as_str() else {
        bail!("no remote access is on; turn on `iphone-use remote tailscale on` or `iphone-use remote cloudflare on` first");
    };
    let minutes = link["expires_in_secs"].as_u64().unwrap_or(300) / 60;
    println!("Open this one-time link on the phone or computer you want to control from ({minutes} min, one use):\n  {url}\n");
    if let Some(qr) = terminal_qr(url) {
        println!("{qr}");
    }
    Ok(0)
}
