//! Real MCP calls over stdio against a scripted daemon.
//!
//! In-process tests can call a tool function and inspect the value it
//! returns, but they never serialise it. `structured_content` only matters if
//! it survives the JSON-RPC encoding an actual client reads, so these tests
//! spawn the built binary, speak the protocol to it, and assert on the wire
//! format. The daemon URL is passed through the child's environment, so
//! nothing here touches process-global state in the test binary.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A daemon that answers every request with one canned response and can
/// always be stopped.
///
/// A blocking `accept` loop with no stop signal turns one mis-written test
/// into a suite that hangs forever, so this uses a non-blocking accept, a stop
/// flag, a hard deadline, and a join in `Drop`.
struct ScriptedDaemon {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
    disconnected: Arc<AtomicBool>,
}

impl ScriptedDaemon {
    fn start(status: &'static str, body: Vec<u8>) -> Self {
        Self::delayed(status, body, Duration::ZERO)
    }

    fn delayed(status: &'static str, body: Vec<u8>, delay: Duration) -> Self {
        Self::routed(status, body, delay, None)
    }

    fn routed(status: &'static str, body: Vec<u8>, delay: Duration, status_body: Option<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        let disconnected = Arc::new(AtomicBool::new(false));
        let peer_closed = disconnected.clone();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !thread_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        // A bounded `accept` still leaves `read` and
                        // `write_all` able to park this thread forever: a peer
                        // that connects and says nothing, or stops reading
                        // mid-response. Both sides get a deadline.
                        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
                        stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
                        let mut buffer = [0_u8; 8_192];
                        let mut request = Vec::new();
                        let read_deadline = Instant::now() + Duration::from_secs(10);
                        while request.len() < 65536 && Instant::now() < read_deadline {
                            stream.set_read_timeout(Some(read_deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)))).ok();
                            match stream.read(&mut buffer) {
                                Ok(0) | Err(_) => break,
                                Ok(count) => request.extend_from_slice(&buffer[..count]),
                            }
                            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                                let length = String::from_utf8_lossy(&request[..end]).lines()
                                    .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|s|s.trim().parse::<usize>().ok())).unwrap_or(0);
                                if request.len() >= end + 4 + length { break; }
                            }
                        }
                        seen.lock().unwrap().push(String::from_utf8_lossy(&request).to_string());
                        let until = Instant::now() + delay;
                        while Instant::now() < until && !thread_stop.load(Ordering::Acquire) {
                            stream.set_read_timeout(Some(Duration::from_millis(20))).ok();
                            if matches!(stream.peek(&mut [0u8;1]), Ok(0)) {
                                peer_closed.store(true, Ordering::Release);
                                break;
                            }
                        }
                        let status_read = request.starts_with(b"GET /agent/status ");
                        let truncated = status == "200 Truncated" && !status_read;
                        let body = if status_read { status_body.as_ref().unwrap_or(&body) } else { &body };
                        let response_status = if status_read && status_body.is_some() || truncated { "200 OK" } else { status };
                        let head = format!(
                            "HTTP/1.1 {response_status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len() + if truncated {64} else {0}
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(body);
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            url: format!("http://{address}"),
            stop,
            thread: Some(thread),
            requests,
            disconnected,
        }
    }
}

impl Drop for ScriptedDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The binary under test, built alongside this integration test.
fn binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("iphone-use-mcp");
    assert!(path.exists(), "built binary missing at {}", path.display());
    path
}

struct McpChild {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl McpChild {
    /// These tests check the whole JSON on the wire, so they ask for it;
    /// [`McpChild::start_with`] runs the default (slim) shape.
    fn start(daemon_url: &str) -> Self {
        Self::start_with(daemon_url, "full")
    }

    fn start_with(daemon_url: &str, structured: &str) -> Self {
        Self::start_options(daemon_url, structured, &[])
    }

    fn start_options(daemon_url: &str, structured: &str, options: &[(&str, &str)]) -> Self {
        let mut command = Command::new(binary());
        command
            .env("IPHONE_USE_URL", daemon_url)
            .env("IPHONE_USE_MCP_STRUCTURED", structured)
            .env_remove("IPHONE_USE_TOKEN")
            .env_remove("IPHONE_USE_MCP_HOST")
            .env_remove("IPHONE_USE_MCP_TOOLS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (name, value) in options { command.env(name, value); }
        let mut child = command.spawn().expect("spawn the MCP server");
        let reader = BufReader::new(child.stdout.take().expect("stdout"));
        let mut session = Self { child, reader };
        session.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "observe-test", "version": "0" }
            }
        }));
        let _ = session.read_reply(1);
        session.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));
        session
    }

    fn send(&mut self, value: &serde_json::Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{value}").expect("write request");
        stdin.flush().expect("flush");
    }

    /// Read until the reply with this id arrives (notifications are skipped).
    fn read_reply(&mut self, id: u64) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let mut line = String::new();
            if self.reader.read_line(&mut line).expect("read stdout") == 0 {
                panic!("MCP server closed stdout before replying to {id}");
            }
            if line.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return value;
            }
        }
        panic!("no reply to request {id} within the deadline");
    }

    fn call_tool(&mut self, id: u64, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }));
        self.read_reply(id)
    }
}

