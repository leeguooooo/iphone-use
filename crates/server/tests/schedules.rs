//! The scheduler end to end: a scripted daemon API plus a fake
//! `iphone-use-mcp`, so the gate, the command line, the owner lease and the
//! run history are exercised without a phone.

mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use server::schedules::{self, Scheduler, SchedulerConfig};
use support::block;

/// A tiny HTTP server: `/agent/status` answers the current scripted body;
/// every request line (and its owner header) is recorded.
struct FakeDaemon {
    url: String,
    status: Arc<Mutex<String>>,
    seen: Arc<Mutex<Vec<String>>>,
}

fn fake_daemon() -> FakeDaemon {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let status = Arc::new(Mutex::new(r#"{"drivable":true,"owner":null}"#.to_string()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (status_c, seen_c) = (status.clone(), seen.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buffer = [0_u8; 8192];
            let Ok(n) = stream.read(&mut buffer) else {
                continue;
            };
            let request = String::from_utf8_lossy(&buffer[..n]).to_string();
            let line = request.lines().next().unwrap_or("").to_string();
            let owner = request
                .lines()
                .find_map(|l| l.strip_prefix("x-phone-owner: "))
                .unwrap_or("")
                .to_string();
            seen_c.lock().unwrap().push(format!("{line} owner={owner}"));
            let body = if line.starts_with("GET /agent/status") {
                status_c.lock().unwrap().clone()
            } else {
                r#"{"ok":true}"#.to_string()
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    FakeDaemon { url, status, seen }
}

/// A stand-in for iphone-use-mcp that logs its arguments and environment.
fn fake_mcp(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("mcp.log");
    let script = dir.join("iphone-use-mcp");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
echo "ARGS $*" >> '{log}'
echo "OWNER $PHONE_REMOTE_OWNER TOKEN ${{PHONE_REMOTE_TOKEN:+set}} URL $PHONE_REMOTE_URL" >> '{log}'
case "$1 $2 $3" in
  "flow validate risky/send") echo '{{"ok":true,"risk":"side_effect"}}' ;;
  "flow validate missing/flow") echo "no such flow" >&2; exit 1 ;;
  "flow validate "*) echo '{{"ok":true,"risk":"read_only"}}' ;;
  "flow run fail/flow") echo '{{"ok":false,"error":"expectation_timeout","failed_step":2}}'; echo "flow failed: boom" >&2; exit 1 ;;
  "flow run "*) echo '{{"ok":true}}' ;;
  *) echo '{{"passed":1,"failed":0}}' ;;
esac
"#,
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

const NINE: u64 = 1_791_363_600; // 2026-10-07 09:00:00 UTC

fn store_with(dir: &std::path::Path, target: &str) -> std::path::PathBuf {
    let path = dir.join("schedules.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "schedules": [{
                "id": "s1", "cron": "0 9 * * *", "kind": "flow", "target": target,
                "inputs": {"query": "Health"}, "confirm_side_effects": false,
                "enabled": true, "window_mins": 60, "created_at": NINE - 86_400,
                "next_run_at": NINE
            }],
            "runs": [], "seq": 0
        })
        .to_string(),
    )
    .unwrap();
    path
}

fn scheduler(
    dir: &std::path::Path,
    daemon: &FakeDaemon,
    mcp: std::path::PathBuf,
    clock: Arc<AtomicU64>,
) -> Arc<Scheduler> {
    Scheduler::with_clock(
        SchedulerConfig {
            store_path: dir.join("schedules.json"),
            artifacts_dir: dir.join("runs"),
            self_url: daemon.url.clone(),
            credential: Some("secret-token".into()),
            mcp: Some(mcp),
            notify: false,
        },
        Arc::new(move || clock.load(Ordering::SeqCst)),
        schedules::local_time,
    )
}

fn last_run(s: &Scheduler) -> serde_json::Value {
    s.runs(None).unwrap()["runs"][0].clone()
}

#[test]
fn a_due_flow_runs_through_iphone_use_mcp_as_its_own_owner_and_releases_the_phone() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = fake_daemon();
    let (mcp, log) = fake_mcp(dir.path());
    store_with(dir.path(), "system/spotlight-search");
    let clock = Arc::new(AtomicU64::new(NINE + 30));
    let s = scheduler(dir.path(), &daemon, mcp, clock);
    block(s.tick());
    let run = last_run(&s);
    assert_eq!(run["state"], "passed", "{run}");
    assert_eq!(run["summary"], "flow passed");
    let log = std::fs::read_to_string(log).unwrap();
    assert!(
        log.contains("ARGS flow run system/spotlight-search --input query=Health --artifacts-dir"),
        "{log}"
    );
    assert!(log.contains("OWNER schedule-s1 TOKEN set"), "{log}");
    assert!(
        !log.contains("secret-token"),
        "the token never reaches a log"
    );
    let seen = daemon.seen.lock().unwrap().join("\n");
    assert!(
        seen.contains("POST /agent/owner HTTP/1.1 owner=schedule-s1"),
        "lease handed back: {seen}"
    );
    // The next occurrence moved to tomorrow, and the history survives a restart.
    let reloaded = scheduler(
        dir.path(),
        &daemon,
        dir.path().join("x"),
        Arc::new(AtomicU64::new(NINE + 60)),
    );
    assert_eq!(last_run(&reloaded)["state"], "passed");
    let next = reloaded.list()["schedules"][0]["next_run_at"]
        .as_u64()
        .unwrap();
    assert!(
        next > NINE + 60 && next <= NINE + 2 * 86_400,
        "moved on to the next 9:00 local: {next}"
    );
}

