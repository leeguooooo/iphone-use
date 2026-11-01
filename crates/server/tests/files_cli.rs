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

#[test]
fn status_cli_preserves_control_lease_metadata_from_the_selected_daemon() {
    use std::io::{Read, Write};
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
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                        if stream.read(&mut byte).unwrap_or(0) == 0 {
                            break;
                        }
                        request.push(byte[0]);
                    }
                    assert!(String::from_utf8_lossy(&request).starts_with("GET /agent/status "));
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
