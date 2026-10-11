//! iOS Simulator support (preview): the same device runner, driven on a
//! simulator instead of a phone.
//!
//! A simulator shares the Mac's loopback, so the runner's HTTP port is
//! `127.0.0.1:<port>` with no usbmux relay, no tunnel and no signing. What
//! changes is everything the daemon does *around* the runner: device logs,
//! crash reports, simulated location, app install/uninstall and the
//! installed-app inventory go through `xcrun simctl` instead of lockdown and
//! devicectl. A daemon started with `IPHONE_USE_SIMULATOR=<udid>` routes those
//! through [`Simulator`]; without it nothing here runs.
//!
//! `iphone-use simulators` and `iphone-use simulator start|stop` (see the
//! `cli_*` functions) find and boot a simulator, build the runner for it
//! (`runner/build.sh --simulator`), launch it with `xcodebuild
//! test-without-building -destination id=<udid>` and print the daemon command
//! to run against it.
//!
//! Everything that parses simctl output or builds an argv is a pure function
//! with fixtures in the tests; the commands themselves go through a
//! [`SimctlRunner`], which tests replace.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::crash::{CrashEntry, CrashFilter, CrashReport};
use crate::syslog::Parsed;

/// The daemon's simulator switch: the UDID of the simulator it drives.
pub const ENV: &str = "IPHONE_USE_SIMULATOR";
/// How long a quick simctl call (list, location, openurl) may take.
const QUICK_TIMEOUT: Duration = Duration::from_secs(30);
/// Listing apps reads every bundle's Info.plist.
const LISTAPPS_TIMEOUT: Duration = Duration::from_secs(60);
/// Installing copies the bundle into the simulator.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
/// A cold boot of a simulator runtime.
const BOOT_TIMEOUT: Duration = Duration::from_secs(240);
/// How much of a crash report is read to tell which simulator it is from.
const CRASH_SNIFF: usize = 256 * 1024;

// ── simctl device list ──────────────────────────────────────────────────────

/// One simulator from `simctl list devices available -j`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SimDevice {
    pub udid: String,
    pub name: String,
    /// `iOS 18.2`.
    pub runtime: String,
    /// `com.apple.CoreSimulator.SimRuntime.iOS-18-2`.
    pub runtime_id: String,
    /// `iOS`, `watchOS`, `tvOS`, `xrOS`.
    pub platform: String,
    /// `18.2`.
    pub os: String,
    /// `Booted`, `Shutdown`, `Booting`, `Shutting Down`.
    pub state: String,
    /// `iPhone 16 Pro`, from the device type identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl SimDevice {
    pub fn booted(&self) -> bool {
        self.state.eq_ignore_ascii_case("booted")
    }

    /// `device` in `/agent/status` for a simulator.
    pub fn to_status_json(&self) -> Value {
        json!({
            "name": self.name,
            "model": self.model,
            "runtime": self.runtime,
            "ios": self.os,
            "udid": self.udid,
            "state": self.state,
            "simulator": true,
        })
    }
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-18-2` → (`iOS`, `18.2`).
pub fn parse_runtime_key(key: &str) -> Option<(String, String)> {
    let tail = key.rsplit('.').next()?;
    let (platform, version) = tail.split_once('-')?;
    if platform.is_empty()
        || !platform.chars().all(|c| c.is_ascii_alphanumeric())
        || version.is_empty()
        || !version.split('-').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some((platform.to_string(), version.replace('-', ".")))
}

/// `com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro` → `iPhone 16 Pro`.
fn model_from_device_type(identifier: &str) -> Option<String> {
    let tail = identifier.rsplit('.').next()?;
    (!tail.is_empty() && identifier.contains("SimDeviceType")).then(|| tail.replace('-', " "))
}

/// Compare `18.2` and `17.5` numerically.
fn version_key(version: &str) -> Vec<u32> {
    version.split('.').map(|part| part.parse().unwrap_or(0)).collect()
}

/// Every available simulator in `simctl list devices available -j` output,
/// newest runtime first, then by name.
pub fn parse_devices(json_text: &str) -> Result<Vec<SimDevice>> {
    let value: Value = serde_json::from_str(json_text).context("simctl device list JSON")?;
    let runtimes = value
        .get("devices")
        .and_then(Value::as_object)
        .context("simctl device list has no \"devices\" object")?;
    let mut out = Vec::new();
    for (key, devices) in runtimes {
        let Some((platform, os)) = parse_runtime_key(key) else {
            continue;
        };
        for device in devices.as_array().into_iter().flatten() {
            if device.get("isAvailable").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let text = |field: &str| {
                device
                    .get(field)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
            };
            let (Some(udid), Some(name)) = (text("udid"), text("name")) else {
                continue;
            };
            out.push(SimDevice {
                udid: udid.to_string(),
                name: name.to_string(),
                runtime: format!("{platform} {os}"),
                runtime_id: key.clone(),
                platform: platform.clone(),
                os: os.clone(),
                state: text("state").unwrap_or("Unknown").to_string(),
                model: text("deviceTypeIdentifier").and_then(model_from_device_type),
            });
        }
    }
    out.sort_by(|a, b| {
        a.platform
            .cmp(&b.platform)
            .then_with(|| version_key(&b.os).cmp(&version_key(&a.os)))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.udid.cmp(&b.udid))
    });
    Ok(out)
}

/// The iOS simulator `query` names: its UDID (any case), or its name (any
/// case) when exactly one has it, or exactly one of those is booted.
pub fn find<'a>(devices: &'a [SimDevice], query: &str) -> Result<&'a SimDevice, String> {
    let query = query.trim();
    let ios = || devices.iter().filter(|d| d.platform == "iOS");
    if let Some(device) = ios().find(|d| d.udid.eq_ignore_ascii_case(query)) {
        return Ok(device);
    }
    let named: Vec<&SimDevice> = ios().filter(|d| d.name.eq_ignore_ascii_case(query)).collect();
    match named.as_slice() {
        [] => Err(format!(
            "no available iOS simulator is named or has the UDID {query:?}; `iphone-use simulators` lists them (create one in Xcode > Window > Devices and Simulators)"
        )),
        [one] => Ok(one),
        several => {
            let booted: Vec<&&SimDevice> = several.iter().filter(|d| d.booted()).collect();
            if let [one] = booted.as_slice() {
                return Ok(one);
            }
            let list = several
                .iter()
                .map(|d| format!("{} ({}, {})", d.udid, d.runtime, d.state))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "several simulators are named {query:?}: {list}; pass the UDID instead"
            ))
        }
    }
}

/// A simulator UDID: a UUID (hex and dashes, 36 characters).
pub fn valid_udid(udid: &str) -> bool {
    udid.len() == 36
        && udid.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

// ── argv builders (everything after `xcrun`) ────────────────────────────────

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| part.to_string()).collect()
}

pub fn list_argv() -> Vec<String> {
    argv(&["simctl", "list", "devices", "available", "-j"])
}

pub fn boot_argv(udid: &str) -> Vec<String> {
    argv(&["simctl", "boot", udid])
}

/// `defaults write com.apple.iphonesimulator ConnectHardwareKeyboard -bool
/// false`: with Simulator's hardware keyboard connected, iOS hides the
/// software keyboard that text input waits for. Read when Simulator.app
/// starts, so it is set before booting.
pub fn software_keyboard_argv() -> Vec<String> {
    argv(&[
        "write",
        "com.apple.iphonesimulator",
        "ConnectHardwareKeyboard",
        "-bool",
        "false",
    ])
}

/// Waits until the simulator has finished booting (`-b` boots it first if
/// needed).
pub fn bootstatus_argv(udid: &str) -> Vec<String> {
    argv(&["simctl", "bootstatus", udid, "-b"])
}

pub fn shutdown_argv(udid: &str) -> Vec<String> {
    argv(&["simctl", "shutdown", udid])
}

pub fn listapps_argv(udid: &str) -> Vec<String> {
    argv(&["simctl", "listapps", udid])
}

pub fn install_argv(udid: &str, app: &Path) -> Vec<String> {
    vec![
        "simctl".into(),
        "install".into(),
        udid.into(),
        app.to_string_lossy().into_owned(),
    ]
}

pub fn uninstall_argv(udid: &str, bundle: &str) -> Vec<String> {
    argv(&["simctl", "uninstall", udid, bundle])
}

pub fn openurl_argv(udid: &str, url: &str) -> Vec<String> {
    argv(&["simctl", "openurl", udid, url])
}

/// `simctl location <udid> set <lat>,<lon>`, refusing out-of-range values.
pub fn location_set_argv(udid: &str, lat: f64, lon: f64) -> Result<Vec<String>> {
    crate::location::validate(lat, lon)?;
    Ok(vec![
        "simctl".into(),
        "location".into(),
        udid.into(),
        "set".into(),
        format!("{lat},{lon}"),
    ])
}

pub fn location_clear_argv(udid: &str) -> Vec<String> {
    argv(&["simctl", "location", udid, "clear"])
}

/// `simctl spawn <udid> log stream --style compact [--predicate <p>]`.
pub fn log_stream_argv(udid: &str, predicate: Option<&str>) -> Vec<String> {
    let mut out = argv(&["simctl", "spawn", udid, "log", "stream", "--style", "compact"]);
    if let Some(predicate) = predicate.map(str::trim).filter(|p| !p.is_empty()) {
        out.push("--predicate".into());
        out.push(predicate.to_string());
    }
    out
}

// ── simctl listapps ─────────────────────────────────────────────────────────

/// One app from `simctl listapps`.
#[derive(Debug, Clone, PartialEq)]
pub struct SimApp {
    pub bundle: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub bundle_version: Option<String>,
    /// `ApplicationType = System`.
    pub system: bool,
}

/// `simctl listapps <udid>` output: an old-style (OpenStep) property list
/// keyed by bundle id, or XML on some Xcode versions.
pub fn parse_listapps(text: &str) -> Result<Vec<SimApp>> {
    let value = plist::Value::from_reader(std::io::Cursor::new(text.as_bytes()))
        .context("simctl listapps output is not a property list")?;
    let apps = value
        .as_dictionary()
        .context("simctl listapps output is not a dictionary")?;
    let mut out: Vec<SimApp> = apps
        .iter()
        .filter_map(|(key, app)| {
            let app = app.as_dictionary()?;
            // OpenStep plists leave a bare `417` unquoted, which reads as a number.
            let text = |field: &str| {
                let value = app.get(field)?;
                let text = match value {
                    plist::Value::String(text) => text.trim().to_string(),
                    plist::Value::Integer(number) => number.to_string(),
                    plist::Value::Real(number) => number.to_string(),
                    _ => return None,
                };
                (!text.is_empty()).then_some(text)
            };
            let bundle = text("CFBundleIdentifier").unwrap_or_else(|| key.clone());
            if bundle.is_empty() {
                return None;
            }
            Some(SimApp {
                bundle,
                name: text("CFBundleDisplayName").or_else(|| text("CFBundleName")),
                version: text("CFBundleShortVersionString"),
                bundle_version: text("CFBundleVersion"),
                system: text("ApplicationType").as_deref() == Some("System"),
            })
        })
        .collect();
    out.sort_by(|a, b| a.bundle.cmp(&b.bundle));
    Ok(out)
}