impl Drop for McpChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// By default the structured payload is slim: the verdict a program branches
/// on and the compact summary, not the 64 KiB tree — Claude Code shows the
/// model only `structuredContent` on success, so this is what it reads.
#[test]
fn the_default_structured_payload_is_the_verdict_and_the_summary() {
    let filler = "x".repeat(64 * 1024);
    let body = format!(
        r#"{{"ok":true,"transport":"runner","tree":"{filler}","snapshot":"snap-1","settle":{{"settled":true,"reason":"stable","captures":2}},"delta":{{"added":["搜索"]}}}}"#
    );
    let daemon = ScriptedDaemon::start("200 OK", body.into_bytes());
    let mut mcp = McpChild::start_with(&daemon.url, "slim");

    let reply = mcp.call_tool(
        2,
        "phone_tap",
        serde_json::json!({ "x": 0.5, "y": 0.5, "observe": true }),
    );

    let result = &reply["result"];
    assert_ne!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["ok"], true, "{reply}");
    assert_eq!(structured["snapshot"], "snap-1");
    assert!(structured.get("tree").is_none(), "{reply}");
    assert!(structured.get("settle").is_none(), "{reply}");
    let summary = structured["summary"].as_str().expect("summary");
    assert_eq!(summary, result["content"][0]["text"].as_str().unwrap());
    assert!(reply.to_string().len() < 8 * 1024, "{}", reply.to_string().len());
}

/// `IPHONE_USE_MCP_STRUCTURED=off` sends the text alone.
#[test]
fn structured_off_sends_the_text_alone() {
    let body = r#"{"ok":true,"transport":"runner","snapshot":"snap-1","settle":{"settled":true,"reason":"stable","captures":2},"delta":{"added":["a"]}}"#;
    let daemon = ScriptedDaemon::start("200 OK", body.as_bytes().to_vec());
    let mut mcp = McpChild::start_with(&daemon.url, "off");

    let reply = mcp.call_tool(
        2,
        "phone_tap",
        serde_json::json!({ "x": 0.5, "y": 0.5, "observe": true }),
    );

    let result = &reply["result"];
    assert_ne!(result["isError"], true, "{reply}");
    assert!(
        result.get("structuredContent").is_none() || result["structuredContent"].is_null(),
        "{reply}"
    );
}

/// The observation has to reach the client whole. A 64 KiB body is well past
/// any display-sized cap, and everything a caller acts on sits at its end.
#[test]
fn an_observed_tap_puts_the_whole_observation_on_the_wire() {
    let filler = "x".repeat(64 * 1024);
    let body = format!(
        r#"{{"ok":true,"transport":"runner","tree":"{filler}","snapshot":"snap-1","settle":{{"settled":true,"reason":"stable","captures":2}},"delta":{{"added":["搜索"]}}}}"#
    );
    let daemon = ScriptedDaemon::start("200 OK", body.into_bytes());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(
        2,
        "phone_tap",
        serde_json::json!({ "x": 0.5, "y": 0.5, "observe": true }),
    );

    let result = &reply["result"];
    assert_ne!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert!(
        structured.is_object(),
        "no structuredContent on the wire: {reply}"
    );
    assert_eq!(structured["settle"]["reason"], "stable");
    assert_eq!(structured["settle"]["captures"], 2);
    assert_eq!(structured["delta"]["added"][0], "搜索");
    assert_eq!(structured["snapshot"], "snap-1");
}

/// Omitting `observe` must behave exactly as before: the short result callers
/// already parse, and no structured payload appearing out of nowhere.
#[test]
fn an_unobserved_tap_is_unchanged_on_the_wire() {
    let daemon = ScriptedDaemon::start("200 OK", br#"{"ok":true,"transport":"runner"}"#.to_vec());
    let mut mcp = McpChild::start(&daemon.url);

    // Observation is on by default; `observe:false` keeps the bare action.
    let reply = mcp.call_tool(
        2,
        "phone_tap",
        serde_json::json!({ "x": 0.5, "y": 0.5, "observe": false }),
    );

    let result = &reply["result"];
    assert_ne!(result["isError"], true, "{reply}");
    assert_eq!(result["content"][0]["text"], "ok");
}

/// A 2xx the client cannot read is not a completed action, and the reply must
/// say so in a form a program can branch on.
#[test]
fn an_unreadable_success_reaches_the_client_as_unknown() {
    let daemon = ScriptedDaemon::start("200 OK", b"<html>proxy error</html>".to_vec());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["outcome"], "unknown", "{reply}");
    assert_eq!(structured["retry_safe"], false, "{reply}");
    assert_eq!(structured["reason"], "unparseable_response", "{reply}");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("Do NOT resend"), "{text}");
}

/// A refusal keeps the fields a caller decides on.
#[test]
fn a_refusal_keeps_its_structure_on_the_wire() {
    let daemon = ScriptedDaemon::start(
        "409 Conflict",
        br#"{"ok":false,"error":"phone_owned","outcome":"not_sent","retry_safe":true}"#.to_vec(),
    );
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    assert_eq!(result["structuredContent"]["error"], "phone_owned");
    assert_eq!(result["structuredContent"]["outcome"], "not_sent");
    assert_eq!(result["structuredContent"]["retry_safe"], true);
}

/// A daemon that accepts the connection and then dies mid-response. The
/// request may already have reached the phone, so the only honest answer is
/// unknown — and above all not "not sent".
#[test]
fn a_broken_connection_is_unknown_not_a_safe_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let killer = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0_u8; 4_096];
            let _ = stream.read(&mut buffer);
            // Promise a body, then hang up without sending it.
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: 4096

{",
            );
        }
    });
    let mut mcp = McpChild::start(&format!("http://{address}"));

    let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));
    let _ = killer.join();

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["outcome"], "unknown", "{reply}");
    assert_eq!(
        structured["retry_safe"], false,
        "a broken connection authorised a resend: {reply}"
    );
    assert_ne!(
        structured["outcome"], "not_sent",
        "a broken connection claimed nothing was sent: {reply}"
    );
}

