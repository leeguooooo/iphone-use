#![cfg(target_os = "macos")]
use std::process::{Command, Output};

#[test]
fn native_provider_rejects_invalid_scope_before_device_or_staging_access() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("must-not-exist");
    let output = Command::new(env!("CARGO_BIN_EXE_iphone-use"))
        .args(["files", "__native-read", "--device", "synthetic", "--bundle", "owned.xctrunner",
            "--source", "Documents/inbox/saved.pdf", "--max-bytes", "100", "--owner", "me",
            "--instance", "default", "--lease", "{}", "--destination"])
        .arg(&destination).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Invalid control lease identity"));
    assert!(!destination.exists());
}

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

/// A connection is not a request. Keep fragmented headers across read timeouts,
/// and ignore a peer that closes before sending any bytes.
fn read_status_request(
    reader: &mut impl std::io::Read,
    deadline: std::time::Instant,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut request = Vec::new();
    let mut byte = [0];
    while request.len() < 16384 && std::time::Instant::now() < deadline {
        match reader.read(&mut byte) {
            Ok(0) if request.is_empty() => return Ok(None),
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(_) => {
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    return Ok(Some(request));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "incomplete status request headers",
    ))
}

#[test]
fn status_fixture_preserves_fragmented_headers_across_read_timeouts() {
    struct Fragmented {
        bytes: std::io::Cursor<Vec<u8>>,
        timeout: bool,
    }
    impl std::io::Read for Fragmented {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            self.timeout = !self.timeout;
            if self.timeout {
                Err(std::io::ErrorKind::TimedOut.into())
            } else {
                std::io::Read::read(&mut self.bytes, out)
            }
        }
    }
    let expected = b"GET /agent/status HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let mut reader = Fragmented {
        bytes: std::io::Cursor::new(expected.to_vec()),
        timeout: false,
    };
    assert_eq!(
        read_status_request(
            &mut reader,
            std::time::Instant::now() + std::time::Duration::from_secs(1)
        )
        .unwrap()
        .unwrap(),
        expected
    );
}

#[test]
fn status_fixture_distinguishes_empty_connections_from_truncated_requests() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    assert!(read_status_request(&mut std::io::empty(), deadline)
        .unwrap()
        .is_none());
    let error =
        read_status_request(&mut std::io::Cursor::new(b"GET /agent/status"), deadline).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[test]
fn status_cli_preserves_control_lease_metadata_from_the_selected_daemon() {
    use std::io::Write;
    use std::time::{Duration, Instant};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let lease = serde_json::json!({"owner":"wire-owner","daemon_instance":"a".repeat(64),
        "owner_epoch":"7","privacy_epoch":"3","lifecycle_stamp":"256","private":false});
    let body=serde_json::json!({"drivable":true,"owner":"wire-owner","udid":"synthetic","control_lease":lease}).to_string();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let Some(request) = read_status_request(&mut stream, deadline).unwrap() else {
                        continue;
                    };
                    assert!(
                        String::from_utf8_lossy(&request).starts_with("GET /agent/status "),
                        "unexpected request: {:?}",
                        String::from_utf8_lossy(&request)
                    );
                    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("{error}"),
            }
        }
        false
    });
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_iphone-use"))
        .args(["status", "--instance", "default", "--json"])
        .env("HOME", home.path())
        .env("IPHONE_USE_URL", format!("http://{address}"))
        .output()
        .unwrap();
    assert!(
        server.join().unwrap(),
        "CLI never contacted the selected mock daemon"
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let returned: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(returned["control_lease"], lease);
}

#[test]
fn instance_context_reports_selected_xcode_without_touching_services() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    let developer = canonical_home.join("Chosen Xcode.app/Contents/Developer");
    let tool = developer.join("usr/bin/xcodebuild");
    std::fs::create_dir_all(tool.parent().unwrap()).unwrap();
    std::fs::write(&tool, "not executed").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let instance = server::instance::Instance::derive("chosen", &canonical_home, None).unwrap();
    server::setup::ctx::write_xcode_choice(&instance.state_dir, Some(&developer)).unwrap();
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_iphone-use"));
        cmd.arg("instance-context")
            .env("HOME", &canonical_home)
            .env("IPHONE_USE_INSTANCE", "chosen")
            .env_remove("IPHONE_USE_STATE_DIR")
            .env_remove("IPHONE_USE_XCODE")
            .env_remove("DEVELOPER_DIR")
            .env_remove("IPU_RUNNER_KEEPALIVE");
        cmd
    };
    let output = command().output().unwrap();
    assert!(output.status.success());
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["state_dir_ok"], true);
    assert_eq!(body["name"], "chosen");
    assert_eq!(body["developer_dir"], developer.to_str().unwrap());
    assert!(body["developer_dir_error"].is_null());
    let invalid = command()
        .env("IPHONE_USE_XCODE", "/missing/xcode")
        .output()
        .unwrap();
    assert!(
        invalid.status.success(),
        "namespace diagnostics must remain available"
    );
    let body: serde_json::Value = serde_json::from_slice(&invalid.stdout).unwrap();
    assert_eq!(body["name"], "chosen");
    assert!(body["developer_dir"].is_null());
    assert!(body["developer_dir_error"].as_str().is_some());
}
