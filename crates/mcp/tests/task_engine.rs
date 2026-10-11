//! The task engine against a fake daemon and a fake assist model: steps, the
//! confirm gate before a payment, and the summary.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iphone_use_mcp::client::DaemonClient;
use iphone_use_mcp::task::{self, AssistEndpoint, ModelsConfig, TaskRequest};
use serde_json::{json, Value};

/// One task at a time per process: the tests take turns.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A tiny HTTP server: `respond(method, path, body) -> (status, json body)`.
fn serve(respond: impl Fn(&str, &str, &str) -> (u16, String) + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut data = Vec::new();
            let mut buffer = [0u8; 65536];
            // Read headers, then the body by Content-Length.
            loop {
                let Ok(n) = stream.read(&mut buffer) else { break };
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buffer[..n]);
                let text = String::from_utf8_lossy(&data);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&data).to_string();
            let line = text.lines().next().unwrap_or("").to_string();
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            let (status, reply) = respond(&method, &path, &body);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .as_bytes(),
            );
        }
    });
    url
}

struct Phone {
    paid: Mutex<bool>,
    inputs: Mutex<Vec<Value>>,
}

fn fake_daemon(phone: Arc<Phone>) -> String {
    serve(move |method, path, body| {
        let reply = match (method, path.split('?').next().unwrap_or("")) {
            ("GET", "/agent/status") => json!({"ok": true, "drivable": true, "backend": "direct"}),
            ("GET", "/agent/elements") => {
                if *phone.paid.lock().unwrap() {
                    json!({"snapshot": "s2", "elements": [
                        {"kind": "Application", "label": "拼多多"},
                        {"kind": "StaticText", "label": "支付成功"}
                    ]})
                } else {
                    json!({"snapshot": "s1", "elements": [
                        {"kind": "Application", "label": "拼多多"},
                        {"kind": "StaticText", "label": "凑单团价 ¥55.51"},
                        {"kind": "Button", "label": "确认支付 ¥55.51", "rect": [20, 800, 400, 50]}
                    ]})
                }
            }
            ("POST", "/agent/input") => {
                let action: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                if action["type"] == "tap" {
                    let progress = task::get("current", None).unwrap();
                    assert_eq!(progress["phase"], "acting");
                    assert_eq!(progress["planned_action"]["action"], "CLICK");
                    assert!(progress["planned_action"]["target"].as_str().unwrap().contains("确认支付"));
                    assert!(progress["planned_action"].get("text").is_none(), "live progress must not expose typed content");
                    *phone.paid.lock().unwrap() = true;
                }
                phone.inputs.lock().unwrap().push(action);
                json!({"ok": true})
            }
            ("GET", "/agent/screenshot") => return (200, "PNG".into()),
            ("POST", "/agent/owner") => json!({"ok": true, "owner": null}),
            _ => return (404, json!({"ok": false}).to_string()),
        };
        (200, reply.to_string())
    })
}

/// An OpenAI-format model: taps the payment button, then says DONE, and
/// writes the summary.
fn fake_model(phone: Arc<Phone>) -> String {
    serve(move |_, _, body| {
        let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let system = request["messages"][0]["content"].as_str().unwrap_or("");
        let content = if system.contains("saved phone automation") {
            r#"{"flow": null}"#.to_string()
        } else if system.contains("operate a real iPhone") {
            if *phone.paid.lock().unwrap() {
                r#"{"operation": "DONE", "target": null, "text": null, "reason": "paid"}"#.to_string()
            } else {
                r#"{"operation": "CLICK", "target": 0, "text": null, "reason": "pay as asked"}"#.to_string()
            }
        } else {
            json!({"goal_achieved": *phone.paid.lock().unwrap(), "summary": "已用免密支付付款 55.51 元，显示支付成功。"}).to_string()
        };
        (200, json!({"choices": [{"message": {"content": content}}]}).to_string())
    })
}

fn request(model_url: &str) -> TaskRequest {
    TaskRequest {
        goal: "在拼多多买下这把指甲刀".into(),
        models: Some(ModelsConfig {
            jev: None,
            text: None,
            assist: vec![AssistEndpoint {
                format: task::Format::Openai,
                base_url: model_url.to_string(),
                key: String::new(),
                model: "fake".into(),
            }],
        }),
        ..Default::default()
    }
}