/// A 500 with an HTML error page says nothing about the phone either.
#[test]
fn a_non_json_server_error_is_unknown_not_not_sent() {
    let daemon = ScriptedDaemon::start(
        "500 Internal Server Error",
        b"<html><body>gateway exploded</body></html>".to_vec(),
    );
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["outcome"], "unknown", "{reply}");
    assert_eq!(structured["retry_safe"], false, "{reply}");
}

/// A plain-text `200 ok` proves nothing about the phone (the iPhone Mirroring
/// backend that sent it is gone). It reads as an unknown outcome — never a
/// success, and never safe to resend.
#[test]
fn a_plain_text_ok_is_an_unknown_outcome() {
    let daemon = ScriptedDaemon::start("200 OK", b"ok".to_vec());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["outcome"], "unknown", "{reply}");
    assert_eq!(structured["retry_safe"], false, "{reply}");
}

/// The recognition is exact. A 2xx whose body merely CONTAINS "ok", or is any
/// other plain text, stays unknown — otherwise an error page mentioning the
/// word would be read as a dispatched action.
#[test]
fn only_the_exact_legacy_ack_is_accepted() {
    for body in [
        b"ok then".to_vec(),
        b"not ok".to_vec(),
        b"<html>ok</html>".to_vec(),
    ] {
        let daemon = ScriptedDaemon::start("200 OK", body.clone());
        let mut mcp = McpChild::start(&daemon.url);

        let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

        let result = &reply["result"];
        let shape = String::from_utf8_lossy(&body).to_string();
        assert_eq!(
            result["isError"], true,
            "{shape:?} was accepted as a legacy ack: {reply}"
        );
        assert_eq!(result["structuredContent"]["outcome"], "unknown", "{shape:?}");
    }
}

/// Parsing is not the same as being told. A body that happens to be valid
/// JSON but is not the daemon saying `ok:false` leaves the outcome unknown —
/// including the shapes that cannot even be attached as structured content.
/// The `reason` must distinguish "we read it and it said nothing" from "we
/// could not read it", because a caller diagnosing a failure needs to know
/// which of the two happened.
#[test]
fn a_parseable_but_meaningless_failure_is_still_unknown() {
    for body in [
        br#"[]"#.to_vec(),
        br#"42"#.to_vec(),
        br#"{"unrelated":1}"#.to_vec(),
    ] {
        let daemon = ScriptedDaemon::start("500 Internal Server Error", body.clone());
        let mut mcp = McpChild::start(&daemon.url);

        let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

        let result = &reply["result"];
        let shape = String::from_utf8_lossy(&body).to_string();
        assert_eq!(result["isError"], true, "{shape}: {reply}");
        let structured = &result["structuredContent"];
        assert_eq!(
            structured["outcome"], "unknown",
            "{shape} was read as a refusal: {reply}"
        );
        assert_eq!(
            structured["retry_safe"], false,
            "{shape} authorised a resend: {reply}"
        );
        // Read but silent, not unreadable — the same vocabulary the batch
        // entry point uses, so a caller can branch on `reason` without
        // knowing which tool produced it.
        assert_eq!(
            structured["reason"], "no_verdict_in_response",
            "{shape} was reported as unreadable: {reply}"
        );
    }
}

/// The one case that IS provably safe to retry: refused locally, before
/// anything was sent.
#[test]
fn a_missing_snapshot_is_refused_locally_and_is_retry_safe() {
    let daemon = ScriptedDaemon::start("200 OK", br#"{"ok":true}"#.to_vec());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(
        2,
        "phone_tap_element",
        serde_json::json!({ "element": 0, "snapshot": "" }),
    );

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["outcome"], "not_sent", "{reply}");
    assert_eq!(structured["retry_safe"], true, "{reply}");
}

/// Structured content is an object in MCP. A body that parsed into an array
/// must not be attached, and must not be handed back as capability data.
#[test]
fn a_non_object_body_is_not_offered_as_structured_content() {
    let daemon = ScriptedDaemon::start("200 OK", br#"[1,2,3]"#.to_vec());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_capabilities", serde_json::json!({}));

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    assert!(
        result.get("structuredContent").is_none()
            || result["structuredContent"].is_null(),
        "a JSON array was offered as structured content: {reply}"
    );
}

/// Discovery is read-only and structured.
#[test]
fn capabilities_reach_the_client_as_structured_content() {
    let daemon = ScriptedDaemon::start(
        "200 OK",
        br#"{"ok":true,"backend":"direct","scope":"ui_control","supported":{"element_tree":true},"available":{"ok":false,"blocked_by":"released"}}"#.to_vec(),
    );
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_capabilities", serde_json::json!({}));

    let result = &reply["result"];
    assert_ne!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["supported"]["element_tree"], true);
    assert_eq!(structured["available"]["blocked_by"], "released");
    assert_eq!(structured["scope"], "ui_control");
}


