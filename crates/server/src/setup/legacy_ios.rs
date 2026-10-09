//! The legacy device path: iOS 15 and 16 phones, driven with a current Xcode.
//!
//! From Xcode 26 on, Xcode drives devices through CoreDevice only, which needs
//! iOS 17: an iOS 15/16 phone pairs and answers lockdownd, but `xcodebuild`
//! and `devicectl` never see it. The runner itself still works there (its
//! test bundle targets iOS 15). What changes is everything around it:
//!
//! - **Build**: the usual `build-for-testing`, for `generic/platform=iOS`
//!   instead of the phone (Xcode cannot name it).
//! - **Host app**: Xcode's own XCTRunner needs iOS 17, so the runner app gets
//!   our small host (`runner/IPhoneUseRunner/LegacyHost/main.m`, compiled
//!   here with `xcrun clang`): it loads the phone's XCTest at run time and
//!   calls `_XCTestMain`. Info.plist: `MinimumOSVersion` 15.0, no scene
//!   manifest, and a `UILaunchScreen` (without one iOS letterboxes the app and
//!   reports a 320×480 screen).
//! - **Signing**: the phone is registered and put into a development profile
//!   of our own through the App Store Connect API (`super::asc`), then the app
//!   is signed with `codesign` using the certificate Xcode signed the build
//!   with. A free Apple ID has no API key, so it cannot use this path.
//! - **Device services**: the Developer Disk Image for the phone's iOS comes
//!   from github.com/doronz88/DeveloperDiskImage (pinned commit, sha256 per
//!   file, cached under `~/.iphone-use/ddi/`; Apple's files, downloaded, never
//!   shipped). Mount, install and launch go through `iphone-use-legacy-launch`
//!   (crates/legacy-launch, on the MIT `idevice` crate), which speaks lockdown
//!   over usbmuxd or straight to `<phone-ip>:62078` with this Mac's pair record.
//! - **Wi-Fi**: on iOS 15/16 the runner lives exactly as long as the
//!   testmanagerd connection that started it, i.e. as long as the launcher
//!   process. Launched over USB it ends when the cable is pulled (iPhone 12
//!   mini, iOS 15.4.1); launched over Wi-Fi it keeps running with no cable. So
//!   setup launches over Wi-Fi whenever the phone's lockdownd answers on its
//!   LAN address, and over USB otherwise.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use super::ctx::{Ctx, RUNNER_APP_NAME};
use super::sys;

/// The lowest iOS the runner's test bundle runs on.
pub const MIN_IOS: &str = "15.0";
/// From here on the regular (CoreDevice) path applies.
pub const MAX_IOS_EXCLUSIVE: &str = "17.0";

/// The phone's iOS takes the legacy path.
pub fn applies(version: &str) -> bool {
    super::checks::valid_os_version(version)
        && !super::checks::version_lt(version, MIN_IOS)
        && super::checks::version_lt(version, MAX_IOS_EXCLUSIVE)
}

// ── pinned downloads ────────────────────────────────────────────────────────

