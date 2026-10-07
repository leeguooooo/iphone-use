//! `iphone-use-mcp test <suite>` — rerunnable test suites over the flow engine.
//!
//! A suite mirrors `chrome-use test`: an optional `setup` that runs once, then
//! `cases`, each a list of `steps` followed by `assert` checks. Steps are flow
//! steps verbatim (the same parser and validator `flow run` uses), plus
//! `{kind: flow, id: …}` to reuse a saved flow. An assertion is a `wait_for`
//! expectation (`application`, `present`, `absent`) polled until it holds or
//! its `timeout_ms` (default 5000) runs out.
//!
//! Each step is sent as its own one-step `/agent/actions` batch: on loopback
//! that costs milliseconds, and it gives every step its own timing and an
//! unambiguous place to fail. A case stops at its first failure, writes its
//! evidence (screenshot, element tree, step timings, error) and the suite
//! moves on to the next case. Exit code: 0 all passed, 1 a case failed, 2 the
//! suite is invalid or the phone could not be driven.

use crate::client::DaemonClient;
use crate::flow::{self, FlowRisk};
use crate::registry;
use crate::server::{phone_steps_request, PhoneStep};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Default per-assertion wait. Short on purpose: an assertion checks the
/// screen a step already led to; a step that needs longer should `wait_for`.
pub const DEFAULT_ASSERT_TIMEOUT_MS: u64 = 5_000;
const ASSERT_POLL_MS: u64 = 250;
/// Exit codes, as documented.
pub const EXIT_PASSED: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_UNUSABLE: i32 = 2;
/// How long `test` waits for a released phone to come back.
const RECONNECT_WAIT: Duration = Duration::from_secs(150);
const MAX_CASES: usize = 200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteDocument {
    #[serde(default)]
    suite: Option<String>,
    /// Bundle id launched before `setup` runs.
    #[serde(default)]
    app: Option<String>,
    /// Same meaning as a flow's `risk`; `side_effect` needs `--confirm`.
    #[serde(default)]
    risk: Option<FlowRisk>,
    #[serde(default)]
    setup: Vec<serde_json::Value>,
    cases: Vec<CaseDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseDocument {
    name: String,
    #[serde(default)]
    steps: Vec<serde_json::Value>,
    #[serde(default, rename = "assert")]
    asserts: Vec<AssertDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssertDocument {
    #[serde(default)]
    application: Option<String>,
    #[serde(default)]
    present: Option<serde_json::Value>,
    #[serde(default)]
    absent: Option<serde_json::Value>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// One compiled step: what it was in the file, and the exact request body.
#[derive(Debug, Clone)]
pub struct CompiledStep {
    /// `setup[2]`, `cases[0].steps[1]`, `cases[0].assert[0]`, or
    /// `cases[0].steps[3] (flow system/x step 2)`.
    pub origin: String,
    /// `step` or `assert`.
    pub role: &'static str,
    /// Short human description, e.g. `tap_locator label=通用`.
    pub describe: String,
    /// `{"steps":[…one step…]}` ready for `/agent/actions`.
    pub request: serde_json::Value,
}

#[derive(Debug)]
pub struct CompiledCase {
    pub name: String,
    pub steps: Vec<CompiledStep>,
}

#[derive(Debug)]
pub struct Suite {
    pub name: String,
    pub path: PathBuf,
    pub risk: Option<FlowRisk>,
    /// True when any step (directly or through a referenced flow) is
    /// declared side_effect; such a suite needs `--confirm`.
    pub side_effect: bool,
    pub setup: Vec<CompiledStep>,
    pub cases: Vec<CompiledCase>,
}

impl Suite {
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "suite": self.name,
            "path": self.path.display().to_string(),
            "risk": self.risk.map(FlowRisk::as_str).unwrap_or("unknown"),
            "side_effect": self.side_effect,
            "setup_steps": self.setup.len(),
            "cases": self.cases.iter().map(|case| serde_json::json!({
                "name": case.name,
                "steps": case.steps.iter().filter(|s| s.role == "step").count(),
                "asserts": case.steps.iter().filter(|s| s.role == "assert").count(),
            })).collect::<Vec<_>>(),
        })
    }
}

