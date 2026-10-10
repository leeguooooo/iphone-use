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

/// A free port no other test in this process has been given: tests run in
/// parallel, and a port released by one probe could otherwise be handed to
/// two daemons.
fn free_port() -> u16 {
    static USED: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    let mut used = USED.lock().unwrap();
    loop {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if !used.contains(&port) {
            used.push(port);
            return port;
        }
    }
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
    spawn_daemon_with(runner, state, home, &[])
}

fn spawn_daemon_with(
    runner: &str,
    state: &std::path::Path,
    home: &std::path::Path,
    extra: &[(&str, &str)],
) -> Daemon {
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_iphone-use"))
        .arg("serve")
        .env_clear()
        .envs(extra.iter().copied())
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("TMPDIR", state)
        .env("IPHONE_USE_HOST", "127.0.0.1")
        .env("IPHONE_USE_PORT", port.to_string())
        .env("IPHONE_USE_STATE_DIR", state)
        .env("IPHONE_USE_INSTANCE", "e2e")
        .env("IPHONE_USE_BACKEND", "direct")
        .env("IPHONE_USE_WDA_URL", runner)
        .env("IPHONE_USE_WDA_MJPEG_URL", runner)
        .env("IPHONE_USE_WDA_MANAGED", "0")
        .env("IPHONE_USE_UDID", "00000000-0000000000000000")
        .env("IPHONE_USE_AGENT_TOKEN", TOKEN)
        // With no password, reads are open to the browser by design; set one
        // so a wrong token is really refused.
        .env("IPHONE_USE_PASSWORD", "e2e-password-not-the-token")
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
    runner_with(TREE)
}

/// A container with nothing to act on: the tree is unusable (Mode A).
const SPARSE_TREE: &str = r#"{"value":{"type":"XCUIElementTypeApplication","label":"游戏","rect":{"x":0,"y":0,"width":390,"height":844},"children":[
    {"type":"XCUIElementTypeOther","label":"","rect":{"x":0,"y":0,"width":390,"height":844}}]}}"#;