/// doronz88/DeveloperDiskImage at this commit.
const DDI_COMMIT: &str = "6eae353ae694bda1c421d4a3eee5459ae59c99a1";
/// `(major.minor, sha256 of DeveloperDiskImage.dmg, sha256 of its .signature)`.
const DDI_PINS: &[(&str, &str, &str)] = &[
    (
        "15.0",
        "3d46ccc14a72b596a15a2981f9e60d4ed1120b8256834d4267023488b3d1a2dc",
        "4cb9b3e9181d1b45313635e737ead789686c853441db2cfd9df13ce42e53845a",
    ),
    (
        "15.1",
        "3d46ccc14a72b596a15a2981f9e60d4ed1120b8256834d4267023488b3d1a2dc",
        "4cb9b3e9181d1b45313635e737ead789686c853441db2cfd9df13ce42e53845a",
    ),
    (
        "15.2",
        "11f79b4aa38bede979b40bf98f189ea725c484291263bbb5c3851e715ddaceab",
        "ac14f5917b60360b7fd1564b0db56e12509fb1253882d46d742ae63b9be53cfb",
    ),
    (
        "15.3",
        "11f79b4aa38bede979b40bf98f189ea725c484291263bbb5c3851e715ddaceab",
        "ac14f5917b60360b7fd1564b0db56e12509fb1253882d46d742ae63b9be53cfb",
    ),
    (
        "15.4",
        "bb2ef35eb1736cbd140870ed63d69496a77548451bd4118e323f10a6ed414bed",
        "2b0a88cce909515b53cdb7ca28368f4ff6b19ccc8f4ddf4227ca8e7813453b47",
    ),
    (
        "15.5",
        "ce681dc880df4ca94d52f7baecedeff04a26e770a01f53bd93952ef4e8688eeb",
        "ab8f34b290af7a244b50d40a9a349b9c2b18b907d0e34ebf3a8ddb6a0644d3a9",
    ),
    (
        "15.6",
        "8160a98ac1f9387fde137d4c703fdf9d39b7709ce72ed16e14cc10b891c0125f",
        "faeee13769be97beb3482f802416b263013a6850acf7672c928298f3c48f7498",
    ),
    (
        "15.7",
        "60e91ebefa29c57120704517321ee24c5a010a9e428a5bff301c68bdb64ababc",
        "acb3c5809014f94c7993cfa2f732d1f9d027085d9500e4c3244b6469cfb77d6b",
    ),
    (
        "15.8",
        "ce681dc880df4ca94d52f7baecedeff04a26e770a01f53bd93952ef4e8688eeb",
        "ab8f34b290af7a244b50d40a9a349b9c2b18b907d0e34ebf3a8ddb6a0644d3a9",
    ),
    (
        "16.0",
        "429d2d05b7c9093f214b117a26571fa7d9596c5ff03e7983b70caff743f0afb1",
        "d30ae9c12874254c5cb453abb7c412c6796913e5533de6b68f1b28f435981100",
    ),
    (
        "16.1",
        "086bf1c1a278615e60dd5ce2d2ff55a21996bbdef3dc67df20e209e7dcd48558",
        "54b0ad8113231b1e11faebfdae4b347b482ed12dbe8caf155d25b17e74b1a60e",
    ),
    (
        "16.2",
        "d30f6d1274e1300910cd22c1104136006f0c55e28110b16cf20ed1ec48fad0e5",
        "247aa8dd50dea880437a1fabfda21552971f88d39384aca6f13d2c0eba7bdac0",
    ),
    (
        "16.3",
        "d30f6d1274e1300910cd22c1104136006f0c55e28110b16cf20ed1ec48fad0e5",
        "247aa8dd50dea880437a1fabfda21552971f88d39384aca6f13d2c0eba7bdac0",
    ),
    (
        "16.4",
        "6e4449ff8c751b4432934ade838382c79330b096b865eebabbc1fa42ead9c185",
        "4e82ee8aa75c5ecdd320e075b7d9e4ea2fc1915d0bb776007e8dbc5728ca5d86",
    ),
    (
        "16.5",
        "99fcf9a7300ab2504a2b9a79f210702f2f6d6eb056053fc8517b80a6a3e18a16",
        "355169a9753c23fff95f6ebc1e39a433f653a9e0e059580642d92629c1ac0071",
    ),
    (
        "16.6",
        "6e4449ff8c751b4432934ade838382c79330b096b865eebabbc1fa42ead9c185",
        "4e82ee8aa75c5ecdd320e075b7d9e4ea2fc1915d0bb776007e8dbc5728ca5d86",
    ),
    (
        "16.7",
        "6e4449ff8c751b4432934ade838382c79330b096b865eebabbc1fa42ead9c185",
        "4e82ee8aa75c5ecdd320e075b7d9e4ea2fc1915d0bb776007e8dbc5728ca5d86",
    ),
];

/// The pinned hashes for iOS `version` (by major.minor).
pub fn ddi_pin(version: &str) -> Option<(&'static str, &'static str)> {
    let want = super::checks::os_major_minor(version);
    DDI_PINS
        .iter()
        .find(|(v, _, _)| *v == want)
        .map(|(_, dmg, sig)| (*dmg, *sig))
}