/// Parse YAML (`.yaml`/`.yml`) or JSON into a generic value.
fn parse_document(bytes: &[u8], path: &Path) -> Result<serde_json::Value> {
    let yaml = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"));
    if yaml {
        let value: serde_yaml::Value = serde_yaml::from_slice(bytes)
            .with_context(|| format!("parse suite YAML: {}", path.display()))?;
        serde_json::to_value(value).context("suite YAML must be plain data (no tags, string keys)")
    } else {
        serde_json::from_slice(bytes)
            .with_context(|| format!("parse suite JSON: {}", path.display()))
    }
}

/// Accept `present: 通用`, `present: {label: 通用}` or a list of either.
fn locator_list(
    value: Option<&serde_json::Value>,
    origin: &str,
    field: &str,
) -> Result<Vec<serde_json::Value>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let one = |item: &serde_json::Value| -> Result<serde_json::Value> {
        match item {
            serde_json::Value::String(label) if !label.is_empty() => {
                Ok(serde_json::json!({ "label": label }))
            }
            serde_json::Value::Object(_) => Ok(item.clone()),
            _ => bail!("{origin}.{field}: each entry is a label string or a locator object"),
        }
    };
    match value {
        serde_json::Value::Array(items) => items.iter().map(one).collect(),
        other => Ok(vec![one(other)?]),
    }
}

fn describe_step(step: &serde_json::Value) -> String {
    let kind = step.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
    let detail = match kind {
        "tap_label" => step
            .get("label")
            .and_then(|v| v.as_str())
            .map(|l| format!(" {l}")),
        "tap_locator" => step
            .get("locator")
            .map(|l| format!(" {}", compact_locator(l))),
        "launch_app" => step
            .get("bundle")
            .and_then(|v| v.as_str())
            .map(|b| format!(" {b}")),
        "shortcut" | "key" => step
            .get("name")
            .and_then(|v| v.as_str())
            .map(|n| format!(" {n}")),
        "type" => Some(" (text)".to_string()),
        "wait_for" => step
            .get("expect")
            .map(|e| format!(" {}", compact_expect(e))),
        "alert" => step
            .get("button")
            .or_else(|| step.get("action"))
            .and_then(|v| v.as_str())
            .map(|b| format!(" {b}")),
        _ => None,
    };
    format!("{kind}{}", detail.unwrap_or_default())
}

fn compact_locator(locator: &serde_json::Value) -> String {
    locator
        .as_object()
        .map(|fields| {
            fields
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{k}={}",
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string())
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

fn compact_expect(expect: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    if let Some(app) = expect.get("application").and_then(|v| v.as_str()) {
        parts.push(format!("application={app}"));
    }
    for field in ["present", "absent"] {
        if let Some(items) = expect.get(field).and_then(|v| v.as_array()) {
            for item in items {
                parts.push(format!("{field}[{}]", compact_locator(item)));
            }
        }
    }
    parts.join(" ")
}

/// Compile one step value into a validated single-step request.
fn compile_step(
    value: &serde_json::Value,
    origin: String,
    role: &'static str,
) -> Result<CompiledStep> {
    let step: PhoneStep =
        serde_json::from_value(value.clone()).with_context(|| format!("{origin}: invalid step"))?;
    let request = phone_steps_request(vec![step]).map_err(|e| anyhow::anyhow!("{origin}: {e}"))?;
    Ok(CompiledStep {
        origin,
        role,
        describe: describe_step(value),
        request,
    })
}

/// Expand a `{kind: flow, id, inputs}` step into the flow's steps.
fn expand_flow_step(
    value: &serde_json::Value,
    origin: &str,
    side_effect: &mut bool,
) -> Result<Vec<CompiledStep>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FlowStep {
        #[allow(dead_code)]
        kind: String,
        id: String,
        #[serde(default)]
        inputs: BTreeMap<String, String>,
    }
    let step: FlowStep = serde_json::from_value(value.clone()).with_context(|| {
        format!("{origin}: a flow step is {{kind: flow, id: <registry id or file>, inputs: {{…}}}}")
    })?;
    let path = registry::resolve_target(&step.id)
        .with_context(|| format!("{origin}: flow {:?}", step.id))?;
    let validated =
        flow::load_flow(&path).with_context(|| format!("{origin}: flow {:?}", step.id))?;
    flow::check_input_map(&step.inputs, &validated.inputs)
        .with_context(|| format!("{origin}: flow {:?}", step.id))?;
    if validated.meta.risk == Some(FlowRisk::SideEffect) {
        *side_effect = true;
    }
    let templates =
        flow::materialize_step_values(&validated.step_templates, &validated.inputs, &step.inputs)
            .with_context(|| format!("{origin}: flow {:?}", step.id))?;
    templates
        .iter()
        .enumerate()
        .map(|(index, template)| {
            compile_step(
                template,
                format!("{origin} (flow {} step {index})", step.id),
                "step",
            )
        })
        .collect()
}

fn compile_steps(
    values: &[serde_json::Value],
    prefix: &str,
    side_effect: &mut bool,
) -> Result<Vec<CompiledStep>> {
    let mut compiled = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let origin = format!("{prefix}[{index}]");
        if value.get("kind").and_then(|v| v.as_str()) == Some("flow") {
            compiled.extend(expand_flow_step(value, &origin, side_effect)?);
        } else {
            compiled.push(compile_step(value, origin, "step")?);
        }
    }
    Ok(compiled)
}

