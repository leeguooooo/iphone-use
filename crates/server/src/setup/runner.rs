//! The device runner product: how it is built, validated, cached and
//! launched, and what its logs say when it fails.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::checks;
use super::ctx::{Ctx, RUNNER_APP_NAME, RUNNER_SCHEME, RUNNER_TEST_ID};
use super::pid::safe_expected;
use super::sys;
use super::term::warn;

// ── xcodebuild arguments ────────────────────────────────────────────────────

/// `args` plus the App Store Connect API-key signing flags when all three
/// `WDA_ASC_*` values are set. Only the strings are validated; the key file
/// is never read or echoed.
pub fn xcodebuild_args(ctx: &Ctx, args: &[String]) -> Result<Vec<String>, String> {
    let mut out = args.to_vec();
    if !ctx.asc_signing_enabled() {
        return Ok(out);
    }
    let path = &ctx.asc_key_path;
    let path_ok = path.starts_with('/')
        && path.ends_with(".p8")
        && path.len() > 4
        && !path.chars().any(|c| c == '|' || c.is_control());
    let id_ok =
        !ctx.asc_key_id.is_empty() && ctx.asc_key_id.bytes().all(|b| b.is_ascii_alphanumeric());
    let issuer_ok = !ctx.asc_issuer_id.is_empty()
        && ctx
            .asc_issuer_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !path_ok
        || !id_ok
        || !issuer_ok
        || !safe_expected(&format!("{path}{}{}", ctx.asc_key_id, ctx.asc_issuer_id))
    {
        return Err(
            "Invalid WDA_ASC_* configuration: use an absolute .p8 path and valid key/issuer IDs."
                .into(),
        );
    }
    if !out.iter().any(|a| a == "-allowProvisioningUpdates") {
        out.push("-allowProvisioningUpdates".into());
    }
    out.extend([
        "-authenticationKeyPath".to_string(),
        path.clone(),
        "-authenticationKeyID".into(),
        ctx.asc_key_id.clone(),
        "-authenticationKeyIssuerID".into(),
        ctx.asc_issuer_id.clone(),
        "-allowProvisioningDeviceRegistration".into(),
    ]);
    Ok(out)
}

/// The launch argv; also the runner's PID identity (see pid.rs).
pub fn runner_argv(ctx: &Ctx, udid: &str, xctestrun: &Path) -> Result<Vec<String>, String> {
    xcodebuild_args(
        ctx,
        &[
            "-destination".into(),
            format!("platform=iOS,id={udid}"),
            "test-without-building".into(),
            "-xctestrun".into(),
            xctestrun.to_string_lossy().into_owned(),
            format!("-only-testing:{RUNNER_TEST_ID}"),
        ],
    )
}

/// Every build-time xcodebuild of the runner project.
pub fn project_argv(ctx: &Ctx, extra: &[String]) -> Result<Vec<String>, String> {
    let mut args = vec![
        "-project".to_string(),
        ctx.runner_project.to_string_lossy().into_owned(),
        "-scheme".into(),
        RUNNER_SCHEME.into(),
        "-derivedDataPath".into(),
        ctx.runner_derived_data.to_string_lossy().into_owned(),
    ];
    args.extend_from_slice(extra);
    xcodebuild_args(ctx, &args)
}

// ── runner log classifiers ──────────────────────────────────────────────────