fn ddi_base_url() -> String {
    std::env::var("IPHONE_USE_DDI_BASE_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| v.starts_with("https://"))
        .unwrap_or_else(|| {
            format!(
                "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/{DDI_COMMIT}/DeveloperDiskImages"
            )
        })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn download(url: &str, limit: Duration) -> Result<Vec<u8>, String> {
    sys::block_on(async {
        let client = reqwest::Client::builder()
            .timeout(limit)
            .build()
            .map_err(|e| e.to_string())?;
        let response = client
            .get(url)
            .header(reqwest::header::USER_AGENT, "iphone-use")
            .send()
            .await
            .map_err(|e| format!("download {url}: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("download {url}: HTTP {}", response.status()));
        }
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("download {url}: {e}"))
    })
}

fn private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

/// The Developer Disk Image (dmg, signature) for iOS `version`, downloaded
/// and verified on first use.
pub fn ensure_ddi(version: &str) -> Result<(PathBuf, PathBuf), String> {
    let Some((dmg_sha, sig_sha)) = ddi_pin(version) else {
        return Err(format!(
            "no Developer Disk Image is pinned for iOS {version}"
        ));
    };
    let ver = super::checks::os_major_minor(version);
    let dir = sys::home().join(".iphone-use/ddi").join(&ver);
    private_dir(&dir)?;
    let dmg = dir.join("DeveloperDiskImage.dmg");
    let sig = dir.join("DeveloperDiskImage.dmg.signature");
    for (path, want, name) in [
        (&dmg, dmg_sha, "DeveloperDiskImage.dmg"),
        (&sig, sig_sha, "DeveloperDiskImage.dmg.signature"),
    ] {
        if std::fs::read(path).is_ok_and(|bytes| sha256_hex(&bytes) == want) {
            continue;
        }
        let url = format!("{}/{ver}/{name}", ddi_base_url());
        let bytes = download(&url, Duration::from_secs(300))?;
        let got = sha256_hex(&bytes);
        if got != want {
            return Err(format!(
                "the Developer Disk Image for iOS {ver} ({name}) did not match its pinned sha256 (got {got})"
            ));
        }
        sys::write_atomic(path, &bytes, 0o600).map_err(|e| e.to_string())?;
    }
    Ok((dmg, sig))
}

// ── the launcher ────────────────────────────────────────────────────────────

/// The launcher binary's file name (crates/legacy-launch); it ships next to
/// `iphone-use` in the app bundle.
pub const LAUNCHER_NAME: &str = "iphone-use-legacy-launch";

/// The launcher: `IPHONE_USE_LEGACY_LAUNCH`, else next to this binary, else
/// next to the daemon's binary, else the installed app.
pub fn launcher(ctx: &Ctx) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("IPHONE_USE_LEGACY_LAUNCH") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe() {
        candidates.push(exe.with_file_name(LAUNCHER_NAME));
    }
    let daemon = sys::plist_program_argument(&ctx.daemon_plist, 0);
    if !daemon.is_empty() {
        candidates.push(Path::new(&daemon).with_file_name(LAUNCHER_NAME));
    }
    candidates.push(ctx.home.join("Applications/iPhoneUse.app/Contents/MacOS").join(LAUNCHER_NAME));
    candidates.push(PathBuf::from("/Applications/iPhoneUse.app/Contents/MacOS").join(LAUNCHER_NAME));
    candidates.into_iter().find(|path| {
        path.is_absolute()
            && sys::is_executable(path)
            && !path.to_string_lossy().chars().any(char::is_whitespace)
    })
}

/// The test runner app's bundle id (Xcode appends `.xctrunner`).
pub fn runner_bundle(bundle: &str) -> String {
    format!("{bundle}.xctrunner")
}

/// `--udid U [--host IP]`: over Wi-Fi when `host` is set, else USB.
fn target_args(udid: &str, host: Option<&str>) -> Vec<String> {
    let mut args = vec!["--udid".to_string(), udid.to_string()];
    if let Some(host) = host {
        args.extend(["--host".to_string(), host.to_string()]);
    }
    args
}

/// `launch` argv; also the runner's PID identity. The launcher holds the
/// testmanagerd session the runner lives on, so it stays up as long as the
/// runner does, and ending it ends the runner.
pub fn launch_argv(udid: &str, host: Option<&str>, bundle: &str) -> Vec<String> {
    let mut args = vec!["launch".to_string()];
    args.extend(target_args(udid, host));
    args.extend(["--bundle-id".to_string(), runner_bundle(bundle)]);
    args
}

/// Run the launcher to completion, bounded: (stdout + stderr, exit code).
pub fn run_launcher(launcher: &Path, args: &[String], limit: Duration) -> (String, Option<i32>) {
    let Ok(output_file) = tempfile::NamedTempFile::new() else {
        return (String::new(), None);
    };
    // One open file description for both streams, so their writes append
    // to the same offset instead of overwriting each other.
    let Ok(out) = output_file.reopen() else {
        return (String::new(), None);
    };
    let Ok(err) = out.try_clone() else {
        return (String::new(), None);
    };
    let Ok(mut child) = std::process::Command::new(launcher)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
    else {
        return (String::new(), None);
    };
    let deadline = std::time::Instant::now() + limit;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    (
        std::fs::read_to_string(output_file.path()).unwrap_or_default(),
        code,
    )
}

/// How a `mount` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    Ready,
    /// Mounted, but developer services answer `InvalidService` even after a
    /// remount: only restarting the phone helps.
    NeedsReboot,
    Failed(String),
}