fn runner_with(tree: &'static str) -> support::MockWda {
    mock_wda(move |request, _| {
        let line = request.lines().next().unwrap_or("");
        let body = if line.starts_with("POST /session ") {
            SESSION.to_string()
        } else if line.contains("/source") {
            tree.to_string()
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
    // An unknown (or evicted) baseline is refused, never answered with a full tree.
    let (status, unknown) = http(
        port,
        "GET",
        "/agent/elements?scope=changed&since=not-a-held-snapshot",
        TOKEN,
        &control,
        "",
    );
    assert_eq!((status, unknown["error"].as_str()), (400, Some("baseline_unavailable")));
    let held = format!(
        "/agent/elements?scope=changed&since={}",
        read["snapshot"].as_str().unwrap()
    );
    let (status, diff) = http(port, "GET", &held, TOKEN, &control, "");
    assert_eq!(status, 200, "{diff}");
    assert!(diff.get("delta").is_some(), "a held baseline gives the change: {diff}");

    // The inferred run for this owner counted the authenticated calls only.
    let (_, metrics) = http(port, "GET", "/agent/metrics?owner=e2e-agent", TOKEN, &[], "");
    let open = metrics["open"].as_array().unwrap_or_else(|| panic!("{metrics}"));
    assert_eq!(open.len(), 1, "{metrics}");
    let run = &open[0];
    assert_eq!(run["inferred"], true);
    // read, 2 taps, scoped read, refused scope, unknown baseline, held diff
    assert_eq!(run["tool_calls"], 7, "{run}");
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
        .env("IPHONE_USE_URL", format!("http://127.0.0.1:{port}"))
        .env("IPHONE_USE_TOKEN", TOKEN)
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
        .env("IPHONE_USE_URL", format!("http://127.0.0.1:{port}"))
        .env("IPHONE_USE_TOKEN", TOKEN)
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
            .env("IPHONE_USE_URL", format!("http://127.0.0.1:{port}"))
            .env("IPHONE_USE_TOKEN", TOKEN)
            .env("IPHONE_USE_OWNER", "e2e-mcp")
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


/// Mode A with `image=auto`: after an observed action left a settled frame
/// in memory, the image is still a fresh capture taken after the read, and
/// says so; with a tiny response budget the image is omitted and the text
/// stays.
#[test]
fn mode_a_images_are_fresh_labelled_and_budgeted() {
    let runner = runner_with(SPARSE_TREE);
    let state = private_dir();
    let home = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let daemon = spawn_daemon(runner.url(), &state_path, &home_path);
    let port = daemon.port;
    let control = [("X-Phone-Control", "1"), ("X-Phone-Owner", "img")];
    // An observed tap leaves a settled frame the screenshot route could reuse.
    http(port, "GET", "/agent/elements", TOKEN, &control, "");
    http(
        port,
        "POST",
        "/agent/input?return=delta",
        TOKEN,
        &control,
        r#"{"type":"tap","x":0.5,"y":0.5}"#,
    );
    let (status, read) = http(port, "GET", "/agent/elements?image=auto", TOKEN, &control, "");
    assert_eq!(status, 200, "{read}");
    let image = &read["image"];
    assert!(image["png_base64"].is_string(), "{read}");
    assert_eq!(image["source"], "wda-capture", "fresh, not the settled frame: {image}");
    let tree = read["tree_returned_at_ms"].as_u64().unwrap();
    let requested = image["requested_at_ms"].as_u64().unwrap();
    let received = image["received_at_ms"].as_u64().unwrap();
    assert!(tree <= requested && requested <= received, "{read}");
    assert!(image.get("captured_at_ms").is_none());
    drop(daemon);

    let state = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let daemon = spawn_daemon_with(
        runner.url(),
        &state_path,
        &home_path,
        &[("IPHONE_USE_IMAGE_BUDGET_BYTES", "200")],
    );
    let (status, read) = http(daemon.port, "GET", "/agent/elements?image=auto", TOKEN, &control, "");
    assert_eq!(status, 200, "{read}");
    assert_eq!(read["image_omitted"], "too_large_for_response_budget", "{read}");
    assert!(read.get("image").is_none());
    assert!(!read["elements"].as_array().unwrap().is_empty(), "text kept: {read}");
}

/// A daemon that answers one request with a fixed status and body.
fn one_shot_daemon(status: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{address}")
}

#[test]
fn cli_metrics_fails_loudly_on_anything_but_an_ok_report() {
    let home = private_dir();
    let home_path = home.path().canonicalize().unwrap();
    for (status, body) in [
        ("500 Internal Server Error", r#"{"ok":false,"error":"boom"}"#),
        ("200 OK", r#"{"ok":false,"error":"not_ready"}"#),
        ("200 OK", r#"{"open":[],"recent":[]}"#),
    ] {
        let url = one_shot_daemon(status, body);
        let output = Command::new(env!("CARGO_BIN_EXE_iphone-use"))
            .args(["metrics"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home_path)
            .env("IPHONE_USE_URL", url)
            .env("IPHONE_USE_TOKEN", TOKEN)
            .env("IPHONE_USE_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{status} {body}: exit 0, printed {stdout}");
        assert!(!stdout.contains("runs:"), "never rendered as a report: {stdout}");
        assert!(stderr.contains("could not report metrics"), "{stderr}");
    }
}

/// Log in with the password and return the session cookie (`name=value`).
fn session_cookie(port: u16, password: &str) -> String {
    let body = format!("password={password}");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "POST /login HTTP/1.1\r\nHost: x\r\nContent-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    raw.lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower
                .starts_with("set-cookie:")
                .then(|| line["set-cookie:".len()..].trim().split(';').next().unwrap().to_string())
        })
        .unwrap_or_else(|| panic!("no session cookie: {raw}"))
}

/// A valid browser cookie with a wrong bearer is refused by the bearer-only
/// routes, and must not let their wrappers touch the owner's state first.
#[test]
fn a_cookie_with_a_wrong_bearer_changes_nothing_on_bearer_routes() {
    let runner = runner();
    let state = private_dir();
    let home = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let daemon = spawn_daemon(runner.url(), &state_path, &home_path);
    let port = daemon.port;
    let cookie = session_cookie(port, "e2e-password-not-the-token");
    let victim = [("X-Phone-Control", "1"), ("X-Phone-Owner", "victim")];
    let tap = r#"{"type":"tap","x":0.5,"y":0.5}"#;
    http(port, "GET", "/agent/elements", TOKEN, &victim, "");
    http(port, "POST", "/agent/input?return=delta", TOKEN, &victim, tap);
    let (_, before) = http(port, "GET", "/agent/metrics?owner=victim", TOKEN, &[], "");

    let with_cookie = [
        ("X-Phone-Control", "1"),
        ("X-Phone-Owner", "victim"),
        ("Cookie", cookie.as_str()),
    ];
    for (path, body) in [
        ("/agent/input", r#"{"type":"text","text":"x"}"#),
        ("/agent/actions", r#"{"steps":[]}"#),
        ("/agent/owner", r#"{"release":true}"#),
    ] {
        let (status, _) = http(port, "POST", path, "wrong-token", &with_cookie, body);
        assert_eq!(status, 401, "{path} refuses a cookie without the bearer");
    }
    let after = get_after_lockout(port, "/agent/metrics?owner=victim");
    assert_eq!(before["open"], after["open"]);
    let (_, second) = http(port, "POST", "/agent/input?return=delta", TOKEN, &victim, tap);
    assert!(second["no_progress"].is_string(), "the streak survived: {second}");
}

/// An explicit run opened and closed through the standard MCP tools carries
/// every call of the session in between, and closes with its summary.
#[test]
fn an_explicit_run_through_the_mcp_tools() {
    let runner = runner();
    let state = private_dir();
    let home = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let daemon = spawn_daemon(runner.url(), &state_path, &home_path);
    let mut mcp = Mcp::start(daemon.port, &home_path);
    let started = mcp.call(
        "phone_run_start",
        serde_json::json!({"run_id": "mcp-task", "complete_trace": true}),
    );
    assert!(started.contains("\"ok\":true"), "{started}");
    mcp.call("phone_elements", serde_json::json!({}));
    mcp.call("phone_tap", serde_json::json!({"x": 0.5, "y": 0.5, "observe": true}));
    // A read that is not elements/screenshot (the flow draft) is in the run too.
    mcp.call("phone_flow_draft", serde_json::json!({}));
    // Ending a run that is not open is refused (404): the session's run stays.
    let refused = mcp.call("phone_run_end", serde_json::json!({"run_id": "not-open"}));
    assert!(refused.contains("run end failed"), "{refused}");
    mcp.call("phone_elements", serde_json::json!({}));
    let ended = mcp.call(
        "phone_run_end",
        serde_json::json!({"run_id": "mcp-task", "turn_ids": ["t1", "t2", "t3"]}),
    );
    let summary: serde_json::Value = serde_json::from_str(&ended).unwrap_or_else(|_| panic!("{ended}"));
    let run = &summary["run"];
    assert_eq!(run["key"]["run_id"], "mcp-task", "{run}");
    assert_eq!(run["key"]["owner"], "e2e-mcp", "{run}");
    // elements, /agent/apps (the first read in a newly entered app carries a
    // registry block, whose compat check reads the installed apps), tap,
    // flow draft, elements after the refused end
    assert_eq!(run["tool_calls"], 5, "{run}");
    assert_eq!(run["observed_calls"], 1, "{run}");
    assert_eq!(run["model_round_trips"], 3, "{run}");
    // After the end, the session's calls no longer carry the run.
    mcp.call("phone_elements", serde_json::json!({}));
    let (_, metrics) = http(daemon.port, "GET", "/agent/metrics?owner=e2e-mcp", TOKEN, &[], "");
    let open = metrics["open"].as_array().unwrap();
    assert!(open.iter().all(|r| r["key"]["kind"] == "inferred"), "{metrics}");
}

/// A scripted daemon: answers each connection with the next (status, body)
/// and records every request it saw.
fn scripted_daemon(
    script: Vec<(&'static str, &'static str)>,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    std::thread::spawn(move || {
        for (status, body) in script {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buffer = [0_u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            record
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buffer[..read]).to_ascii_lowercase());
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (format!("http://{address}"), seen)
}

/// A run end the daemon refuses (500, 401, or a 200 without ok:true) keeps
/// the run active: the next calls still carry it. Only an accepted end
/// clears it.
#[test]
fn a_refused_run_end_keeps_the_run_active_in_the_mcp_client() {
    const READ: &str = r#"{"snapshot":"S","elements":[{"kind":"Button","label":"通用"}]}"#;
    let (url, seen) = scripted_daemon(vec![
        ("200 OK", r#"{"ok":true}"#),
        ("500 Internal Server Error", r#"{"ok":false,"error":"boom"}"#),
        ("200 OK", READ),
        ("401 Unauthorized", "unauthorized"),
        ("200 OK", READ),
        ("200 OK", r#"{"ok":false,"error":"no_such_run"}"#),
        ("200 OK", READ),
        ("200 OK", r#"{"ok":true,"run":{}}"#),
        ("200 OK", READ),
    ]);
    let home = private_dir();
    let home_path = home.path().canonicalize().unwrap();
    let mut child = Command::new(mcp_binary())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home_path)
        .env("IPHONE_USE_URL", &url)
        .env("IPHONE_USE_TOKEN", TOKEN)
        .env("IPHONE_USE_OWNER", "e2e-mcp")
        .env("IPHONE_USE_MCP_PREWARM", "0")
        .env("IPHONE_USE_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let reader = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut mcp = Mcp {
        child,
        reader,
        next_id: 1,
    };
    mcp.request(
        "initialize",
        serde_json::json!({"protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "run-state", "version": "0"}}),
    );
    mcp.call("phone_run_start", serde_json::json!({"run_id": "keep-me"}));
    for _ in 0..3 {
        mcp.call("phone_run_end", serde_json::json!({"run_id": "keep-me"}));
        mcp.call("phone_elements", serde_json::json!({}));
    }
    mcp.call("phone_run_end", serde_json::json!({"run_id": "keep-me"}));
    mcp.call("phone_elements", serde_json::json!({}));

    let seen = seen.lock().unwrap();
    let reads: Vec<&String> = seen
        .iter()
        .filter(|r| r.starts_with("get /agent/elements"))
        .collect();
    assert_eq!(reads.len(), 4, "{seen:?}");
    for read in &reads[..3] {
        assert!(read.contains("x-agent-run: keep-me"), "the run survived a refused end: {read}");
    }
    assert!(!reads[3].contains("x-agent-run"), "an accepted end clears it: {}", reads[3]);
}

/// The MCP read reuses the daemon's image policy: after an observed action
/// left a settled frame in memory, a Mode A `phone_elements` still gets a
/// fresh capture, once, as image content, labelled in the text; with a tiny
/// budget it gets no image and the text survives.
#[test]
fn mcp_mode_a_images_are_fresh_once_and_budgeted() {
    let runner = runner_with(SPARSE_TREE);
    let state = private_dir();
    let home = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let daemon = spawn_daemon(runner.url(), &state_path, &home_path);
    let mut mcp = Mcp::start(daemon.port, &home_path);
    mcp.call("phone_tap", serde_json::json!({"x": 0.5, "y": 0.5, "observe": true}));
    let response = mcp.request(
        "tools/call",
        serde_json::json!({"name": "phone_elements", "arguments": {}}),
    );
    let content = response["result"]["content"].as_array().unwrap();
    let images: Vec<_> = content.iter().filter(|c| c["type"] == "image").collect();
    assert_eq!(images.len(), 1, "{response}");
    let text: String = content
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("source wda-capture"), "a fresh capture, not the settled frame: {text}");
    assert!(!text.contains("iVBOR"), "no base64 in the text");
    let structured = response["result"]["structuredContent"].to_string();
    assert!(!structured.contains("png_base64"), "no base64 in the structured copy");
    drop(mcp);
    drop(daemon);

    let state = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let daemon = spawn_daemon_with(
        runner.url(),
        &state_path,
        &home_path,
        &[("IPHONE_USE_IMAGE_BUDGET_BYTES", "200")],
    );
    let mut mcp = Mcp::start(daemon.port, &home_path);
    let response = mcp.request(
        "tools/call",
        serde_json::json!({"name": "phone_elements", "arguments": {}}),
    );
    let content = response["result"]["content"].as_array().unwrap();
    assert!(content.iter().all(|c| c["type"] != "image"), "{response}");
    let text = content[0]["text"].as_str().unwrap_or("");
    assert!(text.contains("游戏"), "the rows survive: {text}");
    assert!(
        content.iter().any(|c| c["text"].as_str().is_some_and(|t| t.contains("image_omitted"))),
        "{response}"
    );
}

/// A runner whose tree read starts failing once `fail` is set.
fn runner_failing(fail: std::sync::Arc<std::sync::atomic::AtomicBool>) -> support::MockWda {
    mock_wda(move |request, _| {
        let line = request.lines().next().unwrap_or("");
        let body = if line.starts_with("POST /session ") {
            SESSION.to_string()
        } else if line.contains("/source") {
            if fail.load(std::sync::atomic::Ordering::SeqCst) {
                let body = r#"{"value":{"error":"unknown error","message":"source failed"}}"#;
                format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                TREE.to_string()
            }
        } else if line.contains("/wda/locked") {
            r#"{"value":false}"#.to_string()
        } else if line.contains("/alert/text") {
            let body = r#"{"value":{"error":"no such alert","message":"none"}}"#;
            format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        } else {
            r#"{"value":null}"#.to_string()
        };
        Some((Duration::ZERO, body))
    })
}

/// A real read failure under `scope=changed` keeps its own status; only a
/// successful full-tree fallback is reported as a missing baseline.
#[test]
fn scope_changed_keeps_real_read_failures() {
    let fail = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runner = runner_failing(fail.clone());
    let state = private_dir();
    let home = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let daemon = spawn_daemon(runner.url(), &state_path, &home_path);
    let owner = [("X-Phone-Owner", "scope")];
    let (_, read) = http(daemon.port, "GET", "/agent/elements", TOKEN, &owner, "");
    let since = read["snapshot"].as_str().unwrap_or_else(|| panic!("{read}")).to_string();
    fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = http(
        daemon.port,
        "GET",
        &format!("/agent/elements?scope=changed&since={since}"),
        TOKEN,
        &owner,
        "",
    );
    assert!(matches!(status, 502 | 504), "{status} {body}");
    assert_ne!(body["error"], "baseline_unavailable", "{body}");
}

/// Live HTTP/CLI summaries stay open until run_end, which preserves the real
/// close reason, counters, zero-model trace and persisted summary.
#[test]
fn live_metrics_report_open_until_the_run_ends() {
    let runner = runner();
    let state = private_dir();
    let home = private_dir();
    let state_path = state.path().canonicalize().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let daemon = spawn_daemon(runner.url(), &state_path, &home_path);
    let owner = [("X-Phone-Owner", "live-state")];
    let (status, _) = http(
        daemon.port,
        "POST",
        "/agent/run",
        TOKEN,
        &owner,
        r#"{"action":"start","run_id":"task","complete_trace":true}"#,
    );
    assert_eq!(status, 200);
    let in_run = [("X-Phone-Owner", "live-state"), ("X-Agent-Run", "task")];
    let (status, _) = http(daemon.port, "GET", "/agent/elements", TOKEN, &in_run, "");
    assert_eq!(status, 200);
    let (_, report) = http(
        daemon.port,
        "GET",
        "/agent/metrics?owner=live-state",
        TOKEN,
        &[],
        "",
    );
    assert_eq!(report["open"].as_array().unwrap().len(), 1);
    assert_eq!(report["open"][0]["closed"], "open");
    assert_eq!(report["open"][0]["tool_calls"], 1);
    assert!(report["recent"].as_array().unwrap().is_empty());

    let cli = |json: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_iphone-use"));
        command.args(["metrics", "--owner", "live-state"]);
        if json {
            command.arg("--json");
        }
        let output = command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home_path)
            .env(
                "IPHONE_USE_URL",
                format!("http://127.0.0.1:{}", daemon.port),
            )
            .env("IPHONE_USE_TOKEN", TOKEN)
            .env("IPHONE_USE_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let live: serde_json::Value = serde_json::from_str(&cli(true)).unwrap();
    assert_eq!(live["open"][0]["closed"], "open");
    assert!(cli(false).contains("task [state: open]"));
    let (status, ended) = http(
        daemon.port,
        "POST",
        "/agent/run",
        TOKEN,
        &owner,
        r#"{"action":"end","run_id":"task","turn_ids":[]}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(ended["run"]["closed"], "ended");
    assert_eq!(
        ended["run"]["tool_calls"], 1,
        "metrics queries never count as work"
    );
    assert_eq!(ended["run"]["model_round_trips"], 0);
    let recent: serde_json::Value = serde_json::from_str(&cli(true)).unwrap();
    assert!(recent["open"].as_array().unwrap().is_empty());
    assert_eq!(recent["recent"][0]["closed"], "ended");
    assert!(cli(false).contains("task [state: ended]"));
    let path = state_path.join("agent-runs.jsonl");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let logged = std::fs::read_to_string(&path).unwrap_or_default();
        if !logged.is_empty() {
            let summary: serde_json::Value =
                serde_json::from_str(logged.lines().next().unwrap()).unwrap();
            assert_eq!(summary["closed"], "ended");
            assert_eq!(summary["tool_calls"], 1);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "closed summary was not persisted"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