fn read_log(path: &Path) -> String {
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

fn any_line(text: &str, mut matches: impl FnMut(&str) -> bool) -> bool {
    text.lines().any(|line| matches(line))
}

fn ordered(line: &str, first: &str, second: &str) -> bool {
    line.find(first)
        .is_some_and(|i| line[i + first.len()..].contains(second))
}

/// The lock screen blocked the runner (`Unlock iPhone to Continue`, …).
pub fn log_shows_lock(path: &Path) -> bool {
    any_line(&read_log(path).to_lowercase(), |line| {
        line.contains("unlock iphone to continue")
            || line.contains("device is locked")
            || ordered(line, "deviceprep", "code=-3")
            || ordered(line, "code=-3", "deviceprep")
    })
}

/// testmanagerd refused the IDE channel (exit code 74, #126).
pub fn log_shows_ide_refusal(path: &Path) -> bool {
    any_line(&read_log(path), |line| {
        line.contains("XCTestManager_IDEInterface")
            || line.contains("before establishing connection")
            || line
                .match_indices("with code 74")
                .any(|(i, m)| !line[i + m.len()..].starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// Seconds between the runner's last `Running tests...` and testmanagerd's
/// refusal of the IDE channel. About 30 s means the session sat waiting for
/// an authorization that never came; an instant refusal is the phone saying
/// no. `None` when either line or its timestamp is missing.
pub fn ide_refusal_wait_secs(path: &Path) -> Option<u64> {
    refusal_wait_in(&read_log(path))
}

fn refusal_wait_in(text: &str) -> Option<u64> {
    let mut started = None;
    for line in text.lines() {
        if line.contains("Running tests...") {
            started = log_line_time(line);
        } else if line.contains("XCTestManager_IDEInterface") {
            let refused = log_line_time(line)?;
            return started.and_then(|start: i64| u64::try_from(refused - start).ok());
        }
    }
    None
}

/// `2026-10-07 16:35:25.719450+0900 …` → seconds since the epoch.
fn log_line_time(line: &str) -> Option<i64> {
    let stamp = line.get(..19)?;
    let (date, time) = stamp.split_once(' ')?;
    let mut ymd = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let mut hms = time.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (hms.next()??, hms.next()??, hms.next()??);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let rest = &line[19..];
    let rest = rest.strip_prefix('.').map_or(rest, |frac| {
        frac.trim_start_matches(|c: char| c.is_ascii_digit())
    });
    let offset = match rest.get(..5) {
        Some(zone) if zone.starts_with('+') || zone.starts_with('-') => {
            let hours = zone.get(1..3)?.parse::<i64>().ok()?;
            let minutes = zone.get(3..5)?.parse::<i64>().ok()?;
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            sign * (hours * 3600 + minutes * 60)
        }
        _ => 0,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// xcodebuild could not see the phone at all: unplugged, out of range on
/// Wi-Fi, or CoreDevice lost it (#166). Only this exact timeout — "Unable to
/// find a destination" also fires for a missing iOS platform in Xcode, which
/// no cable fixes.
pub fn log_shows_device_unavailable(path: &Path) -> bool {
    read_log(path).to_lowercase().contains(
        "timed out waiting for all destinations matching the provided destination specifier to become available",
    )
}

pub fn log_shows_automation_disabled(path: &Path) -> bool {
    let text = read_log(path).to_lowercase();
    text.contains("timed out while enabling automation mode")
        || text.contains("failed to initialize for ui testing")
}

/// A failure of the product itself (install/launch/signing), the only kind
/// that evicts a recorded product.
pub fn log_shows_product_failure(path: &Path) -> bool {
    let text = read_log(path).to_lowercase();
    [
        "0xe8008001",
        "failed to install",
        "miinstaller",
        "could not launch",
        "not launchable",
        "code signature",
        "invalid signature",
        "provisioning profile",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

pub fn log_shows_no_accounts(path: &Path) -> bool {
    read_log(path).contains("No Accounts:")
}

pub fn log_shows_profile_failure(path: &Path) -> bool {
    any_line(&read_log(path), |line| {
        line.contains("requires a provisioning profile")
            || line
                .find("No profiles for ")
                .is_some_and(|i| line[i + "No profiles for".len()..].contains(" were found"))
    })
}

pub fn log_shows_link_dropped(path: &Path) -> bool {
    let text = read_log(path);
    text.contains("Failed to establish communication with the test runner")
        || text.contains("A connection to this device could not be established")
}

pub fn log_shows_untrusted(path: &Path) -> bool {
    read_log(path).contains("not trusted")
}

/// The URL the runner printed as `ServerURLHere->http…<-ServerURLHere`.
pub fn server_url(path: &Path) -> Option<String> {
    read_log(path).lines().find_map(|line| {
        let start = line.find("ServerURLHere->")? + "ServerURLHere->".len();
        let rest = &line[start..];
        let end = rest.find("<-ServerURLHere")?;
        let url = &rest[..end];
        (url.starts_with("http") && !url.contains('<')).then(|| url.to_string())
    })
}

// ── product validation ──────────────────────────────────────────────────────

/// The structural checks before codesign: no interrupted-signing leftovers,
/// a test bundle, and every bundle with a valid Info.plist and executable.
pub fn validate_bundle_structure(app: &Path) -> Result<(), String> {
    let meta =
        std::fs::symlink_metadata(app).map_err(|_| "runner is missing or symlinked".to_string())?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err("runner is missing or symlinked".into());
    }
    let mut bundles = vec![app.to_path_buf()];
    let mut stack = vec![app.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = path
                .strip_prefix(app)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if name.ends_with(".cstemp") {
                return Err(format!(
                    "signing temporary file found (possible interrupted signing): {relative}"
                ));
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() && !meta.file_type().is_symlink() {
                if name.ends_with(".framework") || name.ends_with(".xctest") {
                    bundles.push(path.clone());
                }
                stack.push(path);
            }
        }
    }
    let has_tests = std::fs::read_dir(app.join("PlugIns"))
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".xctest"))
        })
        .unwrap_or(false);
    if !has_tests {
        return Err("runner contains no PlugIns/*.xctest bundle".into());
    }
    for bundle in bundles {
        let relative = match bundle.strip_prefix(app) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => bundle.to_string_lossy().into_owned(),
        };
        let info = bundle.join("Info.plist");
        if !info.is_file() {
            return Err(format!("{relative}/Info.plist is missing"));
        }
        let Some(plist) = sys::read_plist(&info) else {
            return Err(format!("{relative}/Info.plist is invalid"));
        };
        let executable = plist
            .get("CFBundleExecutable")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if executable.is_empty()
            || executable.contains('/')
            || executable == "."
            || executable == ".."
        {
            return Err(format!("{relative}: invalid CFBundleExecutable"));
        }
        if !sys::is_executable(&bundle.join(executable)) {
            return Err(format!(
                "{relative}/{executable} is missing or not executable"
            ));
        }
    }
    Ok(())
}

pub fn validate_bundle(app: &Path) -> Result<(), String> {
    validate_bundle_structure(app)?;
    let out = std::process::Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("runner signature verification failed: {error}"))?;
    if !out.status.success() {
        let detail = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return Err(format!(
            "runner signature verification failed: {}",
            detail.trim_end()
        ));
    }
    Ok(())
}

/// The `.xctestrun` next to the products directory. xcodebuild names it
/// after the SDK and never deletes an older Xcode's, so with several the one
/// for `sdk` (else the selected SDK) wins; anything ambiguous fails.
pub fn resolve_xctestrun(products: &Path, sdk: Option<&str>) -> Option<PathBuf> {
    let parent = products.parent()?;
    if !parent.is_dir() {
        return None;
    }
    let all: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(".xctestrun"))
        })
        .collect();
    let found = match all.len() {
        0 => return None,
        1 => all[0].clone(),
        _ => {
            let sdk = sdk.map(str::to_string).or_else(checks::ios_sdk_version)?;
            if !checks::valid_os_version(&sdk) {
                return None;
            }
            let marker = format!("_iphoneos{sdk}-");
            let matching: Vec<&PathBuf> = all
                .iter()
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().contains(&marker))
                })
                .collect();
            if matching.len() != 1 {
                return None;
            }
            matching[0].clone()
        }
    };
    // The path rides in the space-delimited PID identity.
    (!found.to_string_lossy().chars().any(char::is_whitespace)).then_some(found)
}