fn compile_assert(document: &AssertDocument, origin: String) -> Result<CompiledStep> {
    let present = locator_list(document.present.as_ref(), &origin, "present")?;
    let absent = locator_list(document.absent.as_ref(), &origin, "absent")?;
    if document.application.is_none() && present.is_empty() && absent.is_empty() {
        bail!("{origin}: an assertion needs application, present or absent");
    }
    let mut expect = serde_json::Map::new();
    if let Some(app) = &document.application {
        expect.insert("application".into(), serde_json::json!(app));
    }
    if !present.is_empty() {
        expect.insert("present".into(), serde_json::Value::Array(present));
    }
    if !absent.is_empty() {
        expect.insert("absent".into(), serde_json::Value::Array(absent));
    }
    let value = serde_json::json!({
        "kind": "wait_for",
        "expect": expect,
        "timeout_ms": document.timeout_ms.unwrap_or(DEFAULT_ASSERT_TIMEOUT_MS),
        "poll_ms": ASSERT_POLL_MS,
    });
    compile_step(&value, origin, "assert")
}

/// Parse and fully validate a suite. Never contacts the daemon.
pub fn parse_suite(bytes: &[u8], path: &Path) -> Result<Suite> {
    let value = parse_document(bytes, path)?;
    let document: SuiteDocument = serde_json::from_value(value)
        .with_context(|| format!("invalid suite: {}", path.display()))?;
    if document.cases.is_empty() {
        bail!("a suite needs at least one case");
    }
    if document.cases.len() > MAX_CASES {
        bail!("a suite holds at most {MAX_CASES} cases");
    }
    let mut side_effect = document.risk == Some(FlowRisk::SideEffect);
    let mut setup = Vec::new();
    if let Some(bundle) = &document.app {
        setup.push(compile_step(
            &serde_json::json!({"kind": "launch_app", "bundle": bundle}),
            "app".to_string(),
            "step",
        )?);
    }
    setup.extend(compile_steps(&document.setup, "setup", &mut side_effect)?);
    let mut cases = Vec::with_capacity(document.cases.len());
    let mut names = std::collections::BTreeSet::new();
    for (index, case) in document.cases.iter().enumerate() {
        let name = case.name.trim();
        if name.is_empty() || name.chars().any(char::is_control) {
            bail!("cases[{index}].name must be printable and non-empty");
        }
        if !names.insert(name.to_string()) {
            bail!("cases[{index}]: duplicate case name {name:?}");
        }
        if case.steps.is_empty() && case.asserts.is_empty() {
            bail!("cases[{index}] ({name}) has no steps and no assertions");
        }
        let mut steps = compile_steps(
            &case.steps,
            &format!("cases[{index}].steps"),
            &mut side_effect,
        )?;
        for (n, assertion) in case.asserts.iter().enumerate() {
            steps.push(compile_assert(
                assertion,
                format!("cases[{index}].assert[{n}]"),
            )?);
        }
        cases.push(CompiledCase {
            name: name.to_string(),
            steps,
        });
    }
    let name = document
        .suite
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "suite".into())
        });
    Ok(Suite {
        name,
        path: path.to_path_buf(),
        risk: document.risk,
        side_effect,
        setup,
        cases,
    })
}

