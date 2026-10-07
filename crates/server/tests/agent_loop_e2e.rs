//! The agent-loop features through the REAL entry points: a spawned
//! `iphone-use serve` daemon (against a scripted runner) driven over HTTP,
//! and the `iphone-use metrics` CLI against that daemon.
//!
//! Engine-level tests already cover the rules. These prove what a caller
//! actually gets: the request layer counts the calls, unauthenticated calls
//! are not counted, a plain HTTP caller gets `no_progress`, explicit runs
//! close with a summary in `agent-runs.jsonl`, and the CLI reads it back.

mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use support::mock_wda;

const TOKEN: &str = "e2e-agent-token-0123456789";
const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;
/// Four plain rows: a usable, non-sparse screen that never changes.
const TREE: &str = r#"{"value":{"type":"XCUIElementTypeApplication","label":"设置","rect":{"x":0,"y":0,"width":390,"height":844},"children":[
    {"type":"XCUIElementTypeButton","label":"通用","rect":{"x":16,"y":200,"width":358,"height":50}},
    {"type":"XCUIElementTypeButton","label":"蓝牙","rect":{"x":16,"y":260,"width":358,"height":50}},
    {"type":"XCUIElementTypeButton","label":"无障碍","rect":{"x":16,"y":320,"width":358,"height":50}}]}}"#;
/// A 1×1 PNG, so screenshot-based settle compares identical frames.
const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Daemon {
    child: Child,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_daemon(runner: &str, state: &std::path::Path, home: &std::path::Path) -> Daemon {
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_iphone-use"))
        .arg("serve")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("TMPDIR", state)
        .env("PHONE_REMOTE_HOST", "127.0.0.1")
        .env("PHONE_REMOTE_PORT", port.to_string())
        .env("PHONE_REMOTE_STATE_DIR", state)
        .env("PHONE_REMOTE_INSTANCE", "e2e")
        .env("PHONE_REMOTE_BACKEND", "direct")
        .env("PHONE_REMOTE_WDA_URL", runner)
        .env("PHONE_REMOTE_WDA_MJPEG_URL", runner)
        .env("PHONE_REMOTE_WDA_MANAGED", "0")
        .env("PHONE_REMOTE_UDID", "00000000-0000000000000000")
        .env("PHONE_REMOTE_AGENT_TOKEN", TOKEN)
        // With no password, reads are open to the browser by design; set one
        // so a wrong token is really refused.
        .env("PHONE_REMOTE_PASSWORD", "e2e-password-not-the-token")
        .env("IPHONE_USE_NO_UPDATE_CHECK", "1")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(state.join("daemon.err")).unwrap())
        .spawn()
        .expect("spawn iphone-use serve");
    let daemon = Daemon { child, port };
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return daemon;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let log = std::fs::read_to_string(state.join("daemon.err")).unwrap_or_default();
    panic!("daemon did not listen on {port}:\n{log}");
}

/// One HTTP/1.1 request; returns (status, JSON body).
fn http(
    port: u16,
    method: &str,
    path: &str,
    token: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, serde_json::Value) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    // Chunked bodies: take the JSON object between the first { and last }.
    let json = match (body.find('{'), body.rfind('}')) {
        (Some(a), Some(b)) if b > a => serde_json::from_str(&body[a..=b]).unwrap_or_default(),
        _ => serde_json::Value::Null,
    };
    (status, json)
}