#[test]
fn text_continuation_schema_and_verdict_survive_real_stdio() {
    for mode in ["full", "slim"] {
        let token = "a".repeat(64);
        let body = serde_json::json!({"ok":false,"error":"text_partially_typed","outcome":"partial",
            "retry_safe":false,"characters_confirmed":200,"characters_uncertain":0,
            "characters_total":300,"remaining_text":"字".repeat(100),"continue_token":token,
            "continuation_safe":true,"continuation_expires_in_ms":60000});
        let daemon=ScriptedDaemon::start("502 Bad Gateway",body.to_string().into_bytes());
        let mut mcp=McpChild::start_with(&daemon.url,mode);
        mcp.send(&serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/list"}));
        let listed = mcp.read_reply(10);
        let schema = &listed["result"]["tools"].as_array().unwrap().iter()
            .find(|tool| tool["name"]=="phone_type").unwrap()["inputSchema"];
        assert!(schema["properties"].get("continue_token").is_some(),"{schema}");
        assert!(schema["properties"].get("budget_ms").is_some(),"{schema}");
        assert!(!schema["required"].as_array().is_some_and(|fields|fields.iter().any(|field|field=="text")),"token-only input must be discoverable: {schema}");
        let first=mcp.call_tool(2,"phone_type",serde_json::json!({"text":"x".repeat(300),"budget_ms":6000,"clear":true,"observe":false}));
        assert_eq!(first["result"]["isError"],true,"{first}");
        assert_eq!(first["result"]["structuredContent"]["continue_token"],token,"{first}");
        let resumed=mcp.call_tool(3,"phone_type",serde_json::json!({"continue_token":token,"observe":false}));
        assert_eq!(resumed["result"]["structuredContent"]["continuation_safe"],true,"{resumed}");
        let requests=daemon.requests.lock().unwrap().clone();
        assert_eq!(requests.len(),2);
        let parse=|request:&str| serde_json::from_str::<serde_json::Value>(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(parse(&requests[0])["budget_ms"],6000);
        assert_eq!(parse(&requests[0])["clear"],true);
        assert_eq!(parse(&requests[1]),serde_json::json!({"type":"text","continue_token":token}));
        let owner=|request:&str| request.lines().find(|line|line.starts_with("x-phone-owner:")).unwrap().to_string();
        assert_eq!(owner(&requests[0]),owner(&requests[1]));
        let mixed=mcp.call_tool(4,"phone_type",serde_json::json!({"continue_token":token,"text":"must not send"}));
        assert_eq!(mixed["result"]["isError"],true,"{mixed}");
        assert_eq!(daemon.requests.lock().unwrap().len(),2,"invalid mix never reaches daemon");
        let mixed_clear=mcp.call_tool(5,"phone_type",serde_json::json!({"continue_token":token,"clear":false}));
        assert_eq!(mixed_clear["result"]["isError"],true,"{mixed_clear}");
        assert_eq!(daemon.requests.lock().unwrap().len(),2,"continuation never repeats clear");
    }
}


#[test]
fn cancelled_tool_drops_the_foreground_http_request_without_replay() {
    let daemon=ScriptedDaemon::delayed("200 OK",br#"{"ok":true}"#.to_vec(),Duration::from_secs(5));
    let mut mcp=McpChild::start(&daemon.url);
    mcp.send(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call", "params":{"name":"phone_type","arguments":{"text":"x".repeat(500),"observe":false}}}));
    let until=Instant::now()+Duration::from_secs(2);
    while daemon.requests.lock().unwrap().is_empty() {
        assert!(Instant::now()<until,"tool never reached the daemon");
        std::thread::sleep(Duration::from_millis(5));
    }
    let started=Instant::now();
    mcp.send(&serde_json::json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2,"reason":"fixture cancellation"}}));
    let result=mcp.read_reply(2);
    assert_eq!(result["result"]["isError"],true,"{result}");
    assert_eq!(result["result"]["structuredContent"]["outcome"],"unknown","{result}");
    assert_eq!(result["result"]["structuredContent"]["retry_safe"],false,"{result}");
    assert!(started.elapsed()<Duration::from_secs(2),"cancellation waited for normal completion");
    let until=Instant::now()+Duration::from_secs(2);
    while !daemon.disconnected.load(Ordering::Acquire) {
        assert!(Instant::now()<until,"HTTP request remained open after MCP cancellation");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(daemon.requests.lock().unwrap().len(),1,"cancelled call must not be resent");
}
#[test]
fn recovery_png_is_one_image_block_and_never_base64_text_or_structured_duplication() {
    use base64::Engine as _;
    let png =
        base64::engine::general_purpose::STANDARD.encode(include_bytes!("fixtures/recovery.png"));
    for mode in ["full", "slim", "off"] {
        for batch in [false, true] {
            let mut body = serde_json::json!({"ok":false,"error":"element_occluded","outcome":"not_sent","retry_safe":true,
            "image":{"png_base64":png,"source":"runner-capture","requested_at_ms":10,"received_at_ms":12,"pixel_size":[16,16]},
            "recovery":{"visual_check_required":true,"replay_action":false,"next_step":"inspect the visible overlay"}});
            if batch {
                body["error"] = "wait_for_timeout".into();
                body["retry_safe"] = false.into();
                body["failed_step"] = 1.into();
                body["completed"] = 1.into();
                body["applied_actions"] = 1.into();
                body["batch_outcome"] = "partially_applied".into();
                body["steps"] = serde_json::json!([{"index":0,"kind":"action","ok":true}]);
            }
            let daemon = ScriptedDaemon::start("409 Conflict", body.to_string().into_bytes());
            let mut mcp = McpChild::start_with(&daemon.url, mode);
            let reply = mcp.call_tool(
                2,
                if batch {
                    "phone_run_steps"
                } else {
                    "phone_tap"
                },
                if batch {
                    serde_json::json!({"steps":[{"kind":"tap","x":0.5,"y":0.5}],"observe":false})
                } else {
                    serde_json::json!({"x":0.5,"y":0.5,"observe":false})
                },
            );
            let result = &reply["result"];
            assert_eq!(result["isError"], true, "{reply}");
            let blocks = result["content"].as_array().unwrap();
            assert_eq!(
                blocks.iter().filter(|b| b["type"] == "image").count(),
                1,
                "{reply}"
            );
            for text in blocks.iter().filter_map(|b| b["text"].as_str()) {
                assert!(!text.contains(&png), "base64 leaked into text");
                assert!(!text.contains("png_base64"), "payload key leaked into text");
            }
            assert_eq!(reply.to_string().matches(&png).count(), 1);
            if mode != "off" {
                assert_eq!(result["structuredContent"]["outcome"], "not_sent");
                assert!(result["structuredContent"]
                    .pointer("/image/png_base64")
                    .is_none());
                assert_eq!(result["structuredContent"]["retry_safe"], !batch);
                if batch {
                    assert_eq!(result["structuredContent"]["applied_actions"], 1);
                }
            }
        }
    }
}

#[test]
fn malformed_recovery_image_keeps_the_failure_without_an_image_block() {
    let body = serde_json::json!({"ok":false,"error":"element_not_found","outcome":"not_sent","retry_safe":true,
        "image":{"png_base64":"not-a-png","source":"runner-capture"}});
    let daemon = ScriptedDaemon::start("422 Unprocessable Entity", body.to_string().into_bytes());
    let mut mcp = McpChild::start(&daemon.url);
    let reply = mcp.call_tool(
        2,
        "phone_tap",
        serde_json::json!({"x":0.5,"y":0.5,"observe":false}),
    );
    let result = &reply["result"];
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["error"], "element_not_found");
    assert_eq!(result["structuredContent"]["retry_safe"], true);
    assert_eq!(result["structuredContent"]["image_unavailable"], true);
    assert!(!result["content"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["type"] == "image"));
}

#[test]
fn host_panels_have_distinct_resources_and_only_one_opener() {
    for host in ["workbuddy", "doubao"] {
        let daemon = ScriptedDaemon::start("200 OK", br#"{"ok":true,"drivable":true}"#.to_vec());
        let mut child = McpChild::start_options(&daemon.url, "slim", &[("IPHONE_USE_MCP_HOST", host)]);
        child.send(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        let listed = child.read_reply(2);
        let tools = listed["result"]["tools"].as_array().unwrap();
        let screen = tools.iter().find(|t|t["name"]=="phone_screen").unwrap();
        let status = tools.iter().find(|t|t["name"]=="phone_status").unwrap();
        assert!(status.pointer("/_meta/ui/resourceUri").is_none());
        assert!(screen["_meta"]["ui"]["resourceUri"].as_str().unwrap().contains(host));
        if host == "workbuddy" { assert_eq!(screen["_meta"]["workbuddy"]["ui"]["launchSurface"], "panel"); }
        let uri = screen["_meta"]["ui"]["resourceUri"].clone();
        child.send(&serde_json::json!({"jsonrpc":"2.0","id":3,"method":"resources/list"}));
        assert_eq!(child.read_reply(3)["result"]["resources"][0]["uri"], uri);
        child.send(&serde_json::json!({"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":uri}}));
        let page=child.read_reply(4);
        let html=page["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(html.contains(if host=="workbuddy" {"RESOURCE_FRAMES = true"} else {"RESOURCE_FRAMES = false"}));
        assert!(!html.contains("__IU_"));
        assert!(daemon.requests.lock().unwrap().is_empty(), "discovery must not wake a phone");
    }
}

#[test]
fn workbuddy_frame_resources_survive_process_restart_without_control_or_policy_bypass() {
    use base64::Engine as _;
    let png=include_bytes!("fixtures/recovery.png").to_vec();
    let status=br#"{"ok":true,"drivable":true,"owner":"somebody-else"}"#.to_vec();
    let daemon=ScriptedDaemon::routed("200 OK",png.clone(),Duration::ZERO,Some(status));
    for nonce in [1,2] {
        let mut child=McpChild::start_options(&daemon.url,"off",&[("IPHONE_USE_MCP_HOST","workbuddy"),("IPHONE_USE_MCP_TOOLS","phone_screen")]);
        child.send(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":format!("ui://iphone-use/frame-v1/600/{nonce}")}}));
        let response=child.read_reply(2);
        let body:serde_json::Value=serde_json::from_str(response["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["owner"],"somebody-else");
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(body["frame"]["data"].as_str().unwrap()).unwrap(),png);
        for uri in ["ui://iphone-use/frame-v1/600/home","ui://iphone-use/frame-v1/1001/1","ui://iphone-use/frame-v1/600/1?fresh=1","ui://iphone-use/frame-v1/600/18446744073709551616"] {
            child.send(&serde_json::json!({"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":uri}}));
            assert!(child.read_reply(3).get("error").is_some());
        }
    }
    let before=daemon.requests.lock().unwrap().len();
    assert_eq!(before,4,"two independent MCP processes each read status and a frame");
    assert!(daemon.requests.lock().unwrap().iter().all(|r|r.starts_with("GET /agent/status ")||r.starts_with("GET /agent/screenshot?max_side=600 ")));
    for (host,profile) in [("workbuddy","phone_status"),("generic","all"),("doubao","all")] {
        let mut child=McpChild::start_options(&daemon.url,"slim",&[("IPHONE_USE_MCP_HOST",host),("IPHONE_USE_MCP_TOOLS",profile)]);
        child.send(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"ui://iphone-use/frame-v1/600/1"}}));
        assert!(child.read_reply(2).get("error").is_some());
    }
    assert_eq!(daemon.requests.lock().unwrap().len(),before,"resource read bypassed profile or host restriction");
}

#[test]
fn workbuddy_private_frames_are_withheld_and_capture_errors_remain_errors() {
    for private in [false,true] {
        let daemon=ScriptedDaemon::routed(if private {"200 OK"} else {"503 Service Unavailable"}, include_bytes!("fixtures/recovery.png").to_vec(),Duration::ZERO,
            Some(serde_json::json!({"ok":true,"drivable":true,"screen_private_secs":if private {60} else {0}}).to_string().into_bytes()));
        let mut child=McpChild::start_options(&daemon.url,"full",&[("IPHONE_USE_MCP_HOST","workbuddy")]);
        child.send(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"ui://iphone-use/frame-v1/600/1"}}));
        let response=child.read_reply(2);
        let body:serde_json::Value=serde_json::from_str(response["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
        assert!(body["frame"].is_null());
        if private {assert_eq!(body["private_secs"],60);} else {assert_eq!(body["ok"],false);assert!(!body["error"].is_null());}
    }
}