/// The `/agent/apps` inventory body (the same contract `crate::apps`
/// builds from devicectl) for a simulator.
pub fn inventory_body(
    udid: &str,
    device: Option<&SimDevice>,
    apps: &[SimApp],
    fetched_at: &str,
) -> Result<Value, String> {
    if apps.is_empty() {
        return Err("simctl listapps returned no apps".to_string());
    }
    let rows: Vec<Value> = apps
        .iter()
        .map(|app| {
            json!({
                "bundle": app.bundle,
                "name": app.name,
                "version": app.version,
                "bundle_version": app.bundle_version,
                "system": app.system,
                "removable": !app.system,
                "hidden": Value::Null,
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "udid": udid,
        "device": {
            "marketing_name": device.and_then(|d| d.model.clone()),
            "product_type": Value::Null,
            "ios": device.map(|d| d.os.clone()),
            "build": Value::Null,
        },
        "fetched_at": fetched_at,
        "source": "simctl",
        "apps": rows,
    }))
}

// ── log stream --style compact ──────────────────────────────────────────────

/// `2026-10-11 07:01:02.123456` (fraction optional): its length, if `text`
/// starts with one.
fn compact_timestamp_len(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| b.get(range).is_some_and(|s| s.iter().all(u8::is_ascii_digit));
    let shape = b.len() >= 19
        && digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && b[10] == b' '
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19);
    if !shape {
        return None;
    }
    let mut end = 19;
    if b.get(19) == Some(&b'.') {
        end = 20;
        while b.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
    }
    // A timezone offset (`-0700`) some styles add.
    if let Some(&sign) = b.get(end) {
        if (sign == b'-' || sign == b'+') && digits(end + 1..end + 5) {
            end += 5;
        }
    }
    Some(end)
}

/// The compact style's type column as the ring's level names.
fn compact_level(code: &str) -> Option<&'static str> {
    Some(match code {
        "Db" => "debug",
        "I" | "In" => "info",
        "Df" => "notice",
        "E" | "Er" => "error",
        "F" | "Ft" => "fault",
        "A" | "Ac" => "info",
        _ => return None,
    })
}

/// One line of `log stream --style compact`:
/// `2026-10-11 07:01:02.123 E  MyApp[1234:5678] [com.example:net] message`
/// (an optional `(Library)` before the subsystem). The header lines are
/// skipped; anything else unrecognized (a multi-line message's continuation)
/// is kept whole as the message.
pub fn parse_compact(line: &str) -> Option<Parsed> {
    if line.starts_with("Filtering the log data")
        || (line.starts_with("Timestamp") && line.contains("Process["))
    {
        return None;
    }
    Some(parse_compact_fields(line).unwrap_or_else(|| Parsed {
        message: line.to_string(),
        ..Parsed::default()
    }))
}

fn parse_compact_fields(line: &str) -> Option<Parsed> {
    let ts_len = compact_timestamp_len(line)?;
    let ts = &line[..ts_len];
    let rest = line[ts_len..].trim_start();
    let (code, rest) = rest.split_once(' ')?;
    let level = compact_level(code)?;
    let rest = rest.trim_start();
    // `Process name[pid:tid]`: the first `[digits:hex]`.
    let mut search = 0;
    let (process, pid, after) = loop {
        let open = search + rest[search..].find('[')?;
        let close = open + rest[open..].find(']')?;
        let inner = &rest[open + 1..close];
        if let Some((pid, tid)) = inner.split_once(':') {
            if let (Ok(pid), true) = (
                pid.parse::<u32>(),
                !tid.is_empty() && tid.chars().all(|c| c.is_ascii_hexdigit()),
            ) {
                break (rest[..open].trim(), pid, &rest[close + 1..]);
            }
        }
        search = open + 1;
    };
    if process.is_empty() {
        return None;
    }
    let mut message = after.trim_start();
    let mut subsystem = None;
    if let Some(inner) = message.strip_prefix('(') {
        if let Some(end) = inner.find(')') {
            subsystem = Some(inner[..end].to_string());
            message = inner[end + 1..].trim_start();
        }
    }
    if let Some(inner) = message.strip_prefix('[') {
        if let Some(end) = inner.find(']') {
            let tag = &inner[..end];
            if !tag.is_empty() && !tag.contains(' ') {
                subsystem = Some(tag.to_string());
                message = inner[end + 1..].trim_start();
            }
        }
    }
    Some(Parsed {
        ts: Some(ts.to_string()),
        device: None,
        process: Some(process.to_string()),
        subsystem,
        pid: Some(pid),
        level: Some(level.to_string()),
        message: message.to_string(),
    })
}

// ── crash reports on the Mac ────────────────────────────────────────────────

/// `~/Library/Logs/DiagnosticReports`, where macOS writes simulator crashes.
pub fn default_diagnostic_reports() -> PathBuf {
    crate::setup::sys::home().join("Library/Logs/DiagnosticReports")
}

/// Whether a report's text is from simulator `udid`: simulated processes run
/// from `…/CoreSimulator/Devices/<UDID>/…` (`.ips` JSON escapes the slashes).
pub fn report_is_from(text: &str, udid: &str) -> bool {
    let needle = format!("CORESIMULATOR/DEVICES/{}", udid.to_ascii_uppercase());
    text.replace("\\/", "/").to_ascii_uppercase().contains(&needle)
}

fn read_prefix(path: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    std::fs::File::open(path)?
        .take(cap as u64)
        .read_to_end(&mut out)?;
    Ok(out)
}

/// The report files in `dir` and its immediate subfolders (`Retired`).
fn report_files(dir: &Path) -> Vec<(PathBuf, Option<String>)> {
    /// Report files in `folder` (labelled `label`) and its subfolders.
    fn visit(folder: &Path, label: Option<String>, out: &mut Vec<(PathBuf, Option<String>)>) -> Vec<PathBuf> {
        let mut subfolders = Vec::new();
        let Ok(entries) = std::fs::read_dir(folder) else {
            return subfolders;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind.is_dir() && !name.starts_with('.') {
                subfolders.push(entry.path());
            } else if kind.is_file() && crate::crash::valid_name(&name) {
                out.push((entry.path(), label.clone()));
            }
        }
        subfolders
    }
    let mut out = Vec::new();
    for sub in visit(dir, None, &mut out) {
        let label = sub.file_name().map(|n| n.to_string_lossy().into_owned());
        visit(&sub, label, &mut out);
    }
    out
}

/// Crash reports from simulator `udid` in `dir`, as `GET /agent/crashes`
/// lists a phone's (newest first, filtered).
pub fn crash_list(dir: &Path, udid: &str, filter: &CrashFilter) -> Vec<CrashEntry> {
    let entries = report_files(dir)
        .into_iter()
        .filter_map(|(path, folder)| {
            let meta = std::fs::metadata(&path).ok()?;
            let text = read_prefix(&path, CRASH_SNIFF).ok()?;
            if !report_is_from(&String::from_utf8_lossy(&text), udid) {
                return None;
            }
            let name = path.file_name()?.to_string_lossy().into_owned();
            let (process, stamp) = crate::crash::parse_name(&name);
            let time = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            Some(CrashEntry {
                name,
                folder,
                process,
                time,
                stamp,
                size: meta.len(),
            })
        })
        .collect();
    crate::crash::select(entries, filter)
}

/// One report from simulator `udid` (first [`crate::crash::READ_CAP`] bytes).
pub fn crash_read(dir: &Path, udid: &str, name: &str) -> Result<CrashReport> {
    crate::crash::validate_name(name)?;
    let not_found = || anyhow::anyhow!("no crash report named {name} from this simulator");
    let path = report_files(dir)
        .into_iter()
        .map(|(path, _)| path)
        .find(|path| path.file_name().is_some_and(|n| n.to_string_lossy() == name))
        .ok_or_else(not_found)?;
    let bytes = read_prefix(&path, crate::crash::READ_CAP + 1)
        .with_context(|| format!("read {}", path.display()))?;
    let truncated = bytes.len() > crate::crash::READ_CAP;
    let bytes = &bytes[..bytes.len().min(crate::crash::READ_CAP)];
    let text = String::from_utf8_lossy(bytes).into_owned();
    if !report_is_from(&text, udid) {
        return Err(not_found());
    }
    Ok(CrashReport {
        name: name.to_string(),
        size: bytes.len(),
        text,
        truncated,
    })
}

// ── running simctl ──────────────────────────────────────────────────────────

/// What a finished command printed.
#[derive(Debug, Clone, Default)]
pub struct CmdOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// A running `log stream`: its output, and what keeps it alive (dropping the
/// guard ends the child).
pub struct LogStream {
    pub stream: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    pub guard: Box<dyn Send>,
}

/// Runs `xcrun …`. Tests substitute a recorder.
pub trait SimctlRunner: Send + Sync {
    /// Run to completion within `timeout` (killed after it).
    fn run(&self, args: &[String], timeout: Duration) -> Result<CmdOutput>;
    /// Start a long-running child whose stdout is streamed.
    fn stream(&self, args: &[String]) -> Result<LogStream>;
}

/// The real `xcrun`.
pub struct Xcrun;

impl SimctlRunner for Xcrun {
    fn run(&self, args: &[String], timeout: Duration) -> Result<CmdOutput> {
        run_with_deadline("xcrun", args, timeout)
    }

    fn stream(&self, args: &[String]) -> Result<LogStream> {
        let mut child = tokio::process::Command::new("xcrun")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("start xcrun simctl")?;
        let stdout = child.stdout.take().context("xcrun simctl has no stdout")?;
        Ok(LogStream {
            stream: Box::new(stdout),
            guard: Box::new(child),
        })
    }
}