pub fn load_suite(path: &Path) -> Result<Suite> {
    let bytes = flow::read_flow_bytes(path)?;
    parse_suite(&bytes, path)
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct StepResult {
    pub origin: String,
    pub role: &'static str,
    pub step: String,
    pub ok: bool,
    pub ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CaseResult {
    pub name: String,
    pub passed: bool,
    pub ms: u64,
    pub steps: Vec<StepResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SuiteReport {
    pub suite: String,
    pub path: String,
    pub passed: usize,
    pub failed: usize,
    pub ms: u64,
    pub setup: Vec<StepResult>,
    pub cases: Vec<CaseResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SuiteReport {
    pub fn exit_code(&self) -> i32 {
        if self.error.is_some() && self.cases.is_empty() {
            EXIT_UNUSABLE
        } else if self.failed > 0 || self.error.is_some() {
            EXIT_FAILED
        } else {
            EXIT_PASSED
        }
    }
}

/// Run one compiled step and judge the answer the way `flow run` does: only
/// an explicit confirmation passes; anything else fails the case.
async fn run_step(daemon: &DaemonClient, step: &CompiledStep) -> StepResult {
    let clock = Instant::now();
    let answer = daemon.flow_actions_outcome(&step.request).await;
    let ms = clock.elapsed().as_millis() as u64;
    let (ok, error, detail) = match answer {
        Ok(response) if response.confirms_action() => (true, None, None),
        Ok(response) => {
            let json = response.json.clone();
            let error = json
                .as_ref()
                .and_then(|j| j.get("error"))
                .and_then(|e| e.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("HTTP {}", response.status.as_u16()));
            let detail = json.map(|mut j| {
                // The failed step's own result is the useful part; drop the
                // bulky observation tree (the artifacts keep a full read).
                if let Some(object) = j.as_object_mut() {
                    object.remove("observation");
                }
                j
            });
            (false, Some(error), detail)
        }
        Err(error) => (
            false,
            Some("outcome_unknown".to_string()),
            Some(serde_json::json!(format!("{error:#}"))),
        ),
    };
    StepResult {
        origin: step.origin.clone(),
        role: step.role,
        step: step.describe.clone(),
        ok,
        ms,
        error,
        detail,
    }
}

/// Run steps in order, stopping at the first failure.
async fn run_steps(
    daemon: &DaemonClient,
    steps: &[CompiledStep],
) -> (Vec<StepResult>, Option<String>) {
    let mut results = Vec::with_capacity(steps.len());
    for step in steps {
        let result = run_step(daemon, step).await;
        let failure = (!result.ok).then(|| {
            format!(
                "{} {} failed: {} ({})",
                step.role,
                step.origin,
                result.error.as_deref().unwrap_or("failed"),
                step.describe
            )
        });
        results.push(result);
        if failure.is_some() {
            return (results, failure);
        }
    }
    (results, None)
}

/// Case name → directory name.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "case".to_string()
    } else {
        trimmed.chars().take(60).collect()
    }
}

fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create artifacts directory {}", path.display()))
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

/// Save what the phone showed when a case failed. Best effort: a capture that
/// fails is noted in `error.json`, it never changes the verdict.
async fn save_failure_evidence(
    daemon: &DaemonClient,
    dir: &Path,
    failure: &str,
    steps: &[StepResult],
) -> Result<()> {
    private_dir(dir)?;
    let mut notes = Vec::new();
    match daemon.screenshot(None).await {
        Ok(png) => private_write(&dir.join("screenshot.png"), &png)?,
        Err(error) => notes.push(format!("screenshot: {error:#}")),
    }
    match daemon.elements().await {
        Ok(body) => {
            let pretty = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| serde_json::to_vec_pretty(&v).ok())
                .unwrap_or_else(|| body.into_bytes());
            private_write(&dir.join("elements.json"), &pretty)?
        }
        Err(error) => notes.push(format!("elements: {error:#}")),
    }
    private_write(&dir.join("steps.json"), &serde_json::to_vec_pretty(steps)?)?;
    private_write(
        &dir.join("error.json"),
        &serde_json::to_vec_pretty(
            &serde_json::json!({ "failure": failure, "capture_notes": notes }),
        )?,
    )?;
    Ok(())
}

/// Make sure the phone can be driven, reconnecting once when it was released.
/// `Err` carries a sentence for the operator; nothing has been sent.
pub async fn ensure_drivable(daemon: &DaemonClient) -> std::result::Result<(), String> {
    let read = |body: String| serde_json::from_str::<serde_json::Value>(&body).ok();
    let status = match daemon.get_text("/agent/status").await {
        Ok(body) => read(body).ok_or_else(|| "unreadable /agent/status".to_string())?,
        Err(error) => return Err(format!("the daemon is not reachable: {error:#}")),
    };
    if status.get("drivable").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(());
    }
    if is_locked(&status) {
        return Err(
            "the iPhone is locked — unlock it and keep it awake, then run again".to_string(),
        );
    }
    let state = status
        .get("device_state")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !matches!(state, "released" | "offline" | "reconnecting" | "degraded") {
        return Err(format!(
            "the phone is not drivable (device_state={state:?}): {}",
            status.get("hint").and_then(|v| v.as_str()).unwrap_or("")
        ));
    }
    if state != "reconnecting" {
        if let Err(error) = daemon.reconnect().await {
            return Err(format!("reconnect failed: {error:#}"));
        }
    }
    let deadline = Instant::now() + RECONNECT_WAIT;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let Ok(body) = daemon.get_text("/agent/status").await else {
            continue;
        };
        let Some(status) = read(body) else { continue };
        if status.get("drivable").and_then(|v| v.as_bool()) == Some(true) {
            return Ok(());
        }
        if is_locked(&status) {
            return Err(
                "the iPhone is locked — unlock it and keep it awake, then run again".to_string(),
            );
        }
    }
    Err("the phone did not become drivable within 150 s of reconnecting".to_string())
}

