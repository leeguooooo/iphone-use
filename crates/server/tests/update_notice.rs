//! The daily "new version" notice, end to end through the real binary.
//!
//! Every run gets a private HOME, XDG cache and TMPDIR, and a cache that is
//! already fresh, so nothing here touches the network, a real install, or a
//! running daemon (`stop` finds no pid file in the private TMPDIR).

use std::path::Path;
use std::process::{Command, Output};

const CURRENT: &str = env!("CARGO_PKG_VERSION");

fn seed_cache(cache_home: &Path, latest: &str) {
    let dir = cache_home.join("iphone-use");
    std::fs::create_dir_all(&dir).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        dir.join("update-check.json"),
        format!(r#"{{"checked_at":{now},"latest":"{latest}"}}"#),
    )
    .unwrap();
}

fn run(root: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_iphone-use"));
    command
        .args(args)
        .env("HOME", root.join("home"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        // CI runners set CI=true; each test opts out explicitly instead.
        .env_remove("CI")
        .env_remove("IPHONE_USE_NO_UPDATE_CHECK")
        .env_remove("USE_NO_UPDATE_CHECK")
        .env_remove("PHONE_REMOTE_NO_UPDATE_CHECK")
        .env("RUST_LOG", "off");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn setup() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for dir in ["home", "cache", "tmp"] {
        std::fs::create_dir_all(root.path().join(dir)).unwrap();
    }
    seed_cache(&root.path().join("cache"), "999.0.0");
    root
}

fn notice() -> String {
    format!("iphone-use 999.0.0 is available (you have {CURRENT}). Upgrade: iphone-use upgrade")
}

#[test]
fn notice_goes_to_stderr_only() {
    let root = setup();
    let output = run(root.path(), &["stop"], &[]);
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stderr.lines().filter(|line| *line == notice()).count(),
        1,
        "exactly one notice line on stderr: {stderr}"
    );
    assert!(
        !stdout.contains("is available"),
        "stdout must stay clean: {stdout}"
    );
}

#[test]
fn opt_out_variables_silence_the_notice() {
    for name in ["CI", "IPHONE_USE_NO_UPDATE_CHECK", "USE_NO_UPDATE_CHECK"] {
        let root = setup();
        let output = run(root.path(), &["stop"], &[(name, "1")]);
        assert!(output.status.success(), "{name}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains("is available"), "{name}: {stderr}");
    }
}

#[test]
fn version_help_and_installer_probe_skip_the_notice() {
    for args in [
        &["--version"][..],
        &["--help"][..],
        &["upgrade", "--help"][..],
        &["instance-context"][..],
    ] {
        let root = setup();
        let output = run(root.path(), args, &[]);
        let both = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!both.contains("is available"), "{args:?}: {both}");
    }
}

#[test]
fn version_flag_reports_the_package_version() {
    let root = setup();
    let output = run(root.path(), &["--version"], &[]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("iphone-use {CURRENT}")
    );
}