/// Run `program args…`, reading both pipes while it runs, killed after
/// `timeout`.
fn run_with_deadline(program: &str, args: &[String], timeout: Duration) -> Result<CmdOutput> {
    use std::io::Read;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut out);
            }
            String::from_utf8_lossy(&out).into_owned()
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("wait for xcrun")? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "`{program} {}` did not finish within {}s and was stopped",
                args.join(" "),
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(CmdOutput {
        success: status.success(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// `run`, as an error unless it succeeded.
fn checked(runner: &dyn SimctlRunner, args: &[String], timeout: Duration) -> Result<String> {
    let out = runner.run(args, timeout)?;
    if !out.success {
        let detail = out.stderr.trim();
        let detail = if detail.is_empty() { out.stdout.trim() } else { detail };
        bail!("`xcrun {}` failed: {detail}", args.join(" "));
    }
    Ok(out.stdout)
}

// ── the daemon's simulator backend ──────────────────────────────────────────

/// The simulator a daemon drives (`IPHONE_USE_SIMULATOR`), and how it reaches
/// it. Device-service routes use this instead of lockdown/devicectl.
pub struct Simulator {
    pub udid: String,
    runner: Arc<dyn SimctlRunner>,
    /// Where crash reports are looked for.
    pub diagnostic_reports: PathBuf,
    /// The last `simctl list` answer for this simulator.
    info: Mutex<Option<SimDevice>>,
}

impl std::fmt::Debug for Simulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Simulator").field("udid", &self.udid).finish()
    }
}

impl Simulator {
    pub fn new(udid: &str, runner: Arc<dyn SimctlRunner>, diagnostic_reports: PathBuf) -> Self {
        Self {
            udid: udid.to_ascii_uppercase(),
            runner,
            diagnostic_reports,
            info: Mutex::new(None),
        }
    }

    /// From `IPHONE_USE_SIMULATOR`: `Ok(None)` when unset, `Err` when it is
    /// not a simulator UDID.
    pub fn from_env() -> Result<Option<Self>> {
        let Some(value) = std::env::var(ENV).ok().filter(|v| !v.trim().is_empty()) else {
            return Ok(None);
        };
        let udid = value.trim();
        if !valid_udid(udid) {
            bail!("{ENV}={udid:?} is not a simulator UDID (`iphone-use simulators` lists them)");
        }
        Ok(Some(Self::new(
            udid,
            Arc::new(Xcrun),
            default_diagnostic_reports(),
        )))
    }

    async fn simctl(&self, args: Vec<String>, timeout: Duration) -> Result<String> {
        let runner = self.runner.clone();
        tokio::task::spawn_blocking(move || checked(runner.as_ref(), &args, timeout))
            .await
            .context("simctl task")?
    }

    #[cfg(test)]
    pub fn info_slot(&self) -> std::sync::MutexGuard<'_, Option<SimDevice>> {
        self.info.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The cached `simctl list` row for this simulator (no I/O).
    pub fn info(&self) -> Option<SimDevice> {
        self.info.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Read this simulator's row from `simctl list` and cache it.
    pub async fn refresh_info(&self) -> Result<SimDevice> {
        let text = self.simctl(list_argv(), QUICK_TIMEOUT).await?;
        let devices = parse_devices(&text)?;
        let device = devices
            .iter()
            .find(|d| d.udid.eq_ignore_ascii_case(&self.udid))
            .cloned()
            .with_context(|| format!("simulator {} is not in `simctl list devices available`", self.udid))?;
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) = Some(device.clone());
        Ok(device)
    }

    /// `device` in `/agent/status`: the simulator's name and runtime.
    pub fn status_device_json(&self) -> Value {
        match self.info() {
            Some(device) => device.to_status_json(),
            None => json!({"udid": self.udid, "simulator": true}),
        }
    }

    /// The installed-app inventory body (`/agent/apps` contract).
    pub async fn installed_apps(&self) -> Result<Value> {
        let text = self.simctl(listapps_argv(&self.udid), LISTAPPS_TIMEOUT).await?;
        let apps = parse_listapps(&text)?;
        if self.info().is_none() {
            let _ = self.refresh_info().await;
        }
        inventory_body(
            &self.udid,
            self.info().as_ref(),
            &apps,
            &crate::apps::now_rfc3339(),
        )
        .map_err(anyhow::Error::msg)
    }

    /// Install a simulator build (`.app` folder).
    pub async fn install(&self, app: &Path) -> Result<()> {
        self.simctl(install_argv(&self.udid, app), INSTALL_TIMEOUT)
            .await
            .map(drop)
    }

    pub async fn uninstall(&self, bundle: &str) -> Result<()> {
        self.simctl(uninstall_argv(&self.udid, bundle), QUICK_TIMEOUT)
            .await
            .map(drop)
    }

    pub async fn set_location(&self, lat: f64, lon: f64) -> Result<()> {
        let args = location_set_argv(&self.udid, lat, lon)?;
        self.simctl(args, QUICK_TIMEOUT).await.map(drop)
    }

    pub async fn clear_location(&self) -> Result<()> {
        self.simctl(location_clear_argv(&self.udid), QUICK_TIMEOUT)
            .await
            .map(drop)
    }

    pub async fn open_url(&self, url: &str) -> Result<()> {
        self.simctl(openurl_argv(&self.udid, url), QUICK_TIMEOUT)
            .await
            .map(drop)
    }

    /// Capture this simulator's log into `capture` (kept running when it
    /// already captures this simulator).
    pub fn start_logs(&self, capture: &crate::syslog::SyslogCapture) -> Result<()> {
        if capture.udid().as_deref() == Some(self.udid.as_str()) {
            return Ok(());
        }
        let LogStream { stream, guard } = self.runner.stream(&log_stream_argv(&self.udid, None))?;
        capture.start_lines(&self.udid, stream, parse_compact, guard);
        Ok(())
    }

    pub async fn crashes(&self, filter: &CrashFilter) -> Result<Vec<CrashEntry>> {
        let (dir, udid, filter) = (self.diagnostic_reports.clone(), self.udid.clone(), filter.clone());
        tokio::task::spawn_blocking(move || crash_list(&dir, &udid, &filter))
            .await
            .context("crash list task")
    }

    pub async fn crash(&self, name: &str) -> Result<CrashReport> {
        let (dir, udid, name) = (self.diagnostic_reports.clone(), self.udid.clone(), name.to_string());
        tokio::task::spawn_blocking(move || crash_read(&dir, &udid, &name))
            .await
            .context("crash read task")?
    }
}

/// `lock_readiness` in `/agent/status` for a simulator: it never locks.
pub fn lock_readiness_json() -> Value {
    json!({
        "passcode_protected": false,
        "auto_lock_secs": "never",
        "keep_awake": {"enabled": false, "supported": false, "active": false},
        "verdict": "ready",
        "applicable": false,
        "hint": {
            "zh": "模拟器不会锁屏，无需保持唤醒。",
            "en": "A simulator never locks; nothing to keep awake."
        },
        "checked_at": Value::Null,
    })
}

// ── CLI: iphone-use simulators / simulator start|stop ───────────────────────

/// The default instance name for simulator `udid`: `sim-<first 8 hex>`.
pub fn instance_name_for(udid: &str) -> String {
    let short: String = udid
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    format!("sim-{short}")
}

/// The instance every simulator tries first, so one MCP config
/// (`IPHONE_USE_INSTANCE=sim`) reaches whichever simulator was started.
pub const SHARED_INSTANCE: &str = "sim";

/// The instance a simulator uses when none is named: `sim`, unless `sim`
/// already belongs to another simulator (`sim_owner`, the UDID in its
/// record), then `sim-<first 8 of the UDID>`.
pub fn default_instance_name(udid: &str, sim_owner: Option<&str>) -> String {
    match sim_owner {
        Some(owner) if !owner.eq_ignore_ascii_case(udid) => instance_name_for(udid),
        _ => SHARED_INSTANCE.to_string(),
    }
}

/// `xcodebuild` arguments that launch the runner on simulator `udid`.
pub fn runner_launch_argv(udid: &str, xctestrun: &Path) -> Vec<String> {
    vec![
        "-destination".into(),
        format!("id={udid}"),
        "test-without-building".into(),
        "-xctestrun".into(),
        xctestrun.to_string_lossy().into_owned(),
        format!("-only-testing:{}", crate::setup::ctx::RUNNER_TEST_ID),
    ]
}

/// The runner's test environment: xcodebuild hands `TEST_RUNNER_<NAME>` to
/// the runner as `<NAME>` (`IPU_RUNNER_PORT`, `IPU_RUNNER_MJPEG_PORT`).
pub fn runner_port_env(port: u16, mjpeg_port: u16) -> [(&'static str, String); 2] {
    [
        ("TEST_RUNNER_IPU_RUNNER_PORT", port.to_string()),
        ("TEST_RUNNER_IPU_RUNNER_MJPEG_PORT", mjpeg_port.to_string()),
    ]
}

/// `runner/build.sh --simulator` arguments and the derived data it uses.
pub fn build_argv() -> Vec<String> {
    vec!["--simulator".into()]
}

/// The `.xctestrun` path build.sh prints on its last stdout line.
pub fn xctestrun_from_build_output(stdout: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .filter(|line| line.starts_with('/') && line.ends_with(".xctestrun"))
        .map(PathBuf::from)
}

/// The runner and MJPEG ports: `requested` (MJPEG at +1000) when both are
/// free, else the first free pair from 8200.
pub fn pick_ports(requested: Option<u16>, free: impl Fn(u16) -> bool) -> Result<(u16, u16), String> {
    let pair = |port: u16| port.checked_add(1000).map(|mjpeg| (port, mjpeg));
    if let Some(port) = requested {
        let (port, mjpeg) = pair(port).ok_or_else(|| format!("--port {port} is too high"))?;
        if !free(port) || !free(mjpeg) {
            return Err(format!(
                "port {port} or {mjpeg} (MJPEG) is already in use on 127.0.0.1; pick another --port"
            ));
        }
        return Ok((port, mjpeg));
    }
    (8200..8300)
        .filter_map(pair)
        .find(|(port, mjpeg)| free(*port) && free(*mjpeg))
        .ok_or_else(|| "no free port pair in 8200-8299 (+1000 for MJPEG); pass --port".to_string())
}

/// The shell line that runs a daemon instance against the started runner.
pub fn daemon_command(instance: &str, udid: &str, port: u16, mjpeg_port: u16, daemon_port: u16) -> String {
    format!(
        "IPHONE_USE_INSTANCE={instance} {ENV}={udid} IPHONE_USE_WDA_URL=http://127.0.0.1:{port} IPHONE_USE_WDA_MJPEG_URL=http://127.0.0.1:{mjpeg_port} IPHONE_USE_PORT={daemon_port} iphone-use serve"
    )
}

/// `<pid>|<lstart>|<command>`, what `simulator stop` verifies before it
/// signals anything.
#[derive(Debug, Clone, PartialEq)]
pub struct PidRecord {
    pub pid: u32,
    pub lstart: String,
    pub command: String,
}

impl PidRecord {
    pub fn encode(&self) -> String {
        format!("{}|{}|{}\n", self.pid, self.lstart, self.command)
    }

    pub fn parse(text: &str) -> Option<Self> {
        let line = text.strip_suffix('\n').unwrap_or(text);
        if line.contains('\n') {
            return None;
        }
        let mut parts = line.splitn(3, '|');
        let pid = parts.next()?.parse::<u32>().ok().filter(|pid| *pid > 1)?;
        let lstart = parts.next()?.to_string();
        let command = parts.next()?.to_string();
        (!lstart.is_empty() && !command.is_empty()).then_some(Self { pid, lstart, command })
    }

    /// Whether the live process (`uid`, `lstart`, `command` as `ps` shows
    /// them) is still the runner this record names for simulator `udid`.
    pub fn matches(&self, our_uid: u32, udid: &str, live: (Option<u32>, &str, &str)) -> bool {
        let (uid, lstart, command) = live;
        let program = self.command.split(' ').next().unwrap_or("");
        uid == Some(our_uid)
            && lstart == self.lstart
            && command == self.command
            && (program == "xcodebuild" || program.ends_with("/xcodebuild"))
            && self.command.contains(&format!(" -destination id={udid} test-without-building "))
    }

    /// Whether the live process is still the daemon `simulator start` ran.
    pub fn matches_daemon(&self, our_uid: u32, live: (Option<u32>, &str, &str)) -> bool {
        let (uid, lstart, command) = live;
        uid == Some(our_uid)
            && lstart == self.lstart
            && command == self.command
            && self.command.ends_with(" serve")
    }
}

const PID_FILE: &str = "simulator-runner.pid";
const RECORD_FILE: &str = "simulator.json";
const RUN_LOG: &str = "simulator-runner.log";
const DAEMON_PID_FILE: &str = "simulator-daemon.pid";
const DAEMON_LOG: &str = "simulator-daemon.log";

/// The simulator UDID a record file names.
fn record_udid(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text).ok()?.get("udid")?.as_str().map(str::to_string)
}

/// The daemon port a simulator record names (`simulator start` writes it).
pub fn record_daemon_port(text: &str) -> Option<u16> {
    let port = serde_json::from_str::<Value>(text).ok()?.get("daemon_port")?.as_u64()?;
    u16::try_from(port).ok().filter(|port| *port > 0)
}

fn print_err(message: &str) {
    eprintln!("iphone-use: {message}");
}

fn list_devices(runner: &dyn SimctlRunner) -> Result<Vec<SimDevice>> {
    parse_devices(&checked(runner, &list_argv(), QUICK_TIMEOUT)?)
}

/// `iphone-use simulators [--json]`.
pub fn cli_list(json_out: bool) -> i32 {
    let devices = match list_devices(&Xcrun) {
        Ok(devices) => devices,
        Err(error) => {
            print_err(&format!("{error:#} (simulators need Xcode on this Mac)"));
            return 1;
        }
    };
    let ios: Vec<&SimDevice> = devices.iter().filter(|d| d.platform == "iOS").collect();
    if json_out {
        println!("{}", serde_json::to_string_pretty(&ios).unwrap_or_else(|_| "[]".into()));
        return 0;
    }
    if ios.is_empty() {
        println!("No iOS simulators. Add one in Xcode > Window > Devices and Simulators.");
        return 0;
    }
    let width = ios.iter().map(|d| d.name.chars().count()).max().unwrap_or(4).max(4);
    println!("{:<width$}  {:<10}  {:<9}  UDID", "NAME", "RUNTIME", "STATE");
    for device in ios {
        println!(
            "{:<width$}  {:<10}  {:<9}  {}",
            device.name, device.runtime, device.state, device.udid
        );
    }
    0
}

/// The state directory of the instance for a simulator.
fn instance_for(udid: &str, instance: Option<&str>) -> Result<crate::instance::Instance, String> {
    let name = match instance {
        Some(name) => name.to_string(),
        None => {
            let shared = crate::instance::Instance::derive(SHARED_INSTANCE, crate::setup::sys::home(), None)?;
            let owner = std::fs::read_to_string(shared.state_dir.join(RECORD_FILE))
                .ok()
                .as_deref()
                .and_then(record_udid);
            default_instance_name(udid, owner.as_deref())
        }
    };
    crate::instance::Instance::derive(&name, crate::setup::sys::home(), None)
}

/// How the simulator runner gets built: a checkout's `build.sh`, or the
/// Xcode project the installer leaves in `~/.iphone-use/runner` (which ships
/// without build.sh), built with xcodebuild directly.
#[derive(Debug, Clone, PartialEq)]
pub enum RunnerBuild {
    Script(PathBuf),
    Project(PathBuf),
}

/// The `xcodebuild build-for-testing` arguments for the simulator runner,
/// the same ones `build.sh --simulator` passes.
pub fn project_build_argv(project: &Path, derived_data: &Path) -> Vec<String> {
    [
        "-project",
        &project.display().to_string(),
        "-scheme",
        "IPhoneUseRunner",
        "-configuration",
        "Debug",
        "-destination",
        "generic/platform=iOS Simulator",
        "-derivedDataPath",
        &derived_data.display().to_string(),
        "CODE_SIGNING_ALLOWED=NO",
        "CODE_SIGNING_REQUIRED=NO",
        "CODE_SIGN_IDENTITY=",
        "build-for-testing",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The newest `.xctestrun` under `<derived data>/Build/Products`.
pub fn newest_xctestrun(derived_data: &Path) -> Option<PathBuf> {
    std::fs::read_dir(derived_data.join("Build/Products"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "xctestrun"))
        .max_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
}

fn runner_build_source(src: &Path) -> Option<RunnerBuild> {
    if src.join("build.sh").is_file() {
        return Some(RunnerBuild::Script(src.join("build.sh")));
    }
    let project = src.join("IPhoneUseRunner/IPhoneUseRunner.xcodeproj");
    project.is_dir().then_some(RunnerBuild::Project(project))
}

fn runner_sources() -> Result<RunnerBuild, String> {
    let src = std::env::var("IPU_RUNNER_SRC")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::setup::sys::home().join(".iphone-use/runner"));
    runner_build_source(&src).ok_or_else(|| {
        format!(
            "no runner sources under {} (neither build.sh nor IPhoneUseRunner/IPhoneUseRunner.xcodeproj); rerun the installer, set IPU_RUNNER_SRC to an iphone-use checkout's runner/ folder, or pass --xctestrun",
            src.display()
        )
    })
}

fn xcodebuild_path() -> PathBuf {
    let found = crate::setup::sys::stdout_of("xcrun", &["--find", "xcodebuild"]);
    if found.starts_with('/') {
        PathBuf::from(found)
    } else {
        PathBuf::from("/usr/bin/xcodebuild")
    }
}

fn free_port(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn log_tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Stop the process a pid file records, once `verify` confirms the live
/// process is still that one. `Ok(true)` when it stopped one.
fn stop_recorded(
    pid_file: &Path,
    what: &str,
    verify: impl Fn(&PidRecord, (Option<u32>, &str, &str)) -> bool,
) -> Result<bool, String> {
    use crate::setup::sys;
    let Ok(text) = std::fs::read_to_string(pid_file) else {
        return Ok(false);
    };
    let record = PidRecord::parse(&text)
        .ok_or_else(|| format!("invalid pid record {}; refusing to act", pid_file.display()))?;
    if !sys::pid_exists(record.pid) {
        let _ = std::fs::remove_file(pid_file);
        println!("The recorded {what} (pid {}) is no longer running.", record.pid);
        return Ok(false);
    }
    let live = (sys::ps_uid(record.pid), sys::ps_lstart(record.pid), sys::ps_command(record.pid));
    if !verify(&record, (live.0, &live.1, &live.2)) {
        return Err(format!(
            "pid {} is not the {what} this command started; refusing to signal it",
            record.pid
        ));
    }
    // SAFETY: a verified pid of our own process.
    unsafe {
        libc::kill(record.pid as libc::pid_t, libc::SIGTERM);
    }
    for _ in 0..40 {
        if !sys::pid_exists(record.pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if sys::pid_exists(record.pid) {
        return Err(format!("{what} pid {} did not stop after SIGTERM", record.pid));
    }
    let _ = std::fs::remove_file(pid_file);
    println!("Stopped the {what} (pid {}).", record.pid);
    Ok(true)
}

/// The environment of the daemon that drives a started simulator.
pub fn daemon_env(instance: &str, udid: &str, port: u16, mjpeg_port: u16, daemon_port: u16) -> Vec<(&'static str, String)> {
    vec![
        ("IPHONE_USE_INSTANCE", instance.to_string()),
        (ENV, udid.to_string()),
        ("IPHONE_USE_WDA_URL", format!("http://127.0.0.1:{port}")),
        ("IPHONE_USE_WDA_MJPEG_URL", format!("http://127.0.0.1:{mjpeg_port}")),
        ("IPHONE_USE_PORT", daemon_port.to_string()),
    ]
}

/// `iphone-use simulator start <name|udid> [--port N] [--xctestrun P] [--instance N] [--no-serve]`.
pub fn cli_start(query: &str, port: Option<u16>, xctestrun: Option<PathBuf>, instance: Option<&str>, serve: bool) -> i32 {
    match start(query, port, xctestrun, instance, serve) {
        Ok(()) => 0,
        Err(message) => {
            print_err(&message);
            1
        }
    }
}

fn start(query: &str, port: Option<u16>, xctestrun: Option<PathBuf>, instance: Option<&str>, serve: bool) -> Result<(), String> {
    use crate::setup::{proc, sys};
    let runner = Xcrun;
    let devices = list_devices(&runner).map_err(|e| format!("{e:#} (simulators need Xcode on this Mac)"))?;
    let device = find(&devices, query)?.clone();
    let udid = device.udid.clone();
    let instance = instance_for(&udid, instance)?;
    let state_dir = instance.state_dir.clone();
    std::fs::create_dir_all(&state_dir).map_err(|e| format!("{}: {e}", state_dir.display()))?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700));
    }
    let pid_file = state_dir.join(PID_FILE);
    if let Some(record) = std::fs::read_to_string(&pid_file).ok().as_deref().and_then(PidRecord::parse) {
        let live = (sys::ps_uid(record.pid), sys::ps_lstart(record.pid), sys::ps_command(record.pid));
        if record.matches(sys::uid(), &udid, (live.0, &live.1, &live.2)) {
            return Err(format!(
                "a runner for {} is already running (pid {}); `iphone-use simulator stop {}` first",
                device.name, record.pid, udid
            ));
        }
    }
    // A daemon left from an earlier start drives a runner that is gone.
    let our_uid = sys::uid();
    stop_recorded(&state_dir.join(DAEMON_PID_FILE), "daemon", |record, live| record.matches_daemon(our_uid, live))?;
    let (port, mjpeg_port) = pick_ports(port, free_port)?;

    println!("Simulator: {} ({}, {}) {udid}", device.name, device.runtime, device.state);
    // Best effort: a failure only means typing may need the keyboard shown by hand.
    let _ = std::process::Command::new("/usr/bin/defaults")
        .args(software_keyboard_argv())
        .status();
    if !device.booted() {
        println!("Booting ...");
        // "Unable to boot device in current state: Booted" is a race, not a failure.
        let _ = runner.run(&boot_argv(&udid), BOOT_TIMEOUT);
    }
    checked(&runner, &bootstatus_argv(&udid), BOOT_TIMEOUT)
        .map_err(|e| format!("the simulator did not finish booting: {e:#}"))?;

    let xctestrun = match xctestrun {
        Some(path) if path.is_file() => path,
        Some(path) => return Err(format!("{} is not a file", path.display())),
        None => {
            let derived_data = state_dir.join("runner-build-simulator");
            println!("Building the runner for the simulator (first build takes a few minutes) ...");
            match runner_sources()? {
                RunnerBuild::Script(script) => {
                    let out = std::process::Command::new("/bin/bash")
                        .arg(script)
                        .args(build_argv())
                        .env("IPU_RUNNER_DERIVED_DATA", &derived_data)
                        .stderr(std::process::Stdio::inherit())
                        .output()
                        .map_err(|e| format!("run build.sh: {e}"))?;
                    if !out.status.success() {
                        return Err("the simulator runner build failed (see above)".into());
                    }
                    xctestrun_from_build_output(&String::from_utf8_lossy(&out.stdout))
                        .ok_or("build.sh did not print an .xctestrun path")?
                }
                RunnerBuild::Project(project) => {
                    let log = state_dir.join("runner-build-simulator.log");
                    let log_file = std::fs::File::create(&log)
                        .map_err(|e| format!("create {}: {e}", log.display()))?;
                    let status = std::process::Command::new(xcodebuild_path())
                        .args(project_build_argv(&project, &derived_data))
                        .stdout(log_file.try_clone().map_err(|e| e.to_string())?)
                        .stderr(log_file)
                        .status()
                        .map_err(|e| format!("run xcodebuild: {e}"))?;
                    if !status.success() {
                        return Err(format!(
                            "the simulator runner build failed; full log: {}",
                            log.display()
                        ));
                    }
                    newest_xctestrun(&derived_data).ok_or_else(|| {
                        format!(
                            "xcodebuild produced no .xctestrun under {}/Build/Products",
                            derived_data.display()
                        )
                    })?
                }
            }
        }
    };
    println!("Runner product: {}", xctestrun.display());

    let xcodebuild = xcodebuild_path();
    let args = runner_launch_argv(&udid, &xctestrun);
    let command = format!("{} {}", xcodebuild.display(), args.join(" "));
    if command.contains(['|', '\n']) {
        return Err("the runner path holds a character the pid record cannot store".into());
    }
    let token = crate::runner_token::rotate(&state_dir)?;
    let [port_env, mjpeg_env] = runner_port_env(port, mjpeg_port);
    let run_log = state_dir.join(RUN_LOG);
    let pid = proc::spawn_detached_env(
        &xcodebuild,
        &args,
        Some(&state_dir),
        &run_log,
        None,
        &[
            (crate::runner_token::XCODEBUILD_TOKEN_ENV, token.as_str()),
            (port_env.0, port_env.1.as_str()),
            (mjpeg_env.0, mjpeg_env.1.as_str()),
        ],
    )
    .map_err(|e| format!("start xcodebuild: {e}"))?;
    // Record it once `ps` shows exactly the command (exec takes a moment).
    let mut record = None;
    for _ in 0..30 {
        let lstart = sys::ps_lstart(pid);
        if !lstart.is_empty() && sys::ps_command(pid) == command {
            record = Some(PidRecord { pid, lstart, command: command.clone() });
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let record = record.ok_or_else(|| {
        format!(
            "xcodebuild (pid {pid}) did not become the expected runner process; nothing was recorded. Log: {}",
            run_log.display()
        )
    })?;
    sys::write_atomic(&pid_file, record.encode().as_bytes(), 0o600)
        .map_err(|e| format!("{}: {e}", pid_file.display()))?;
    let summary = json!({
        "udid": udid, "name": device.name, "runtime": device.runtime,
        "port": port, "mjpeg_port": mjpeg_port, "pid": pid,
    });
    let _ = sys::write_atomic(&state_dir.join(RECORD_FILE), summary.to_string().as_bytes(), 0o600);

    println!("Runner started (pid {pid}); waiting for it on 127.0.0.1:{port} (log: {}) ...", run_log.display());
    let auth = crate::runner_token::TokenSource::at(&state_dir);
    let status_url = format!("http://127.0.0.1:{port}/status");
    let started = std::time::Instant::now();
    loop {
        if sys::runner_ok(&status_url, Duration::from_secs(2), &auth) {
            break;
        }
        if !sys::pid_exists(pid) {
            let _ = std::fs::remove_file(&pid_file);
            return Err(format!(
                "xcodebuild exited before the runner answered. Last lines of {}:\n{}",
                run_log.display(),
                log_tail(&run_log, 20)
            ));
        }
        if started.elapsed() > Duration::from_secs(600) {
            return Err(format!(
                "the runner did not answer within 10 minutes (it keeps running; `iphone-use simulator stop {udid}` stops it). Log: {}",
                run_log.display()
            ));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let daemon_port = (44331..44431).find(|p| free_port(*p)).unwrap_or(44331);
    println!("The runner answers on http://127.0.0.1:{port}.");
    let mut summary = summary;
    summary["instance"] = json!(instance.name);
    summary["daemon_port"] = json!(daemon_port);
    let _ = sys::write_atomic(&state_dir.join(RECORD_FILE), summary.to_string().as_bytes(), 0o600);
    if !serve {
        println!();
        println!("Run a daemon for this simulator (preview; logs, crashes, location and apps go through simctl):");
        println!("  {}", daemon_command(&instance.name, &udid, port, mjpeg_port, daemon_port));
        println!();
        println!("Then point an agent at http://127.0.0.1:{daemon_port} (iphone-use status --instance {} checks it).", instance.name);
        println!("Stop the runner with: iphone-use simulator stop {udid} [--shutdown]");
        return Ok(());
    }

    let exe = std::env::current_exe().map_err(|e| format!("locate iphone-use: {e}"))?;
    let daemon_args = vec!["serve".to_string()];
    let daemon_command_line = format!("{} serve", exe.display());
    if daemon_command_line.contains(['|', '\n']) {
        return Err("the iphone-use path holds a character the pid record cannot store".into());
    }
    let env = daemon_env(&instance.name, &udid, port, mjpeg_port, daemon_port);
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(key, value)| (*key, value.as_str())).collect();
    let daemon_log = state_dir.join(DAEMON_LOG);
    let daemon_pid = proc::spawn_detached_env(&exe, &daemon_args, Some(&state_dir), &daemon_log, None, &env_refs)
        .map_err(|e| format!("start the daemon: {e}"))?;
    let mut daemon_record = None;
    for _ in 0..30 {
        let lstart = sys::ps_lstart(daemon_pid);
        if !lstart.is_empty() && sys::ps_command(daemon_pid) == daemon_command_line {
            daemon_record = Some(PidRecord { pid: daemon_pid, lstart, command: daemon_command_line.clone() });
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let daemon_record = daemon_record.ok_or_else(|| {
        format!(
            "the daemon (pid {daemon_pid}) did not become the expected process; nothing was recorded. Log: {}",
            daemon_log.display()
        )
    })?;
    let daemon_pid_file = state_dir.join(DAEMON_PID_FILE);
    sys::write_atomic(&daemon_pid_file, daemon_record.encode().as_bytes(), 0o600)
        .map_err(|e| format!("{}: {e}", daemon_pid_file.display()))?;
    let started = std::time::Instant::now();
    loop {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], daemon_port));
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
            break;
        }
        if !sys::pid_exists(daemon_pid) {
            let _ = std::fs::remove_file(&daemon_pid_file);
            return Err(format!(
                "the daemon exited before it listened. Last lines of {}:\n{}",
                daemon_log.display(),
                log_tail(&daemon_log, 20)
            ));
        }
        if started.elapsed() > Duration::from_secs(60) {
            return Err(format!(
                "the daemon did not listen on 127.0.0.1:{daemon_port} within a minute. Log: {}",
                daemon_log.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!();
    println!("Ready: http://127.0.0.1:{daemon_port}/phone (instance {}, preview).", instance.name);
    for line in agent_hints(&instance.name) {
        println!("{line}");
    }
    println!("Stop it with: iphone-use simulator stop {udid} [--shutdown]");
    Ok(())
}

/// How to point an agent at a simulator's daemon.
pub fn agent_hints(instance: &str) -> Vec<String> {
    vec![
        "Point an agent at it:".to_string(),
        format!("  claude mcp add iphone-{instance} -e IPHONE_USE_INSTANCE={instance} -- iphone-use-mcp"),
        format!("  (any MCP client: run iphone-use-mcp with IPHONE_USE_INSTANCE={instance})"),
    ]
}

/// `iphone-use simulator stop <name|udid> [--shutdown] [--instance N]`.
pub fn cli_stop(query: &str, shutdown: bool, instance: Option<&str>) -> i32 {
    match stop(query, shutdown, instance) {
        Ok(()) => 0,
        Err(message) => {
            print_err(&message);
            1
        }
    }
}

fn stop(query: &str, shutdown: bool, instance: Option<&str>) -> Result<(), String> {
    use crate::setup::sys;
    let runner = Xcrun;
    let udid = if valid_udid(query.trim()) {
        query.trim().to_ascii_uppercase()
    } else {
        let devices = list_devices(&runner).map_err(|e| format!("{e:#}"))?;
        find(&devices, query)?.udid.clone()
    };
    let instance = instance_for(&udid, instance)?;
    let our_uid = sys::uid();
    stop_recorded(&instance.state_dir.join(DAEMON_PID_FILE), "daemon", |record, live| record.matches_daemon(our_uid, live))?;
    let pid_file = instance.state_dir.join(PID_FILE);
    if !pid_file.exists() {
        println!("No runner recorded for {udid} (nothing to stop).");
    }
    stop_recorded(&pid_file, "runner", |record, live| record.matches(our_uid, &udid, live))?;
    // Frees the shared `sim` instance for the next simulator.
    let _ = std::fs::remove_file(instance.state_dir.join(RECORD_FILE));
    if shutdown {
        checked(&runner, &shutdown_argv(&udid), BOOT_TIMEOUT)
            .map_err(|e| format!("shut the simulator down: {e:#}"))?;
        println!("Shut down simulator {udid}.");
    }
    Ok(())
}

// ── device state, setup, doctor ─────────────────────────────────────────────

/// `device_state` for a simulator that is not ready, from its simctl state:
/// `booting` while it boots, `shutdown` when it is off. `None` otherwise
/// (the daemon's own state stands).
pub fn device_state_for(sim_state: &str) -> Option<&'static str> {
    match sim_state.trim().to_ascii_lowercase().as_str() {
        "booting" => Some("booting"),
        "shutdown" | "shutting down" => Some("shutdown"),
        _ => None,
    }
}

/// `iphone-use setup --simulator <name|udid>`: the simulator named, if the
/// arguments ask for one (`Err` when the flag has no value).
pub fn setup_simulator_arg(args: &[String]) -> Option<Result<String, String>> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--simulator=") {
            return Some(Ok(value.to_string()).and_then(non_empty));
        }
        if arg == "--simulator" {
            return Some(
                iter.next()
                    .filter(|value| !value.starts_with("--"))
                    .cloned()
                    .ok_or_else(|| "--simulator needs a simulator name or UDID (`iphone-use simulators` lists them)".to_string())
                    .and_then(non_empty),
            );
        }
    }
    None
}

fn non_empty(value: String) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("--simulator needs a simulator name or UDID (`iphone-use simulators` lists them)".to_string())
    } else {
        Ok(value)
    }
}

/// The doctor's simulator line: what is available and booted, and how to
/// start one. Empty when simctl could not be read (Xcode is checked above it).
pub fn doctor_lines(devices: &[SimDevice]) -> Vec<String> {
    let ios: Vec<&SimDevice> = devices.iter().filter(|d| d.platform == "iOS").collect();
    if ios.is_empty() {
        return vec![
            "Simulators (preview): none installed; add one in Xcode > Settings > Components to drive a simulator without a phone".to_string(),
        ];
    }
    let booted: Vec<&str> = ios.iter().filter(|d| d.booted()).map(|d| d.name.as_str()).collect();
    let mut line = format!("Simulators (preview): {} available", ios.len());
    if !booted.is_empty() {
        line.push_str(&format!(", booted: {}", booted.join(", ")));
    }
    vec![
        line,
        format!("  drive one without a phone: iphone-use simulator start \"{}\"", booted.first().copied().unwrap_or(ios[0].name.as_str())),
    ]
}

/// Print [`doctor_lines`] (best effort; silent when simctl is missing).
pub fn print_doctor_section() {
    if let Ok(devices) = list_devices(&Xcrun) {
        println!();
        for line in doctor_lines(&devices) {
            println!("{line}");
        }
    }
}

// ── screen recording (simctl io recordVideo) ────────────────────────────────

/// How long a simulator recording may run before it stops by itself.
pub const RECORD_MAX: Duration = Duration::from_secs(10 * 60);
/// What `simctl io recordVideo` prints on stderr once frames are being
/// written, and how long `start` waits for it.
const RECORD_READY_LINE: &str = "Recording started";
const RECORD_READY_WAIT: Duration = Duration::from_secs(15);

pub fn record_video_argv(udid: &str, path: &Path) -> Vec<String> {
    let mut args = argv(&["simctl", "io", udid, "recordVideo", "--codec=h264", "--force"]);
    args.push(path.display().to_string());
    args
}

struct SimRecording {
    path: PathBuf,
    started: std::time::Instant,
    child: tokio::process::Child,
    ended: Option<&'static str>,
}

impl SimRecording {
    fn status(&self) -> Value {
        json!({
            "path": self.path,
            "seconds": (self.started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            "bytes": std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
            "ended": self.ended,
            "via": "simctl",
        })
    }
}

/// The daemon's one simulator recording slot: `simctl io recordVideo`,
/// stopped with SIGINT so it finishes the file. Replaces the H.264 recorder
/// on a simulator, whose runner encoder is unverified.
#[derive(Default)]
pub struct SimRecorder {
    current: tokio::sync::Mutex<Option<SimRecording>>,
}

impl SimRecorder {
    pub fn global() -> &'static SimRecorder {
        static RECORDER: std::sync::OnceLock<SimRecorder> = std::sync::OnceLock::new();
        RECORDER.get_or_init(SimRecorder::default)
    }

    /// Start `xcrun simctl io <udid> recordVideo` into a new file in `dir`.
    pub async fn start(&'static self, udid: &str, dir: &Path) -> Result<Value> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = crate::recording::new_path(dir);
        let args = record_video_argv(udid, &path);
        self.start_with("xcrun", &args, path, RECORD_MAX).await
    }

    /// [`SimRecorder::start`] with any program (tests use a shell script).
    pub async fn start_with(&'static self, program: &str, args: &[String], path: PathBuf, max: Duration) -> Result<Value> {
        let mut current = self.current.lock().await;
        if let Some(recording) = current.as_ref() {
            bail!("already recording to {} (stop it first)", recording.path.display());
        }
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start {program}"))?;
        // recordVideo takes a moment to set up, and a SIGINT before it says
        // "Recording started" leaves no file. Wait for that line (bounded),
        // then keep draining stderr so the child never blocks on it.
        if let Some(stderr) = child.stderr.take() {
            use tokio::io::AsyncBufReadExt as _;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            let _ = tokio::time::timeout(RECORD_READY_WAIT, async {
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.contains(RECORD_READY_LINE) {
                        break;
                    }
                }
            })
            .await;
            tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        }
        let recording = SimRecording { path: path.clone(), started: std::time::Instant::now(), child, ended: None };
        let status = recording.status();
        *current = Some(recording);
        drop(current);
        // The 10-minute limit: interrupt it, and let `stop` collect the file.
        tokio::spawn(async move {
            tokio::time::sleep(max).await;
            let mut current = self.current.lock().await;
            if let Some(recording) = current.as_mut().filter(|r| r.path == path && r.ended.is_none()) {
                interrupt(&recording.child);
                recording.ended = Some("max_duration");
            }
        });
        Ok(status)
    }

    /// Stop the recording and return its summary; `Ok(None)` when none runs.
    pub async fn stop(&self) -> Result<Option<Value>> {
        let Some(mut recording) = self.current.lock().await.take() else {
            return Ok(None);
        };
        let seconds = (recording.started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        interrupt(&recording.child);
        if tokio::time::timeout(Duration::from_secs(20), recording.child.wait()).await.is_err() {
            let _ = recording.child.kill().await;
            bail!("simctl recordVideo did not finish the file within 20 s and was stopped");
        }
        let bytes = std::fs::metadata(&recording.path).map(|m| m.len()).unwrap_or(0);
        if bytes == 0 {
            let _ = std::fs::remove_file(&recording.path);
            bail!("no video frames were written by simctl recordVideo");
        }
        let mut summary = json!({"path": recording.path, "seconds": seconds, "bytes": bytes, "via": "simctl"});
        if let Some(ended) = recording.ended {
            summary["ended"] = json!(ended);
        }
        Ok(Some(summary))
    }

    pub async fn status(&self) -> Option<Value> {
        self.current.lock().await.as_ref().map(SimRecording::status)
    }
}

/// SIGINT: recordVideo writes the file's index and exits.
fn interrupt(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: our own child's pid, still unreaped.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGINT);
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const UDID: &str = "8A1BC3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5D";

    /// Realistic `xcrun simctl list devices available -j` output.
    pub const DEVICES_JSON: &str = r#"{
  "devices" : {
    "com.apple.CoreSimulator.SimRuntime.iOS-17-5" : [
      {
        "lastBootedAt" : "2026-06-02T08:15:31Z",
        "dataPath" : "/Users/me/Library/Developer/CoreSimulator/Devices/11111111-2222-4333-8444-555555555555/data",
        "dataPathSize" : 1220608000,
        "logPath" : "/Users/me/Library/Logs/CoreSimulator/11111111-2222-4333-8444-555555555555",
        "udid" : "11111111-2222-4333-8444-555555555555",
        "isAvailable" : true,
        "deviceTypeIdentifier" : "com.apple.CoreSimulator.SimDeviceType.iPhone-15",
        "state" : "Shutdown",
        "name" : "iPhone 15"
      },
      {
        "dataPath" : "/Users/me/Library/Developer/CoreSimulator/Devices/99999999-2222-4333-8444-555555555555/data",
        "udid" : "99999999-2222-4333-8444-555555555555",
        "isAvailable" : true,
        "deviceTypeIdentifier" : "com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro",
        "state" : "Shutdown",
        "name" : "iPhone 16 Pro"
      }
    ],
    "com.apple.CoreSimulator.SimRuntime.iOS-18-2" : [
      {
        "lastBootedAt" : "2026-10-10T21:40:02Z",
        "dataPath" : "/Users/me/Library/Developer/CoreSimulator/Devices/8A1BC3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5D/data",
        "dataPathSize" : 2843209728,
        "logPath" : "/Users/me/Library/Logs/CoreSimulator/8A1BC3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5D",
        "udid" : "8A1BC3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5D",
        "isAvailable" : true,
        "deviceTypeIdentifier" : "com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro",
        "state" : "Booted",
        "name" : "iPhone 16 Pro"
      },
      {
        "udid" : "DEADBEEF-0000-4000-8000-000000000000",
        "isAvailable" : false,
        "availabilityError" : "runtime profile not found",
        "deviceTypeIdentifier" : "com.apple.CoreSimulator.SimDeviceType.iPhone-SE-3rd-generation",
        "state" : "Shutdown",
        "name" : "iPhone SE (3rd generation)"
      }
    ],
    "com.apple.CoreSimulator.SimRuntime.watchOS-11-2" : [
      {
        "udid" : "0F0F0F0F-1111-4222-8333-444444444444",
        "isAvailable" : true,
        "deviceTypeIdentifier" : "com.apple.CoreSimulator.SimDeviceType.Apple-Watch-Series-10-46mm",
        "state" : "Shutdown",
        "name" : "Apple Watch Series 10 (46mm)"
      }
    ],
    "com.apple.CoreSimulator.SimRuntime.iOS-16-4" : []
  }
}"#;

    /// Realistic `xcrun simctl listapps <udid>` output (OpenStep plist).
    pub const LISTAPPS: &str = r#"{
    "com.apple.Preferences" =     {
        ApplicationType = System;
        Bundle = "file:///Library/Developer/CoreSimulator/Volumes/iOS_22C150/Library/Developer/CoreSimulator/Profiles/Runtimes/iOS%2018.2.simruntime/Contents/Resources/RuntimeRoot/Applications/Preferences.app/";
        BundleContainer = "file:///Library/Developer/CoreSimulator/Volumes/iOS_22C150/Library/Developer/CoreSimulator/Profiles/Runtimes/iOS%2018.2.simruntime/Contents/Resources/RuntimeRoot/Applications/";
        CFBundleDisplayName = Settings;
        CFBundleExecutable = Preferences;
        CFBundleIdentifier = "com.apple.Preferences";
        CFBundleName = Preferences;
        CFBundleVersion = "1.0";
        GroupContainers =         {
            "243LU875E5.groups.com.apple.podcasts" = "file:///Users/me/Library/Developer/CoreSimulator/Devices/8A1BC3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5D/data/Containers/Shared/AppGroup/0C7D/";
        };
        Path = "/Library/Developer/CoreSimulator/Volumes/iOS_22C150/Library/Developer/CoreSimulator/Profiles/Runtimes/iOS 18.2.simruntime/Contents/Resources/RuntimeRoot/Applications/Preferences.app";
        SBAppTags =         (
        );
    };
    "com.example.demo" =     {
        ApplicationType = User;
        Bundle = "file:///Users/me/Library/Developer/CoreSimulator/Devices/8A1BC3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5D/data/Containers/Bundle/Application/5B2C/Demo.app/";
        CFBundleDisplayName = "Demo App";
        CFBundleExecutable = Demo;
        CFBundleIdentifier = "com.example.demo";
        CFBundleName = Demo;
        CFBundleShortVersionString = "2.3.1";
        CFBundleVersion = 417;
        DataContainer = "file:///Users/me/Library/Developer/CoreSimulator/Devices/8A1BC3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5D/data/Containers/Data/Application/77AA/";
        Path = "/Users/me/Library/Developer/CoreSimulator/Devices/8A1BC3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5D/data/Containers/Bundle/Application/5B2C/Demo.app";
        SBAppTags =         (
        );
    };
    "com.apple.mobilesafari" =     {
        ApplicationType = System;
        CFBundleDisplayName = Safari;
        CFBundleExecutable = MobileSafari;
        CFBundleIdentifier = "com.apple.mobilesafari";
        CFBundleName = Safari;
        CFBundleVersion = "8620.1.16.10.12";
    };
}
"#;

    #[test]
    fn runtime_keys_parse_into_platform_and_version() {
        let parse = |key: &str| parse_runtime_key(key);
        assert_eq!(
            parse("com.apple.CoreSimulator.SimRuntime.iOS-18-2"),
            Some(("iOS".into(), "18.2".into()))
        );
        assert_eq!(
            parse("com.apple.CoreSimulator.SimRuntime.watchOS-11-0"),
            Some(("watchOS".into(), "11.0".into()))
        );
        assert_eq!(
            parse("com.apple.CoreSimulator.SimRuntime.iOS-26-0-1"),
            Some(("iOS".into(), "26.0.1".into()))
        );
        for bad in ["", "iOS", "com.apple.CoreSimulator.SimRuntime.iOS-", "x.iOS-18-beta", "x.-18"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn device_list_keeps_available_devices_newest_runtime_first() {
        let devices = parse_devices(DEVICES_JSON).unwrap();
        let rows: Vec<(&str, &str, &str)> = devices
            .iter()
            .map(|d| (d.name.as_str(), d.runtime.as_str(), d.state.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("iPhone 16 Pro", "iOS 18.2", "Booted"),
                ("iPhone 15", "iOS 17.5", "Shutdown"),
                ("iPhone 16 Pro", "iOS 17.5", "Shutdown"),
                ("Apple Watch Series 10 (46mm)", "watchOS 11.2", "Shutdown"),
            ]
        );
        let first = &devices[0];
        assert_eq!(first.udid, UDID);
        assert_eq!(first.os, "18.2");
        assert_eq!(first.platform, "iOS");
        assert_eq!(first.model.as_deref(), Some("iPhone 16 Pro"));
        assert!(first.booted());
        assert_eq!(first.to_status_json()["runtime"], "iOS 18.2");
        assert!(parse_devices("{}").is_err());
        assert!(parse_devices("not json").is_err());
    }

    #[test]
    fn find_takes_a_udid_or_an_unambiguous_name() {
        let devices = parse_devices(DEVICES_JSON).unwrap();
        assert_eq!(find(&devices, &UDID.to_lowercase()).unwrap().udid, UDID);
        assert_eq!(find(&devices, "iphone 15").unwrap().os, "17.5");
        // Two named iPhone 16 Pro: the booted one wins.
        assert_eq!(find(&devices, "iPhone 16 Pro").unwrap().udid, UDID);
        assert!(find(&devices, "Pixel 9").unwrap_err().contains("iphone-use simulators"));
        // Watch simulators cannot run the runner.
        assert!(find(&devices, "Apple Watch Series 10 (46mm)").is_err());
        let mut both_shut = devices.clone();
        both_shut[0].state = "Shutdown".into();
        let error = find(&both_shut, "iPhone 16 Pro").unwrap_err();
        assert!(error.contains("pass the UDID"), "{error}");
        assert!(error.contains(UDID));
    }

    #[test]
    fn udids_are_uuids() {
        assert!(valid_udid(UDID));
        assert!(valid_udid(&UDID.to_lowercase()));
        assert!(!valid_udid("00008110-001234567890001E"), "a phone UDID");
        assert!(!valid_udid("8A1BC3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5"));
        assert!(!valid_udid("8A1BC3D4_5E6F-4A7B-8C9D-0E1F2A3B4C5D"));
        assert!(!valid_udid("8A1BC3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5Z"));
    }

    #[test]
    fn simctl_argv() {
        let u = UDID;
        assert_eq!(list_argv(), ["simctl", "list", "devices", "available", "-j"]);
        assert_eq!(boot_argv(u), ["simctl", "boot", u]);
        assert_eq!(bootstatus_argv(u), ["simctl", "bootstatus", u, "-b"]);
        assert_eq!(shutdown_argv(u), ["simctl", "shutdown", u]);
        assert_eq!(listapps_argv(u), ["simctl", "listapps", u]);
        assert_eq!(
            install_argv(u, Path::new("/tmp/My App.app")),
            ["simctl", "install", u, "/tmp/My App.app"]
        );
        assert_eq!(uninstall_argv(u, "com.example.demo"), ["simctl", "uninstall", u, "com.example.demo"]);
        assert_eq!(openurl_argv(u, "https://example.com/?a=1"), ["simctl", "openurl", u, "https://example.com/?a=1"]);
        assert_eq!(
            location_set_argv(u, 35.6812, 139.7671).unwrap(),
            ["simctl", "location", u, "set", "35.6812,139.7671"]
        );
        assert_eq!(
            location_set_argv(u, -33.5, -70.0).unwrap(),
            ["simctl", "location", u, "set", "-33.5,-70"]
        );
        assert!(location_set_argv(u, 91.0, 0.0).is_err());
        assert!(location_set_argv(u, 0.0, f64::NAN).is_err());
        assert_eq!(location_clear_argv(u), ["simctl", "location", u, "clear"]);
        assert_eq!(
            log_stream_argv(u, None),
            ["simctl", "spawn", u, "log", "stream", "--style", "compact"]
        );
        assert_eq!(
            log_stream_argv(u, Some("process == \"Demo\"")),
            ["simctl", "spawn", u, "log", "stream", "--style", "compact", "--predicate", "process == \"Demo\""]
        );
        assert_eq!(log_stream_argv(u, Some("  ")).len(), 7);
    }

    #[test]
    fn listapps_plist_becomes_the_apps_inventory() {
        let apps = parse_listapps(LISTAPPS).unwrap();
        let bundles: Vec<&str> = apps.iter().map(|a| a.bundle.as_str()).collect();
        assert_eq!(bundles, ["com.apple.Preferences", "com.apple.mobilesafari", "com.example.demo"]);
        let demo = &apps[2];
        assert_eq!(demo.name.as_deref(), Some("Demo App"));
        assert_eq!(demo.version.as_deref(), Some("2.3.1"));
        assert_eq!(demo.bundle_version.as_deref(), Some("417"));
        assert!(!demo.system);
        assert!(apps[0].system);
        assert_eq!(apps[0].name.as_deref(), Some("Settings"));

        let devices = parse_devices(DEVICES_JSON).unwrap();
        let body = inventory_body(UDID, Some(&devices[0]), &apps, "2026-10-11T07:00:00Z").unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["source"], "simctl");
        assert_eq!(body["udid"], UDID);
        assert_eq!(body["device"]["ios"], "18.2");
        assert_eq!(body["apps"][2]["bundle"], "com.example.demo");
        assert_eq!(body["apps"][2]["removable"], true);
        assert_eq!(body["apps"][0]["system"], true);
        // The same rows app lookup by name reads from a phone's inventory.
        let installed = crate::app_lookup::installed_from_inventory(&body);
        assert_eq!(installed.len(), 3);
        assert!(inventory_body(UDID, None, &[], "t").is_err());

        // Some Xcode versions print XML.
        let mut dict = plist::Dictionary::new();
        let mut app = plist::Dictionary::new();
        app.insert("CFBundleName".into(), plist::Value::String("X".into()));
        dict.insert("com.example.x".into(), plist::Value::Dictionary(app));
        let mut xml = Vec::new();
        plist::Value::Dictionary(dict).to_writer_xml(&mut xml).unwrap();
        let apps = parse_listapps(std::str::from_utf8(&xml).unwrap()).unwrap();
        assert_eq!(apps[0].bundle, "com.example.x");
        assert_eq!(apps[0].name.as_deref(), Some("X"));
        assert!(parse_listapps("garbage {{{").is_err());
    }

    #[test]
    fn compact_log_lines_parse_into_ring_fields() {
        assert_eq!(parse_compact("Filtering the log data using \"process == \\\"Demo\\\"\""), None);
        assert_eq!(parse_compact("Timestamp               Ty Process[PID:TID]"), None);

        let line = parse_compact(
            "2026-10-11 07:01:02.123 E  Demo[4242:1a2b3] [com.example.demo:network] request failed: -1009",
        )
        .unwrap();
        assert_eq!(line.ts.as_deref(), Some("2026-10-11 07:01:02.123"));
        assert_eq!(line.level.as_deref(), Some("error"));
        assert_eq!(line.process.as_deref(), Some("Demo"));
        assert_eq!(line.pid, Some(4242));
        assert_eq!(line.subsystem.as_deref(), Some("com.example.demo:network"));
        assert_eq!(line.message, "request failed: -1009");

        let line = parse_compact(
            "2026-10-11 07:01:02.500 Df SpringBoard[57:2f] (FrontBoard) [com.apple.FrontBoard:Process] Bootstrapping com.example.demo",
        )
        .unwrap();
        assert_eq!(line.level.as_deref(), Some("notice"));
        assert_eq!(line.subsystem.as_deref(), Some("com.apple.FrontBoard:Process"));
        assert_eq!(line.message, "Bootstrapping com.example.demo");

        let line = parse_compact("2026-10-11 07:01:03.000 Db Google Chrome Helper[88:9] plain [x] text").unwrap();
        assert_eq!(line.process.as_deref(), Some("Google Chrome Helper"));
        assert_eq!(line.level.as_deref(), Some("debug"));
        assert_eq!(line.subsystem, None);
        assert_eq!(line.message, "plain [x] text");

        let line = parse_compact("2026-10-11 07:01:04.0 F  Demo[1:a] boom").unwrap();
        assert_eq!(line.level.as_deref(), Some("fault"));
        let line = parse_compact("2026-10-11 07:01:04.0 I  Demo[1:a] fyi").unwrap();
        assert_eq!(line.level.as_deref(), Some("info"));

        // A continuation line stays whole.
        let line = parse_compact("    at frame 3").unwrap();
        assert_eq!(line.message, "    at frame 3");
        assert_eq!(line.process, None);
        // An unknown type column is not guessed at.
        assert_eq!(parse_compact("2026-10-11 07:01:04.0 ?? Demo[1:a] x").unwrap().process, None);
    }

    fn report(udid: &str, app: &str) -> String {
        format!(
            "{{\"app_name\":\"{app}\",\"timestamp\":\"2026-10-11 07:01:02.00 +0900\",\"bug_type\":\"309\",\"name\":\"{app}\"}}\n{{\n  \"uptime\" : 1200,\n  \"procPath\" : \"\\/Users\\/me\\/Library\\/Developer\\/CoreSimulator\\/Devices\\/{udid}\\/data\\/Containers\\/Bundle\\/Application\\/5B2C\\/{app}.app\\/{app}\",\n  \"exception\" : {{\"type\":\"EXC_CRASH\",\"signal\":\"SIGABRT\"}}\n}}\n"
        )
    }

    #[test]
    fn crash_reports_are_the_ones_from_this_simulator() {
        let dir = tempfile::tempdir().unwrap();
        let other = "11111111-2222-4333-8444-555555555555";
        std::fs::write(dir.path().join("Demo-2026-10-11-070102.ips"), report(UDID, "Demo")).unwrap();
        std::fs::write(dir.path().join("Other-2026-10-11-070103.ips"), report(other, "Other")).unwrap();
        std::fs::write(dir.path().join("Safari-2026-10-11-070104.ips"), "{\"app_name\":\"Safari\"}\nmacOS process").unwrap();
        std::fs::write(dir.path().join("notes.txt"), report(UDID, "Demo")).unwrap();
        std::fs::create_dir(dir.path().join("Retired")).unwrap();
        std::fs::write(
            dir.path().join("Retired/Demo-2026-10-10-090000.ips"),
            report(&UDID.to_lowercase(), "Demo"),
        )
        .unwrap();

        let all = crash_list(dir.path(), UDID, &CrashFilter::default());
        let mut names: Vec<(&str, Option<&str>)> =
            all.iter().map(|e| (e.name.as_str(), e.folder.as_deref())).collect();
        names.sort();
        assert_eq!(
            names,
            [
                ("Demo-2026-10-10-090000.ips", Some("Retired")),
                ("Demo-2026-10-11-070102.ips", None)
            ]
        );
        let entry = all.iter().find(|e| e.folder.is_none()).unwrap();
        assert_eq!(entry.process.as_deref(), Some("Demo"));
        assert_eq!(entry.stamp.as_deref(), Some("2026-10-11 07:01:02"));
        let none = crash_list(
            dir.path(),
            UDID,
            &CrashFilter { process: Some("Other".into()), ..CrashFilter::default() },
        );
        assert!(none.is_empty());

        let read = crash_read(dir.path(), UDID, "Demo-2026-10-11-070102.ips").unwrap();
        assert!(read.text.contains("EXC_CRASH"));
        assert!(!read.truncated);
        assert_eq!(read.size, read.text.len());
        // Another simulator's report, a missing one and a path are refused.
        let error = crash_read(dir.path(), UDID, "Other-2026-10-11-070103.ips").unwrap_err();
        assert!(format!("{error:#}").contains("no crash report named"));
        assert!(crash_read(dir.path(), UDID, "Gone-2026-10-11-070102.ips").is_err());
        assert!(crash_read(dir.path(), UDID, "../Demo-2026-10-11-070102.ips").is_err());
        assert!(crash_list(&dir.path().join("missing"), UDID, &CrashFilter::default()).is_empty());
    }

    #[test]
    fn simulator_states_show_as_device_states() {
        assert_eq!(device_state_for("Booting"), Some("booting"));
        assert_eq!(device_state_for("Shutdown"), Some("shutdown"));
        assert_eq!(device_state_for("Shutting Down"), Some("shutdown"));
        assert_eq!(device_state_for("Booted"), None);
        assert_eq!(device_state_for(""), None);
    }

    #[test]
    fn setup_takes_a_simulator_flag() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(setup_simulator_arg(&args(&[])), None);
        assert_eq!(setup_simulator_arg(&args(&["--wifi"])), None);
        assert_eq!(setup_simulator_arg(&args(&["--simulator", "iPhone 16 Pro"])), Some(Ok("iPhone 16 Pro".into())));
        assert_eq!(setup_simulator_arg(&args(&["--simulator=iPhone 15"])), Some(Ok("iPhone 15".into())));
        assert!(matches!(setup_simulator_arg(&args(&["--simulator"])), Some(Err(_))));
        assert!(matches!(setup_simulator_arg(&args(&["--simulator", "--json"])), Some(Err(_))));
        assert!(matches!(setup_simulator_arg(&args(&["--simulator="])), Some(Err(_))));
    }

    #[test]
    fn doctor_names_what_can_be_started() {
        let devices = parse_devices(DEVICES_JSON).unwrap();
        let lines = doctor_lines(&devices);
        assert!(lines[0].starts_with("Simulators (preview): "), "{lines:?}");
        assert!(lines[1].contains("iphone-use simulator start \""), "{lines:?}");
        assert!(doctor_lines(&[])[0].contains("none installed"));
    }

    #[test]
    fn a_recording_runs_simctl_record_video() {
        let args = record_video_argv(UDID, Path::new("/r/a.mp4"));
        assert_eq!(args, ["simctl", "io", UDID, "recordVideo", "--codec=h264", "--force", "/r/a.mp4"]);
    }

    #[test]
    fn a_simulator_recording_stops_with_sigint_and_keeps_the_file() {
        static RECORDER: std::sync::OnceLock<SimRecorder> = std::sync::OnceLock::new();
        let recorder: &'static SimRecorder = RECORDER.get_or_init(SimRecorder::default);
        let dir = std::env::temp_dir().join(format!("iu-simrec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.mp4");
        // Stands in for recordVideo: writes the file when interrupted.
        let script = "echo 'Recording started' >&2; trap 'printf movie > \"$0\"; exit 0' INT; while :; do sleep 0.05; done";
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let started = recorder
                .start_with("sh", &["-c".into(), script.into(), path.display().to_string()], path.clone(), RECORD_MAX)
                .await
                .unwrap();
            assert_eq!(started["via"], "simctl");
            let again = recorder.start_with("sh", &["-c".into(), "sleep 5".into()], dir.join("b.mp4"), RECORD_MAX).await;
            assert!(format!("{:#}", again.unwrap_err()).contains("already recording"));
            assert!(recorder.status().await.is_some());
            tokio::time::sleep(Duration::from_millis(200)).await;
            let summary = recorder.stop().await.unwrap().unwrap();
            assert_eq!(summary["bytes"], 5);
            assert!(summary.get("ended").is_none());
            assert!(recorder.stop().await.unwrap().is_none());
            // Nothing written: no file, and an error that says so.
            recorder.start_with("sh", &["-c".into(), "echo 'Recording started' >&2; trap 'exit 0' INT; while :; do sleep 0.05; done".into()], dir.join("c.mp4"), RECORD_MAX).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(format!("{:#}", recorder.stop().await.unwrap_err()).contains("no video frames"));
            // `start` returns only once the recorder says it is recording.
            let e = dir.join("e.mp4");
            let slow = "sleep 0.5; echo 'Recording started' >&2; trap 'printf movie > \"$0\"; exit 0' INT; while :; do sleep 0.05; done";
            let began = std::time::Instant::now();
            recorder.start_with("sh", &["-c".into(), slow.into(), e.display().to_string()], e.clone(), RECORD_MAX).await.unwrap();
            assert!(began.elapsed() >= Duration::from_millis(400), "{:?}", began.elapsed());
            assert_eq!(recorder.stop().await.unwrap().unwrap()["bytes"], 5);
            // The limit interrupts it by itself.
            let d = dir.join("d.mp4");
            recorder.start_with("sh", &["-c".into(), script.into(), d.display().to_string()], d.clone(), Duration::from_millis(300)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(800)).await;
            assert_eq!(recorder.status().await.unwrap()["ended"], "max_duration");
            assert_eq!(recorder.stop().await.unwrap().unwrap()["ended"], "max_duration");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_first_simulator_takes_the_shared_instance() {
        assert_eq!(default_instance_name(UDID, None), "sim");
        assert_eq!(default_instance_name(UDID, Some(UDID)), "sim");
        assert_eq!(default_instance_name(UDID, Some(&UDID.to_ascii_lowercase())), "sim");
        assert_eq!(default_instance_name(UDID, Some("11111111-2222-3333-4444-555555555555")), "sim-8a1bc3d4");
        assert!(crate::instance::valid_name(SHARED_INSTANCE));
        let record = format!(r#"{{"udid":"{UDID}","instance":"sim","daemon_port":44331}}"#);
        assert_eq!(record_udid(&record).as_deref(), Some(UDID));
        assert_eq!(record_daemon_port(&record), Some(44331));
        assert_eq!(record_daemon_port(r#"{"daemon_port":0}"#), None);
        assert_eq!(record_udid("not json"), None);
    }

    #[test]
    fn the_daemon_runs_against_the_started_runner() {
        let env = daemon_env("sim", UDID, 8200, 9200, 44331);
        let get = |key: &str| env.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("IPHONE_USE_INSTANCE"), Some("sim"));
        assert_eq!(get(ENV), Some(UDID));
        assert_eq!(get("IPHONE_USE_WDA_URL"), Some("http://127.0.0.1:8200"));
        assert_eq!(get("IPHONE_USE_WDA_MJPEG_URL"), Some("http://127.0.0.1:9200"));
        assert_eq!(get("IPHONE_USE_PORT"), Some("44331"));
        // The printed --no-serve command carries the same settings.
        let line = daemon_command("sim", UDID, 8200, 9200, 44331);
        for (key, value) in &env {
            assert!(line.contains(&format!("{key}={value}")), "{key} missing from {line}");
        }
        assert!(agent_hints("sim").iter().any(|l| l.contains("IPHONE_USE_INSTANCE=sim -- iphone-use-mcp")));
    }

    #[test]
    fn stop_only_signals_the_recorded_daemon() {
        let command = "/Users/me/.local/bin/iphone-use serve".to_string();
        let lstart = "Sun Oct 11 07:01:02 2026";
        let record = PidRecord { pid: 4321, lstart: lstart.into(), command: command.clone() };
        assert!(record.matches_daemon(501, (Some(501), lstart, &command)));
        assert!(!record.matches_daemon(501, (Some(0), lstart, &command)), "another user");
        assert!(!record.matches_daemon(501, (Some(501), "Sun Oct 11 07:09:09 2026", &command)), "a reused pid");
        assert!(!record.matches_daemon(501, (Some(501), lstart, "/bin/zsh")), "another program");
        let other = PidRecord { command: "/usr/bin/xcodebuild test".into(), ..record.clone() };
        assert!(!other.matches_daemon(501, (Some(501), lstart, &other.command)), "not a serve command");
    }

    #[test]
    fn cli_helpers() {
        assert_eq!(instance_name_for(UDID), "sim-8a1bc3d4");
        assert!(crate::instance::valid_name(&instance_name_for(UDID)));

        let xctestrun = Path::new("/Users/me/.iphone-use/instances/sim-8a1bc3d4/runner-build-simulator/Build/Products/IPhoneUseRunner_iphonesimulator18.2-arm64.xctestrun");
        assert_eq!(
            runner_launch_argv(UDID, xctestrun),
            [
                "-destination".to_string(),
                format!("id={UDID}"),
                "test-without-building".into(),
                "-xctestrun".into(),
                xctestrun.display().to_string(),
                "-only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe".into(),
            ]
        );
        let [port, mjpeg] = runner_port_env(8201, 9201);
        assert_eq!(port, ("TEST_RUNNER_IPU_RUNNER_PORT", "8201".to_string()));
        assert_eq!(mjpeg, ("TEST_RUNNER_IPU_RUNNER_MJPEG_PORT", "9201".to_string()));
        assert_eq!(build_argv(), ["--simulator"]);

        let out = "noise\n/Users/me/x/Build/Products/IPhoneUseRunner_iphonesimulator18.2-arm64.xctestrun\n\n";
        assert_eq!(
            xctestrun_from_build_output(out),
            Some(PathBuf::from("/Users/me/x/Build/Products/IPhoneUseRunner_iphonesimulator18.2-arm64.xctestrun"))
        );
        assert_eq!(xctestrun_from_build_output("/a.xctestrun\nfailed"), None);
        assert_eq!(xctestrun_from_build_output(""), None);

        assert_eq!(pick_ports(None, |_| true), Ok((8200, 9200)));
        assert_eq!(pick_ports(None, |p| p != 8200 && p != 9201), Ok((8202, 9202)));
        assert_eq!(pick_ports(Some(8105), |_| true), Ok((8105, 9105)));
        assert!(pick_ports(Some(8105), |p| p != 9105).unwrap_err().contains("in use"));
        assert!(pick_ports(Some(65000), |_| true).is_err());
        assert!(pick_ports(None, |_| false).is_err());

        assert_eq!(
            daemon_command("sim-8a1bc3d4", UDID, 8200, 9200, 44331),
            format!("IPHONE_USE_INSTANCE=sim-8a1bc3d4 IPHONE_USE_SIMULATOR={UDID} IPHONE_USE_WDA_URL=http://127.0.0.1:8200 IPHONE_USE_WDA_MJPEG_URL=http://127.0.0.1:9200 IPHONE_USE_PORT=44331 iphone-use serve")
        );
    }

    #[test]
    fn stop_only_signals_the_recorded_runner() {
        let command = format!(
            "/Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild -destination id={UDID} test-without-building -xctestrun /x/IPhoneUseRunner_iphonesimulator18.2-arm64.xctestrun -only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe"
        );
        let record = PidRecord { pid: 4321, lstart: "Sun Oct 11 07:01:02 2026".into(), command: command.clone() };
        assert_eq!(PidRecord::parse(&record.encode()), Some(record.clone()));
        for bad in ["", "1|a|b", "x|a|b", "42||cmd", "42|a|", "42|a|b\n43|a|b"] {
            assert_eq!(PidRecord::parse(bad), None, "{bad}");
        }
        let lstart = record.lstart.clone();
        assert!(record.matches(501, UDID, (Some(501), &lstart, &command)));
        assert!(!record.matches(501, UDID, (Some(0), &lstart, &command)), "another user");
        assert!(!record.matches(501, UDID, (Some(501), "Sun Oct 11 07:09:09 2026", &command)), "a reused pid");
        assert!(!record.matches(501, UDID, (Some(501), &lstart, "/bin/sleep 100")), "another program");
        let other = "11111111-2222-4333-8444-555555555555";
        assert!(!record.matches(501, other, (Some(501), &lstart, &command)), "another simulator");
        let forged = PidRecord { command: "/bin/sh -destination id=x test-without-building ".into(), ..record.clone() };
        let forged_command = forged.command.clone();
        assert!(!forged.matches(501, "x", (Some(501), &lstart, &forged_command)), "not xcodebuild");
    }

    #[test]
    fn a_simulator_never_reports_a_lock() {
        let value = lock_readiness_json();
        assert_eq!(value["verdict"], "ready");
        assert_eq!(value["applicable"], false);
        assert_eq!(value["keep_awake"]["enabled"], false);
    }

    /// Records every argv; answers from a table keyed by the simctl verb.
    #[derive(Default)]
    pub struct FakeSimctl {
        pub calls: Mutex<Vec<Vec<String>>>,
        pub fail: Mutex<Option<String>>,
        pub log_lines: Mutex<Option<String>>,
    }

    impl SimctlRunner for FakeSimctl {
        fn run(&self, args: &[String], _timeout: Duration) -> Result<CmdOutput> {
            self.calls.lock().unwrap().push(args.to_vec());
            if let Some(stderr) = self.fail.lock().unwrap().clone() {
                return Ok(CmdOutput { success: false, stdout: String::new(), stderr });
            }
            let stdout = match args.get(1).map(String::as_str) {
                Some("list") => DEVICES_JSON.to_string(),
                Some("listapps") => LISTAPPS.to_string(),
                _ => String::new(),
            };
            Ok(CmdOutput { success: true, stdout, stderr: String::new() })
        }

        fn stream(&self, args: &[String]) -> Result<LogStream> {
            self.calls.lock().unwrap().push(args.to_vec());
            let text = self.log_lines.lock().unwrap().clone().unwrap_or_default();
            Ok(LogStream {
                stream: Box::new(std::io::Cursor::new(text.into_bytes())),
                guard: Box::new(()),
            })
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn the_backend_runs_simctl_for_each_service() {
        let fake = Arc::new(FakeSimctl::default());
        *fake.log_lines.lock().unwrap() = Some(
            "Filtering the log data using \"x\"\n2026-10-11 07:01:02.123 E  Demo[42:1] [com.example:net] offline\n".into(),
        );
        let dir = tempfile::tempdir().unwrap();
        let sim = Simulator::new(&UDID.to_lowercase(), fake.clone(), dir.path().to_path_buf());
        assert_eq!(sim.udid, UDID);
        block_on(async {
            assert_eq!(sim.status_device_json()["simulator"], true);
            let device = sim.refresh_info().await.unwrap();
            assert_eq!(device.runtime, "iOS 18.2");
            assert_eq!(sim.status_device_json()["name"], "iPhone 16 Pro");
            let body = sim.installed_apps().await.unwrap();
            assert_eq!(body["apps"].as_array().unwrap().len(), 3);
            sim.install(Path::new("/tmp/Demo.app")).await.unwrap();
            sim.uninstall("com.example.demo").await.unwrap();
            sim.set_location(1.5, 2.5).await.unwrap();
            sim.clear_location().await.unwrap();
            sim.open_url("https://example.com").await.unwrap();

            let capture = crate::syslog::SyslogCapture::new(100, Duration::from_secs(60));
            sim.start_logs(&capture).unwrap();
            for _ in 0..200 {
                if !capture.read(&crate::syslog::LogFilter::default()).items.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let read = capture.read(&crate::syslog::LogFilter::default());
            assert_eq!(read.items.len(), 1);
            assert_eq!(read.items[0].process.as_deref(), Some("Demo"));
            assert_eq!(read.items[0].level.as_deref(), Some("error"));

            assert!(sim.crashes(&CrashFilter::default()).await.unwrap().is_empty());
            *fake.fail.lock().unwrap() = Some("Invalid device: x".into());
            let error = sim.uninstall("com.example.demo").await.unwrap_err();
            assert!(format!("{error:#}").contains("Invalid device"));
        });
        let calls = fake.calls.lock().unwrap();
        let verbs: Vec<&str> = calls.iter().map(|c| c[1].as_str()).collect();
        assert_eq!(
            verbs,
            ["list", "listapps", "install", "uninstall", "location", "location", "openurl", "spawn", "uninstall"]
        );
        assert!(calls.iter().all(|c| c[0] == "simctl" && c[2] == UDID || c[1] == "list"));
    }

    #[test]
    fn an_installed_runner_tree_builds_through_xcodebuild_directly() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(runner_build_source(dir.path()), None);
        let project = dir.path().join("IPhoneUseRunner/IPhoneUseRunner.xcodeproj");
        std::fs::create_dir_all(&project).unwrap();
        assert_eq!(runner_build_source(dir.path()), Some(RunnerBuild::Project(project.clone())));
        // A checkout's build.sh wins.
        std::fs::write(dir.path().join("build.sh"), "").unwrap();
        assert_eq!(
            runner_build_source(dir.path()),
            Some(RunnerBuild::Script(dir.path().join("build.sh")))
        );

        let argv = project_build_argv(&project, Path::new("/dd"));
        let at = |flag: &str| argv[argv.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(at("-destination"), "generic/platform=iOS Simulator");
        assert_eq!(at("-scheme"), "IPhoneUseRunner");
        assert_eq!(at("-derivedDataPath"), "/dd");
        assert!(argv.contains(&"CODE_SIGNING_ALLOWED=NO".to_string()));
        assert_eq!(argv.last().unwrap(), "build-for-testing");

        let products = dir.path().join("dd/Build/Products");
        std::fs::create_dir_all(&products).unwrap();
        assert_eq!(newest_xctestrun(&dir.path().join("dd")), None);
        std::fs::write(products.join("IPhoneUseRunner_iphonesimulator.xctestrun"), "").unwrap();
        std::fs::write(products.join("notes.txt"), "").unwrap();
        assert_eq!(
            newest_xctestrun(&dir.path().join("dd")),
            Some(products.join("IPhoneUseRunner_iphonesimulator.xctestrun"))
        );
    }

    #[test]
    fn start_turns_off_the_hardware_keyboard() {
        assert_eq!(
            software_keyboard_argv(),
            ["write", "com.apple.iphonesimulator", "ConnectHardwareKeyboard", "-bool", "false"]
        );
    }
}
