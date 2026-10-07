//! `iphone-use-mcp schedule …` — the daemon's scheduled flows and suites,
//! from the command line (`/agent/schedules`).

use crate::client::{DaemonClient, DaemonResponse};
use anyhow::{bail, Result};
use reqwest::Method;

fn answer(response: DaemonResponse) -> Result<serde_json::Value> {
    let json = response.json.clone().unwrap_or_default();
    if response.status == reqwest::StatusCode::NOT_FOUND && response.json.is_none() {
        bail!("this daemon has no schedules API (it predates them); upgrade with `iphone-use upgrade`");
    }
    if !response.status.is_success() {
        let why = json
            .get("message")
            .or_else(|| json.get("error"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| response.preview());
        bail!("{why} (HTTP {})", response.status.as_u16());
    }
    Ok(json)
}

fn when(unix: Option<u64>) -> String {
    let Some(unix) = unix else {
        return "—".into();
    };
    // Local wall time without pulling in a date crate.
    let t = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

pub struct AddOptions {
    pub cron: String,
    pub flow: Option<String>,
    pub test: Option<String>,
    pub inputs: Vec<String>,
    pub confirm: bool,
    pub name: Option<String>,
    pub webhook: Option<String>,
    pub window_mins: Option<u32>,
}

pub async fn add(daemon: &DaemonClient, options: AddOptions) -> Result<()> {
    let (kind, target) = match (options.flow, options.test) {
        (Some(flow), None) => {
            // A local file is sent as an absolute path; a registry id as is.
            let path = std::path::Path::new(&flow);
            let target = if path.exists() {
                std::fs::canonicalize(path)?.display().to_string()
            } else {
                flow
            };
            ("flow", target)
        }
        (None, Some(test)) => ("test", std::fs::canonicalize(&test)?.display().to_string()),
        _ => bail!("give exactly one of --flow <id|file> or --test <suite file>"),
    };
    let mut inputs = serde_json::Map::new();
    for assignment in &options.inputs {
        let Some((name, value)) = assignment.split_once('=') else {
            bail!("--input must be NAME=VALUE");
        };
        inputs.insert(name.to_string(), serde_json::json!(value));
    }
    let mut body = serde_json::json!({
        "cron": options.cron,
        "kind": kind,
        "target": target,
        "confirm_side_effects": options.confirm,
    });
    if !inputs.is_empty() {
        body["inputs"] = serde_json::Value::Object(inputs);
    }
    if let Some(name) = options.name {
        body["name"] = serde_json::json!(name);
    }
    if let Some(hook) = options.webhook {
        body["webhook"] = serde_json::json!(hook);
    }
    if let Some(window) = options.window_mins {
        body["window_mins"] = serde_json::json!(window);
    }
    let json = answer(
        daemon
            .request_json(Method::POST, "/agent/schedules", Some(&body))
            .await?,
    )?;
    let schedule = &json["schedule"];
    println!(
        "scheduled {} ({} {}), next run {}",
        schedule["id"].as_str().unwrap_or("?"),
        kind,
        schedule["target"].as_str().unwrap_or("?"),
        when(schedule["next_run_at"].as_u64())
    );
    Ok(())
}

pub async fn list(daemon: &DaemonClient, json_out: bool) -> Result<()> {
    let json = answer(
        daemon
            .request_json(Method::GET, "/agent/schedules", None)
            .await?,
    )?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json)?);
        return Ok(());
    }
    let schedules = json["schedules"].as_array().cloned().unwrap_or_default();
    if schedules.is_empty() {
        println!("no schedules — add one with `iphone-use-mcp schedule add --cron \"0 9 * * *\" --flow <id>`");
    }
    for s in schedules {
        let last = &s["last_run"];
        println!(
            "{}  {:<16} {} {}{}\n    next {}  last {}{}",
            s["id"].as_str().unwrap_or("?"),
            format!("\"{}\"", s["cron"].as_str().unwrap_or("")),
            s["kind"].as_str().unwrap_or("?"),
            s["name"]
                .as_str()
                .or_else(|| s["target"].as_str())
                .unwrap_or("?"),
            if s["enabled"].as_bool() == Some(false) {
                "  (paused)"
            } else {
                ""
            },
            when(s["next_run_at"].as_u64()),
            last["state"].as_str().unwrap_or("—"),
            last["summary"]
                .as_str()
                .or_else(|| last["error"].as_str())
                .or_else(|| last["reason"].as_str())
                .map(|d| format!(" — {d}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

pub async fn runs(daemon: &DaemonClient, id: Option<&str>, json_out: bool) -> Result<()> {
    let path = match id {
        Some(id) => format!("/agent/schedules/{id}/runs"),
        None => "/agent/schedules/runs".to_string(),
    };
    let json = answer(daemon.request_json(Method::GET, &path, None).await?)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json)?);
        return Ok(());
    }
    let runs = json["runs"].as_array().cloned().unwrap_or_default();
    if runs.is_empty() {
        println!("no runs yet");
    }
    for r in runs {
        println!(
            "{}  {}  {:<14} {}{}",
            when(r["scheduled_for"].as_u64()),
            r["schedule_id"].as_str().unwrap_or("?"),
            r["state"].as_str().unwrap_or("?"),
            r["duration_ms"]
                .as_u64()
                .map(|ms| format!("{:.1}s ", ms as f64 / 1000.0))
                .unwrap_or_default(),
            r["summary"]
                .as_str()
                .or_else(|| r["error"].as_str())
                .or_else(|| r["reason"].as_str())
                .unwrap_or("")
        );
        if let Some(artifacts) = r["artifacts"].as_str() {
            println!("    {artifacts}");
        }
    }
    Ok(())
}

pub async fn remove(daemon: &DaemonClient, id: &str) -> Result<()> {
    answer(
        daemon
            .request_json(Method::DELETE, &format!("/agent/schedules/{id}"), None)
            .await?,
    )?;
    println!("removed {id}");
    Ok(())
}

pub async fn run_now(daemon: &DaemonClient, id: &str) -> Result<()> {
    let json = answer(
        daemon
            .request_json(Method::POST, &format!("/agent/schedules/{id}/run"), None)
            .await?,
    )?;
    println!(
        "queued run {} of {id}; follow it with `iphone-use-mcp schedule runs {id}`",
        json["run"]["id"].as_str().unwrap_or("?")
    );
    Ok(())
}

pub async fn set_enabled(daemon: &DaemonClient, id: &str, enabled: bool) -> Result<()> {
    let body = serde_json::json!({ "enabled": enabled });
    answer(
        daemon
            .request_json(
                Method::PATCH,
                &format!("/agent/schedules/{id}"),
                Some(&body),
            )
            .await?,
    )?;
    println!("{id} {}", if enabled { "resumed" } else { "paused" });
    Ok(())
}
