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
    let original_entitlements = plist::Value::from_file(&ent)
        .map_err(|_| "could not read the original runner entitlements")?;
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
    if plist::Value::from_file(&signed_ent).as_ref().ok() != Some(&original_entitlements) {
        return Err("runner entitlements changed after signing".into());
    }
    if legacy_ios::signing_certificate(&staged).as_deref() != Some(certificate.as_slice()) {
        return Err("runner signing certificate changed".into());
    }
    // Both paths remain present; a process kill cannot land between two renames.
    // Unsupported filesystems fail without falling back to a non-atomic swap.
    exchange_product(app, &staged)
        .map_err(|error| format!("runner Files atomic publish failed: {error}"))?;
    Ok(())
}

/// Exchange verified and installed bundles in one filesystem operation.
#[cfg(target_os = "macos")]
fn exchange_product(installed: &Path, staged: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let installed = CString::new(installed.as_os_str().as_bytes())?;
    let staged = CString::new(staged.as_os_str().as_bytes())?;
    // SAFETY: Both C strings live across the call; the kernel owns path lookup.
    if unsafe { libc::renamex_np(installed.as_ptr(), staged.as_ptr(), libc::RENAME_SWAP) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn exchange_product(_installed: &Path, _staged: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic Runner product exchange requires macOS",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires the owned GitHub-hosted temporary signing fixture"]
    fn signed_product_transaction_preserves_bundle_and_fails_without_private_key() {
        use std::os::unix::fs::PermissionsExt;
        fn snapshot(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, (Vec<u8>, u32)> {
            let mut result = std::collections::BTreeMap::new();
            let mut todo = vec![root.to_path_buf()];
            while let Some(dir) = todo.pop() {
                for entry in std::fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    let metadata = std::fs::symlink_metadata(&path).unwrap();
                    assert!(!metadata.file_type().is_symlink());
                    if metadata.is_dir() {
                        todo.push(path);
                    } else {
                        result.insert(
                            path.strip_prefix(root).unwrap().to_path_buf(),
                            (std::fs::read(&path).unwrap(), metadata.permissions().mode()),
                        );
                    }
                }
            }
            result
        }
        fn entitlements(app: &Path) -> plist::Value {
            let bytes = checked(
                Command::new("/usr/bin/codesign")
                    .args(["-d", "--entitlements", "-", "--xml"])
                    .arg(app),
            )
            .unwrap()
            .stdout;
            plist::Value::from_reader(std::io::Cursor::new(bytes)).unwrap()
        }
        assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
        assert_eq!(
            std::env::var("RUNNER_ENVIRONMENT").as_deref(),
            Ok("github-hosted")
        );
        let root = std::path::PathBuf::from(std::env::var_os("IPU_SIGNING_FIXTURE_ROOT").unwrap())
            .canonicalize()
            .unwrap();
        let runner_temp = std::path::PathBuf::from(std::env::var_os("RUNNER_TEMP").unwrap())
            .canonicalize()
            .unwrap();
        assert!(root.starts_with(runner_temp));
        let template = root.join("template/iPhoneUse-Runner.app");
        let keychain = root.join("fixture.keychain-db");
        assert!(template.is_dir() && keychain.is_file());
        let ctx = Ctx::paths(
            crate::instance::Instance::derive("signing-fixture", root.join("home"), None).unwrap(),
        );
        std::fs::create_dir_all(&ctx.runner_products_dir).unwrap();
        let app = ctx.runner_products_dir.join("iPhoneUse-Runner.app");
        checked(Command::new("/usr/bin/ditto").arg(&template).arg(&app)).unwrap();
        runner::validate_bundle(&app).unwrap();
        let certificate = legacy_ios::signing_certificate(&app).unwrap();
        let identity = legacy_ios::sha1_hex(&certificate);
        assert_eq!(
            identity,
            std::env::var("IPU_SIGNING_FIXTURE_IDENTITY").unwrap()
        );
        let original_entitlements = entitlements(&app);
        let nested = snapshot(&app.join("PlugIns"));
        assert!(matches(&app, false));
        for _ in 0..2 {
            apply(&ctx, &app, true).unwrap();
            runner::validate_bundle(&app).unwrap();
            assert!(matches(&app, true) && customized(&app));
            assert_eq!(legacy_ios::signing_certificate(&app).unwrap(), certificate);
            assert_eq!(entitlements(&app), original_entitlements);
            assert_eq!(snapshot(&app.join("PlugIns")), nested);
            let info = sys::read_plist(&app.join("Info.plist")).unwrap();
            assert_eq!(
                info.get("CFBundleIdentifier").and_then(|v| v.as_str()),
                Some("owned.signing.fixture")
            );
            assert_eq!(
                info.get("CFBundleIconName").and_then(|v| v.as_str()),
                Some("FixtureIcon")
            );
        }
        let shared = snapshot(&app);
        assert!(apply(&ctx, &app, false).is_err());
        assert_eq!(snapshot(&app), shared);
        let missing = ctx.runner_products_dir.join("MissingKey-Runner.app");
        checked(Command::new("/usr/bin/ditto").arg(&template).arg(&missing)).unwrap();
        let original = snapshot(&missing);
        checked(
            Command::new("/usr/bin/security")
                .args(["delete-identity", "-Z", &identity])
                .arg(&keychain),
        )
        .unwrap();
        runner::validate_bundle(&missing).unwrap();
        let error = apply(&ctx, &missing, true).unwrap_err();
        assert!(error.contains("command failed"), "{error}");
        assert_eq!(snapshot(&missing), original);
        runner::validate_bundle(&missing).unwrap();
        assert!(std::fs::read_dir(&ctx.runner_products_dir)
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("runner-files.")));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn product_exchange_keeps_both_nonempty_bundles_and_cleans_only_old_product() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("iPhoneUse-Runner.app");
        let work = tempfile::tempdir_in(dir.path()).unwrap();
        let staged = work.path().join("iPhoneUse-Runner.app");
        std::fs::create_dir(&installed).unwrap();
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(installed.join("identity"), b"old").unwrap();
        std::fs::write(staged.join("identity"), b"verified").unwrap();
        exchange_product(&installed, &staged).unwrap();
        assert_eq!(
            std::fs::read(installed.join("identity")).unwrap(),
            b"verified"
        );
        assert_eq!(std::fs::read(staged.join("identity")).unwrap(), b"old");
        drop(work);
        assert_eq!(
            std::fs::read(installed.join("identity")).unwrap(),
            b"verified"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn failed_product_exchange_preserves_installed_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("iPhoneUse-Runner.app");
        std::fs::create_dir(&installed).unwrap();
        std::fs::write(installed.join("identity"), b"old").unwrap();
        let missing = dir.path().join("missing.app");
        assert!(exchange_product(&installed, &missing).is_err());
        assert_eq!(std::fs::read(installed.join("identity")).unwrap(), b"old");
        assert!(!missing.exists());
    }

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
        assert_eq!(
            value.get("CFBundleIdentifier").and_then(|v| v.as_str()),
            Some("owned.xctrunner")
        );
        assert_eq!(
            value.get("CFBundleIconName").and_then(|v| v.as_str()),
            Some("AppIcon")
        );
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