/// The three ways `/agent/status` says the screen is locked.
pub fn is_locked(status: &serde_json::Value) -> bool {
    status.get("device_state").and_then(|v| v.as_str()) == Some("locked")
        || status.get("setup_blocked_on").and_then(|v| v.as_str()) == Some("locked")
        || status.get("wda_locked").and_then(|v| v.as_bool()) == Some(true)
}

/// Run a validated suite. The caller has already checked `--confirm`.
pub async fn run_suite(suite: &Suite, daemon: &DaemonClient, artifacts_root: &Path) -> SuiteReport {
    let clock = Instant::now();
    let mut report = SuiteReport {
        suite: suite.name.clone(),
        path: suite.path.display().to_string(),
        passed: 0,
        failed: 0,
        ms: 0,
        setup: Vec::new(),
        cases: Vec::new(),
        error: None,
    };
    if let Err(reason) = ensure_drivable(daemon).await {
        report.error = Some(reason);
        report.ms = clock.elapsed().as_millis() as u64;
        return report;
    }
    let (setup, failure) = run_steps(daemon, &suite.setup).await;
    report.setup = setup;
    if let Some(failure) = failure {
        let dir = artifacts_root.join("setup");
        let _ = save_failure_evidence(daemon, &dir, &failure, &report.setup).await;
        report.error = Some(format!(
            "setup failed, no case ran: {failure} (artifacts: {})",
            dir.display()
        ));
        report.failed = suite.cases.len();
        report.ms = clock.elapsed().as_millis() as u64;
        return report;
    }
    for case in &suite.cases {
        let case_clock = Instant::now();
        let (steps, failure) = run_steps(daemon, &case.steps).await;
        let mut result = CaseResult {
            name: case.name.clone(),
            passed: failure.is_none(),
            ms: case_clock.elapsed().as_millis() as u64,
            steps,
            failure: failure.clone(),
            artifacts: None,
        };
        if let Some(failure) = &failure {
            let dir = artifacts_root.join(slug(&case.name));
            match save_failure_evidence(daemon, &dir, failure, &result.steps).await {
                Ok(()) => result.artifacts = Some(dir.display().to_string()),
                Err(error) => result.artifacts = Some(format!("(not written: {error:#})")),
            }
            report.failed += 1;
        } else {
            report.passed += 1;
        }
        report.cases.push(result);
    }
    report.ms = clock.elapsed().as_millis() as u64;
    report
}

fn seconds(ms: u64) -> String {
    format!("{:.2}", ms as f64 / 1000.0)
}

