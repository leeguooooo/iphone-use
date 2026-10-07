//! The runner's Home Screen icon.
//!
//! Xcode synthesises the runner app (`iPhoneUse-Runner.app`, the .xctrunner)
//! from a template, so a target asset catalog lands inside the nested .xctest
//! and the Home Screen shows a blank placeholder. After the product is built
//! and validated, the installed iPhoneUse app's icon (`WDA_RUNNER_ICON=auto`,
//! the default; `none` keeps the placeholder; or a local .png/.icns) is
//! compiled with actool, merged into the runner's Info.plist
//! (CFBundleIcons › CFBundlePrimaryIcon › CFBundleIconName = AppIcon), and the
//! app is re-signed strictly inside-out: nested frameworks, then
//! PlugIns/*.xctest, then the app with its own entitlements. Signing only the
//! outer app breaks the seal (installd rejects it with 0xe8008001, and the
//! poisoned product wedges the runner offline).
//!
//! The runner is launched with `test-without-building -xctestrun`, which
//! installs the product as built, so the icon survives the launch. Any failure
//! restores the pristine signed app from its backup and setup continues
//! without an icon: the phone working matters more than it looking right.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest as _, Sha256};

use super::ctx::Ctx;
use super::sys;
use super::term::{ok, warn};

/// A file the icon step leaves in the app; its presence marks an injected
/// product (the next build must start from a pristine one, #75).
pub const INJECTED_MARKER: &str = "AppIcon60x60@2x.png";

/// Where the icon comes from, resolved from `WDA_RUNNER_ICON`.
pub fn source(ctx: &Ctx) -> Option<PathBuf> {
    let setting = std::env::var("WDA_RUNNER_ICON")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            let persisted = sys::plist_env(&ctx.wda_agent_plist, "WDA_RUNNER_ICON");
            if persisted.is_empty() {
                "auto".into()
            } else {
                persisted
            }
        });
    match setting.as_str() {
        "none" => {
            ok("Runner icon injection disabled (WDA_RUNNER_ICON=none)");
            None
        }
        "auto" => {
            // The app this instance runs, then the standard install locations.
            let program = sys::plist_program_argument(&ctx.daemon_plist, 0);
            let from_program = Path::new(&program)
                .ancestors()
                .find(|p| p.extension().is_some_and(|e| e == "app"))
                .map(|app| app.join("Contents/Resources/AppIcon.icns"));
            let candidates = [
                from_program,
                Some(
                    ctx.home
                        .join("Applications/iPhoneUse.app/Contents/Resources/AppIcon.icns"),
                ),
                Some(PathBuf::from(
                    "/Applications/iPhoneUse.app/Contents/Resources/AppIcon.icns",
                )),
            ];
            let found = candidates.into_iter().flatten().find(|p| p.is_file());
            if found.is_none() {
                warn("Runner icon source is not installed with the iPhoneUse app; continuing with the runner's placeholder icon");
            }
            found
        }
        path => {
            let path = PathBuf::from(path);
            match path.canonicalize() {
                Ok(canonical) if canonical.is_file() => Some(canonical),
                _ => {
                    warn(&format!(
                        "WDA_RUNNER_ICON does not name a readable file: {}; continuing with the runner's placeholder icon",
                        path.display()
                    ));
                    None
                }
            }
        }
    }
}

/// The icon's part of the product cache key: its path and content hash, so a
/// changed icon rebuilds and an app reinstall that rewrites the same bytes
/// does not.
pub fn cache_component(source: Option<&Path>) -> String {
    match source {
        None => "icon:none".into(),
        Some(path) => match std::fs::read(path) {
            Ok(bytes) => format!(
                "icon:{}:{}",
                path.display(),
                Sha256::digest(&bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ),
            Err(_) => format!("icon:{}:unreadable", path.display()),
        },
    }
}

/// Drop an app a previous round injected, so build-for-testing starts from a
/// pristine product: building over a hand-signed app re-emplaces Info.plist
/// without re-signing and fails validation (#75). Only this instance's own
/// app at its fixed products path is ever removed.
pub fn discard_previous_injection(ctx: &Ctx, app: &Path) -> Result<(), String> {
    if !app.join(INJECTED_MARKER).exists() {
        return Ok(());
    }
    owned_app(ctx, app)?;
    std::fs::remove_dir_all(app)
        .map_err(|e| format!("could not remove the previously injected runner: {e}"))
}

fn owned_app(ctx: &Ctx, app: &Path) -> Result<(), String> {
    let products = &ctx.runner_products_dir;
    let owned = products.to_string_lossy().contains("/Build/Products/")
        && app.parent() == Some(products.as_path())
        && app
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with("-Runner.app"))
        && !std::fs::symlink_metadata(app).is_ok_and(|m| m.file_type().is_symlink());
    if owned {
        Ok(())
    } else {
        Err(format!(
            "refusing to modify an unowned runner product: {}",
            app.display()
        ))
    }
}