#[test]
fn local_daemon_frames_ignore_gui_proxy_environment() {
    let daemon=ScriptedDaemon::routed("200 OK",include_bytes!("fixtures/recovery.png").to_vec(),Duration::ZERO,
        Some(br#"{"ok":true,"drivable":true}"#.to_vec()));
    let mut child=McpChild::start_options(&daemon.url,"full",&[
        ("IPHONE_USE_MCP_HOST","workbuddy"),
        ("HTTP_PROXY","http://127.0.0.1:1"),("http_proxy","http://127.0.0.1:1"),
        ("HTTPS_PROXY","http://127.0.0.1:1"),("https_proxy","http://127.0.0.1:1"),
        ("ALL_PROXY","http://127.0.0.1:1"),("all_proxy","http://127.0.0.1:1"),
        ("NO_PROXY",""),("no_proxy","")]);
    child.send(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"ui://iphone-use/frame-v1/600/1"}}));
    let response=child.read_reply(2);
    let body:serde_json::Value=serde_json::from_str(response["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["ok"],true,"{body}");
    assert!(body["frame"]["data"].is_string(),"{body}");
    assert_eq!(daemon.requests.lock().unwrap().len(),2);
}

#[test]
fn capture_privacy_refusal_clears_a_panel_even_when_status_still_says_open() {
    for resource in [false,true] {
        for code in ["screen_private","screen_privacy_changed"] {
            let daemon=ScriptedDaemon::routed("423 Locked",serde_json::json!({"ok":false,"error":code}).to_string().into_bytes(),Duration::ZERO,
                Some(br#"{"ok":true,"drivable":true,"screen_private_secs":0}"#.to_vec()));
            let mut child=McpChild::start_options(&daemon.url,"off",&[("IPHONE_USE_MCP_HOST","workbuddy")]);
            let body=if resource {
                child.send(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"ui://iphone-use/frame-v1/600/1"}}));
                let response=child.read_reply(2);
                serde_json::from_str(response["result"]["contents"][0]["text"].as_str().unwrap()).unwrap()
            } else {
                child.call_tool(2,"phone_screen_frame",serde_json::json!({}))["result"]["structuredContent"].clone()
            };
            assert!(body["private_secs"].is_null(),"status intentionally raced before the private hold");
            assert!(body["frame"].is_null());
            assert_eq!(body["clear_frame"],true,"{body}");
        }
    }
}