pub fn text_report(report: &SuiteReport) -> String {
    let mut out = format!(
        "suite: {} ({} cases)\n",
        report.suite,
        report.cases.len().max(report.failed)
    );
    if let Some(error) = &report.error {
        out.push_str(&format!("  ! {error}\n"));
    }
    for case in &report.cases {
        let mark = if case.passed { "✓" } else { "✗" };
        out.push_str(&format!(
            "  {mark} {:<40} {:>6}s\n",
            case.name,
            seconds(case.ms)
        ));
        if let Some(failure) = &case.failure {
            out.push_str(&format!("      {failure}\n"));
        }
        if let Some(artifacts) = &case.artifacts {
            out.push_str(&format!("      artifacts: {artifacts}\n"));
        }
    }
    out.push_str(&format!(
        "{} passed, {} failed ({}s)\n",
        report.passed,
        report.failed,
        seconds(report.ms)
    ));
    out
}

fn xml_escape(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&apos;".to_string(),
            other => other.to_string(),
        })
        .collect()
}

pub fn junit_report(report: &SuiteReport) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let tests = report.cases.len().max(report.failed);
    out.push_str(&format!(
        "<testsuite name=\"{}\" tests=\"{tests}\" failures=\"{}\" errors=\"{}\" time=\"{}\">\n",
        xml_escape(&report.suite),
        report.failed,
        usize::from(report.error.is_some() && report.cases.is_empty()),
        seconds(report.ms)
    ));
    if report.cases.is_empty() {
        if let Some(error) = &report.error {
            out.push_str(&format!(
                "  <testcase name=\"suite\" time=\"0\"><error message=\"{}\"/></testcase>\n",
                xml_escape(error)
            ));
        }
    }
    for case in &report.cases {
        out.push_str(&format!(
            "  <testcase name=\"{}\" time=\"{}\"",
            xml_escape(&case.name),
            seconds(case.ms)
        ));
        match &case.failure {
            Some(failure) => out.push_str(&format!(
                ">\n    <failure message=\"{}\"/>\n  </testcase>\n",
                xml_escape(failure)
            )),
            None => out.push_str("/>\n"),
        }
    }
    out.push_str("</testsuite>\n");
    out
}

/// Options for [`test_command`].
pub struct TestOptions {
    pub json: bool,
    pub junit: Option<PathBuf>,
    pub confirm: bool,
    pub artifacts_dir: Option<PathBuf>,
    pub validate_only: bool,
}

/// `iphone-use-mcp test <suite>`; returns the process exit code.
pub async fn test_command(target: &str, options: TestOptions) -> i32 {
    let path = PathBuf::from(target);
    let suite = match load_suite(&path) {
        Ok(suite) => suite,
        Err(error) => {
            let message = format!("{error:#}");
            if options.json {
                println!(
                    "{}",
                    serde_json::json!({ "ok": false, "error": "invalid_suite", "message": message })
                );
            } else {
                eprintln!("invalid suite: {message}");
            }
            return EXIT_UNUSABLE;
        }
    };
    if options.validate_only {
        println!("{}", suite.summary());
        return EXIT_PASSED;
    }
    if suite.side_effect && !options.confirm {
        let message = "this suite sends, publishes, pays or deletes (risk side_effect, or a side_effect flow step); re-run with --confirm after checking it. Nothing was sent";
        if options.json {
            println!(
                "{}",
                serde_json::json!({ "ok": false, "error": "confirm_required", "message": message })
            );
        } else {
            eprintln!("{message}");
        }
        return EXIT_UNUSABLE;
    }
    let daemon = DaemonClient::from_env();
    let artifacts_root = options
        .artifacts_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("iphone-use-test-artifacts"));
    let report = run_suite(&suite, &daemon, &artifacts_root).await;
    // The run is over either way: hand the phone back for the next session.
    let _ = daemon.release_owner().await;
    if let Some(junit) = &options.junit {
        if let Err(error) = std::fs::write(junit, junit_report(&report)) {
            eprintln!("could not write {}: {error}", junit.display());
        }
    }
    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        print!("{}", text_report(&report));
    }
    report.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str, name: &str) -> Result<Suite> {
        parse_suite(text.as_bytes(), Path::new(name))
    }

    const SETTINGS: &str = r#"
suite: settings smoke
app: com.apple.Preferences
cases:
  - name: root lists General
    assert:
      - present: 通用
  - name: open General
    steps:
      - {kind: tap_locator, locator: {label: 通用, kind: Button}}
    assert:
      - present: [{label: 关于本机}]
        absent: 蓝牙
        timeout_ms: 8000
      - application: 设置
