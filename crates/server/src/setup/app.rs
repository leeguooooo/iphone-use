//! `iphone-use setup-app`: build the iOS app for the user's own signing team
//! and install it on the phone. The app is not distributed through the App
//! Store; every user already has Xcode and a team for the device runner.
//!
//! A paid team gets the full app, whose packet-tunnel extension starts the
//! device runner on the phone itself (local control, no Mac at run time).
//! A free Apple ID cannot sign network extensions, so it gets the remote app,
//! which drives phones that a Mac serves.
//!
//! Sources come from the release asset `iphone-use-app.tar.gz` (the app
//! sources, generated Xcode projects and prebuilt iOS libraries), verified
//! against its `.sha256` and kept at `~/.iphone-use/app`. A checkout can point
//! `IPHONE_USE_APP_SRC` at its own root instead.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::ctx::{Ctx, RUNNER_APP_NAME};
use super::{embed, runner, sys, term};

const REPO: &str = "leeguooooo/iphone-use";
const ASSET: &str = "iphone-use-app.tar.gz";

struct Options {
    remote_only: bool,
    no_install: bool,
    no_pair: bool,
    /// Another phone (instance name or UDID) whose app connects to this
    /// phone's local control, with nobody tapping either phone.
    connect_to: Option<String>,
    connect_only: bool,
}

const OPTIONS_HINT: &str = "--remote-only, --no-install, --no-pair, --connect-to <instance|udid>, --connect-only";

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options { remote_only: false, no_install: false, no_pair: false, connect_to: None, connect_only: false };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--remote-only" => options.remote_only = true,
            "--no-install" => options.no_install = true,
            "--no-pair" => options.no_pair = true,
            "--connect-only" => options.connect_only = true,
            "--connect-to" => match args.next() {
                Some(value) if !value.starts_with('-') => options.connect_to = Some(value.clone()),
                _ => return Err("--connect-to needs an instance name or UDID".into()),
            },
            other => return Err(format!("unknown setup-app option: {other} ({OPTIONS_HINT})")),
        }
    }
    if options.connect_only && options.connect_to.is_none() {
        return Err("--connect-only needs --connect-to <instance|udid>".into());
    }
    Ok(options)
}

/// The app's bundle identifier for `team`, beside the runner's
/// `com.leeguoo.iphone-use.runner.<team>`: only our team can sign the bare one.
pub fn app_bundle_id(team: &str) -> String {
    format!("com.leeguoo.iphone-use.remote.{}", team.to_ascii_lowercase())
}

pub fn run(args: &[String]) -> i32 {
    let options = match parse(args) {
        Ok(options) => options,
        Err(message) => {
            term::die_line(&message);
            return 2;
        }
    };
    sys::extend_path();
    term::start_clock();
    let ctx = match Ctx::resolve() {
        Ok(ctx) => ctx,
        Err((message, code)) => {
            term::die_line(&message);
            return code;
        }
    };
    let result = if options.connect_only {
        Ok(())
    } else {
        install(&ctx, &options)
    };
    let result = result.and_then(|()| match &options.connect_to {
        Some(controller) => connect(&ctx, controller),
        None => Ok(()),
    });
    match result {
        Ok(()) => 0,
        Err(message) => {
            term::die_line(&message);
            1
        }
    }
}