#[test]
fn privacy_stop_after_clear_stays_partial_and_never_becomes_retry_safe() {
    for mode in ["full","slim","off"] {
        let body=serde_json::json!({"ok":false,"error":"text_partially_typed","outcome":"partial","retry_safe":false,
            "characters_confirmed":0,"characters_uncertain":0,"characters_total":500,"remaining_text":"字".repeat(500),
            "detail":{"input_error":"input_privacy_changed","clear_applied":true}});
        let daemon=ScriptedDaemon::start("502 Bad Gateway",body.to_string().into_bytes());
        let mut child=McpChild::start_with(&daemon.url,mode);
        let response=child.call_tool(2,"phone_type",serde_json::json!({"text":"字".repeat(500),"clear":true,"observe":false}));
        let result=&response["result"];
        assert_eq!(result["isError"],true,"{response}");
        let text=result["content"].as_array().unwrap().iter().filter_map(|block|block["text"].as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("input_privacy_changed")&&text.contains("clear_applied"),"{response}");
        assert!(text.contains("\"retry_safe\":false"),"{response}");
        if mode!="off" {
            assert_eq!(result["structuredContent"]["outcome"],"partial");
            assert_eq!(result["structuredContent"]["retry_safe"],false);
        }
        assert!(!response.to_string().contains("continue_token"));
        assert_eq!(daemon.requests.lock().unwrap().len(),1,"no replay after a private interruption");
    }
}

#[test]
fn omitted_ui_after_scope_change_keeps_success_and_notice_in_every_output_mode() {
    for mode in ["full","slim","off"] {for observe in [false,true] {
        let daemon=ScriptedDaemon::start("200 OK",serde_json::json!({"ok":true,"transport":"runner","scope_changed":"input_privacy_changed","observation_omitted":true}).to_string().into_bytes());
        let mut child=McpChild::start_with(&daemon.url,mode);
        let response=child.call_tool(2,"phone_tap",serde_json::json!({"x":0.5,"y":0.5,"observe":observe}));
        let result=&response["result"];assert_ne!(result["isError"],true,"the mutation was confirmed: {response}");
        let text=result["content"].as_array().unwrap().iter().filter_map(|c|c["text"].as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("withheld")&&text.contains("phone_status"),"{response}");
        if mode!="off" {assert_eq!(result["structuredContent"]["ok"],true);assert_eq!(result["structuredContent"]["observation_omitted"],true);}
        assert_eq!(daemon.requests.lock().unwrap().len(),1,"no replay or surprise extra observation");
    }}
}