// ── product cache ───────────────────────────────────────────────────────────

/// Everything that changes the product: sources, signing identity, target,
/// and the Xcode / SDK / deployment target it was built with.
#[allow(clippy::too_many_arguments)]
pub fn cache_key(
    ctx: &Ctx,
    source_hash: &str,
    bundle: &str,
    team: &str,
    udid: &str,
    xcode_version: &str,
    sdk: &str,
    deployment_override: &str,
) -> String {
    let signer = if ctx.asc_signing_enabled() {
        format!("asc:{}", ctx.asc_key_id)
    } else {
        "account".to_string()
    };
    format!("v3|{source_hash}|{bundle}|{team}|{signer}|{udid}|{xcode_version}|{sdk}|{deployment_override}")
}

pub fn cache_drop(ctx: &Ctx) -> bool {
    match std::fs::symlink_metadata(&ctx.runner_cache) {
        Ok(meta) if meta.file_type().is_symlink() => {
            warn(&format!(
                "refusing to remove a symlinked runner cache record: {}",
                ctx.runner_cache.display()
            ));
            false
        }
        Ok(_) => std::fs::remove_file(&ctx.runner_cache).is_ok(),
        Err(_) => true,
    }
}

pub fn cache_write(ctx: &Ctx, key: &str, products: &Path, xctestrun: &Path) -> bool {
    if std::fs::symlink_metadata(&ctx.runner_cache).is_ok_and(|m| m.file_type().is_symlink()) {
        warn(&format!(
            "refusing to write a symlinked runner cache record: {}",
            ctx.runner_cache.display()
        ));
        return false;
    }
    let record = json!({
        "schema_version": 1,
        "key": key,
        "products_dir": products.to_string_lossy(),
        "xctestrun": xctestrun.to_string_lossy(),
        "recorded_at": super::retry::now(),
    });
    sys::write_atomic(&ctx.runner_cache, record.to_string().as_bytes(), 0o600).is_ok()
}