/// A GET that waits out the auth limiter (a refused burst may have tripped
/// it) and returns the first non-429 answer.
fn get_after_lockout(port: u16, path: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, body) = http(port, "GET", path, TOKEN, &[], "");
        if status != 429 {
            assert_eq!(status, 200, "{path}: {body}");
            return body;
        }
        assert!(Instant::now() < deadline, "limiter never cleared");
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Wait until the daemon reports the phone drivable: one successful read
/// marks the runner actionable, then the status follows.
fn wait_drivable(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    http(port, "GET", "/agent/elements", TOKEN, &[], "");
    loop {
        let (_, status) = http(port, "GET", "/agent/status", TOKEN, &[], "");
        if status["drivable"] == true {
            return;
        }
        assert!(Instant::now() < deadline, "never drivable: {status}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn runner() -> support::MockWda {
    mock_wda(|request, _| {
        let line = request.lines().next().unwrap_or("");
        let body = if line.starts_with("POST /session ") {
            SESSION.to_string()
        } else if line.contains("/source") {
            TREE.to_string()
        } else if line.contains("/wda/locked") {
            r#"{"value":false}"#.to_string()
        } else if line.contains("/window/size") {
            r#"{"value":{"width":390,"height":844}}"#.to_string()
        } else if line.contains("/screenshot") {
            format!(r#"{{"value":"{PNG_1X1}"}}"#)
        } else if line.contains("/alert/text") {
            let body = r#"{"value":{"error":"no such alert","message":"none"}}"#;
            format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        } else if line.starts_with("GET /status") {
            r#"{"value":{"ready":true,"state":"success","sessionId":"SESSION","message":"ready"}}"#
                .to_string()
        } else {
            r#"{"value":null}"#.to_string()
        };
        Some((Duration::ZERO, body))
    })
}

fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

#[test]
fn metrics_runs_advice_and_cli_through_the_real_daemon() {
    let runner = runner();
    let state = private_dir();
    let home = private_dir();
    // The daemon refuses a state dir reached through a symlink (/var →
    // /private/var on macOS), so hand it the canonical paths.
    let state_path = state.path().canonicalize().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let daemon = spawn_daemon(runner.url(), &state_path, &home_path);
    let port = daemon.port;
    let control = [("X-Phone-Control", "1"), ("X-Phone-Owner", "e2e-agent")];

    // A full read, then the same observed tap twice on an unchanged screen.
    let (status, read) = http(port, "GET", "/agent/elements", TOKEN, &control, "");
    assert_eq!(status, 200, "{read}");
    assert!(read["tree_returned_at_ms"].as_u64().is_some(), "{read}");
    let tap = r#"{"type":"tap","x":0.5,"y":0.5}"#;
    let (status, first) = http(port, "POST", "/agent/input?return=delta", TOKEN, &control, tap);
    assert_eq!(status, 200, "{first}");
    assert!(first.get("no_progress").is_none(), "{first}");
    let (_, second) = http(port, "POST", "/agent/input?return=delta", TOKEN, &control, tap);
    let advice = second["no_progress"].as_str().unwrap_or_else(|| panic!("{second}"));
    assert!(advice.contains("Nothing was resent"), "{advice}");

    // An unauthenticated call carrying the same owner is refused and not counted.
    let (status, _) = http(port, "GET", "/agent/elements", "wrong-token", &control, "");
    assert_eq!(status, 401);

    // Scoped view: same snapshot, original indexes; `changed` needs a baseline.
    let (_, scoped) = http(port, "GET", "/agent/elements?scope=interactive", TOKEN, &control, "");
    assert_eq!(scoped["snapshot"], read["snapshot"], "{scoped}");
    assert!(scoped["elements"].as_array().unwrap().iter().all(|r| r["index"].is_u64()));
    let (status, refused) = http(port, "GET", "/agent/elements?scope=changed", TOKEN, &control, "");
    assert_eq!((status, refused["error"].as_str()), (400, Some("scope_needs_baseline")));

    // The inferred run for this owner counted the authenticated calls only.
    let (_, metrics) = http(port, "GET", "/agent/metrics?owner=e2e-agent", TOKEN, &[], "");
    let open = metrics["open"].as_array().unwrap_or_else(|| panic!("{metrics}"));
    assert_eq!(open.len(), 1, "{metrics}");
    let run = &open[0];
    assert_eq!(run["inferred"], true);
    assert_eq!(run["tool_calls"], 5, "{run}"); // read, 2 taps, scoped read, refused scope
    assert_eq!(run["observed_calls"], 2, "{run}");

    // An explicit run with a complete trace closes with a summary on disk.
    let owner_only = [("X-Phone-Owner", "e2e-agent")];
    let (status, _) = http(
        port,
        "POST",
        "/agent/run",
        TOKEN,
        &owner_only,
        r#"{"action":"start","run_id":"task-1","complete_trace":true}"#,
    );
    assert_eq!(status, 200);
    let in_run = [("X-Phone-Owner", "e2e-agent"), ("X-Agent-Run", "task-1")];
    http(port, "GET", "/agent/elements", TOKEN, &in_run, "");
    let (status, ended) = http(
        port,
        "POST",
        "/agent/run",
        TOKEN,
        &owner_only,
        r#"{"action":"end","run_id":"task-1","turn_ids":["m1","m2"]}"#,
    );
    assert_eq!(status, 200, "{ended}");
    assert_eq!(ended["run"]["tool_calls"], 1, "{ended}");
    assert_eq!(ended["run"]["model_round_trips"], 2, "{ended}");
    assert_eq!(ended["run"]["trace_incomplete"], false);

    let log = state_path.join("agent-runs.jsonl");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !log.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(std::fs::read_to_string(&log).unwrap().contains("task-1"));

    // The CLI reads the same metrics from the same daemon.
    let output = Command::new(env!("CARGO_BIN_EXE_iphone-use"))
        .args(["metrics", "--json", "--owner", "e2e-agent"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home_path)
        .env("PHONE_REMOTE_URL", format!("http://127.0.0.1:{port}"))
        .env("PHONE_REMOTE_TOKEN", TOKEN)
        .env("IPHONE_USE_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let recent = cli["recent"].as_array().unwrap();
    assert!(recent.iter().any(|r| r["key"]["run_id"] == "task-1"), "{cli}");
    let text = Command::new(env!("CARGO_BIN_EXE_iphone-use"))
        .args(["metrics", "--owner", "e2e-agent"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home_path)
        .env("PHONE_REMOTE_URL", format!("http://127.0.0.1:{port}"))
        .env("PHONE_REMOTE_TOKEN", TOKEN)
        .env("IPHONE_USE_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("task-1") && text.contains("not model turns"), "{text}");
}

/// `iphone-use-mcp` lives in another package, so `CARGO_BIN_EXE_` cannot name
/// it: build it (incremental, so cheap when current) and use the copy next
/// to the daemon binary. Never a stale one from an earlier build.
fn mcp_binary() -> std::path::PathBuf {
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "iphone-use-mcp", "--bin", "iphone-use-mcp"])
            .status()
            .unwrap();
        assert!(status.success(), "build iphone-use-mcp");
    });
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_iphone-use")).with_file_name("iphone-use-mcp")
}

struct Mcp {
    child: Child,
    reader: std::io::BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(port: u16, home: &std::path::Path) -> Self {
        let mut child = Command::new(mcp_binary())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home)
            .env("PHONE_REMOTE_URL", format!("http://127.0.0.1:{port}"))
            .env("PHONE_REMOTE_TOKEN", TOKEN)
            .env("PHONE_REMOTE_OWNER", "e2e-mcp")
            .env("IPHONE_USE_MCP_PREWARM", "0")
            .env("IPHONE_USE_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the MCP server starts");
        let reader = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Self {
            child,
            reader,
            next_id: 1,
        };
        mcp.request(
            "initialize",
            serde_json::json!({"protocolVersion": "2024-11-05", "capabilities": {},
                "clientInfo": {"name": "agent-loop-e2e", "version": "0"}}),
        );
        let stdin = mcp.child.stdin.as_mut().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).unwrap();
        mcp
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        use std::io::BufRead as _;
        let id = self.next_id;
        self.next_id += 1;
        let message =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(self.reader.read_line(&mut line).unwrap() > 0, "MCP closed stdout");
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value["id"].as_u64() == Some(id) {
                return value;
            }
        }
    }

    fn call(&mut self, name: &str, arguments: serde_json::Value) -> String {
        let response = self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        );
        response["result"]["content"]
            .as_array()
            .unwrap_or_else(|| panic!("{response}"))
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn an_mcp_client_gets_the_advice_once_and_is_counted() {
    let runner = runner();
    let state = private_dir();
    let home = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let daemon = spawn_daemon(runner.url(), &state_path, &home_path);
    let mut mcp = Mcp::start(daemon.port, &home_path);

    let screen = mcp.call("phone_elements", serde_json::json!({}));
    assert!(screen.contains("通用"), "{screen}");
    let tap = serde_json::json!({"x": 0.5, "y": 0.5, "observe": true});
    let first = mcp.call("phone_tap", tap.clone());
    assert!(!first.contains("no_progress"), "{first}");
    let second = mcp.call("phone_tap", tap);
    assert_eq!(
        second.matches("no_progress").count(),
        1,
        "the daemon's advice, shown once: {second}"
    );

    // A near-miss label: suggested, nothing sent.
    let miss = mcp.call("phone_tap_label", serde_json::json!({"label": "通 用"}));
    assert!(miss.contains("did you mean") && miss.contains("\"通用\""), "{miss}");

    // The MCP session's calls are in the daemon's metrics under its owner.
    let (_, metrics) = http(
        daemon.port,
        "GET",
        "/agent/metrics?owner=e2e-mcp",
        TOKEN,
        &[],
        "",
    );
    let open = metrics["open"].as_array().unwrap_or_else(|| panic!("{metrics}"));
    let calls: u64 = open.iter().filter_map(|r| r["tool_calls"].as_u64()).sum();
    assert!(calls >= 4, "elements, two taps, the label read: {metrics}");
    let observed: u64 = open.iter().filter_map(|r| r["observed_calls"].as_u64()).sum();
    assert_eq!(observed, 2, "{metrics}");
}


/// Everything a request refused as unauthenticated could touch, tried many
/// times between two observed taps: the run and the streak are untouched.
#[test]
fn a_burst_of_refused_requests_changes_nothing() {
    let runner = runner();
    let state = private_dir();
    let home = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let daemon = spawn_daemon(runner.url(), &state_path, &home_path);
    let port = daemon.port;
    let victim = [("X-Phone-Control", "1"), ("X-Phone-Owner", "victim")];
    let tap = r#"{"type":"tap","x":0.5,"y":0.5}"#;
    http(port, "GET", "/agent/elements", TOKEN, &victim, "");
    http(port, "POST", "/agent/input?return=delta", TOKEN, &victim, tap);
    let (_, before) = http(port, "GET", "/agent/metrics?owner=victim", TOKEN, &[], "");

    let forged = [
        ("X-Phone-Control", "1"),
        ("X-Phone-Owner", "victim"),
        ("X-Agent-Run", "hijack"),
    ];
    for _ in 0..4 {
        for (method, path, body) in [
            ("GET", "/agent/elements", ""),
            ("GET", "/agent/elements?scope=changed", ""),
            ("POST", "/agent/input?return=delta", tap),
            ("POST", "/agent/input", r#"{"type":"text","text":"x"}"#),
            ("POST", "/agent/actions", r#"{"steps":[]}"#),
            ("POST", "/agent/mode", r#"{"mode":"agent"}"#),
            ("POST", "/agent/owner", r#"{"release":true}"#),
            ("POST", "/agent/run", r#"{"action":"start","run_id":"hijack"}"#),
        ] {
            let (status, _) = http(port, method, path, "wrong-token", &forged, body);
            assert!(matches!(status, 401 | 429), "{method} {path}: {status}");
        }
    }

    let after = get_after_lockout(port, "/agent/metrics?owner=victim");
    assert_eq!(before["open"], after["open"], "refused requests left no trace");
    assert_eq!(before["stats"], after["stats"]);
    // The burst may have tripped the limiter; wait it out, then the streak
    // continues as if nothing happened.
    let deadline = Instant::now() + Duration::from_secs(40);
    let second = loop {
        let (status, body) = http(port, "POST", "/agent/input?return=delta", TOKEN, &victim, tap);
        if status != 429 || Instant::now() > deadline {
            break body;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(second["no_progress"].is_string(), "{second}");
}

/// The run contract end to end: start with a complete trace, four calls of
/// different kinds, end with an empty turn list → zero model turns; an
/// invalid start is refused.
#[test]
fn an_explicit_run_with_no_model_turns_reports_zero() {
    let runner = runner();
    let state = private_dir();
    let home = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let daemon = spawn_daemon(runner.url(), &state_path, &home_path);
    let port = daemon.port;
    let owner = [("X-Phone-Owner", "runner-1")];
    let (status, refused) = http(
        port,
        "POST",
        "/agent/run",
        TOKEN,
        &owner,
        r#"{"action":"start","run_id":"bad id!"}"#,
    );
    assert_eq!((status, refused["error"].as_str()), (400, Some("invalid_run")));

    let (status, _) = http(
        port,
        "POST",
        "/agent/run",
        TOKEN,
        &owner,
        r#"{"action":"start","run_id":"r1","complete_trace":true}"#,
    );
    assert_eq!(status, 200);
    let in_run = [
        ("X-Phone-Control", "1"),
        ("X-Phone-Owner", "runner-1"),
        ("X-Agent-Run", "r1"),
    ];
    let tap = r#"{"type":"tap","x":0.5,"y":0.5}"#;
    http(port, "GET", "/agent/elements", TOKEN, &in_run, "");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET /agent/screenshot HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\
         X-Phone-Owner: runner-1\r\nX-Agent-Run: r1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut sink = Vec::new();
    stream.read_to_end(&mut sink).unwrap();
    http(port, "POST", "/agent/input?return=delta", TOKEN, &in_run, tap);
    http(port, "POST", "/agent/input?return=delta", TOKEN, &in_run, tap);
    let (status, ended) = http(
        port,
        "POST",
        "/agent/run",
        TOKEN,
        &owner,
        r#"{"action":"end","run_id":"r1","turn_ids":[]}"#,
    );
    assert_eq!(status, 200, "{ended}");
    let run = &ended["run"];
    assert_eq!(run["tool_calls"], 4, "{run}");
    assert_eq!(run["observed_calls"], 2, "{run}");
    assert_eq!(run["model_round_trips"], 0, "{run}");
    assert_eq!(run["trace_incomplete"], false, "{run}");
    assert_eq!(run["incomplete"], false, "{run}");
}

/// A real flow replay, through the MCP server, is counted as flow calls.
#[test]
fn a_real_flow_replay_counts_as_flow_calls() {
    let runner = runner();
    let state = private_dir();
    let home = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let daemon = spawn_daemon(runner.url(), &state_path, &home_path);
    let flow = home_path.join("home.json");
    std::fs::write(
        &flow,
        r#"{"version":1,"name":"Home","steps":[{"kind":"shortcut","name":"home"}]}"#,
    )
    .unwrap();
    wait_drivable(daemon.port);
    let mut mcp = Mcp::start(daemon.port, &home_path);
    let out = mcp.call(
        "phone_flow_run",
        serde_json::json!({"id": flow.to_string_lossy(), "force": true}),
    );
    let (_, metrics) = http(
        daemon.port,
        "GET",
        "/agent/metrics?owner=e2e-mcp",
        TOKEN,
        &[],
        "",
    );
    let flow_calls: u64 = metrics["open"]
        .as_array()
        .unwrap_or_else(|| panic!("{metrics}"))
        .iter()
        .filter_map(|r| r["flow_calls"].as_u64())
        .sum();
    assert!(flow_calls > 0, "flow output: {out}\nmetrics: {metrics}");
}
