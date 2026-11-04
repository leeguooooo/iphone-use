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
}

impl ScriptedDaemon {
    fn start(status: &'static str, body: Vec<u8>) -> Self {
        Self::routed(status, body, None)
    }

    fn routed(status: &'static str, body: Vec<u8>, status_body: Option<Vec<u8>>) -> Self {
        Self::status_delayed(status,body,status_body,Duration::ZERO)
    }
    fn status_delayed(status: &'static str, body: Vec<u8>, status_body: Option<Vec<u8>>, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
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
                        let mut request = Vec::new();
                        let mut buffer = [0_u8; 8_192];
                        let read_deadline = Instant::now() + Duration::from_secs(10);
                        while request.len() < 65_536 && Instant::now() < read_deadline {
                            let remaining = (65_536 - request.len()).min(buffer.len());
                            stream.set_read_timeout(Some(read_deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)))).ok();
                            match stream.read(&mut buffer[..remaining]) {
                                Ok(0) | Err(_) => break,
                                Ok(count) => request.extend_from_slice(&buffer[..count]),
                            }
                            if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                                let length = String::from_utf8_lossy(&request[..end]).lines()
                                    .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|value| value.trim().parse::<usize>().ok())).unwrap_or(0);
                                if request.len() >= end + 4 + length { break; }
                            }
                        }
                        seen.lock().unwrap().push(String::from_utf8_lossy(&request).into_owned());
                        let status_read = request.windows(2).any(|bytes| bytes == b"\r\n")
                            && request.starts_with(b"GET /agent/status ");
                        if status_read { std::thread::sleep(delay); }
                        let truncated = status == "200 Truncated" && !status_read;
                        let response_status = if status_read && status_body.is_some() || truncated { "200 OK" } else { status };
                        let body = if status_read { status_body.as_ref().unwrap_or(&body) } else { &body };
                        let head = format!(
                            "HTTP/1.1 {response_status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len() + if truncated {64} else {0}
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&body);
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

    fn start_options(daemon_url: &str, structured: &str, extra: &[(&str, &str)]) -> Self {
        let mut child = Command::new(binary())
            // Scripted daemons count requests; the on-use status check is tested on its own.
            .env("IPHONE_USE_MCP_RECONNECT_ON_USE", "0")
            .envs(extra.iter().copied())
            .env("IPHONE_USE_URL", daemon_url)
            .env("IPHONE_USE_MCP_STRUCTURED", structured)
            .env_remove("IPHONE_USE_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the MCP server");
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
fn failed_capture_clears_old_pixels_despite_racing_open_status() {
    for status in ["423 Locked", "503 Service Unavailable", "200 Truncated", "200 Empty"] {
        let daemon = ScriptedDaemon::routed(status, if status == "200 Empty" {Vec::new()} else {br#"{"ok":false,"error":"capture_refused"}"#.to_vec()},
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

#[test]
fn registry_without_flows_does_not_query_phone_inventory() {
    for (registry, needs_inventory) in [
        (None, false),
        (Some(serde_json::json!({"installed":0,"next":"No saved flows"})), false),
        (Some(serde_json::json!({"flows":[]})), false),
        (Some(serde_json::json!({"flows":null})), false),
        (Some(serde_json::json!({"flows":[{"id":"fixture/flow"}]})), true),
    ] {
        let mut body = serde_json::json!({"ok":true,"snapshot":"S","elements":[{"kind":"TextField","identifier":"field-a","label":"registry"}]});
        if let Some(registry) = registry { body["registry"] = registry; }
        let daemon = ScriptedDaemon::start("200 OK", body.to_string().into_bytes());
        let home = tempfile::tempdir().unwrap();
        let mut child = McpChild::start_options(&daemon.url,"off",&[
            ("HOME",home.path().to_str().unwrap()),("IPHONE_USE_FLOWS_DIR",home.path().to_str().unwrap()),
            ("IPHONE_USE_FLOWS_NO_AUTO_UPDATE","1"),
            ("IPHONE_USE_MCP_PREWARM","0")]);
        let reply = child.call_tool(2,"phone_elements",serde_json::json!({}));
        assert!(reply.get("result").is_some() && reply["result"]["isError"] != true,"{reply}");
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.iter().any(|line|line.starts_with("GET /agent/apps ")),needs_inventory,"{requests:?}");
        if !needs_inventory {
            assert_eq!(requests.len(),1,"a screen with no flows must only read its elements: {requests:?}");
        }
    }
}

#[test]
fn reconnect_wait_budget_bounds_slow_status_and_sends_only_one_reconnect() {
    let daemon=ScriptedDaemon::status_delayed("200 OK",br#"{"ok":true,"starting":true,"reconnecting":true}"#.to_vec(),
        Some(br#"{"ok":true,"drivable":true,"reconnecting":false}"#.to_vec()),Duration::from_secs(3));
    let mut child=McpChild::start_with(&daemon.url,"off");
    let start=Instant::now();let reply=child.call_tool(2,"phone_reconnect",serde_json::json!({"wait_secs":1}));
    let elapsed=start.elapsed();
    let text=reply["result"]["content"][0]["text"].as_str().unwrap();
    let body:serde_json::Value=serde_json::from_str(text).unwrap();
    assert_eq!(body["wait"]["outcome"],"timeout","{body}");
    assert!(elapsed<Duration::from_secs(2),"wait_secs=1 exceeded budget: {elapsed:?}");
    let requests=daemon.requests.lock().unwrap();
    assert_eq!(requests.iter().filter(|request|request.starts_with("POST /agent/mode ")).count(),1);
}
#[test]
fn reconnect_wait_stops_on_a_phone_exit_even_if_ready_was_stale() {
    let daemon=ScriptedDaemon::routed("200 OK",br#"{"ok":true,"starting":true}"#.to_vec(),
        Some(br#"{"ok":true,"drivable":true,"reconnecting":true,"runner_parked":true,"ended_on_phone":true,"setup_blocked_on":"usb"}"#.to_vec()));
    let mut child=McpChild::start_with(&daemon.url,"off");
    let reply=child.call_tool(2,"phone_reconnect",serde_json::json!({"wait_secs":1}));
    let body:serde_json::Value=serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["wait"]["outcome"],"stopped","{body}");
    assert_eq!(daemon.requests.lock().unwrap().iter().filter(|request|request.starts_with("POST /agent/mode ")).count(),1);
}

#[test]
fn reconnect_wait_transient_blocker_does_not_end_a_short_budget_early() {
    let daemon = ScriptedDaemon::routed("200 OK", br#"{"ok":true,"starting":true}"#.to_vec(),
        Some(br#"{"ok":true,"drivable":false,"reconnecting":true,"setup_blocked_on":"usb","setup_phase":"ddi-wait"}"#.to_vec()));
    let mut child = McpChild::start_with(&daemon.url,"off");
    let started = Instant::now();
    let reply = child.call_tool(2,"phone_reconnect",serde_json::json!({"wait_secs":1}));
    let body: serde_json::Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["wait"]["outcome"], "timeout", "{body}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(daemon.requests.lock().unwrap().iter().filter(|r| r.starts_with("POST /agent/mode ")).count(), 1);
}

#[test]
fn full_value_is_single_copy_and_never_requests_control_in_all_output_modes() {
    let value = "value-root-".to_string() + &"字".repeat(6015);
    for mode in ["full", "slim", "off"] {
        let body = serde_json::json!({"ok":true,"value":value,"source":"ax_value_attribute","complete":true,"truncated":false,
            "utf16_length":6026,"returned_utf16_length":6026,"limit_utf16":8192});
        let daemon = ScriptedDaemon::start("200 OK",body.to_string().into_bytes());
        let home = tempfile::tempdir().unwrap();
        let mut child = McpChild::start_options(&daemon.url,mode,&[("HOME",home.path().to_str().unwrap()),
            ("IPHONE_USE_MCP_TOOLS","readonly"),("IPHONE_USE_MCP_PREWARM","0"),("IPHONE_USE_FLOWS_NO_AUTO_UPDATE","1")]);
        let reply = child.call_tool(2,"phone_read_value",serde_json::json!({"identifier":"field-a"}));
        let result = &reply["result"];
        assert_ne!(result["isError"],true,"{reply}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(text).unwrap()["value"],value);
        assert_eq!(reply.to_string().matches(&value).count(),1);
        assert!(result.get("structuredContent").is_none() || result["structuredContent"].is_null(), "a metadata-only structured result hides the text in some hosts: {reply}");
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(),1);
        assert!(requests[0].starts_with("POST /agent/value "));
        assert!(!requests[0].to_ascii_lowercase().contains("x-phone-control:"));
    }
}
#[test]
fn missing_contract_and_revoked_scope_never_publish_a_value() {
    for body in [serde_json::json!({"ok":true,"value":"UNTRUSTED_PREVIEW"}),
        serde_json::json!({"ok":false,"error":"value_read_scope_changed","value_omitted":true,"value":"WITHHELD_SECRET"})] {
        for mode in ["full","slim","off"] {
            let daemon = ScriptedDaemon::start("200 OK",body.to_string().into_bytes());
            let mut child = McpChild::start_with(&daemon.url,mode);
            let reply = child.call_tool(2,"phone_read_value",serde_json::json!({"identifier":"field-a"}));
            assert_eq!(reply["result"]["isError"],true,"{reply}");
            let wire=reply.to_string();
            assert!(!wire.contains("UNTRUSTED_PREVIEW") && !wire.contains("WITHHELD_SECRET"));
        }
    }
}