/// The recorded product, when its key still matches and it still validates.
pub fn cache_read(ctx: &Ctx, key: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let meta = std::fs::symlink_metadata(&ctx.runner_cache).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&ctx.runner_cache).ok()?).ok()?;
    let (products, xctestrun) = cache_record_paths(&record, key, &ctx.runner_products_dir)?;
    let xctestrun_meta = std::fs::symlink_metadata(&xctestrun).ok()?;
    if !products.is_dir() || xctestrun_meta.file_type().is_symlink() || !xctestrun_meta.is_file() {
        return None;
    }
    let app = products.join(RUNNER_APP_NAME);
    let app_meta = std::fs::symlink_metadata(&app).ok()?;
    if app_meta.file_type().is_symlink() || !app_meta.is_dir() {
        return None;
    }
    validate_bundle(&app).ok()?;
    Some((products, app, xctestrun))
}

/// The paths a product-cache record names, when it is a v1 record for `key`
/// that points at this instance's own products directory and the
/// `.xctestrun` beside it. Nothing on disk is consulted here.
pub fn cache_record_paths(
    record: &serde_json::Value,
    key: &str,
    expected_products: &Path,
) -> Option<(PathBuf, PathBuf)> {
    if record.get("schema_version").and_then(|v| v.as_i64()) != Some(1)
        || record.get("key").and_then(|v| v.as_str()) != Some(key)
    {
        return None;
    }
    let products = record.get("products_dir")?.as_str()?;
    let xctestrun = record.get("xctestrun")?.as_str()?;
    let expected = expected_products.to_string_lossy();
    if products.trim_end_matches('/') != expected.trim_end_matches('/') {
        return None;
    }
    if !products.contains("/Build/Products/") || !xctestrun.ends_with(".xctestrun") {
        return None;
    }
    let parent = Path::new(products.trim_end_matches('/')).parent()?;
    if Path::new(xctestrun).parent()? != parent {
        return None;
    }
    Some((PathBuf::from(products), PathBuf::from(xctestrun)))
}

