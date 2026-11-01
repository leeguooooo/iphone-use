#![cfg(target_os = "macos")]
use std::process::{Command, Output};

#[test]
fn packaged_files_entry_works_without_checkout_or_python_path_imports() {
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("iphone-use");
    std::fs::copy(env!("CARGO_BIN_EXE_iphone-use"), &binary).unwrap();
    for name in ["common.py", "publication.py", "json.py"] {
        std::fs::write(
            root.path().join(name),
            "raise RuntimeError('untrusted import')\n",
        )
        .unwrap();
    }
    let run = |args: &[&str]| -> Output {
        Command::new(&binary)
            .arg("files")
            .args(args)
            .current_dir(root.path())
            .env("HOME", root.path())
            .env("PYTHONPATH", root.path())
            .env_remove("IPHONE_USE_OWNER")
            .output()
            .unwrap()
    };
    let help = run(&["--help"]);
    assert!(
        help.status.success(),
        "{}",
        String::from_utf8_lossy(&help.stderr)
    );
    assert!(String::from_utf8_lossy(&help.stdout).contains("receive"));
    let state_dir = root.path().join("state");
    let state = state_dir.to_str().unwrap();
    let status = run(&[
        "status",
        "--device",
        "synthetic",
        "--bundle",
        "owned.xctrunner",
        "--state-dir",
        state,
    ]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(body["active"], false);
    assert!(
        !state_dir.exists(),
        "idle status must not create receiver state"
    );
    let refused = run(&[
        "receive",
        "--device",
        "synthetic",
        "--bundle",
        "owned.xctrunner",
        "--state-dir",
        state,
        "--filename",
        "fixture.pdf",
        "--destination",
        root.path().to_str().unwrap(),
    ]);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "argument refusal exit code must reach caller"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("receive requires --owner"),
        "wrong refusal: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !state_dir.exists(),
        "missing owner must refuse before session or device access"
    );
}