fn run(log: &Path, program: &str, args: &[&str]) -> bool {
    let Ok(out) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    else {
        return false;
    };
    let Ok(err) = out.try_clone() else {
        return false;
    };
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .status()
        .is_ok_and(|status| status.success())
}

fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// One injection attempt's scratch space; removed when done.
struct Work {
    dir: PathBuf,
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Inject the icon into the built, validated `app`. `true` when the app now
/// carries it and verifies; on any failure the pristine app is restored and
/// `false` is returned (setup continues without an icon).
pub fn inject(ctx: &Ctx, app: &Path, source: &Path) -> bool {
    let log = ctx.state_dir().join("wda-runner-icon.log");
    let _ = std::fs::write(&log, b"");
    match try_inject(ctx, app, source, &log) {
        Ok(()) => {
            ok(&format!(
                "Runner icon injected and signature verified (source: {})",
                source.display()
            ));
            true
        }
        Err((reason, recovery)) => {
            warn(&format!(
                "Runner icon skipped: {reason}{recovery}. Setup continues without a custom icon (log: {}).",
                log.display()
            ));
            false
        }
    }
}

type Failure = (String, String);

fn fail<T>(reason: impl Into<String>) -> Result<T, Failure> {
    Err((reason.into(), String::new()))
}

fn try_inject(ctx: &Ctx, app: &Path, source: &Path, log: &Path) -> Result<(), Failure> {
    owned_app(ctx, app).map_err(|e| (e, String::new()))?;
    let extension = source
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if extension != "icns" && extension != "png" {
        return fail("WDA_RUNNER_ICON must name a .png or .icns file");
    }
    for tool in ["sips", "codesign", "iconutil"] {
        if sys::which(tool).is_none() && (tool != "iconutil" || extension == "icns") {
            return fail(format!("{tool} is unavailable"));
        }
    }
    if !sys::is_executable(Path::new("/usr/bin/ditto")) {
        return fail("/usr/bin/ditto is unavailable");
    }
    if output("xcrun", &["--find", "actool"]).is_none() {
        return fail("Xcode actool is unavailable");
    }
    let dir = tempfile::Builder::new()
        .prefix("wda-runner-icon.")
        .tempdir_in(ctx.state_dir())
        .map_err(|_| {
            (
                "could not create a private icon work directory".to_string(),
                String::new(),
            )
        })?
        .keep();
    let work = Work { dir };
    let icon_png = work.dir.join("icon-1024.png");
    let catalog = work.dir.join("Assets.xcassets");
    let set = catalog.join("AppIcon.appiconset");
    let compiled = work.dir.join("compiled");
    let partial = work.dir.join("partial.plist");
    if std::fs::create_dir_all(&set).is_err() || std::fs::create_dir_all(&compiled).is_err() {
        return fail("could not prepare the asset catalog");
    }
    let s = |p: &Path| p.to_string_lossy().into_owned();

    // A 1024 px, alpha-free PNG.
    if extension == "icns" {
        let iconset = work.dir.join("source.iconset");
        if !run(
            log,
            "iconutil",
            &["-c", "iconset", &s(source), "-o", &s(&iconset)],
        ) || !iconset.join("icon_512x512@2x.png").is_file()
        {
            return fail("the ICNS has no usable 1024px representation");
        }
        if std::fs::copy(iconset.join("icon_512x512@2x.png"), &icon_png).is_err() {
            return fail("could not stage the ICNS image");
        }
    } else if !run(
        log,
        "sips",
        &[
            "-s",
            "format",
            "png",
            "-z",
            "1024",
            "1024",
            &s(source),
            "--out",
            &s(&icon_png),
        ],
    ) {
        return fail("the PNG could not be converted to 1024x1024");
    }
    let properties = output(
        "sips",
        &[
            "-g",
            "pixelWidth",
            "-g",
            "pixelHeight",
            "-g",
            "hasAlpha",
            &s(&icon_png),
        ],
    )
    .unwrap_or_default();
    let property = |name: &str| {
        properties
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix(&format!("{name}:"))
                    .map(|v| v.trim().to_ascii_lowercase())
            })
            .unwrap_or_default()
    };
    if property("pixelWidth") != "1024" || property("pixelHeight") != "1024" {
        return fail(format!(
            "the staged icon is {}x{} instead of 1024x1024",
            property("pixelWidth"),
            property("pixelHeight")
        ));
    }
    // iOS rejects primary app icons with alpha; actool still compiles them.
    if matches!(property("hasAlpha").as_str(), "yes" | "true")
        && !run(
            log,
            "sips",
            &["--setProperty", "hasAlpha", "false", &s(&icon_png)],
        )
    {
        return fail("the icon alpha channel could not be flattened");
    }
    let contents = r#"{"images":[{"filename":"icon-1024.png","idiom":"universal","platform":"ios","size":"1024x1024"}],"info":{"author":"xcode","version":1}}"#;
    if std::fs::write(set.join("Contents.json"), contents).is_err()
        || std::fs::copy(&icon_png, set.join("icon-1024.png")).is_err()
    {
        return fail("could not populate the asset catalog");
    }
    if !run(
        log,
        "xcrun",
        &[
            "actool",
            "--compile",
            &s(&compiled),
            "--app-icon",
            "AppIcon",
            "--minimum-deployment-target",
            "13.0",
            "--platform",
            "iphoneos",
            "--target-device",
            "iphone",
            "--output-partial-info-plist",
            &s(&partial),
            &s(&catalog),
        ],
    ) {
        return fail("actool could not compile the runner icon");
    }
    for produced in [
        "Assets.car",
        "AppIcon60x60@2x.png",
        "AppIcon76x76@2x~ipad.png",
    ] {
        if !compiled.join(produced).is_file() {
            return fail(format!("actool did not produce {produced}"));
        }
    }
    if std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0) == 0 {
        return fail("actool did not produce its partial Info.plist");
    }

    // What the app is signed with, to re-sign it the same way.
    let details = output("codesign", &["-dvv", &s(app)]).unwrap_or_default();
    let Some(identity) = details
        .lines()
        .find_map(|l| l.strip_prefix("Authority="))
        .map(str::to_string)
    else {
        return fail("the runner signing identity could not be read");
    };
    let entitlements = work.dir.join("runner-entitlements.plist");
    let saved = Command::new("codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(app)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success() && !out.stdout.is_empty())
        .is_some_and(|out| std::fs::write(&entitlements, &out.stdout).is_ok());
    if !saved || !run(log, "plutil", &["-lint", &s(&entitlements)]) {
        return fail("the runner entitlements could not be preserved");
    }

    // Back up the pristine signed app; every failure from here restores it.
    let backup = work.dir.join("original.app");
    if !run(log, "/usr/bin/ditto", &[&s(app), &s(&backup)])
        || !run(
            log,
            "codesign",
            &["--verify", "--deep", "--strict", &s(&backup)],
        )
    {
        return fail("the pristine runner could not be backed up safely");
    }
    let mutate = || -> Result<(), String> {
        for produced in [
            "Assets.car",
            "AppIcon60x60@2x.png",
            "AppIcon76x76@2x~ipad.png",
        ] {
            std::fs::copy(compiled.join(produced), app.join(produced)).map_err(|_| {
                "compiled icon assets could not be copied into the runner".to_string()
            })?;
        }
        merge_icon_plist(&partial, &app.join("Info.plist"))?;
        // Strictly inside-out.
        let mut nested: Vec<PathBuf> = std::fs::read_dir(app.join("Frameworks"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .is_some_and(|e| e == "dylib" || e == "framework")
                    })
                    .collect()
            })
            .unwrap_or_default();
        nested.sort();
        for path in &nested {
            if !run(log, "codesign", &["-f", "-s", &identity, &s(path)]) {
                return Err("a nested runner framework could not be re-signed".into());
            }
        }
        let mut tests: Vec<PathBuf> = std::fs::read_dir(app.join("PlugIns"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "xctest"))
                    .collect()
            })
            .unwrap_or_default();
        tests.sort();
        if tests.is_empty() {
            return Err("the runner contains no xctest bundle to re-sign".into());
        }
        for path in &tests {
            if !run(log, "codesign", &["-f", "-s", &identity, &s(path)]) {
                return Err("the runner xctest bundle could not be re-signed".into());
            }
        }
        if !run(
            log,
            "codesign",
            &[
                "-f",
                "-s",
                &identity,
                "--entitlements",
                &s(&entitlements),
                &s(app),
            ],
        ) || !run(
            log,
            "codesign",
            &["--verify", "--deep", "--strict", &s(app)],
        ) {
            return Err("the final runner signature did not verify".into());
        }
        super::runner::validate_bundle(app)
    };
    if let Err(reason) = mutate() {
        let recovery = restore(app, &backup, log);
        // Keep the backup when the restore failed: it is the only good copy.
        if recovery.contains("retained") {
            std::mem::forget(work);
        }
        return Err((reason, recovery));
    }
    Ok(())
}