async fn wait_for(id: &str, status: &str) -> Value {
    for _ in 0..200 {
        let task = task::get(id, None).unwrap();
        if task["status"] == status {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("task never reached {status}: {}", task::get(id, None).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_without_the_requested_result_is_not_reported_as_success() {
    let _serial = SERIAL.lock().await;
    let flows = tempfile::tempdir().unwrap();
    std::env::set_var("IPHONE_USE_FLOWS_DIR", flows.path());
    let phone = Arc::new(Phone { paid: Mutex::new(false), inputs: Mutex::new(Vec::new()) });
    let daemon_url = fake_daemon(phone.clone());
    let model = serve(move |_, _, body| {
        let body: Value = serde_json::from_str(body).unwrap();
        let system = body["messages"][0]["content"].as_str().unwrap_or("");
        let content = if system.contains("operate a real iPhone") {
            r#"{"operation":"DONE","target":null}"#
        } else {
            r#"{"goal_achieved":false,"summary":"没有显示付款结果，任务未完成。"}"#
        };
        (200, json!({"choices":[{"message":{"content":content}}]}).to_string())
    });
    let id = task::start(request(&model), |owner| DaemonClient::new(daemon_url.clone(), None).with_owner(owner)).ok().unwrap();
    let blocked = wait_for(&id, "blocked").await;
    assert_eq!(blocked["summary"], "没有显示付款结果，任务未完成。");
    assert!(phone.inputs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicitly_opened_app_can_start_fresh_without_changing_the_default() {
    let _serial = SERIAL.lock().await;
    let flows = tempfile::tempdir().unwrap();
    std::env::set_var("IPHONE_USE_FLOWS_DIR", flows.path());
    for fresh in [false, true] {
        let phone = Arc::new(Phone { paid: Mutex::new(false), inputs: Mutex::new(Vec::new()) });
        let daemon_url = fake_daemon(phone.clone());
        let model = serve(|_, _, body| {
            let body: Value = serde_json::from_str(body).unwrap();
            let system = body["messages"][0]["content"].as_str().unwrap_or("");
            let content = if system.contains("operate a real iPhone") {
                r#"{"operation":"DONE","target":null}"#
            } else {
                r#"{"goal_achieved":false,"summary":"Fixture did not achieve the goal."}"#
            };
            (200, json!({"choices":[{"message":{"content":content}}]}).to_string())
        });
        let mut req = request(&model);
        req.app = Some("com.apple.Preferences".into());
        req.app_fresh = fresh;
        let id = task::start(req, |owner| DaemonClient::new(daemon_url.clone(), None).with_owner(owner)).ok().unwrap();
        wait_for(&id, "blocked").await;
        let inputs = phone.inputs.lock().unwrap();
        assert_eq!(inputs.as_slice(), &[json!({"type":"launch_app","bundle":"com.apple.Preferences","fresh":fresh})]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declining_a_flow_ends_the_task_without_step_by_step_fallback() {
    let _serial = SERIAL.lock().await;
    let flows = tempfile::tempdir().unwrap();
    std::env::set_var("IPHONE_USE_FLOWS_DIR", flows.path());
    let file = flows.path().join("fixture.json");
    std::fs::write(&file, json!({
        "version": 1, "name": "Synthetic confirmation", "app": "com.apple.Preferences",
        "risk": "side_effect", "steps": [{"kind": "shortcut", "name": "home"}]
    }).to_string()).unwrap();
    iphone_use_mcp::registry::add(&file, "fixture/confirm").unwrap();
    let phone = Arc::new(Phone { paid: Mutex::new(false), inputs: Mutex::new(Vec::new()) });
    let daemon_url = fake_daemon(phone.clone());
    let decisions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = decisions.clone();
    let model = serve(move |_, _, body| {
        let body: Value = serde_json::from_str(body).unwrap();
        let system = body["messages"][0]["content"].as_str().unwrap_or("");
        let content = if system.contains("saved phone automation") {
            r#"{"flow":"fixture/confirm","inputs":{}}"#
        } else if system.contains("operate a real iPhone") {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            r#"{"operation":"CLICK","target":0}"#
        } else { r#"{"goal_achieved":false,"summary":"用户没有批准，未执行。"}"# };
        (200, json!({"choices":[{"message":{"content":content}}]}).to_string())
    });
    let id = task::start(request(&model), |owner| DaemonClient::new(daemon_url.clone(), None).with_owner(owner)).ok().unwrap();
    assert_eq!(wait_for(&id, "needs_confirm").await["confirm"]["action"], "RUN_FLOW");
    task::answer(&id, false).unwrap();
    wait_for(&id, "blocked").await;
    assert_eq!(decisions.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(phone.inputs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_cancels_a_waiting_model_without_a_late_phone_action() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let _serial = SERIAL.lock().await;
    let flows = tempfile::tempdir().unwrap();
    std::env::set_var("IPHONE_USE_FLOWS_DIR", flows.path());
    let phone = Arc::new(Phone { paid: Mutex::new(false), inputs: Mutex::new(Vec::new()) });
    let daemon_url = fake_daemon(phone.clone());
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let model_entered = entered.clone();
    let model_release = release.clone();
    let model_url = serve(move |_, _, _| {
        model_entered.store(true, Ordering::SeqCst);
        while !model_release.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        (200, json!({"choices":[{"message":{"content":"{\"operation\":\"CLICK\",\"target\":0}"}}]}).to_string())
    });
    let id = task::start(request(&model_url), |owner| DaemonClient::new(daemon_url.clone(), None).with_owner(owner)).ok().unwrap();
    let started = tokio::time::timeout(Duration::from_secs(5), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await;
    if started.is_err() {
        release.store(true, Ordering::SeqCst);
    }
    started.unwrap();
    task::cancel(&id).unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(2), wait_for(&id, "cancelled")).await;
    // Always unblock the fake server before asserting, even if cancellation failed.
    release.store(true, Ordering::SeqCst);
    let stopped = stopped.expect("Stop waited for the model to answer");
    assert!(stopped["confirm"].is_null());
    assert!(phone.inputs.lock().unwrap().is_empty(), "Stop must not tap a late answer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payment_waits_for_the_owner_then_runs_once_and_is_summarized() {
    let _serial = SERIAL.lock().await;
    // No saved flows anywhere.
    let flows = tempfile::tempdir().unwrap();
    std::env::set_var("IPHONE_USE_FLOWS_DIR", flows.path());

    // Approved.
    let phone = Arc::new(Phone { paid: Mutex::new(false), inputs: Mutex::new(Vec::new()) });
    let daemon_url = fake_daemon(phone.clone());
    let model_url = fake_model(phone.clone());
    let id = task::start(request(&model_url), |owner| DaemonClient::new(daemon_url.clone(), None).with_owner(owner))
        .ok()
        .unwrap();
    let waiting = wait_for(&id, "needs_confirm").await;
    assert_eq!(waiting["confirm"]["target"], "确认支付 ¥55.51", "{waiting}");
    assert!(phone.inputs.lock().unwrap().is_empty(), "nothing tapped before the owner answers");
    assert!(matches!(task::start(request(&model_url), |o| DaemonClient::new(daemon_url.clone(), None).with_owner(o)), Err(task::StartError::Busy(_))));
    task::answer(&id, true).unwrap();
    let done = wait_for(&id, "done").await;
    let taps: Vec<Value> = phone.inputs.lock().unwrap().iter().filter(|a| a["type"] == "tap").cloned().collect();
    assert_eq!(taps.len(), 1, "the approved step ran exactly once: {taps:?}");
    assert_eq!(taps[0]["element"], 2, "the payment button's tree row");
    let steps = done["steps"].as_array().unwrap();
    assert!(steps.iter().any(|s| s["confirmed"] == true && s["by"] == "assist"), "{done}");
    assert_eq!(done["summary"], "已用免密支付付款 55.51 元，显示支付成功。");
    assert!(done["confirm"].is_null());

    // Declined: nothing is tapped and the task ends blocked.
    let phone = Arc::new(Phone { paid: Mutex::new(false), inputs: Mutex::new(Vec::new()) });
    let daemon_url = fake_daemon(phone.clone());
    let model_url = fake_model(phone.clone());
    let id = task::start(request(&model_url), |owner| DaemonClient::new(daemon_url.clone(), None).with_owner(owner))
        .ok()
        .unwrap();
    wait_for(&id, "needs_confirm").await;
    task::answer(&id, false).unwrap();
    let blocked = wait_for(&id, "blocked").await;
    assert!(phone.inputs.lock().unwrap().iter().all(|a| a["type"] != "tap"), "declined means not tapped");
    assert!(blocked["error"].as_str().unwrap().contains("did not allow"), "{blocked}");
    assert!(task::list()["tasks"].as_array().unwrap().len() >= 2);
}

/// `free` picks from the gateway's free list: a gone model (404) and a
/// rate-limited one (429) are skipped, a vision model is preferred for the
/// screenshot, and the summary comes from whichever free model answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_free_model_falls_through_the_gateways_free_list() {
    let _serial = SERIAL.lock().await;
    let flows = tempfile::tempdir().unwrap();
    std::env::set_var("IPHONE_USE_FLOWS_DIR", flows.path());
    let phone = Arc::new(Phone { paid: Mutex::new(true), inputs: Mutex::new(Vec::new()) });
    let daemon_url = fake_daemon(phone.clone());
    let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let asked_c = asked.clone();
    let gateway = serve(move |method, path, body| {
        if method == "GET" && path == "/models" {
            return (200, json!({"data": [
                {"id": "meta/llama-guard:free", "architecture": {"input_modalities": ["text", "image"]}},
                {"id": "gone/model:free", "architecture": {"input_modalities": ["text", "image"]}},
                {"id": "busy/model:free", "architecture": {"input_modalities": ["text", "image"]}},
                {"id": "invalid/model:free", "architecture": {"input_modalities": ["text", "image"]}},
                {"id": "text-only/model:free", "architecture": {"input_modalities": ["text"]}},
                {"id": "good/model:free", "architecture": {"input_modalities": ["text", "image"]}},
                {"id": "paid/model", "architecture": {"input_modalities": ["text", "image"]}}
            ]}).to_string());
        }
        let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let model = request["model"].as_str().unwrap_or("").to_string();
        asked_c.lock().unwrap().push(model.clone());
        match model.as_str() {
            "gone/model:free" => (404, json!({"error": "unavailable_model"}).to_string()),
            "busy/model:free" => (429, json!({"error": "rate limited"}).to_string()),
            "invalid/model:free" => (200, json!({"choices": [{"message": {"content": "User Safety: unsafe"}}]}).to_string()),
            "good/model:free" => {
                let system = request["messages"][0]["content"].as_str().unwrap_or("");
                let content = if system.contains("operate a real iPhone") {
                    r#"{"operation":"DONE","target":null,"text":null,"reason":"already paid"}"#
                } else {
                    r#"{"goal_achieved":true,"summary":"已经付好了。"}"#
                };
                (200, json!({"choices": [{"message": {"content": content}}]}).to_string())
            }
            _ => (500, "{}".to_string()),
        }
    });
    let mut request = request(&gateway);
    request.models.as_mut().unwrap().assist[0].model = "free".into();
    let id = task::start(request, |owner| DaemonClient::new(daemon_url.clone(), None).with_owner(owner)).ok().unwrap();
    let done = wait_for(&id, "done").await;
    assert_eq!(done["summary"], "已经付好了。", "{done}");
    assert_eq!(done["models_used"]["assist"], "good/model:free");
    let asked = asked.lock().unwrap().clone();
    assert!(!asked.iter().any(|m| m == "paid/model"), "never a paid model: {asked:?}");
    assert!(!asked.iter().any(|m| m == "meta/llama-guard:free"), "a safety classifier cannot operate a phone: {asked:?}");
    assert!(!asked.iter().any(|m| m == "text-only/model:free"), "the screenshot step skips text-only models: {asked:?}");
    let first_good = asked.iter().position(|m| m == "good/model:free").unwrap();
    assert_eq!(&asked[..first_good], ["gone/model:free", "busy/model:free", "invalid/model:free"], "{asked:?}");
    assert!(asked[first_good..].iter().all(|m| m == "good/model:free"), "the one that answered is tried first afterwards: {asked:?}");
}