/// Mount the Developer Disk Image unless one is, and prove testmanagerd
/// starts (one unmount and remount when it does not).
pub fn mount(launcher: &Path, udid: &str, host: Option<&str>, dmg: &Path, sig: &Path) -> Mount {
    let mut args = vec!["mount".to_string()];
    args.extend(target_args(udid, host));
    args.extend([
        "--image".to_string(),
        dmg.to_string_lossy().into_owned(),
        "--signature".to_string(),
        sig.to_string_lossy().into_owned(),
        "--remount".to_string(),
    ]);
    let (out, code) = run_launcher(launcher, &args, Duration::from_secs(120));
    match code {
        Some(0) => Mount::Ready,
        Some(3) => Mount::NeedsReboot,
        _ => Mount::Failed(last_error(&out)),
    }
}

/// Install (or replace) the runner app.
pub fn install(launcher: &Path, udid: &str, host: Option<&str>, app: &Path) -> Result<(), String> {
    let mut args = vec!["install".to_string()];
    args.extend(target_args(udid, host));
    args.extend(["--app".to_string(), app.to_string_lossy().into_owned()]);
    let (out, code) = run_launcher(launcher, &args, Duration::from_secs(240));
    if code == Some(0) {
        Ok(())
    } else {
        Err(last_error(&out))
    }
}

/// The launcher's last `error:` line, else its last line.
pub fn last_error(output: &str) -> String {
    output
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("error: ").map(str::to_string))
        .or_else(|| output.lines().rev().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_string()))
        .unwrap_or_else(|| "no output".to_string())
}

/// A launcher log says the phone is locked (lockdown or the test session).
pub fn log_shows_locked(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("passwordprotected") || lower.contains("devicelocked") || lower.contains("device is locked")
}

/// A launcher log says the runner app is not on the phone.
pub fn log_shows_not_installed(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("not installed") || lower.contains("notfound") || lower.contains("not found")
}

/// A launcher log says the developer services are unusable.
pub fn log_shows_invalid_service(text: &str) -> bool {
    text.contains("InvalidService")
}

// ── the phone's LAN address ─────────────────────────────────────────────────

/// The phone's IPv4 address from Bonjour: its `_apple-mobdev2._tcp`
/// instance is `<Wi-Fi MAC>@<fe80 address>`; resolving it names the host,
/// and the host resolves to IPv4. Only this phone's MAC is resolved, nothing
/// on the LAN is probed.
pub fn bonjour_ipv4(wifi_mac: &str) -> Option<String> {
    let mac = wifi_mac.trim().to_ascii_lowercase();
    if mac.len() != 17 || !mac.bytes().all(|b| b.is_ascii_hexdigit() || b == b':') {
        return None;
    }
    let browse = dns_sd(&["-B", "_apple-mobdev2._tcp", "local."], Duration::from_secs(3));
    let instance = browse.lines().find_map(|line| {
        let start = line.to_ascii_lowercase().find(&format!("{mac}@"))?;
        Some(line[start..].trim().to_string())
    })?;
    let lookup = dns_sd(&["-L", &instance, "_apple-mobdev2._tcp", "local."], Duration::from_secs(3));
    let host = lookup.lines().find_map(|line| {
        let rest = line.split("can be reached at ").nth(1)?;
        let host = rest.split(':').next()?.trim().trim_end_matches('.');
        (!host.is_empty()).then(|| host.to_string())
    })?;
    let addr = dns_sd(&["-G", "v4", &host], Duration::from_secs(3));
    addr.lines().find_map(|line| {
        line.split_whitespace()
            .find_map(|word| word.parse::<std::net::Ipv4Addr>().ok())
            .filter(|ip| !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified())
            .map(|ip| ip.to_string())
    })
}

fn dns_sd(args: &[&str], limit: Duration) -> String {
    sys::run_bounded("/usr/bin/dns-sd", args, limit).0
}

/// lockdownd answers on the phone's LAN address (Wi-Fi lockdown is on and
/// the phone is awake on this network).
pub fn lockdown_reachable(ip: &str) -> bool {
    let Ok(ip) = ip.parse::<std::net::IpAddr>() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&std::net::SocketAddr::new(ip, 62078), Duration::from_secs(2)).is_ok()
}