/// Put the pristine app back. Returns the recovery note for the warning.
fn restore(app: &Path, backup: &Path, log: &Path) -> String {
    let _ = std::fs::remove_dir_all(app);
    if run(
        log,
        "/usr/bin/ditto",
        &[&backup.to_string_lossy(), &app.to_string_lossy()],
    ) && run(
        log,
        "codesign",
        &["--verify", "--deep", "--strict", &app.to_string_lossy()],
    ) {
        return "; restored the pristine signed runner".into();
    }
    let _ = std::fs::remove_dir_all(app);
    format!(
        "; automatic restore failed, removed the runner so the next round rebuilds it, and the recovery backup was retained at {}",
        backup.display()
    )
}

/// Merge actool's partial Info.plist into the runner's, keeping the original
/// format (binary or XML) and mode, and requiring the primary icon name.
fn merge_icon_plist(partial: &Path, info: &Path) -> Result<(), String> {
    let format = std::fs::read(info)
        .map(|bytes| {
            if bytes.starts_with(b"bplist00") {
                "binary1"
            } else {
                "xml1"
            }
        })
        .map_err(|_| "the runner Info.plist could not be read".to_string())?;
    let merged = info.with_file_name(".Info.plist.icon");
    let _ = std::fs::remove_file(&merged);
    std::fs::copy(info, &merged).map_err(|_| "could not stage Info.plist".to_string())?;
    // PlistBuddy's Merge adds the partial's top-level keys (CFBundleIcons,
    // CFBundleIcons~ipad) to the runner's dictionary.
    let merged_ok = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", &format!("Merge {}", partial.display())])
        .arg(&merged)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    let name = Command::new("/usr/libexec/PlistBuddy")
        .args([
            "-c",
            "Print :CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconName",
        ])
        .arg(&merged)
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    let converted = merged_ok
        && name == "AppIcon"
        && Command::new("plutil")
            .args(["-convert", format])
            .arg(&merged)
            .status()
            .is_ok_and(|status| status.success());
    if !converted {
        let _ = std::fs::remove_file(&merged);
        return Err(
            "actool's icon metadata could not be merged into Info.plist (CFBundleIconName=AppIcon)"
                .into(),
        );
    }
    if let Ok(meta) = std::fs::metadata(info) {
        let _ = std::fs::set_permissions(&merged, meta.permissions());
    }
    std::fs::rename(&merged, info).map_err(|_| "could not replace Info.plist".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_component_follows_the_content() {
        let dir = tempfile::tempdir().unwrap();
        let icon = dir.path().join("AppIcon.icns");
        std::fs::write(&icon, b"one").unwrap();
        let first = cache_component(Some(&icon));
        std::fs::write(&icon, b"one").unwrap();
        assert_eq!(
            cache_component(Some(&icon)),
            first,
            "rewriting the same bytes keeps the key"
        );
        std::fs::write(&icon, b"two").unwrap();
        assert_ne!(cache_component(Some(&icon)), first);
        assert_eq!(cache_component(None), "icon:none");
    }

    #[test]
    fn merging_requires_the_primary_icon_name() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("Info.plist");
        std::fs::write(&info, r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleExecutable</key><string>Runner</string></dict></plist>"#).unwrap();
        assert!(Command::new("plutil")
            .args(["-convert", "binary1"])
            .arg(&info)
            .status()
            .unwrap()
            .success());
        let partial = dir.path().join("partial.plist");
        std::fs::write(&partial, r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIcons</key><dict><key>CFBundlePrimaryIcon</key><dict><key>CFBundleIconName</key><string>AppIcon</string></dict></dict></dict></plist>"#).unwrap();
        merge_icon_plist(&partial, &info).unwrap();
        assert!(
            std::fs::read(&info).unwrap().starts_with(b"bplist00"),
            "the binary format is kept"
        );
        let plist = sys::read_plist(&info).unwrap();
        assert_eq!(
            plist.get("CFBundleExecutable").and_then(|v| v.as_str()),
            Some("Runner")
        );
        let name = plist
            .get("CFBundleIcons")
            .and_then(|v| v.get("CFBundlePrimaryIcon"))
            .and_then(|v| v.get("CFBundleIconName"))
            .and_then(|v| v.as_str());
        assert_eq!(name, Some("AppIcon"));
        std::fs::write(&partial, r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>Other</key><string>x</string></dict></plist>"#).unwrap();
        let info2 = dir.path().join("Info2.plist");
        std::fs::write(
            &info2,
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict/></plist>"#,
        )
        .unwrap();
        assert!(merge_icon_plist(&partial, &info2).is_err());
    }
}