#[test]
fn a_busy_phone_postpones_a_locked_one_waits_and_a_failure_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = fake_daemon();
    let (mcp, _log) = fake_mcp(dir.path());
    store_with(dir.path(), "fail/flow");
    let clock = Arc::new(AtomicU64::new(NINE + 30));
    let s = scheduler(dir.path(), &daemon, mcp, clock.clone());

    *daemon.status.lock().unwrap() = r#"{"drivable":true,"owner":"claude-42"}"#.into();
    block(s.tick());
    let run = last_run(&s);
    assert_eq!(run["state"], "postponed", "{run}");
    assert!(run["reason"].as_str().unwrap().contains("claude-42"));

    // Not retried before its next attempt time.
    block(s.tick());
    assert_eq!(last_run(&s)["attempts"], 1);

    *daemon.status.lock().unwrap() =
        r#"{"drivable":false,"device_state":"locked","owner":null}"#.into();
    clock.store(NINE + 30 + 181, Ordering::SeqCst);
    block(s.tick());
    assert_eq!(last_run(&s)["state"], "waiting_unlock");

    *daemon.status.lock().unwrap() = r#"{"drivable":true,"owner":null}"#.into();
    clock.store(NINE + 30 + 181 + 61, Ordering::SeqCst);
    block(s.tick());
    let run = last_run(&s);
    assert_eq!(run["state"], "failed", "{run}");
    assert_eq!(run["exit_code"], 1);
    assert_eq!(run["error"], "flow failed: boom");
    assert_eq!(run["summary"], "flow failed: expectation_timeout at step 2");
    let artifacts = std::path::PathBuf::from(run["artifacts"].as_str().unwrap());
    assert!(artifacts.join("stdout.txt").exists());
}

#[test]
fn the_api_validates_with_iphone_use_mcp_and_refuses_unconfirmed_side_effects() {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let dir = tempfile::tempdir().unwrap();
    let daemon = fake_daemon();
    let (mcp, _log) = fake_mcp(dir.path());
    let s = scheduler(dir.path(), &daemon, mcp, Arc::new(AtomicU64::new(NINE)));
    schedules::install(s.clone());
    let app = server::http::router(support::build_state(None));
    let call = |method: &str, path: &str, body: Option<serde_json::Value>, control: bool| {
        let mut request = axum::http::Request::builder().method(method).uri(path);
        if control {
            request = request.header("x-phone-control", "1");
        }
        let request = request
            .header("content-type", "application/json")
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        let app = app.clone();
        block(async move {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status().as_u16();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default(),
            )
        })
    };
    let create = |target: &str, confirm: bool| serde_json::json!({"cron": "0 9 * * *", "kind": "flow", "target": target, "confirm_side_effects": confirm});

    let (status, body) = call(
        "POST",
        "/agent/schedules",
        Some(create("system/x", false)),
        false,
    );
    assert_eq!(status, 403, "a mutation needs X-Phone-Control: {body}");
    let (status, body) = call(
        "POST",
        "/agent/schedules",
        Some(create("risky/send", false)),
        true,
    );
    assert_eq!(
        (status, body["error"].as_str()),
        (400, Some("confirm_required")),
        "{body}"
    );
    let (status, body) = call(
        "POST",
        "/agent/schedules",
        Some(create("missing/flow", false)),
        true,
    );
    assert_eq!(
        (status, body["error"].as_str()),
        (400, Some("invalid_target")),
        "{body}"
    );
    assert!(body["message"].as_str().unwrap().contains("no such flow"));
    let (status, body) = call(
        "POST",
        "/agent/schedules",
        Some(serde_json::json!({"cron":"bad","kind":"flow","target":"x/y"})),
        true,
    );
    assert_eq!(
        (status, body["error"].as_str()),
        (400, Some("invalid_cron")),
        "{body}"
    );

    let (status, body) = call(
        "POST",
        "/agent/schedules",
        Some(create("risky/send", true)),
        true,
    );
    assert_eq!(status, 201, "{body}");
    let id = body["schedule"]["id"].as_str().unwrap().to_string();
    let (status, body) = call("GET", "/agent/schedules", None, false);
    assert_eq!(status, 200);
    assert_eq!(body["schedules"][0]["id"], id.as_str());
    assert!(body["schedules"][0]["next_run_at"].as_u64().is_some());

    let (status, body) = call("POST", &format!("/agent/schedules/{id}/run"), None, true);
    assert_eq!(status, 202, "{body}");
    let (status, body) = call("POST", &format!("/agent/schedules/{id}/run"), None, true);
    assert_eq!(
        (status, body["error"].as_str()),
        (409, Some("run_open")),
        "{body}"
    );
    let (status, body) = call(
        "PATCH",
        &format!("/agent/schedules/{id}"),
        Some(serde_json::json!({"enabled": false})),
        true,
    );
    assert_eq!(
        (status, body["schedule"]["enabled"].as_bool()),
        (200, Some(false)),
        "{body}"
    );
    let (status, body) = call("GET", &format!("/agent/schedules/{id}/runs"), None, false);
    assert_eq!(
        (status, body["runs"][0]["manual"].as_bool()),
        (200, Some(true)),
        "{body}"
    );
    let (status, _) = call("DELETE", &format!("/agent/schedules/{id}"), None, true);
    assert_eq!(status, 200);
    let (status, _) = call("DELETE", &format!("/agent/schedules/{id}"), None, true);
    assert_eq!(status, 404);
    let (status, _) = call("GET", "/agent/schedules/..%2Fx/runs", None, false);
    assert_eq!(status, 404);
}