// ── the legacy runner app ───────────────────────────────────────────────────

/// The host source: the runner source tree's copy when there is one, else the
/// copy built into this binary.
pub fn host_source(ctx: &Ctx) -> String {
    std::fs::read_to_string(ctx.runner_src.join("IPhoneUseRunner/LegacyHost/main.m"))
        .unwrap_or_else(|_| HOST_SOURCE.to_string())
}

pub const HOST_SOURCE: &str = include_str!("../../../../runner/IPhoneUseRunner/LegacyHost/main.m");

pub fn legacy_dir(ctx: &Ctx) -> PathBuf {
    ctx.state_dir().join("legacy-runner")
}

/// The assembled app.
pub fn legacy_app(ctx: &Ctx) -> PathBuf {
    legacy_dir(ctx).join(RUNNER_APP_NAME)
}

/// SHA-1 (hex, upper case) of a certificate: what `codesign -s` takes.
pub fn sha1_hex(der: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, der);
    digest.as_ref().iter().map(|b| format!("{b:02X}")).collect()
}

/// The leaf certificate a bundle is signed with (DER).
pub fn signing_certificate(bundle: &Path) -> Option<Vec<u8>> {
    let dir = tempfile::tempdir().ok()?;
    let prefix = dir.path().join("cert");
    let ok = std::process::Command::new("/usr/bin/codesign")
        .arg("-d")
        .arg(format!("--extract-certificates={}", prefix.display()))
        .arg(bundle)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return None;
    }
    std::fs::read(dir.path().join("cert0")).ok()
}

/// What a profile (decoded with `security cms -D`) covers.
pub struct ProfileFacts {
    pub expiry: Option<u64>,
    pub has_device: bool,
    pub has_certificate: bool,
}

pub fn profile_facts(profile: &Path, udid: &str, cert_der: &[u8]) -> ProfileFacts {
    let xml = sys::stdout_of("security", &["cms", "-D", "-i", &profile.to_string_lossy()]);
    profile_facts_in(&xml, udid, cert_der, super::runner::profile_expiry(profile))
}

pub fn profile_facts_in(
    xml: &str,
    udid: &str,
    cert_der: &[u8],
    expiry: Option<u64>,
) -> ProfileFacts {
    use base64::Engine as _;
    let compact: String = xml.chars().filter(|c| !c.is_whitespace()).collect();
    let cert = base64::engine::general_purpose::STANDARD.encode(cert_der);
    ProfileFacts {
        expiry,
        has_device: compact
            .to_ascii_lowercase()
            .contains(&format!("<string>{}</string>", udid.to_ascii_lowercase())),
        has_certificate: !cert.is_empty() && compact.contains(&cert),
    }
}

/// A profile is reused while it has more than this left.
pub const PROFILE_MIN_LEFT_SECS: u64 = 30 * 86_400;

/// Entitlements the runner is signed with.
pub fn entitlements(team: &str, runner_bundle: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>application-identifier</key><string>{team}.{runner_bundle}</string>
<key>com.apple.developer.team-identifier</key><string>{team}</string>
<key>get-task-allow</key><true/>
<key>keychain-access-groups</key><array><string>{team}.{runner_bundle}</string></array>
</dict></plist>
"#
    )
}

