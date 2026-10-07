//! What one setup run resolves before it does anything: the instance, its
//! files and launchd labels, the ports, the target iPhone, and the signing
//! policy. Same precedence as `setup-wda.sh` always had: explicit environment
//! > the runner supervisor's persisted plist > the daemon's plist > default.

use std::path::{Path, PathBuf};

use crate::instance::Instance;

use super::sys;

pub const LABEL_PREFIX: &str = "com.leeguoo.iphone-use";
/// Names whose label is another product LaunchAgent's (see instance.rs).
pub const RESERVED_NAMES: &[&str] = &["wda", "autoupdate", "daily-maintenance", "flow-reverify"];
pub const PORT_SLOTS: u32 = 500;
pub const DAEMON_PORT_BASE: u32 = 45500;
pub const WDA_PORT_BASE: u32 = 8200;
pub const MJPEG_PORT_BASE: u32 = 9200;

pub const RUNNER_SCHEME: &str = "IPhoneUseRunner";
pub const RUNNER_TEST_ID: &str = "IPhoneUseRunnerUITests/RunnerTests/testServe";
pub const RUNNER_APP_NAME: &str = "iPhoneUse-Runner.app";
pub const XCODE_APP_STORE_URL: &str = "https://apps.apple.com/app/xcode/id497799835";
pub const SUPERVISOR_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/sbin:/sbin:/usr/bin:/bin";

