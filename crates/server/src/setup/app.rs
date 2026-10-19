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
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options { remote_only: false, no_install: false, no_pair: false };
    for arg in args {
        match arg.as_str() {
            "--remote-only" => options.remote_only = true,
            "--no-install" => options.no_install = true,
            "--no-pair" => options.no_pair = true,
            other => return Err(format!("unknown setup-app option: {other} (--remote-only, --no-install, --no-pair)")),
        }
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
    match install(&ctx, &options) {
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