#[test]
fn acknowledged_native_prefix_is_visible_even_when_no_whole_step_completed(){
 for mode in ["full","slim","off"]{
  let body=serde_json::json!({"ok":false,"error":"action_interrupted","outcome":"partial","failed_step_outcome":"partial","batch_outcome":"partially_applied","failed_step":0,"completed":0,"applied_actions":0,"retry_safe":false,"native_mutations_confirmed":1,"native_mutations_uncertain":0,"steps":[]});
  let daemon=ScriptedDaemon::start("409 Conflict",body.to_string().into_bytes());let mut child=McpChild::start_with(&daemon.url,mode);
  let response=child.call_tool(2,"phone_run_steps",serde_json::json!({"steps":[{"kind":"tap_locator","locator":{"label":"Target"}}],"observe":false}));
  let result=&response["result"];assert_eq!(result["isError"],true,"{response}");
  let text=result["content"].as_array().unwrap().iter().filter_map(|c|c["text"].as_str()).collect::<Vec<_>>().join("\n");
  assert!(text.contains("1 acknowledged")&&text.contains("DO NOT replay"),"{response}");
  if mode!="off"{assert_eq!(result["structuredContent"]["native_mutations_confirmed"],1);assert_eq!(result["structuredContent"]["retry_safe"],false);}
  assert_eq!(daemon.requests.lock().unwrap().len(),1);
 }
}

#[test]
fn list_diagnostics_lift_one_image_and_keep_coverage_delivery_in_all_modes() {
    use base64::Engine as _;
    let png =
        base64::engine::general_purpose::STANDARD.encode(include_bytes!("fixtures/recovery.png"));
    for mode in ["full", "slim", "off"] {
        for find in [false, true] {
            for uncertain in [false, true] {
                let mut body = serde_json::json!({"ok":true,"complete":false,"rows":[],"pages":[{"page":1,"rows_seen":0,"new_rows":0}],"swipes":1,"stop_reason":"no_rows","native_mutations_confirmed":1,"native_mutations_uncertain":0,"retry_safe":false,
            "image":{"png_base64":png,"source":"runner-capture","requested_at_ms":10,"received_at_ms":12},"recovery":{"visual_check_required":true,"replay_action":false}});
                body["swipes"] = if uncertain {0}else{1}.into();
                body["native_mutations_confirmed"] = if uncertain { 0 } else { 1 }.into();
                body["native_mutations_uncertain"] = if uncertain { 1 } else { 0 }.into();
                if find {
                    body["ok"] = false.into();
                    body["found"] = false.into();
                    body["error"] = "element_not_found".into();
                }
                let daemon = ScriptedDaemon::start("200 OK", body.to_string().into_bytes());
                let mut mcp = McpChild::start_with(&daemon.url, mode);
                let reply = mcp.call_tool(
                    2,
                    if find {
                        "phone_scroll_find"
                    } else {
                        "phone_collect_list"
                    },
                    if find {
                        serde_json::json!({"label":"missing","max_swipes":0})
                    } else {
                        serde_json::json!({"max_pages":1})
                    },
                );
                let result = &reply["result"];
                assert_eq!(
                    result["isError"].as_bool().unwrap_or(false),
                    find,
                    "{reply}"
                );
                assert_eq!(reply.to_string().matches(&png).count(), 1);
                let blocks = result["content"].as_array().unwrap();
                assert_eq!(blocks.iter().filter(|b| b["type"] == "image").count(), 1);
                let text = blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    text.contains(if uncertain {
                        "1 uncertain"
                    } else {
                        "1 acknowledged"
                    }) && text.contains("do not replay"),
                    "{text}"
                );
                assert!(!text.contains("png_base64"));
                if mode != "off" {
                    assert_eq!(result["structuredContent"]["retry_safe"], false);
                    assert_eq!(result["structuredContent"]["swipes"], if uncertain {0}else{1});
                }
                assert_eq!(daemon.requests.lock().unwrap().len(), 1);
            }
        }
    }
}