#[derive(Debug, Clone)]
pub struct Ctx {
    pub instance: Instance,
    pub home: PathBuf,
    pub uid: u32,
    pub gui_domain: String,
    pub daemon_plist: PathBuf,
    pub wda_agent_plist: PathBuf,
    /// The installed copy of the setup script (`<state>/setup-wda.sh`): what the
    /// supervisor plist runs and what messages tell people to run.
    pub self_install: PathBuf,
    /// The script that started this run, when a script did (the shim passes
    /// its own path); decides whether a repo checkout's `runner/` is used.
    pub script: Option<PathBuf>,
    pub run_log: PathBuf,
    pub runner_pid_file: PathBuf,
    pub relay_pid_file: PathBuf,
    pub mjpeg_relay_pid_file: PathBuf,
    pub wda_agent_log: PathBuf,
    pub retry_state: PathBuf,
    pub status_file: PathBuf,
    pub xcconfig_file: PathBuf,
    pub runner_cache: PathBuf,
    pub runner_derived_data: PathBuf,
    pub runner_products_dir: PathBuf,
    pub runner_default_src: PathBuf,
    pub runner_src: PathBuf,
    pub runner_project: PathBuf,
    /// The WebDriverAgent checkout older releases built from; only matched
    /// against a PID-only record's cwd now.
    pub wda_dir: PathBuf,
    /// Raw, as configured; validated where they are used.
    pub wda_port: String,
    pub mjpeg_port: String,
    pub bundle_id: String,
    pub team_id: String,
    pub asc_key_path: String,
    pub asc_key_id: String,
    pub asc_issuer_id: String,
    /// `0` or `1`.
    pub allow_lan: String,
    pub udid: String,
    /// `WDA_KEEPALIVE=1`: the launchd supervisor's run.
    pub keepalive: bool,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

/// `${KEY+x}`: set at all, even to the empty string.
fn env_is_set(key: &str) -> bool {
    std::env::var_os(key).is_some()
}

impl Ctx {
    /// Resolve from the environment. `Err` carries the message and exit code
    /// `setup-wda.sh` used (2 for an unusable instance, 1 otherwise).
    pub fn resolve() -> Result<Ctx, (String, i32)> {
        let instance = Instance::from_env().map_err(|message| (message, 2))?;
        let home = instance.home.clone();
        let state = instance.state_dir.clone();
        let daemon_plist = home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", instance.daemon_label));
        let wda_agent_plist = instance.wda_plist();
        let runner_derived_data = state.join("runner-build");
        let runner_default_src = home.join(".iphone-use/runner");
        let uid = sys::uid();
        let script = env("IPHONE_USE_SETUP_SCRIPT").map(PathBuf::from);
        let mut ctx = Ctx {
            gui_domain: format!("gui/{uid}"),
            self_install: state.join("setup-wda.sh"),
            run_log: state.join("wda-runner.log"),
            runner_pid_file: state.join("wda-runner.pid"),
            relay_pid_file: state.join("wda-relay.pid"),
            mjpeg_relay_pid_file: state.join("wda-mjpeg-relay.pid"),
            wda_agent_log: state.join("wda-agent.log"),
            retry_state: state.join("wda-retry-state.v1"),
            status_file: state.join("wda-setup-status.json"),
            xcconfig_file: state.join("wda-xcode-compat.xcconfig"),
            runner_cache: state.join("wda-runner-product.json"),
            runner_products_dir: runner_derived_data.join("Build/Products/Debug-iphoneos"),
            runner_derived_data,
            runner_default_src: runner_default_src.clone(),
            runner_src: PathBuf::new(),
            runner_project: PathBuf::new(),
            wda_dir: PathBuf::new(),
            wda_port: String::new(),
            mjpeg_port: String::new(),
            bundle_id: String::new(),
            team_id: String::new(),
            asc_key_path: String::new(),
            asc_key_id: String::new(),
            asc_issuer_id: String::new(),
            allow_lan: String::new(),
            udid: String::new(),
            keepalive: std::env::var("WDA_KEEPALIVE").is_ok_and(|v| v == "1"),
            instance,
            home,
            uid,
            daemon_plist,
            wda_agent_plist,
            script,
        };
        let wda_env = |key: &str| sys::plist_env(&ctx.wda_agent_plist, key);
        let daemon_env = |key: &str| sys::plist_env(&ctx.daemon_plist, key);

        let wda_dir = env("WDA_DIR").unwrap_or_else(|| wda_env("WDA_DIR"));
        let wda_dir = if wda_dir.is_empty() {
            state.join("WebDriverAgent")
        } else {
            PathBuf::from(wda_dir)
        };

        let mut runner_src = env("IPU_RUNNER_SRC").unwrap_or_else(|| wda_env("IPU_RUNNER_SRC"));
        if runner_src.is_empty() {
            runner_src = repo_runner_src(ctx.script.as_deref(), &ctx.home)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        if runner_src.is_empty() {
            runner_src = runner_default_src.to_string_lossy().into_owned();
        }
        let runner_src = PathBuf::from(runner_src.trim_end_matches('/'));
        let runner_project = runner_src.join("IPhoneUseRunner/IPhoneUseRunner.xcodeproj");

        let port_from_url = |key: &str| port_from_loopback_url(&daemon_env(key));
        let mut wda_port = env("WDA_PORT").unwrap_or_else(|| wda_env("WDA_PORT"));
        if wda_port.is_empty() {
            wda_port = port_from_url("PHONE_REMOTE_WDA_URL");
        }
        let mut mjpeg_port = env("MJPEG_PORT").unwrap_or_else(|| wda_env("MJPEG_PORT"));
        if mjpeg_port.is_empty() {
            mjpeg_port = port_from_url("PHONE_REMOTE_WDA_MJPEG_URL");
        }
        if ctx.instance.is_default() {
            if wda_port.is_empty() {
                wda_port = "8100".into();
            }
            if mjpeg_port.is_empty() {
                mjpeg_port = "9100".into();
            }
        } else if wda_port.is_empty() || mjpeg_port.is_empty() {
            if wda_port.is_empty() {
                wda_port = daemon_env("WDA_PORT");
            }
            if mjpeg_port.is_empty() {
                mjpeg_port = daemon_env("MJPEG_PORT");
            }
            if wda_port.is_empty() || mjpeg_port.is_empty() {
                let Some(slot) = derive_ports(&ctx, true) else {
                    return Err((
                        format!(
                            "no free port slot for instance \"{}\"; set WDA_PORT and MJPEG_PORT",
                            ctx.instance.name
                        ),
                        1,
                    ));
                };
                if wda_port.is_empty() {
                    wda_port = slot.1.to_string();
                }
                if mjpeg_port.is_empty() {
                    mjpeg_port = slot.2.to_string();
                }
            }
        }

        let mut bundle_id = env("WDA_BUNDLE_ID").unwrap_or_else(|| wda_env("WDA_BUNDLE_ID"));
        let mut team_id = env("WDA_TEAM_ID").unwrap_or_else(|| wda_env("WDA_TEAM_ID"));
        let (mut asc_path, mut asc_id, mut asc_issuer) = (
            std::env::var("WDA_ASC_KEY_PATH").unwrap_or_default(),
            std::env::var("WDA_ASC_KEY_ID").unwrap_or_default(),
            std::env::var("WDA_ASC_ISSUER_ID").unwrap_or_default(),
        );
        // The saved trio comes back only when no ASC override was supplied: a
        // partial explicit override must not combine with a different saved key.
        if !env_is_set("WDA_ASC_KEY_PATH")
            && !env_is_set("WDA_ASC_KEY_ID")
            && !env_is_set("WDA_ASC_ISSUER_ID")
        {
            asc_path = wda_env("WDA_ASC_KEY_PATH");
            asc_id = wda_env("WDA_ASC_KEY_ID");
            asc_issuer = wda_env("WDA_ASC_ISSUER_ID");
        }
        let mut allow_lan = env("WDA_ALLOW_LAN").unwrap_or_default();
        // A named instance's first setup has no supervisor yet: its signing
        // policy is what `install.sh --instance` persisted in the daemon plist.
        if !ctx.instance.is_default() {
            if bundle_id.is_empty() {
                bundle_id = daemon_env("WDA_BUNDLE_ID");
            }
            if team_id.is_empty() {
                team_id = daemon_env("WDA_TEAM_ID");
            }
            if allow_lan.is_empty() {
                allow_lan = daemon_env("WDA_ALLOW_LAN");
            }
            if asc_path.is_empty() && asc_id.is_empty() && asc_issuer.is_empty() {
                asc_path = daemon_env("WDA_ASC_KEY_PATH");
                asc_id = daemon_env("WDA_ASC_KEY_ID");
                asc_issuer = daemon_env("WDA_ASC_ISSUER_ID");
            }
        }
        if allow_lan.is_empty() {
            allow_lan = wda_env("WDA_ALLOW_LAN");
        }
        if allow_lan.is_empty() {
            allow_lan = "0".into();
        }
        if allow_lan != "0" && allow_lan != "1" {
            return Err(("WDA_ALLOW_LAN must be 0 or 1".into(), 1));
        }
        let mut udid = env("WDA_UDID")
            .or_else(|| env("PHONE_REMOTE_UDID"))
            .unwrap_or_default();
        if udid.is_empty() {
            udid = daemon_env("PHONE_REMOTE_UDID");
        }
        if udid.is_empty() {
            udid = wda_env("WDA_UDID");
        }

        ctx.wda_dir = wda_dir;
        ctx.runner_src = runner_src;
        ctx.runner_project = runner_project;
        ctx.wda_port = wda_port;
        ctx.mjpeg_port = mjpeg_port;
        ctx.bundle_id = bundle_id;
        ctx.team_id = team_id;
        ctx.asc_key_path = asc_path;
        ctx.asc_key_id = asc_id;
        ctx.asc_issuer_id = asc_issuer;
        ctx.allow_lan = allow_lan;
        ctx.udid = udid;
        Ok(ctx)
    }

    pub fn state_dir(&self) -> &Path {
        &self.instance.state_dir
    }

    pub fn lan(&self) -> bool {
        self.allow_lan == "1"
    }

    pub fn asc_signing_enabled(&self) -> bool {
        !self.asc_key_path.is_empty()
            && !self.asc_key_id.is_empty()
            && !self.asc_issuer_id.is_empty()
    }

    /// `iphone-use setup`, with `--instance` for a named one.
    pub fn rerun_command(&self) -> String {
        if self.instance.is_default() {
            "iphone-use setup".into()
        } else {
            format!("iphone-use setup --instance {}", self.instance.name)
        }
    }

    pub fn wda_port_number(&self) -> Option<u16> {
        valid_port(&self.wda_port)
    }

    pub fn mjpeg_port_number(&self) -> Option<u16> {
        valid_port(&self.mjpeg_port)
    }
}

/// A decimal TCP port from 1 to 65535.
pub fn valid_port(text: &str) -> Option<u16> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>()
        .ok()
        .filter(|port| (1..=65535).contains(port))
        .map(|port| port as u16)
}

/// `http://127.0.0.1:<port>…` → `<port>`; anything else → empty.
pub fn port_from_loopback_url(url: &str) -> String {
    url.strip_prefix("http://127.0.0.1:")
        .map(|rest| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// `<repo>/runner` when the script that started this run sits in a checkout
/// (`<repo>/scripts/setup-wda.sh` next to `crates/server` and the runner
/// project). The installed copy under ~/.iphone-use never looks next to itself.
fn repo_runner_src(script: Option<&Path>, home: &Path) -> Option<PathBuf> {
    let script = script?;
    let dir = script.parent()?.canonicalize().ok()?;
    let installed = home.join(".iphone-use");
    if dir == installed || dir.starts_with(&installed) {
        return None;
    }
    let repo = dir.parent()?.to_path_buf();
    (repo
        .join("runner/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/project.pbxproj")
        .is_file()
        && repo.join("crates/server").is_dir())
    .then(|| repo.join("runner"))
}

// ── instance context helpers (#67) ─────────────────────────────────────────

/// Every other instance's LaunchAgent plists, as (instance, plist). Only
/// plists that carry a phone or a port count; the maintenance agents share
/// the label prefix but bind neither.
pub fn other_instance_plists(ctx: &Ctx) -> Vec<(String, PathBuf)> {
    let dir = ctx.home.join("Library/LaunchAgents");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut plists: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            path.is_file()
                && (name == format!("{LABEL_PREFIX}.plist")
                    || (name.starts_with(&format!("{LABEL_PREFIX}.")) && name.ends_with(".plist")))
        })
        .collect();
    plists.sort();
    let mut out = Vec::new();
    for plist in plists {
        let label = sys::plist_top(&plist, "Label");
        let name = if label == LABEL_PREFIX || label == format!("{LABEL_PREFIX}.wda") {
            "default".to_string()
        } else if let Some(name) = label.strip_prefix(&format!("{LABEL_PREFIX}.wda.")) {
            name.to_string()
        } else if let Some(name) = label.strip_prefix(&format!("{LABEL_PREFIX}.")) {
            name.to_string()
        } else {
            continue;
        };
        if name == ctx.instance.name || RESERVED_NAMES.contains(&name.as_str()) {
            continue;
        }
        out.push((name, plist));
    }
    out
}

/// Ports other instances own: what their plists persist, plus the default
/// instance's built-in defaults, which an older plist may leave implicit.
pub fn claimed_ports(ctx: &Ctx) -> Vec<u32> {
    let mut ports = Vec::new();
    if !ctx.instance.is_default() {
        ports.extend([44321, 8100, 9100]);
    }
    for (_, plist) in other_instance_plists(ctx) {
        for key in [
            "PHONE_REMOTE_PORT",
            "WDA_PORT",
            "MJPEG_PORT",
            "PHONE_REMOTE_WDA_URL",
            "PHONE_REMOTE_WDA_MJPEG_URL",
        ] {
            let value = sys::plist_env(&plist, key);
            let port = if value.bytes().all(|b| b.is_ascii_digit()) {
                value
            } else {
                port_from_loopback_url(&value)
            };
            if let Ok(port) = port.parse::<u32>() {
                ports.push(port);
            }
        }
    }
    ports
}

/// The instance a UDID is already bound to, if it is not this one.
pub fn udid_owner(ctx: &Ctx, udid: &str) -> Option<String> {
    let wanted = udid.to_ascii_uppercase();
    if wanted.is_empty() {
        return None;
    }
    for (name, plist) in other_instance_plists(ctx) {
        for key in ["PHONE_REMOTE_UDID", "WDA_UDID"] {
            let value = sys::plist_env(&plist, key).to_ascii_uppercase();
            if !value.is_empty() && value == wanted {
                return Some(name);
            }
        }
    }
    None
}

fn port_listening(port: u32) -> bool {
    let port = port.to_string();
    sys::run(
        "lsof",
        &["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"],
    )
    .is_some_and(|out| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// POSIX `cksum` CRC of `data` (what the shell derivation uses, so every
/// shell and this agree on a name's first-choice port slot).
pub fn posix_cksum(data: &[u8]) -> u32 {
    fn update(crc: u32, byte: u8) -> u32 {
        let mut c = crc ^ (u32::from(byte) << 24);
        for _ in 0..8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04C1_1DB7
            } else {
                c << 1
            };
        }
        c
    }
    let mut crc = data.iter().fold(0u32, |crc, &byte| update(crc, byte));
    let mut length = data.len();
    while length > 0 {
        crc = update(crc, (length & 0xff) as u8);
        length >>= 8;
    }
    !crc
}

pub fn port_slot(name: &str) -> u32 {
    posix_cksum(name.as_bytes()) % PORT_SLOTS
}

/// (daemon, control, video) ports of the first slot from the name's own whose
/// three ports are unclaimed and free. `probe=false` returns the first-choice
/// slot unchecked.
pub fn derive_ports(ctx: &Ctx, probe: bool) -> Option<(u32, u32, u32)> {
    let slot = port_slot(&ctx.instance.name);
    let claimed = if probe {
        claimed_ports(ctx)
    } else {
        Vec::new()
    };
    for attempt in 0..20 {
        let offset = (slot + attempt) % PORT_SLOTS;
        let ports = (
            DAEMON_PORT_BASE + offset,
            WDA_PORT_BASE + offset,
            MJPEG_PORT_BASE + offset,
        );
        if !probe {
            return Some(ports);
        }
        let taken = [ports.0, ports.1, ports.2]
            .iter()
            .any(|p| claimed.contains(p));
        if !taken
            && !port_listening(ports.0)
            && !port_listening(ports.1)
            && !port_listening(ports.2)
        {
            return Some(ports);
        }
    }
    None
}

/// Refuse ports another instance owns and a phone another instance drives.
pub fn check_bindings(ctx: &Ctx, ports: &[&str]) -> Result<(), String> {
    let claimed = claimed_ports(ctx);
    for port in ports.iter().filter(|p| !p.is_empty()) {
        if port.parse::<u32>().is_ok_and(|p| claimed.contains(&p)) {
            return Err(format!(
                "TCP {port} is already assigned to another iphone-use instance; pick a different port for instance \"{}\"",
                ctx.instance.name
            ));
        }
    }
    if !ctx.udid.is_empty() {
        if let Some(owner) = udid_owner(ctx, &ctx.udid) {
            return Err(format!(
                "iPhone {} is already driven by iphone-use instance \"{owner}\"; one phone cannot be bound to two daemons",
                ctx.udid
            ));
        }
    }
    Ok(())
}

/// [`check_bindings`] reads every instance plist on each reconnect, yet its
/// answer depends only on those files: remember a passing verdict keyed on
/// the instance, the ports, the target and each plist's path, mtime and size.
/// A refusal is never cached.
pub fn check_bindings_cached(ctx: &Ctx, ports: &[&str]) -> Result<(), String> {
    let cache = ctx.state_dir().join(".instance-bindings.ok");
    let stamp = bindings_stamp(ctx, ports);
    let is_link = std::fs::symlink_metadata(&cache).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link && std::fs::read_to_string(&cache).is_ok_and(|text| text == stamp) {
        return Ok(());
    }
    if let Err(error) = check_bindings(ctx, ports) {
        let _ = std::fs::remove_file(&cache);
        return Err(error);
    }
    if !is_link {
        let _ = std::fs::write(&cache, &stamp);
    }
    Ok(())
}

fn bindings_stamp(ctx: &Ctx, ports: &[&str]) -> String {
    use std::os::unix::fs::MetadataExt as _;
    let mut stamp = format!("{}|{}|{}|", ctx.instance.name, ports.join(" "), ctx.udid);
    let dir = ctx.home.join("Library/LaunchAgents");
    let mut plists: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    plists.retain(|path| {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        path.is_file()
            && (name == format!("{LABEL_PREFIX}.plist")
                || (name.starts_with(&format!("{LABEL_PREFIX}.")) && name.ends_with(".plist")))
    });
    plists.sort();
    for plist in plists {
        match std::fs::metadata(&plist) {
            Ok(meta) => stamp.push_str(&format!(
                "{}:{}:{};",
                plist.display(),
                meta.mtime(),
                meta.size()
            )),
            Err(_) => stamp.push_str(&format!("{}:?;", plist.display())),
        }
    }
    stamp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cksum_matches_the_posix_tool() {
        for name in ["lab", "i13", "a", "phone-two", "x".repeat(32).as_str(), ""] {
            let out = std::process::Command::new("cksum")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write as _;
                    child.stdin.take().unwrap().write_all(name.as_bytes())?;
                    child.wait_with_output()
                })
                .expect("cksum runs");
            let expected: u32 = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(posix_cksum(name.as_bytes()), expected, "{name:?}");
        }
    }

    #[test]
    fn ports_and_loopback_urls() {
        assert_eq!(valid_port("8100"), Some(8100));
        assert_eq!(valid_port("0"), None);
        assert_eq!(valid_port("65536"), None);
        assert_eq!(valid_port("81a"), None);
        assert_eq!(valid_port(""), None);
        assert_eq!(port_from_loopback_url("http://127.0.0.1:8538"), "8538");
        assert_eq!(port_from_loopback_url("http://127.0.0.1:9538/x"), "9538");
        assert_eq!(port_from_loopback_url("http://192.168.0.2:8100"), "");
    }

    #[test]
    fn repo_runner_src_needs_a_checkout_outside_the_install() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("scripts")).unwrap();
        std::fs::create_dir_all(repo.path().join("crates/server")).unwrap();
        let project = repo
            .path()
            .join("runner/IPhoneUseRunner/IPhoneUseRunner.xcodeproj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("project.pbxproj"), "").unwrap();
        let script = repo.path().join("scripts/setup-wda.sh");
        std::fs::write(&script, "").unwrap();
        let found = repo_runner_src(Some(&script), home.path()).unwrap();
        assert_eq!(found, repo.path().canonicalize().unwrap().join("runner"));
        let installed = home.path().join(".iphone-use");
        std::fs::create_dir_all(&installed).unwrap();
        assert_eq!(
            repo_runner_src(Some(&installed.join("setup-wda.sh")), home.path()),
            None
        );
        assert_eq!(repo_runner_src(None, home.path()), None);
    }
}