"#;

    #[test]
    fn a_yaml_suite_compiles_steps_and_asserts_into_single_step_requests() {
        let suite = parse(SETTINGS, "settings.yaml").unwrap();
        assert_eq!(suite.name, "settings smoke");
        assert_eq!(suite.setup.len(), 1, "app launches during setup");
        assert_eq!(
            suite.setup[0].request["steps"][0]["action"]["type"],
            "launch_app"
        );
        assert_eq!(suite.cases.len(), 2);
        let open = &suite.cases[1];
        assert_eq!(open.steps.len(), 3);
        assert_eq!(open.steps[0].role, "step");
        assert_eq!(open.steps[1].role, "assert");
        let wait = &open.steps[1].request["steps"][0];
        assert_eq!(wait["kind"], "wait_for");
        assert_eq!(wait["timeout_ms"], 8000);
        assert_eq!(wait["expect"]["present"][0]["label"], "关于本机");
        assert_eq!(wait["expect"]["absent"][0]["label"], "蓝牙");
        assert_eq!(
            open.steps[2].request["steps"][0]["timeout_ms"],
            DEFAULT_ASSERT_TIMEOUT_MS
        );
        assert!(!suite.side_effect);
    }

    #[test]
    fn json_suites_work_too() {
        let suite = parse(
            r#"{"cases":[{"name":"home","steps":[{"kind":"shortcut","name":"home"}]}]}"#,
            "smoke.json",
        )
        .unwrap();
        assert_eq!(suite.name, "smoke");
        assert_eq!(suite.cases[0].steps[0].describe, "shortcut home");
    }

    #[test]
    fn invalid_suites_name_the_place_that_is_wrong() {
        let unknown_kind = parse(
            "cases:\n  - name: x\n    steps:\n      - {kind: teleport}\n",
            "s.yaml",
        )
        .unwrap_err();
        assert!(
            format!("{unknown_kind:#}").contains("cases[0].steps[0]"),
            "{unknown_kind:#}"
        );
        let empty_assert = parse(
            "cases:\n  - name: x\n    assert:\n      - {timeout_ms: 10}\n",
            "s.yaml",
        )
        .unwrap_err();
        assert!(
            format!("{empty_assert:#}").contains("cases[0].assert[0]"),
            "{empty_assert:#}"
        );
        let duplicate = parse(
            "cases:\n  - {name: a, assert: [{present: x}]}\n  - {name: a, assert: [{present: y}]}\n",
            "s.yaml",
        )
        .unwrap_err();
        assert!(
            format!("{duplicate:#}").contains("duplicate"),
            "{duplicate:#}"
        );
        assert!(parse("cases: []\n", "s.yaml").is_err());
        assert!(
            parse("cases:\n  - {name: a}\n", "s.yaml").is_err(),
            "a case must do something"
        );
        assert!(parse(
            "cases:\n  - {name: a, assert: [{present: x}]}\nextra: 1\n",
            "s.yaml"
        )
        .is_err());
    }

    #[test]
    fn a_side_effect_suite_is_marked_so_the_runner_asks_for_confirm() {
        let suite = parse(
            "risk: side_effect\ncases:\n  - {name: a, assert: [{present: x}]}\n",
            "s.yaml",
        )
        .unwrap();
        assert!(suite.side_effect);
    }

    #[test]
    fn slugs_are_safe_directory_names() {
        assert_eq!(slug("open General"), "open-general");
        assert_eq!(slug("../../etc"), "etc");
        assert_eq!(slug("通用 / 关于"), "通用-关于");
        assert_eq!(slug("!!!"), "case");
    }

    fn sample_report() -> SuiteReport {
        SuiteReport {
            suite: "s <1>".into(),
            path: "s.yaml".into(),
            passed: 1,
            failed: 1,
            ms: 2500,
            setup: vec![],
            cases: vec![
                CaseResult {
                    name: "ok".into(),
                    passed: true,
                    ms: 1000,
                    steps: vec![],
                    failure: None,
                    artifacts: None,
                },
                CaseResult {
                    name: "bad & worse".into(),
                    passed: false,
                    ms: 1500,
                    steps: vec![],
                    failure: Some("assert cases[1].assert[0] failed: expectation_timeout".into()),
                    artifacts: Some("dir/bad-worse".into()),
                },
            ],
            error: None,
        }
    }

    /// A daemon that answers by path: status drivable, every action OK except
    /// a `wait_for` looking for "关于本机", which times out.
    fn path_daemon() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buffer = vec![0_u8; 65_536];
                let Ok(n) = stream.read(&mut buffer) else {
                    continue;
                };
                let request = String::from_utf8_lossy(&buffer[..n]).to_string();
                let line = request.lines().next().unwrap_or("").to_string();
                log.lock().unwrap().push(line.clone());
                let (status, content_type, body): (&str, &str, Vec<u8>) = if line
                    .starts_with("GET /agent/status")
                {
                    (
                        "200 OK",
                        "application/json",
                        br#"{"drivable":true,"backend":"direct"}"#.to_vec(),
                    )
                } else if line.starts_with("GET /agent/screenshot") {
                    ("200 OK", "image/png", b"\x89PNG fake".to_vec())
                } else if line.starts_with("GET /agent/elements") {
                    (
                        "200 OK",
                        "application/json",
                        br#"{"snapshot":"s1","elements":[{"kind":"Button","label":"x"}]}"#.to_vec(),
                    )
                } else if line.starts_with("POST /agent/actions") && request.contains("关于本机")
                {
                    ("504 Gateway Timeout", "application/json",
                     br#"{"ok":false,"error":"expectation_timeout","failed_step":0,"outcome":"no_effect","retry_safe":true,"steps":[]}"#.to_vec())
                } else {
                    (
                        "200 OK",
                        "application/json",
                        br#"{"ok":true,"steps":[{"index":0,"ok":true}]}"#.to_vec(),
                    )
                };
                let mut response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend(body);
                let _ = stream.write_all(&response);
            }
        });
        (url, seen)
    }

    #[tokio::test]
    async fn a_failed_assert_fails_its_case_saves_evidence_and_the_next_case_still_runs() {
        let (url, seen) = path_daemon();
        let daemon = DaemonClient::new(url, None);
        let suite = parse(SETTINGS, "settings.yaml").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let report = run_suite(&suite, &daemon, dir.path()).await;
        assert_eq!((report.passed, report.failed), (1, 1), "{report:?}");
        assert_eq!(report.exit_code(), EXIT_FAILED);
        assert_eq!(report.setup.len(), 1);
        let failed = &report.cases[1];
        assert!(!failed.passed);
        assert_eq!(
            failed.steps.len(),
            2,
            "stopped at the failed assert, the second assert never ran"
        );
        assert!(failed
            .failure
            .as_deref()
            .unwrap()
            .starts_with("assert cases[1].assert[0] failed: expectation_timeout"));
        let evidence = dir.path().join("open-general");
        for file in [
            "screenshot.png",
            "elements.json",
            "steps.json",
            "error.json",
        ] {
            assert!(evidence.join(file).exists(), "{file} missing");
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&evidence).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let seen = seen.lock().unwrap();
        // status, setup launch, case 1 assert, case 2 step + assert, evidence reads.
        assert_eq!(
            seen.iter()
                .filter(|l| l.starts_with("POST /agent/actions"))
                .count(),
            4,
            "{seen:?}"
        );
    }

    #[test]
    fn reports_carry_the_verdict_in_text_junit_and_exit_code() {
        let report = sample_report();
        assert_eq!(report.exit_code(), EXIT_FAILED);
        let text = text_report(&report);
        assert!(
            text.contains("✓ ok")
                && text.contains("✗ bad & worse")
                && text.contains("1 passed, 1 failed"),
            "{text}"
        );
        let junit = junit_report(&report);
        assert!(junit.contains("tests=\"2\" failures=\"1\""), "{junit}");
        assert!(junit.contains("name=\"bad &amp; worse\""), "{junit}");
        assert!(
            junit.contains(
                "<failure message=\"assert cases[1].assert[0] failed: expectation_timeout\"/>"
            ),
            "{junit}"
        );
        let unusable = SuiteReport {
            cases: vec![],
            passed: 0,
            failed: 0,
            error: Some("locked".into()),
            ..report
        };
        assert_eq!(unusable.exit_code(), EXIT_UNUSABLE);
    }
}