#[test]
fn deadline_keeps_partial_collection_rows_in_all_output_modes() {
    for mode in ["full", "slim", "off"] {
        let body = serde_json::json!({"ok":false,"error":"list_deadline","outcome":"partial","retry_safe":false,"stop_reason":"deadline","complete":false,"rows":[{"kind":"Cell","label":"KNOWN_ROW","page":1}],"pages":[{"page":1,"rows_seen":1,"new_rows":1}],"swipes":1,"native_mutations_confirmed":1,"native_mutations_uncertain":0,"coverage":"Rows are historical, incomplete and have no current snapshot","recovery":{"image_omitted":"request_deadline","replay_action":false}});
        let daemon = ScriptedDaemon::start("504 Gateway Timeout", body.to_string().into_bytes());
        let mut mcp = McpChild::start_with(&daemon.url, mode);
        let reply = mcp.call_tool(2, "phone_collect_list", serde_json::json!({"max_pages":3}));
        let result = &reply["result"];
        assert_eq!(result["isError"], true, "{reply}");
        let text = result["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("KNOWN_ROW")
                && text.contains("deadline")
                && text.contains("1 acknowledged")
                && text.contains("do not replay"),
            "{text}"
        );
        assert!(result["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block["type"] != "image"));
        assert_eq!(daemon.requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn readonly_value_text_is_complete_and_single_copy_in_all_modes() {
    let value = "value-root-".to_string() + &"字".repeat(6015);
    for mode in ["full", "slim", "off"] {
        let body = serde_json::json!({"ok":true,"value":value,"source":"ax_value_attribute","complete":true,"truncated":false,"utf16_length":6026,"returned_utf16_length":6026,"limit_utf16":8192});
        let daemon = ScriptedDaemon::start("200 OK", body.to_string().into_bytes());
        let mut mcp =
            McpChild::start_options(&daemon.url, mode, &[("IPHONE_USE_MCP_TOOLS", "readonly")]);
        let reply = mcp.call_tool(
            2,
            "phone_read_value",
            serde_json::json!({"identifier":"field-a"}),
        );
        let result = &reply["result"];
        assert!(!result["isError"].as_bool().unwrap_or(false), "{reply}");
        let text = result["content"][0]["text"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["value"], value);
        assert_eq!(reply.to_string().matches(&value).count(), 1);
        assert!(result["structuredContent"].get("value").is_none());
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0]
            .to_ascii_lowercase()
            .contains("x-phone-control:"));
    }
}

#[test]
fn selected_replacement_forwards_the_original_snapshot_and_unicode_value() {
    let daemon = ScriptedDaemon::start("200 OK", br#"{"ok":true,"native_mutations_confirmed":1}"#.to_vec());
    let mut mcp = McpChild::start(&daemon.url);
    let reply = mcp.call_tool(2, "phone_set_value", serde_json::json!({
        "element":2,"snapshot":"caller-original","value":"替换🙂","observe":false
    }));
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let requests = daemon.requests.lock().unwrap();
    let writes: Vec<_> = requests.iter().filter(|r|r.starts_with("POST /agent/input ")).collect();
    assert_eq!(writes.len(),1,"{requests:?}");
    let body: serde_json::Value = serde_json::from_str(writes[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body,serde_json::json!({"type":"set_value","element":2,"snapshot":"caller-original","value":"替换🙂"}));
}

#[test]
fn selected_replacement_observes_by_default_and_can_clear() {
    let daemon = ScriptedDaemon::start("200 OK", br#"{"ok":true}"#.to_vec());
    let mut mcp = McpChild::start(&daemon.url);
    let reply = mcp.call_tool(2, "phone_set_value", serde_json::json!({
        "element":0,"snapshot":"caller-original","value":""
    }));
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let requests = daemon.requests.lock().unwrap();
    let writes: Vec<_> = requests.iter().filter(|r|r.starts_with("POST /agent/input?")).collect();
    assert_eq!(writes.len(),1,"{requests:?}");
    assert!(writes[0].starts_with("POST /agent/input?return=delta "), "{requests:?}");
    let body: serde_json::Value = serde_json::from_str(writes[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["value"], "");
    assert_eq!(body["snapshot"], "caller-original");
}

#[test]
fn selected_replacement_forwards_explicit_focus_recovery_without_substituting_binding() {
    let daemon = ScriptedDaemon::start("200 OK", br#"{"ok":true}"#.to_vec());
    let mut mcp = McpChild::start(&daemon.url);
    let reply = mcp.call_tool(2, "phone_set_value", serde_json::json!({
        "element":2,"snapshot":"original","value":"恢复后的正文","recover_focus_after_clear":true,"observe":false
    }));
    assert_ne!(reply["result"]["isError"],true,"{reply}");
    let requests=daemon.requests.lock().unwrap();
    let writes:Vec<_>=requests.iter().filter(|r|r.starts_with("POST /agent/input ")).collect();
    assert_eq!(writes.len(),1,"{requests:?}");
    let body:serde_json::Value=serde_json::from_str(writes[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body,serde_json::json!({"type":"set_value","element":2,"snapshot":"original","value":"恢复后的正文","recover_focus_after_clear":true}));
}

#[test]
fn selected_replacement_rejects_missing_binding_or_oversized_value_without_dispatch() {
    let daemon = ScriptedDaemon::start("200 OK", br#"{"ok":true}"#.to_vec());
    let mut mcp = McpChild::start(&daemon.url);
    for (id,args) in [(2,serde_json::json!({"element":0,"snapshot":" ","value":"x","observe":false})),
                      (3,serde_json::json!({"element":0,"snapshot":"original","value":"字".repeat(1001),"observe":false}))] {
        let reply=mcp.call_tool(id,"phone_set_value",args);
        assert_eq!(reply["result"]["isError"],true,"{reply}");
    }
    assert!(!daemon.requests.lock().unwrap().iter().any(|r|r.starts_with("POST /agent/input ")));
}

#[test]
fn selected_replacement_keeps_unknown_delivery_and_never_retries_the_write() {
    let daemon = ScriptedDaemon::start("502 Bad Gateway", br#"{"ok":false,"outcome":"unknown","retry_safe":false,"native_mutations_uncertain":1}"#.to_vec());
    let mut mcp = McpChild::start(&daemon.url);
    let reply=mcp.call_tool(2,"phone_set_value",serde_json::json!({"element":1,"snapshot":"original","value":"replacement","observe":false}));
    assert_eq!(reply["result"]["isError"],true,"{reply}");
    assert_eq!(reply["result"]["structuredContent"]["outcome"],"unknown","{reply}");
    assert_eq!(reply["result"]["structuredContent"]["retry_safe"],false,"{reply}");
    assert_eq!(daemon.requests.lock().unwrap().iter().filter(|r|r.starts_with("POST /agent/input ")).count(),1);
}

#[test]
fn failed_capture_clears_old_pixels_despite_racing_open_status() {
    for status in ["423 Locked", "503 Service Unavailable", "200 Truncated", "200 Empty"] {
        let daemon = ScriptedDaemon::routed(status, if status == "200 Empty" {Vec::new()} else {br#"{"ok":false,"error":"capture_refused"}"#.to_vec()}, Duration::ZERO,
            Some(br#"{"ok":true,"drivable":true,"screen_private_secs":0}"#.to_vec()));
        let mut child = McpChild::start_with(&daemon.url, "off");
        let reply = child.call_tool(2, "phone_screen_frame", serde_json::json!({}));
        let body = &reply["result"]["structuredContent"];
        assert_eq!(body["drivable"], true, "status fixture must remain open: {body}");
        assert!(body["private_secs"].is_null());
        assert!(body["frame"].is_null());
        assert_eq!(body["clear_frame"], true, "{body}");
    }
}