/// `/usr/libexec/PlistBuddy -c <command> <plist>`.
fn plist_buddy(plist: &Path, command: &str) -> bool {
    std::process::Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", command])
        .arg(plist)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Turn a copy of Xcode's runner app into one iOS 15/16 launches: our host
/// as the executable, the Info.plist changes, the profile, and a signature.
pub fn assemble(
    ctx: &Ctx,
    built_app: &Path,
    profile: &[u8],
    identity_sha1: &str,
    team: &str,
    bundle: &str,
) -> Result<PathBuf, String> {
    let dir = legacy_dir(ctx);
    private_dir(&dir)?;
    let app = legacy_app(ctx);
    if app.exists() {
        std::fs::remove_dir_all(&app).map_err(|e| format!("remove {}: {e}", app.display()))?;
    }
    let copied = std::process::Command::new("/usr/bin/ditto")
        .arg(built_app)
        .arg(&app)
        .status()
        .is_ok_and(|s| s.success());
    if !copied {
        return Err(format!("could not copy {}", built_app.display()));
    }
    let _ = std::fs::remove_dir_all(app.join("PlugIns/iPhoneUse.xctest.dSYM"));
    // The host.
    let source = dir.join("ipu-host.m");
    std::fs::write(&source, host_source(ctx)).map_err(|e| e.to_string())?;
    let host = dir.join("ipu-host");
    let sdk = sys::stdout_of("xcrun", &["--sdk", "iphoneos", "--show-sdk-path"]);
    if sdk.is_empty() {
        return Err("the selected Xcode has no iOS SDK (xcrun --sdk iphoneos)".into());
    }
    let output = std::process::Command::new("xcrun")
        .args([
            "--sdk",
            "iphoneos",
            "clang",
            "-target",
            "arm64-apple-ios15.0",
            "-isysroot",
        ])
        .arg(&sdk)
        .args([
            "-fobjc-arc",
            "-O2",
            "-framework",
            "UIKit",
            "-framework",
            "Foundation",
        ])
        .args([
            "-Wl,-rpath,/Developer/Library/Frameworks",
            "-Wl,-rpath,/Developer/usr/lib",
            "-Wl,-rpath,/System/Developer/Library/Frameworks",
            "-Wl,-rpath,/System/Developer/usr/lib",
            "-Wl,-rpath,@executable_path/Frameworks",
            "-o",
        ])
        .arg(&host)
        .arg(&source)
        .output()
        .map_err(|e| format!("xcrun clang: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not compile the legacy runner host: {}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .take(5)
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    let plist = app.join("Info.plist");
    let executable = sys::stdout_of(
        "/usr/libexec/PlistBuddy",
        &["-c", "Print CFBundleExecutable", &plist.to_string_lossy()],
    );
    if executable.is_empty() || executable.contains('/') {
        return Err("the runner app has no CFBundleExecutable".into());
    }
    std::fs::copy(&host, app.join(&executable)).map_err(|e| e.to_string())?;
    let _ = plist_buddy(&plist, "Delete UIApplicationSceneManifest");
    let _ = plist_buddy(&plist, "Delete UILaunchScreen");
    let _ = plist_buddy(&plist, "Delete NSBonjourServices");
    let _ = plist_buddy(&plist, "Delete NSLocalNetworkUsageDescription");
    let edits = [
        "Set MinimumOSVersion 15.0",
        "Add UILaunchScreen dict",
        "Add NSBonjourServices array",
        "Add NSBonjourServices:0 string _iphoneuse._tcp",
        "Add NSLocalNetworkUsageDescription string iphone-use reaches this iPhone over Wi-Fi.",
    ];
    for edit in edits {
        if !plist_buddy(&plist, edit) {
            return Err(format!("could not edit the runner Info.plist ({edit})"));
        }
    }
    std::fs::write(app.join("embedded.mobileprovision"), profile).map_err(|e| e.to_string())?;
    let ents = dir.join("entitlements.plist");
    std::fs::write(&ents, entitlements(team, &runner_bundle(bundle))).map_err(|e| e.to_string())?;
    let sign = |target: &Path, with_entitlements: bool| -> Result<(), String> {
        let mut command = std::process::Command::new("/usr/bin/codesign");
        command.args(["-f", "-s", identity_sha1]);
        if with_entitlements {
            command.arg("--entitlements").arg(&ents);
        }
        let output = command.arg(target).output().map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "codesign {}: {}",
                target.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    };
    if let Ok(entries) = std::fs::read_dir(app.join("Frameworks")) {
        for entry in entries.flatten() {
            sign(&entry.path(), false)?;
        }
    }
    sign(&app.join("PlugIns/iPhoneUse.xctest"), false)?;
    sign(&app, true)?;
    Ok(app)
}

// ── the record the daemon reads ─────────────────────────────────────────────

/// State-dir record of a phone on the legacy path; the daemon and `stop`
/// read it (stop has to end the runner on the phone, idle release must not
/// strand a Wi-Fi-only phone).
pub const RECORD_FILE: &str = "legacy-ios.json";

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Record {
    pub ios: String,
    pub udid: String,
    pub bundle: String,
    /// The launcher binary (`stop` ends the runner by ending it).
    pub launcher: String,
    /// How the runner was last started: `wifi` (it survives the cable being
    /// pulled) or `usb` (it ends with the cable).
    #[serde(default)]
    pub launch_transport: String,
    /// The phone's LAN address, once the runner answered on it.
    #[serde(default)]
    pub lan_ip: Option<String>,
    /// The runner is reachable on its LAN address (iOS allowed its network access).
    #[serde(default)]
    pub wifi_ready: bool,
    /// The phone's Wi-Fi MAC: finds its LAN address again over Bonjour.
    #[serde(default)]
    pub wifi_mac: Option<String>,
    /// The relays fall back to the phone's LAN address off the cable
    /// (WDA_ALLOW_LAN=1). Without it nothing on this Mac reaches an iOS
    /// 15/16 runner off the cable: it has no CoreDevice tunnel, and usbmuxd's
    /// Wi-Fi attachment does not carry the runner's port (iPhone X, iOS 16.5).
    #[serde(default)]
    pub lan_relay: bool,
}

pub fn read_record(state_dir: &Path) -> Option<Record> {
    let text = std::fs::read_to_string(state_dir.join(RECORD_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_record(state_dir: &Path, record: &Record) {
    if let Ok(text) = serde_json::to_string(record) {
        let _ = sys::write_atomic(&state_dir.join(RECORD_FILE), text.as_bytes(), 0o600);
    }
}

pub fn clear_record(state_dir: &Path) {
    let _ = std::fs::remove_file(state_dir.join(RECORD_FILE));
}

// ── Wi-Fi authorization ─────────────────────────────────────────────────────

/// POST a JSON body to the runner (through its loopback relay).
pub fn http_post_json(
    url: &str,
    body: &serde_json::Value,
    limit: Duration,
    auth: &crate::runner_token::TokenSource,
) -> Option<(u16, Vec<u8>)> {
    sys::block_on(async {
        let client = reqwest::Client::builder().timeout(limit).build().ok()?;
        let response = crate::runner_token::send(client.post(url).json(body), auth)
            .await
            .ok()?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.ok()?;
        Some((status, bytes.to_vec()))
    })
}

/// The runner's own `/status` names its LAN address (`value.ios.ip`).
pub fn status_ip(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let ip = value.pointer("/value/ios/ip")?.as_str()?;
    let parsed: std::net::Ipv4Addr = ip.parse().ok()?;
    (!parsed.is_loopback() && !parsed.is_unspecified() && !parsed.is_link_local())
        .then(|| ip.to_string())
}

/// The button that grants network access on iOS's prompt: China's "wireless
/// data" prompt (WLAN & cellular), or the local-network prompt (OK).
pub fn grant_button(buttons: &[String]) -> Option<String> {
    let deny = |b: &str| {
        let l = b.to_lowercase();
        l.contains("关闭")
            || l.contains("不允许")
            || l == "off"
            || l.contains("don’t")
            || l.contains("don't")
            || l.contains("only")
            || l.contains("仅")
    };
    let preferred = [
        "无线局域网与蜂窝网络",
        "无线局域网与蜂窝数据",
        "WLAN & Cellular",
        "Wi-Fi & Cellular",
        "WLAN & Cellular Data",
        "Wi-Fi & Cellular Data",
        "好",
        "允许",
        "OK",
        "Allow",
    ];
    for want in preferred {
        if let Some(found) = buttons.iter().find(|b| b.eq_ignore_ascii_case(want)) {
            return Some(found.clone());
        }
    }
    buttons
        .iter()
        .find(|b| {
            let l = b.to_lowercase();
            !deny(b) && (l.contains("cellular") || l.contains("蜂窝"))
        })
        .cloned()
}

/// The prompt is iOS asking for this runner's network access.
pub fn is_network_prompt(text: &str) -> bool {
    let l = text.to_lowercase();
    l.contains("无线数据")
        || l.contains("wireless data")
        || l.contains("本地网络")
        || l.contains("local network")
        || l.contains("局域网")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ios_15_and_16_take_the_legacy_path() {
        assert!(applies("15.0"));
        assert!(applies("15.4.1"));
        assert!(applies("16.5"));
        assert!(applies("16.7.10"));
        assert!(!applies("14.8"));
        assert!(!applies("17.0"));
        assert!(!applies("27.2"));
        assert!(!applies(""));
    }

    #[test]
    fn every_supported_minor_has_a_pinned_image() {
        for v in ["15.0", "15.4.1", "15.8.3", "16.0.2", "16.5", "16.7.10"] {
            let (dmg, sig) = ddi_pin(v).unwrap_or_else(|| panic!("{v}"));
            assert_eq!(dmg.len(), 64);
            assert_eq!(sig.len(), 64);
        }
        assert!(ddi_pin("14.8").is_none());
        assert!(ddi_pin("17.0").is_none());
    }

    #[test]
    fn launch_argv_picks_wifi_with_a_host() {
        let usb = launch_argv("00008101-000409443404001E", None, "com.leeguoo.iphone-use.wda.6zpxg4kvvs");
        assert_eq!(usb, ["launch", "--udid", "00008101-000409443404001E", "--bundle-id", "com.leeguoo.iphone-use.wda.6zpxg4kvvs.xctrunner"]);
        let wifi = launch_argv("U", Some("192.168.0.59"), "b");
        assert_eq!(wifi, ["launch", "--udid", "U", "--host", "192.168.0.59", "--bundle-id", "b.xctrunner"]);
    }

    #[test]
    fn launcher_errors_and_classifiers() {
        assert_eq!(last_error("+0.1s heartbeat up\nerror: lockdown session: PasswordProtected\n"), "lockdown session: PasswordProtected");
        assert_eq!(last_error("one\ntwo\n"), "two");
        assert!(log_shows_locked("error: lockdown session: PasswordProtected"));
        assert!(log_shows_invalid_service("StartService failed: InvalidService"));
        assert!(!log_shows_locked("ok"));
    }

    #[test]
    fn a_bad_mac_never_runs_dns_sd() {
        assert_eq!(bonjour_ipv4("not-a-mac"), None);
        assert_eq!(bonjour_ipv4(""), None);
    }

    #[test]
    fn the_status_ip_is_a_lan_address_only() {
        assert_eq!(
            status_ip(br#"{"value":{"ios":{"ip":"192.168.0.149"}}}"#).as_deref(),
            Some("192.168.0.149")
        );
        assert_eq!(status_ip(br#"{"value":{"ios":{"ip":null}}}"#), None);
        assert_eq!(status_ip(br#"{"value":{"ios":{"ip":"127.0.0.1"}}}"#), None);
        assert_eq!(
            status_ip(br#"{"value":{"ios":{"ip":"169.254.3.4"}}}"#),
            None
        );
    }

    #[test]
    fn the_grant_button_allows_wifi_and_cellular() {
        let china = ["关闭", "仅限无线局域网", "无线局域网与蜂窝网络"].map(String::from);
        assert_eq!(
            grant_button(&china).as_deref(),
            Some("无线局域网与蜂窝网络")
        );
        let en = ["Off", "WLAN Only", "WLAN & Cellular"].map(String::from);
        assert_eq!(grant_button(&en).as_deref(), Some("WLAN & Cellular"));
        let local = ["Don’t Allow", "OK"].map(String::from);
        assert_eq!(grant_button(&local).as_deref(), Some("OK"));
        let zh_local = ["不允许", "好"].map(String::from);
        assert_eq!(grant_button(&zh_local).as_deref(), Some("好"));
        assert_eq!(grant_button(&["关闭".to_string()]), None);
        assert!(is_network_prompt("允许“iPhoneUse-Runner”使用无线数据？"));
        assert!(is_network_prompt(
            "“iPhoneUse-Runner” would like to find and connect to devices on your local network."
        ));
        assert!(!is_network_prompt("Software Update"));
    }

    #[test]
    fn profile_facts_read_devices_and_certificates() {
        let xml = "<key>ProvisionedDevices</key><array>\n\t<string>00008101-000409443404001E</string></array><key>DeveloperCertificates</key><array><data>\n\tAAEC\n\tAw==</data></array>";
        let facts = profile_facts_in(xml, "00008101-000409443404001e", &[0, 1, 2, 3], Some(5));
        assert!(facts.has_device);
        assert!(facts.has_certificate);
        let other = profile_facts_in(xml, "63f53bbb05918cbf4154ba9d1d1f95b28e532597", &[9], None);
        assert!(!other.has_device && !other.has_certificate);
    }

    #[test]
    fn certificate_sha1_is_upper_hex() {
        assert_eq!(sha1_hex(b"abc"), "A9993E364706816ABA3E25717850C26C9CD0D89D");
    }

    #[test]
    fn records_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_record(dir.path()).is_none());
        let record = Record {
            ios: "16.5".into(),
            udid: "63f53bbb05918cbf4154ba9d1d1f95b28e532597".into(),
            bundle: "com.example.wda".into(),
            launcher: "/x/iphone-use-legacy-launch".into(),
            launch_transport: "wifi".into(),
            wifi_mac: Some("9c:e3:3f:8d:d9:bd".into()),
            lan_relay: true,
            lan_ip: Some("192.168.0.149".into()),
            wifi_ready: true,
        };
        write_record(dir.path(), &record);
        assert_eq!(read_record(dir.path()), Some(record));
        clear_record(dir.path());
        assert!(read_record(dir.path()).is_none());
    }
}