/// The generated xcconfig: an inherited one (kept in effect) first, then the
/// raised deployment target.
pub fn xcconfig_text(target: &str, inherited: Option<&str>) -> String {
    let mut text =
        String::from("// Generated by setup-wda.sh on every run; edits are overwritten.\n");
    text.push_str("// The selected Xcode rejects the runner project's iOS deployment target.\n");
    if let Some(inherited) = inherited {
        text.push_str(&format!("#include? \"{inherited}\"\n"));
    }
    text.push_str(&format!("IPHONEOS_DEPLOYMENT_TARGET = {target}\n"));
    text
}

// ── Xcode compatibility xcconfig ────────────────────────────────────────────

/// Write (or retire) the xcconfig that raises the runner's deployment target
/// when the selected Xcode rejects the project's own. Returns the override
/// (empty when none) and the XCODE_XCCONFIG_FILE every xcodebuild of this run
/// must see (`None` = unset it).
pub fn prepare_xcconfig(ctx: &Ctx) -> Result<(String, Option<PathBuf>), String> {
    let file = &ctx.xcconfig_file;
    if std::fs::symlink_metadata(file).is_ok_and(|m| m.file_type().is_symlink()) {
        warn(&format!(
            "refusing to use a symlinked xcconfig: {}",
            file.display()
        ));
        return Err(String::new());
    }
    let inherited_env = std::env::var("XCODE_XCCONFIG_FILE")
        .ok()
        .filter(|v| !v.is_empty());
    let Some(target) = checks::required_deployment_target(&ctx.runner_project) else {
        let _ = std::fs::remove_file(file);
        // Keep a caller's own xcconfig; drop only ours.
        let keep = inherited_env
            .filter(|v| Path::new(v) != file.as_path())
            .map(PathBuf::from);
        return Ok((String::new(), keep));
    };
    // Keep an xcconfig the caller already exported in effect; ours is applied after it.
    let mut inherited = None;
    if let Some(value) = &inherited_env {
        if Path::new(value) != file.as_path() {
            if value.contains('"') || value.contains('\n') {
                warn("ignoring inherited XCODE_XCCONFIG_FILE with an unsupported path");
            } else if value.starts_with('/') && Path::new(value).is_file() {
                inherited = Some(value.clone());
            }
        }
    }
    let text = xcconfig_text(&target, inherited.as_deref());
    sys::write_atomic(file, text.as_bytes(), 0o600).map_err(|e| e.to_string())?;
    Ok((target, Some(file.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusal_wait_is_measured_from_the_last_running_tests_line() {
        // Hardware (guouli over Wi-Fi): 30 s from "Running tests" to the refusal.
        let text = "\
2026-10-07 16:35:25.719450+0900 iPhoneUse-Runner[3279:926495] [Default] Running tests...
2026-10-07 16:35:55.771658+0900 iPhoneUse-Runner[3279:926528] [DTXConnection] Connection peer refused channel request for \"dtxproxy:XCTestDriverInterface:XCTestManager_IDEInterface\"; channel canceled
";
        assert_eq!(refusal_wait_in(text), Some(30));
        // An instant refusal, across midnight and a timezone offset.
        let instant = "\
2026-10-07 23:59:59.900000-0700 r[1:2] [Default] Running tests...
2026-10-08 00:00:00.100000-0700 r[1:3] refused channel request for \"dtxproxy:XCTestDriverInterface:XCTestManager_IDEInterface\"
";
        assert_eq!(refusal_wait_in(instant), Some(1));
        assert_eq!(refusal_wait_in("Testing failed: exited with code 74"), None);
        assert_eq!(
            refusal_wait_in(
                "garbage Running tests...\n2026-10-07 16:35:55 refused XCTestManager_IDEInterface"
            ),
            None
        );
    }

    fn log(text: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), text).unwrap();
        file
    }

    #[test]
    fn classifiers_follow_the_shell_patterns() {
        assert!(log_shows_lock(log("x Unlock iPhone to Continue").path()));
        assert!(log_shows_lock(log("DevicePrep failed Code=-3 now").path()));
        assert!(
            !log_shows_lock(log("Code=-3\ndeviceprep").path()),
            "the two must share a line"
        );
        assert!(log_shows_ide_refusal(
            log("exited with code 74 before").path()
        ));
        assert!(log_shows_ide_refusal(log("exited with code 74").path()));
        assert!(!log_shows_ide_refusal(log("exited with code 745").path()));
        assert!(log_shows_ide_refusal(
            log("dtxproxy:XCTestDriverInterface:XCTestManager_IDEInterface").path()
        ));
        assert!(log_shows_automation_disabled(
            log("(Underlying Error: Timed out while enabling automation mode.)").path()
        ));
        assert!(log_shows_device_unavailable(
            log("xcodebuild: error: Timed out waiting for all destinations matching the provided destination specifier to become available\n\t\t{ platform:iOS, id:00008150-000A60EC1A02401C, error:Browsing on the local area network for iPhone }").path()
        ));
        assert!(
            !log_shows_device_unavailable(
                log("xcodebuild: error: Unable to find a destination matching the provided destination specifier: { generic:1, platform:iOS }").path()
            ),
            "a missing iOS platform is not an unreachable phone"
        );
        assert!(log_shows_product_failure(log("Error 0xE8008001").path()));
        assert!(!log_shows_product_failure(
            log("Failed to establish communication with the test runner").path()
        ));
        assert!(log_shows_profile_failure(
            log("error: No profiles for 'com.x' were found").path()
        ));
        assert!(log_shows_profile_failure(
            log("Signing for \"X\" requires a provisioning profile.").path()
        ));
        assert!(!log_shows_profile_failure(log("No profiles here").path()));
        assert!(log_shows_no_accounts(
            log("No Accounts: Add a new account").path()
        ));
        assert_eq!(
            server_url(log("t ServerURLHere->http://192.168.0.51:8100<-ServerURLHere\n").path())
                .as_deref(),
            Some("http://192.168.0.51:8100")
        );
        assert_eq!(server_url(log("nothing").path()), None);
    }

    #[test]
    fn asc_flags_are_appended_in_order() {
        let mut ctx = crate::setup::ctx::tests_support::ctx();
        let base = vec!["-destination".to_string(), "platform=iOS,id=AB".to_string()];
        assert_eq!(xcodebuild_args(&ctx, &base).unwrap(), base);
        ctx.asc_key_path = "/k/My Key.p8".into();
        ctx.asc_key_id = "KEY1".into();
        ctx.asc_issuer_id = "iss-1".into();
        let args = xcodebuild_args(&ctx, &base).unwrap();
        assert_eq!(
            args[2..],
            [
                "-allowProvisioningUpdates",
                "-authenticationKeyPath",
                "/k/My Key.p8",
                "-authenticationKeyID",
                "KEY1",
                "-authenticationKeyIssuerID",
                "iss-1",
                "-allowProvisioningDeviceRegistration"
            ]
        );
        ctx.asc_key_path = "relative.p8".into();
        assert!(xcodebuild_args(&ctx, &base).is_err());
    }

    #[test]
    fn cache_records_must_point_at_this_instances_products() {
        let products = Path::new("/s/runner-build/Build/Products/Debug-iphoneos");
        let good = json!({
            "schema_version": 1, "key": "k",
            "products_dir": "/s/runner-build/Build/Products/Debug-iphoneos",
            "xctestrun": "/s/runner-build/Build/Products/IPhoneUseRunner_iphoneos27.0-arm64.xctestrun",
        });
        assert!(cache_record_paths(&good, "k", products).is_some());
        assert!(
            cache_record_paths(&good, "other-key", products).is_none(),
            "a changed key rebuilds"
        );
        let mut bad = good.clone();
        bad["schema_version"] = json!(2);
        assert!(cache_record_paths(&bad, "k", products).is_none());
        let mut elsewhere = good.clone();
        elsewhere["products_dir"] = json!("/other/Build/Products/Debug-iphoneos");
        assert!(
            cache_record_paths(&elsewhere, "k", products).is_none(),
            "another instance's products are never reused"
        );
        let mut stray = good.clone();
        stray["xctestrun"] = json!("/s/runner-build/Build/IPhoneUseRunner.xctestrun");
        assert!(
            cache_record_paths(&stray, "k", products).is_none(),
            "the .xctestrun must sit beside the products"
        );
        let mut not_run = good;
        not_run["xctestrun"] = json!("/s/runner-build/Build/Products/x.plist");
        assert!(cache_record_paths(&not_run, "k", products).is_none());
    }

    #[test]
    fn the_xcconfig_keeps_an_inherited_one_first() {
        let text = xcconfig_text("15.0", Some("/u/mine.xcconfig"));
        let include = text.find("#include? \"/u/mine.xcconfig\"").unwrap();
        let target = text.find("IPHONEOS_DEPLOYMENT_TARGET = 15.0").unwrap();
        assert!(include < target);
        assert!(!xcconfig_text("15.0", None).contains("#include"));
    }

    #[test]
    fn xctestrun_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let products = dir.path().join("Build/Products/Debug-iphoneos");
        std::fs::create_dir_all(&products).unwrap();
        let parent = products.parent().unwrap();
        assert_eq!(resolve_xctestrun(&products, Some("27.0")), None);
        std::fs::write(
            parent.join("IPhoneUseRunner_iphoneos26.4-arm64.xctestrun"),
            "",
        )
        .unwrap();
        assert!(
            resolve_xctestrun(&products, Some("27.0")).is_some(),
            "one file is used as-is"
        );
        std::fs::write(
            parent.join("IPhoneUseRunner_iphoneos27.0-arm64.xctestrun"),
            "",
        )
        .unwrap();
        assert_eq!(
            resolve_xctestrun(&products, Some("27.0")).unwrap(),
            parent.join("IPhoneUseRunner_iphoneos27.0-arm64.xctestrun")
        );
        assert_eq!(
            resolve_xctestrun(&products, Some("28.0")),
            None,
            "ambiguous"
        );
    }

    #[test]
    fn bundle_structure() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("iPhoneUse-Runner.app");
        std::fs::create_dir_all(&app).unwrap();
        assert_eq!(
            validate_bundle_structure(&app).unwrap_err(),
            "runner contains no PlugIns/*.xctest bundle"
        );
        let test = app.join("PlugIns/IPhoneUseRunnerUITests.xctest");
        std::fs::create_dir_all(&test).unwrap();
        assert_eq!(
            validate_bundle_structure(&app).unwrap_err(),
            "./Info.plist is missing"
        );
        let plist = |exe: &str| {
            format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleExecutable</key><string>{exe}</string></dict></plist>")
        };
        std::fs::write(app.join("Info.plist"), plist("Runner")).unwrap();
        std::fs::write(test.join("Info.plist"), plist("Tests")).unwrap();
        assert_eq!(
            validate_bundle_structure(&app).unwrap_err(),
            "./Runner is missing or not executable"
        );
        for exe in [app.join("Runner"), test.join("Tests")] {
            std::fs::write(&exe, "").unwrap();
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        validate_bundle_structure(&app).unwrap();
        std::fs::write(app.join("PlugIns/x.cstemp"), "").unwrap();
        assert!(validate_bundle_structure(&app)
            .unwrap_err()
            .starts_with("signing temporary file found"));
    }
}
