//! Opt-in native Files visibility of the generated outer XCTest runner.
//! The receiver remains separate; this exposes the whole Documents directory.

use std::path::Path;
use std::process::{Command, Stdio};

use super::{ctx::Ctx, icon, legacy_ios, runner, sys};

pub const ENV: &str = "IPU_RUNNER_FILES";
pub const MARKER: &str = ".ipu-runner-files-v1";
const KEYS: [&str; 2] = ["UIFileSharingEnabled", "LSSupportsOpeningDocumentsInPlace"];

pub fn parse(value: &str) -> Result<bool, String> {
    match value {
        "" | "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(format!("{ENV} must be 0 or 1")),
    }
}

pub fn setting(ctx: &Ctx) -> Result<bool, String> {
    let value = std::env::var(ENV).unwrap_or_else(|_| sys::plist_env(&ctx.runner_agent_plist, ENV));
    parse(&value)
}

pub fn cache_component(enabled: bool) -> &'static str {
    if enabled {
        "files:v1:on"
    } else {
        "files:v1:off"
    }
}

pub fn matches(app: &Path, enabled: bool) -> bool {
    let Some(info) = sys::read_plist(&app.join("Info.plist")) else {
        return false;
    };
    KEYS.into_iter().all(|key| match info.get(key) {
        None => !enabled,
        Some(value) => value.as_bool() == Some(enabled),
    })
}

/// Hand-patched products from the older staging helper also need a fresh build.
pub fn customized(app: &Path) -> bool {
    app.join(MARKER).exists()
        || sys::read_plist(&app.join("Info.plist")).is_some_and(|info| {
            KEYS.into_iter()
                .any(|key| info.get(key).is_some_and(|v| v.as_bool() != Some(false)))
        })
}

fn checked(command: &mut Command) -> Result<std::process::Output, String> {
    let out = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("runner Files customization: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "runner Files customization command failed ({})",
            out.status
        ));
    }
    Ok(out)
}

fn patch_info(app: &Path) -> Result<(), String> {
    for key in KEYS {
        // plutil -replace inserts missing keys and keeps unrelated metadata.
        checked(
            Command::new("/usr/bin/plutil")
                .args(["-replace", key, "-bool", "true"])
                .arg(app.join("Info.plist")),
        )?;
    }
    if !matches(app, true) {
        return Err("runner Files plist did not verify".into());
    }
    Ok(())
}

/// Modify a private copy, verify it, then replace only this instance's product.
/// A signing failure leaves the original product untouched and fails setup.
pub fn apply(ctx: &Ctx, app: &Path, enabled: bool) -> Result<(), String> {
    if !enabled {
        return if matches(app, false) {
            Ok(())
        } else {
            Err("runner Files sharing is enabled without the opt-in; rebuild the product".into())
        };
    }
    icon::owned_app(ctx, app)?;
    runner::validate_bundle(app)?;
    let certificate = legacy_ios::signing_certificate(app)
        .ok_or("could not preserve the runner signing certificate")?;
    let identity = legacy_ios::sha1_hex(&certificate);
    let work = tempfile::Builder::new()
        .prefix("runner-files.")
        .tempdir_in(&ctx.runner_products_dir)
        .map_err(|e| e.to_string())?;
    let staged = work.path().join("iPhoneUse-Runner.app");
    checked(Command::new("/usr/bin/ditto").arg(app).arg(&staged))?;
    let entitlements = checked(
        Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", "-", "--xml"])
            .arg(app),
    )?
    .stdout;
    if entitlements.is_empty() {
        return Err("runner entitlements are missing".into());
    }
    let ent = work.path().join("entitlements.plist");
    sys::write_atomic(&ent, &entitlements, 0o600).map_err(|e| e.to_string())?;
    checked(Command::new("/usr/bin/plutil").arg("-lint").arg(&ent))?;
    let original_entitlements =
        sys::read_plist(&ent).ok_or("could not read the original runner entitlements")?;
    patch_info(&staged)?;
    std::fs::write(staged.join(MARKER), b"1\n").map_err(|e| e.to_string())?;
    // Nested code is unchanged; preserve the original outer entitlements.
    checked(
        Command::new("/usr/bin/codesign")
            .args(["-f", "-s", &identity, "--entitlements"])
            .arg(&ent)
            .arg(&staged),
    )?;
    runner::validate_bundle(&staged)?;
    let signed_entitlements = checked(
        Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", "-", "--xml"])
            .arg(&staged),
    )?
    .stdout;
    let signed_ent = work.path().join("signed-entitlements.plist");
    sys::write_atomic(&signed_ent, &signed_entitlements, 0o600).map_err(|e| e.to_string())?;
    if sys::read_plist(&signed_ent).as_ref() != Some(&original_entitlements) {
        return Err("runner entitlements changed after signing".into());
    }
    if legacy_ios::signing_certificate(&staged).as_deref() != Some(certificate.as_slice()) {
        return Err("runner signing certificate changed".into());
    }
    let original = work.path().join("original.app");
    std::fs::rename(app, &original).map_err(|e| e.to_string())?;
    if let Err(error) = std::fs::rename(&staged, app) {
        if let Err(restore) = std::fs::rename(&original, app) {
            let retained = work.keep();
            return Err(format!("runner Files publish failed: {error}; restore failed: {restore}; backup retained at {}", retained.display()));
        }
        return Err(format!(
            "runner Files publish failed: {error}; original restored"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_opt_in_and_distinct_cache_keys() {
        assert_eq!(parse("1"), Ok(true));
        for value in ["", "0"] {
            assert_eq!(parse(value), Ok(false));
        }
        for value in ["true", "false", "yes", " 1", "2"] {
            assert!(parse(value).is_err());
        }
        assert_ne!(cache_component(true), cache_component(false));
    }

    #[test]
    fn outer_plist_opt_in_preserves_identity_and_icon() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("Info.plist");
        std::fs::write(&info, r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>owned.xctrunner</string><key>CFBundleIconName</key><string>AppIcon</string></dict></plist>"#).unwrap();
        assert!(matches(dir.path(), false));
        assert!(!customized(dir.path()));
        assert!(!matches(dir.path(), true));
        patch_info(dir.path()).unwrap();
        patch_info(dir.path()).unwrap();
        assert!(matches(dir.path(), true));
        assert!(
            customized(dir.path()),
            "unmarked legacy customizations require a rebuild"
        );
        assert!(!matches(dir.path(), false));
        let value = sys::read_plist(&info).unwrap();
        assert_eq!(value["CFBundleIdentifier"], "owned.xctrunner");
        assert_eq!(value["CFBundleIconName"], "AppIcon");
    }

    #[test]
    fn cache_rejects_partial_or_non_boolean_sharing_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("Info.plist");
        for body in [
            "<key>UIFileSharingEnabled</key><true/>",
            "<key>UIFileSharingEnabled</key><string>true</string><key>LSSupportsOpeningDocumentsInPlace</key><true/>",
            "<key>UIFileSharingEnabled</key><integer>1</integer><key>LSSupportsOpeningDocumentsInPlace</key><true/>",
        ] {
            std::fs::write(&info, format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>{body}</dict></plist>")).unwrap();
            assert!(!matches(dir.path(), true));
            assert!(!matches(dir.path(), false));
        }
    }
}