fn install(ctx: &Ctx, options: &Options) -> Result<(), String> {
    if ctx.team_id.is_empty() || ctx.udid.is_empty() {
        return Err("set the phone up first: iphone-use setup (the app reuses its team and device)".into());
    }
    let runner_app = ctx.runner_products_dir.join(RUNNER_APP_NAME);
    let paid = runner::paid_profile(&runner_app);
    let full = match (options.remote_only, paid) {
        (true, _) => false,
        (false, Some(paid)) => paid,
        (false, None) => {
            return Err("no built device runner to read the team type from; run iphone-use setup first".into());
        }
    };
    let bundle = app_bundle_id(&ctx.team_id);
    if full {
        term::ok(&format!("Paid team {}: the full app, with local control", ctx.team_id));
    } else if options.remote_only {
        term::ok("Remote app (--remote-only): drives phones this Mac serves");
    } else {
        term::warn(&format!(
            "Team {} is a free Apple ID: it cannot sign the network extension local control needs. \
             Installing the remote app, which drives phones this Mac serves.",
            ctx.team_id
        ));
    }

    let root = sources()?;
    let staging = sys::home().join(".iphone-use/app-build");
    let output = staging.join(format!("PhoneUse-{}.app", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("create {}: {e}", staging.display()))?;

    term::info("Building the app with your team (a few minutes the first time)");
    let tools = root.join("apps/ios/Tools");
    let mut build = Command::new("python3");
    build.current_dir(&tools).arg("build_single_app.py").args([
        "--team",
        &ctx.team_id,
        "--bundle-id",
        &bundle,
        "--output",
        &output.to_string_lossy(),
        "--build-dir",
        &staging.join("derived").to_string_lossy(),
    ]);
    if checkout_override().is_none() {
        build.arg("--no-generate");
    }
    // QA builds (`Debug`) accept the local-control launch switches.
    if let Ok(configuration) = std::env::var("IPU_APP_CONFIGURATION") {
        build.args(["--configuration", &configuration]);
    }
    if full {
        let host = root.join("native/libipu_phone_host.a");
        let embed = embed_library(ctx)?;
        prepare_image(&root)?;
        build.args([
            "--host-lib",
            &host.to_string_lossy(),
            "--embed-lib",
            &embed.to_string_lossy(),
            "--runner-project",
            &ctx.runner_src.join("IPhoneUseRunner/IPhoneUseRunner.xcodeproj").to_string_lossy(),
            "--runner-bundle-id",
            &ctx.bundle_id,
        ]);
    } else {
        build.arg("--remote-only");
    }
    for (key, value) in [
        ("IPU_RUNNER_ASC_KEY_PATH", &ctx.asc_key_path),
        ("IPU_RUNNER_ASC_KEY_ID", &ctx.asc_key_id),
        ("IPU_RUNNER_ASC_ISSUER_ID", &ctx.asc_issuer_id),
    ] {
        if !value.is_empty() {
            build.env(key, value);
        }
    }
    if let Some(dir) = &ctx.developer_dir {
        build.env("DEVELOPER_DIR", dir);
    }
    let status = build.status().map_err(|e| format!("run python3: {e}"))?;
    if !status.success() {
        return Err("the app build failed (see the private log named above)".into());
    }
    term::ok(&format!("Built {bundle}"));
    if options.no_install {
        term::ok(&format!("Not installed (--no-install): {}", output.display()));
        return Ok(());
    }

    term::info("Installing on the phone");
    // The exit status, not the text: an error report can name the bundle too.
    let installed = Command::new("xcrun")
        .args(["devicectl", "device", "install", "app", "--device", &ctx.udid])
        .arg(&output)
        .output()
        .map_err(|e| format!("run devicectl: {e}"))?;
    if !installed.status.success() {
        return Err(format!(
            "install failed: {}",
            String::from_utf8_lossy(&installed.stderr).trim()
        ));
    }
    term::ok("Installed: Phone Use");
    let _ = std::fs::remove_dir_all(&output);

    if full && !options.no_pair {
        stage_pairing(ctx, &root, &bundle)?;
    }
    next_steps(full);
    Ok(())
}

/// IPHONE_USE_APP_SRC: a checkout to build from instead of the release asset.
fn checkout_override() -> Option<PathBuf> {
    std::env::var_os("IPHONE_USE_APP_SRC").filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The app sources: a checkout named by IPHONE_USE_APP_SRC, or this
/// release's verified asset at ~/.iphone-use/app.
fn sources() -> Result<PathBuf, String> {
    if let Some(dir) = checkout_override() {
        if !dir.join("apps/ios/Tools/build_single_app.py").is_file() {
            return Err(format!("IPHONE_USE_APP_SRC is not an iphone-use checkout: {}", dir.display()));
        }
        term::ok(&format!("App sources: {} (IPHONE_USE_APP_SRC)", dir.display()));
        return Ok(dir);
    }
    let version = env!("CARGO_PKG_VERSION");
    let dir = sys::home().join(".iphone-use/app");
    let current = std::fs::read_to_string(dir.join("VERSION")).unwrap_or_default();
    if current.trim() == version {
        term::ok(&format!("App sources: {} ({version})", dir.display()));
        return Ok(dir);
    }
    download(version, &dir)?;
    Ok(dir)
}

fn download(version: &str, dir: &Path) -> Result<(), String> {
    let url = format!("https://github.com/{REPO}/releases/download/v{version}/{ASSET}");
    term::info(&format!("Downloading the app sources for v{version}"));
    let work = sys::home().join(format!(".iphone-use/.app.new.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("create {}: {e}", work.display()))?;
    let result = fetch_and_swap(&url, &work, dir, version);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn fetch_and_swap(url: &str, work: &Path, dir: &Path, version: &str) -> Result<(), String> {
    let tarball = work.join(ASSET);
    let checksum = work.join(format!("{ASSET}.sha256"));
    for (from, to) in [(url.to_string(), &tarball), (format!("{url}.sha256"), &checksum)] {
        let ok = Command::new("curl")
            .args(["-fsSL", &from, "-o"])
            .arg(to)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return Err(format!("download failed: {from}"));
        }
    }
    let expected = std::fs::read_to_string(&checksum)
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let actual = sys::stdout_of("shasum", &["-a", "256", &tarball.to_string_lossy()])
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    if expected.len() != 64 || actual != expected {
        return Err(format!("SHA-256 mismatch for {ASSET} (expected {expected}, got {actual})"));
    }
    let listing = sys::stdout_of("tar", &["-tzf", &tarball.to_string_lossy()]);
    if let Some(entry) = listing.lines().find(|entry| !archive_entry_safe(entry)) {
        return Err(format!("{ASSET} contains an unexpected entry: {entry}"));
    }
    let long = sys::stdout_of("tar", &["-tvzf", &tarball.to_string_lossy()]);
    if long.lines().any(|line| line.starts_with('l') || line.starts_with('h')) {
        return Err(format!("{ASSET} contains a link; refusing to extract it"));
    }
    let ok = Command::new("tar")
        .args(["-xzf", &tarball.to_string_lossy(), "-C", &work.to_string_lossy()])
        .status()
        .is_ok_and(|s| s.success());
    let extracted = work.join("app");
    if !ok || !extracted.join("apps/ios/Tools/build_single_app.py").is_file() {
        return Err(format!("{ASSET} did not extract a valid app/ tree"));
    }
    std::fs::write(extracted.join("VERSION"), format!("{version}\n")).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&extracted, dir).map_err(|e| format!("install {}: {e}", dir.display()))?;
    term::ok(&format!("SHA-256 verified; app sources at {}", dir.display()));
    Ok(())
}

/// Only an `app/` tree, with no absolute or parent paths.
fn archive_entry_safe(entry: &str) -> bool {
    (entry == "app" || entry == "app/" || entry.starts_with("app/"))
        && !entry.split('/').any(|part| part == ".." || part == ".")
        && !entry.contains("//")
}

/// The on-phone daemon library, found the way runner setup finds it (the
/// release's copy beside the runner sources, or a checkout's own build).
fn embed_library(ctx: &Ctx) -> Result<PathBuf, String> {
    let inputs = embed::Inputs {
        embed: &ctx.embed,
        embed_lib: &ctx.embed_lib,
        runner_src: &ctx.runner_src,
        keepalive: false,
    };
    let (lib, line) = embed::resolve(&inputs, |repo| {
        term::info("Building the on-phone daemon library (scripts/build-ios-embed.sh)");
        let argv = embed::build_argv(repo, &sys::home());
        Command::new("/usr/bin/env").args(&argv).current_dir(repo).status().is_ok_and(|s| s.success())
    });
    match lib {
        Some(lib) => {
            term::ok(&line);
            Ok(lib.path)
        }
        None => Err(format!("local control needs the on-phone daemon library: {line}")),
    }
}

/// Copy this Mac's Apple developer image into the build (never into a release).
fn prepare_image(root: &Path) -> Result<(), String> {
    let ok = Command::new("python3")
        .arg(root.join("experiments/phone-self-host/prepare-image.py"))
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        Ok(())
    } else {
        Err("the Apple developer image is not installed yet: connect the phone to Xcode once, then retry".into())
    }
}

/// Make the first RemotePairing record over USB and hand it to the app's
/// Documents, which it imports into its Keychain the next time local control
/// opens. The record never stays on the Mac.
fn stage_pairing(ctx: &Ctx, root: &Path, bundle: &str) -> Result<(), String> {
    let tool = root.join("bin/ipu-phone-host");
    if !sys::is_executable(&tool) {
        term::warn("No pairing tool in these sources; import the pairing record in the app by hand");
        return Ok(());
    }
    term::info("Pairing the phone for local control (needs the USB cable)");
    let dir = tempfile_dir()?;
    let record = dir.join("pair-record-import.plist");
    let made = Command::new(&tool)
        .args(["--usb-pair", &ctx.udid, "--pair-out"])
        .arg(&record)
        .status()
        .is_ok_and(|s| s.success())
        && record.is_file();
    if !made {
        let _ = std::fs::remove_dir_all(&dir);
        term::warn(
            "Could not pair over USB (cable unplugged, or the phone untrusted). \
             Connect the cable, unlock the phone and run iphone-use setup-app again; \
             the app is installed either way.",
        );
        return Ok(());
    }
    // The exit status, not the text: devicectl reports a failed copy on stderr.
    let copied = Command::new("xcrun")
        .args(["devicectl", "device", "copy", "to", "--device", &ctx.udid])
        .args(["--domain-type", "appDataContainer", "--domain-identifier", bundle, "--source"])
        .arg(&record)
        .args(["--destination", "Documents/pair-record-import.plist"])
        .output();
    let _ = std::fs::remove_dir_all(&dir);
    match copied {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            return Err(format!(
                "could not hand the pairing record to the app: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
        Err(e) => return Err(format!("run devicectl: {e}")),
    }
    term::ok("Pairing record handed to the app (it imports it on first open)");
    Ok(())
}

/// Connect `controller`'s app to this phone's local control over USB: drop a
/// one-time code into this phone's app container (the on-phone daemon reads
/// it from there), read back the daemon's LAN address, and open the matching
/// `iphoneuse://pair` link in the controller's app. Neither phone is touched.
fn connect(ctx: &Ctx, controller: &str) -> Result<(), String> {
    let bundle = app_bundle_id(&ctx.team_id);
    let controller_udid = resolve_udid(controller)?;
    if controller_udid == ctx.udid {
        return Err("--connect-to names this same phone; pick the phone that should control it".into());
    }
    term::info(&format!("Connecting {controller} to this phone's local control"));
    let dir = tempfile_dir()?;
    let result = connect_in(ctx, &bundle, &controller_udid, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn connect_in(ctx: &Ctx, bundle: &str, controller_udid: &str, dir: &Path) -> Result<(), String> {
    use base64::Engine as _;
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("randomness: {e}"))?;
    let code = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs()
        + crate::pairing::CODE_TTL.as_secs();
    let drop = dir.join("connect-code-import.json");
    std::fs::write(&drop, serde_json::json!({ "code": code, "expires_at": expires_at }).to_string())
        .map_err(|e| format!("write {}: {e}", drop.display()))?;
    devicectl(&["device", "copy", "to", "--device", &ctx.udid, "--domain-type", "appDataContainer",
        "--domain-identifier", bundle, "--source", &drop.to_string_lossy(),
        "--destination", "Documents/connect-code-import.json"])
        .map_err(|e| format!("could not hand the connection code to the app: {e}"))?;

    let lan = dir.join("daemon-lan.json");
    devicectl(&["device", "copy", "from", "--device", &ctx.udid, "--domain-type", "appDataContainer",
        "--domain-identifier", bundle, "--source", "Documents/daemon-lan.json",
        "--destination", &lan.to_string_lossy()])
        .map_err(|_| "this phone's local control is not running (no daemon address yet): \
            start it in Phone Use › Settings › 本机控制, then retry with --connect-only".to_string())?;
    let urls: Vec<String> = std::fs::read_to_string(&lan)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| v["lan_urls"].as_array().map(|a| a.iter().filter_map(|u| u.as_str().map(String::from)).collect()))
        .unwrap_or_default();
    let Some(base) = urls.into_iter().find(|u| u.starts_with("http://") && !u.ends_with(".local:18787")) else {
        return Err("this phone has no Wi-Fi address: join Wi-Fi on both phones and retry with --connect-only".into());
    };

    let controller_app = installed_app(controller_udid)?;
    let encoded: String = base
        .bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect();
    let link = format!("iphoneuse://pair?u={encoded}&c={code}");
    devicectl(&["device", "process", "launch", "--device", controller_udid, "--terminate-existing",
        "--payload-url", &link, &controller_app])
        .map_err(|e| format!("could not open the connection link on the other phone: {e}"))?;
    term::ok(&format!("Opened the connection in {controller_app} on the other phone (this phone at {base})"));
    Ok(())
}

/// An instance name → its phone's UDID; a UDID passes through.
fn resolve_udid(value: &str) -> Result<String, String> {
    if value.len() >= 24 && value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Ok(value.to_string());
    }
    let previous = std::env::var_os("IPHONE_USE_INSTANCE");
    std::env::set_var("IPHONE_USE_INSTANCE", if value == "default" { "" } else { value });
    let resolved = Ctx::resolve();
    match previous {
        Some(v) => std::env::set_var("IPHONE_USE_INSTANCE", v),
        None => std::env::remove_var("IPHONE_USE_INSTANCE"),
    }
    let ctx = resolved.map_err(|(message, _)| format!("instance {value}: {message}"))?;
    if ctx.udid.is_empty() {
        return Err(format!("instance {value} has no phone set up"));
    }
    Ok(ctx.udid)
}

/// The Phone Use app on `udid`: our team's build first, else any of ours.
fn installed_app(udid: &str) -> Result<String, String> {
    let out = Command::new("xcrun")
        .args(["devicectl", "device", "info", "apps", "--device", udid])
        .output()
        .map_err(|e| format!("run devicectl: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut ids: Vec<&str> = text
        .split_whitespace()
        .filter(|w| *w == "com.leeguoo.iphone-use.remote" || w.starts_with("com.leeguoo.iphone-use.remote."))
        .filter(|w| !w.ends_with(".tunnel"))
        .collect();
    ids.sort_by_key(|id| std::cmp::Reverse(id.len()));
    ids.first()
        .map(|id| id.to_string())
        .ok_or_else(|| "the other phone has no Phone Use app: run iphone-use setup-app for it first".into())
}

fn devicectl(args: &[&str]) -> Result<(), String> {
    // The exit status, not the text: devicectl reports failures on stderr.
    let out = Command::new("xcrun")
        .arg("devicectl")
        .args(args)
        .output()
        .map_err(|e| format!("run devicectl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn tempfile_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("ipu-pair-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

fn next_steps(full: bool) {
    println!();
    println!("Next, on the phone:");
    println!("  1. Settings › General › VPN & Device Management: trust your developer certificate if asked.");
    if full {
        println!("  2. Open Phone Use › Settings › 本机控制, tap 启用本机控制 and allow the VPN configuration.");
        println!("     Local control then runs on the phone with no Mac. It needs the phone's automation");
        println!("     session: if this Mac's runner holds it, run iphone-use setup pause first. To drive");
        println!("     the phone from this Mac again, stop local control in the app, then iphone-use setup resume.");
    } else {
        println!("  2. Open Phone Use and scan the QR code on this Mac's iphone-use page to connect.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_id_follows_the_team() {
        assert_eq!(app_bundle_id("6ZPXG4KVVS"), "com.leeguoo.iphone-use.remote.6zpxg4kvvs");
    }

    #[test]
    fn archive_entries_stay_inside_app() {
        assert!(archive_entry_safe("app/"));
        assert!(archive_entry_safe("app/apps/ios/project.yml"));
        assert!(!archive_entry_safe("runner/x"));
        assert!(!archive_entry_safe("app/../etc/passwd"));
        assert!(!archive_entry_safe("/app/x"));
        assert!(!archive_entry_safe("app//x"));
        assert!(!archive_entry_safe("app/./x"));
    }

    #[test]
    fn options_reject_unknown_flags() {
        assert!(parse(&["--remote-only".into()]).unwrap().remote_only);
        assert!(parse(&["--bogus".into()]).is_err());
    }
}
