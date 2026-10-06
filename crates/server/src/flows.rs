//! Flow registry awareness inside the daemon.
//!
//! An agent learns about flows from the responses it is already reading, not
//! from a skill it may not have loaded. Three jobs:
//!
//! 1. **Discovery** — a `registry` block naming the installed flows for the
//!    app the agent just entered: on every successful `launch_app`, and on the
//!    first element-tree read after the foreground app changes. Never on every
//!    read of the same app.
//! 2. **Suggestion** — a one-shot `flow_suggestion` block once an agent has
//!    driven one app for [`SUGGEST_MIN_STEPS`] successful steps, pointing at
//!    the draft. Cooled down per app for [`SUGGEST_COOLDOWN_DAYS`] days.
//! 3. **Draft** — `GET /agent/flow/draft` turns that trail into a flow v1
//!    document the agent can validate, show the user, and (with their OK)
//!    publish.
//!
//! The local store is written only by `iphone-use-mcp flow update`; the daemon
//! reads its `.index.json` and, for auto-update, runs that binary. Typed text
//! never leaves memory: a draft carries a named input in its place.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Opt-out of the background `flow update`.
pub const NO_AUTO_UPDATE_ENV: &str = "IPHONE_USE_FLOWS_NO_AUTO_UPDATE";
/// Opt-out of `flow_suggestion` blocks (discovery and drafts stay on).
pub const NO_SUGGEST_ENV: &str = "IPHONE_USE_FLOWS_NO_SUGGEST";
/// Same override the MCP's store uses (`registry::STORE_ENV`).
const STORE_ENV: &str = "IPHONE_USE_FLOWS_DIR";
const LOCAL_INDEX_FILE: &str = ".index.json";
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;

/// Successful steps in one app before a suggestion is offered.
pub const SUGGEST_MIN_STEPS: usize = 5;
pub const SUGGEST_COOLDOWN_DAYS: u64 = 14;
/// A pause this long ends a trail: the next action is a new task.
const TRAIL_IDLE: Duration = Duration::from_secs(15 * 60);
const TRAIL_MAX_STEPS: usize = 120;
/// How stale the store may get before the background refresh runs again.
pub const AUTO_UPDATE_TTL: Duration = Duration::from_secs(24 * 3600);

fn env_on(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|v| !v.trim().is_empty() && v.trim() != "0")
}

pub fn auto_update_disabled() -> bool {
    env_on(NO_AUTO_UPDATE_ENV)
}

fn suggestions_disabled() -> bool {
    env_on(NO_SUGGEST_ENV)
}

// ---------------------------------------------------------------------------
// The local store, read leniently
// ---------------------------------------------------------------------------

/// `.index.json` as `flow update` writes it, minus everything the daemon does
/// not need. Lenient on purpose: a newer CLI adding fields must not blind it.
#[derive(Debug, Default, Deserialize)]
struct Index {
    #[serde(default)]
    flows: BTreeMap<String, IndexFlow>,
    #[serde(default)]
    apps: Vec<IndexApp>,
}

#[derive(Debug, Deserialize)]
struct IndexFlow {
    #[serde(default)]
    name: String,
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    risk: Option<String>,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    verified_on: Vec<Value>,
    #[serde(default)]
    locale: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IndexApp {
    id: String,
    #[serde(default)]
    bundle: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

pub fn store_dir() -> Option<PathBuf> {
    match std::env::var(STORE_ENV) {
        Ok(value) if !value.trim().is_empty() => Some(PathBuf::from(value)),
        _ => std::env::var("HOME")
            .ok()
            .map(|home| PathBuf::from(home).join(".iphone-use").join("flows")),
    }
}

pub fn index_path() -> Option<PathBuf> {
    store_dir().map(|dir| dir.join(LOCAL_INDEX_FILE))
}

enum Store {
    Missing,
    Unreadable,
    Ready(Index),
}

fn load_store(path: Option<&Path>) -> Store {
    let Some(path) = path else {
        return Store::Unreadable;
    };
    match std::fs::metadata(path) {
        Err(_) => return Store::Missing,
        Ok(meta) if meta.len() > MAX_INDEX_BYTES => return Store::Unreadable,
        Ok(_) => {}
    }
    match std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Index>(&bytes).ok())
    {
        Some(index) => Store::Ready(index),
        None => Store::Unreadable,
    }
}

/// How the agent identifies the app it is in.
#[derive(Debug, Clone, Copy)]
pub enum AppKey<'a> {
    /// Exact: what `launch_app` was given.
    Bundle(&'a str),
    /// The foreground `Application` row's label, matched against `app.json`
    /// `name` / `aliases` (one per UI language).
    Label(&'a str),
}

impl AppKey<'_> {
    fn as_str(&self) -> &str {
        match self {
            AppKey::Bundle(s) | AppKey::Label(s) => s,
        }
    }
}

fn risk_rank(risk: Option<&str>) -> u8 {
    match risk {
        Some("read_only") => 0,
        Some("navigation") => 1,
        Some("side_effect") => 3,
        _ => 2,
    }
}

/// The `registry` block for one app. `None` only when the store is unreadable
/// (a hint must never break the response it decorates).
pub fn registry_block(key: AppKey<'_>) -> Option<Value> {
    registry_block_from(index_path().as_deref(), key)
}

fn registry_block_from(path: Option<&Path>, key: AppKey<'_>) -> Option<Value> {
    let index = match load_store(path) {
        Store::Unreadable => return None,
        Store::Missing => {
            return Some(json!({
                "key": key.as_str(),
                "installed": 0,
                "next": "The flow registry is not on this Mac yet; the daemon fetches it in the background. \
                         Run `iphone-use-mcp flow update` (MCP: phone_flow_update) if you need it now."
            }))
        }
        Store::Ready(index) => index,
    };
    let apps: Vec<&IndexApp> = index
        .apps
        .iter()
        .filter(|app| match key {
            AppKey::Bundle(bundle) => app.bundle.as_deref() == Some(bundle),
            AppKey::Label(label) => {
                let label = label.trim();
                !label.is_empty()
                    && (app.aliases.iter().any(|a| a.eq_ignore_ascii_case(label))
                        || app
                            .name
                            .as_deref()
                            .is_some_and(|n| n.eq_ignore_ascii_case(label)))
            }
        })
        .collect();
    let bundles: Vec<&str> = match key {
        AppKey::Bundle(bundle) => vec![bundle],
        AppKey::Label(_) => apps.iter().filter_map(|a| a.bundle.as_deref()).collect(),
    };
    let dirs: Vec<&str> = apps.iter().map(|a| a.id.as_str()).collect();
    let mut flows: Vec<(&String, &IndexFlow)> = index
        .flows
        .iter()
        .filter(|(id, flow)| {
            flow.app
                .as_deref()
                .is_some_and(|bundle| bundles.contains(&bundle))
                || id.split_once('/').is_some_and(|(dir, _)| dirs.contains(&dir))
        })
        .collect();
    flows.sort_by_key(|(id, flow)| (risk_rank(flow.risk.as_deref()), (*id).clone()));

    let mut block = json!({ "key": key.as_str() });
    if let Some(app) = apps.first() {
        block["app"] = json!(app.name.as_deref().unwrap_or(&app.id));
        if let Some(bundle) = &app.bundle {
            block["bundle"] = json!(bundle);
        }
    }
    block["flows"] = Value::Array(
        flows
            .iter()
            .map(|(id, flow)| {
                let mut row = json!({
                    "id": id,
                    "name": flow.name,
                    "risk": flow.risk.as_deref().unwrap_or("unknown"),
                    "verified": !flow.verified_on.is_empty(),
                });
                if !flow.inputs.is_empty() {
                    row["inputs"] = json!(flow.inputs);
                }
                if let Some(locale) = &flow.locale {
                    row["locale"] = json!(locale);
                }
                row
            })
            .collect(),
    );
    block["next"] = json!(if flows.is_empty() {
        "No saved flow for this app yet. Drive it step by step; after several steps the daemon \
         offers a draft flow (GET /agent/flow/draft, MCP phone_flow_draft) — ask the user before saving it."
    } else {
        "Before driving this app by hand, check whether one of these flows already does the task: \
         `iphone-use-mcp flow run <id> --input name=value` (MCP: phone_flow_run). It checks compatibility \
         and refuses broken ones. side_effect flows need the user's explicit go-ahead."
    });
    Some(block)
}

// ---------------------------------------------------------------------------
// The action trail
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Trail {
    bundle: Option<String>,
    label: Option<String>,
    steps: Vec<Value>,
    /// Things a human must fix before the draft is a good flow.
    todo: Vec<String>,
    /// Typed-text inputs, in order (`text1`, `text2`, …).
    inputs: usize,
    last: Option<Instant>,
    /// A `registry` block already went out for this app.
    hinted: bool,
    /// A `flow_suggestion` already went out for this trail.
    suggested: bool,
}

impl Trail {
    fn key(&self) -> Option<&str> {
        self.bundle.as_deref().or(self.label.as_deref())
    }
    fn worth_a_flow(&self) -> bool {
        self.steps.len() >= SUGGEST_MIN_STEPS
    }
}

/// Per-daemon record of what the driving agent did, for suggestions and drafts.
#[derive(Debug, Default)]
pub struct FlowTrail {
    current: Trail,
    previous: Option<Trail>,
}

/// One action, translated into a flow v1 step.
pub enum Converted {
    Step(Value),
    /// Typed text: becomes a named input; the text itself is dropped.
    Typed { clear: bool },
    /// Not expressible in a flow; the note goes into the draft's `todo`.
    Unsupported(String),
}

/// Translate one `/agent/input` action into a flow step. `rows` is the element
/// tree the action's `snapshot` token referred to, so a snapshot-index tap
/// becomes a durable locator instead of an index that means nothing tomorrow.
pub fn convert_action(action: &Value, rows: Option<&[crate::wda::ElementRow]>) -> Converted {
    let typ = action.get("type").and_then(Value::as_str).unwrap_or("");
    let num = |k: &str| action.get(k).and_then(Value::as_f64);
    let copy = |kind: &str, keys: &[&str]| {
        let mut step = json!({ "kind": kind });
        for key in keys {
            if let Some(value) = action.get(*key) {
                step[*key] = value.clone();
            }
        }
        Converted::Step(step)
    };
    match typ {
        "tap" if action.get("element").is_some() => {
            let row = action
                .get("element")
                .and_then(Value::as_u64)
                .and_then(|i| rows.and_then(|rows| rows.get(i as usize)));
            match row.and_then(locator_for_row) {
                Some(locator) => Converted::Step(json!({ "kind": "tap_locator", "locator": locator })),
                None => Converted::Unsupported(
                    "a tap on an element with no label or identifier; replace it with a tap_locator or a coordinate tap"
                        .into(),
                ),
            }
        }
        "tap" if action.get("label").is_some() => copy("tap_label", &["label"]),
        "tap" => match (num("x"), num("y")) {
            (Some(x), Some(y)) => Converted::Step(json!({ "kind": "tap", "x": x, "y": y })),
            _ => Converted::Unsupported("a tap without coordinates".into()),
        },
        "tap_locator" => copy("tap_locator", &["locator"]),
        "text" => Converted::Typed {
            clear: action.get("clear").and_then(Value::as_bool).unwrap_or(false),
        },
        "key" => copy("key", &["name"]),
        "shortcut" => copy("shortcut", &["name"]),
        "home" => Converted::Step(json!({ "kind": "shortcut", "name": "home" })),
        "back" => Converted::Step(json!({ "kind": "back" })),
        "launch_app" if action.get("bundle").is_some() => copy("launch_app", &["bundle"]),
        "scroll" if action.get("element").is_none() => copy("scroll", &["x", "y", "dx", "dy"]),
        "swipe" => copy("swipe", &["x1", "y1", "x2", "y2", "duration_ms"]),
        "drag" => copy("drag", &["x1", "y1", "x2", "y2", "hold_ms", "duration_ms"]),
        "longpress" => copy("longpress", &["x", "y", "duration_ms"]),
        "alert" => copy("alert", &["button", "action"]),
        "picker" => copy("picker", &["column", "value"]),
        other => Converted::Unsupported(format!(
            "a `{other}` action, which flows cannot express yet; redo that part with supported steps"
        )),
    }
}

/// A durable locator for a tree row. Never the snapshot index.
///
/// An identifier stands ALONE: `tap_locator` re-resolves through WDA, which
/// can query an identifier only by itself (accessibility id) — paired with
/// `kind` it degrades to a kind-only query and every key on a keypad matches
/// (hardware: Calculator `{identifier:"One",kind:"Key"}` → ambiguous).
/// Without one, label + kind.
fn locator_for_row(row: &crate::wda::ElementRow) -> Option<Value> {
    if let Some(identifier) = row.identifier.as_deref().filter(|s| !s.trim().is_empty()) {
        return Some(json!({ "identifier": identifier }));
    }
    if row.label.trim().is_empty() {
        return None;
    }
    let mut locator = json!({ "label": row.label });
    if !row.kind.is_empty() {
        locator["kind"] = json!(row.kind);
    }
    Some(locator)
}

impl FlowTrail {
    fn rotate(&mut self) {
        let finished = std::mem::take(&mut self.current);
        if finished.worth_a_flow() {
            self.previous = Some(finished);
        }
    }

    fn expire_if_idle(&mut self, now: Instant) {
        if self
            .current
            .last
            .is_some_and(|last| now.duration_since(last) > TRAIL_IDLE)
        {
            self.rotate();
        }
    }

    /// A flow (or any caller that marks itself) ran: what came before was a
    /// task of its own, and the run's steps are already a flow.
    pub fn flow_ran(&mut self) {
        self.rotate();
    }

    /// The phone was released or handed to a person.
    pub fn reset(&mut self) {
        self.rotate();
    }

    /// `launch_app` applied. Returns whether a `registry` block should go out.
    pub fn launched(&mut self, bundle: &str, now: Instant) -> bool {
        self.expire_if_idle(now);
        if self.current.bundle.as_deref() != Some(bundle) || self.current.steps.is_empty() {
            self.rotate();
            self.current.bundle = Some(bundle.to_string());
        }
        self.current.hinted = true;
        true
    }

    /// An element tree showed `label` as the foreground app. Returns whether a
    /// `registry` block should go out (first look at a newly entered app).
    pub fn saw_app(&mut self, label: Option<&str>, now: Instant) -> bool {
        // SpringBoard reports its Application label as " ": not an app name.
        let Some(label) = label.filter(|l| !l.trim().is_empty()) else {
            return false;
        };
        self.expire_if_idle(now);
        match self.current.label.as_deref() {
            Some(known) if known == label => {}
            // Launched by bundle, label not yet known: same app, learn it.
            None if self.current.bundle.is_some() => self.current.label = Some(label.to_string()),
            _ => {
                self.rotate();
                self.current.label = Some(label.to_string());
            }
        }
        if self.current.hinted {
            return false;
        }
        self.current.hinted = true;
        true
    }

    /// Record one applied step.
    pub fn record(&mut self, converted: Converted, now: Instant) {
        self.expire_if_idle(now);
        if self.current.steps.len() >= TRAIL_MAX_STEPS {
            return;
        }
        match converted {
            Converted::Step(step) => self.current.steps.push(step),
            Converted::Typed { clear } => {
                self.current.inputs += 1;
                let mut step = json!({ "kind": "type", "input": format!("text{}", self.current.inputs) });
                if clear {
                    step["clear"] = json!(true);
                }
                self.current.steps.push(step);
            }
            Converted::Unsupported(note) => {
                self.current
                    .todo
                    .push(format!("step {}: {note}", self.current.steps.len() + 1));
            }
        }
        self.current.last = Some(now);
    }

    /// Record a step that is already in flow form (`wait_for`, `pause`).
    pub fn record_step(&mut self, step: Value, now: Instant) {
        self.record(Converted::Step(step), now);
    }

    /// The one-shot `flow_suggestion` block, if one is due now.
    pub fn suggestion(&mut self, cooldown: &Path, today: u64) -> Option<Value> {
        if suggestions_disabled() {
            return None;
        }
        let trail = if self.current.worth_a_flow() && !self.current.suggested {
            &mut self.current
        } else {
            match self.previous.as_mut() {
                Some(previous) if !previous.suggested => previous,
                _ => return None,
            }
        };
        trail.suggested = true;
        let key = trail.key()?.to_string();
        if registry_has_flows(&key) || !cooldown_allows(cooldown, &key, today) {
            return None;
        }
        remember_cooldown(cooldown, &key, today);
        Some(json!({
            "key": key,
            "steps": trail.steps.len(),
            "message": format!(
                "You have driven {key} for {} steps and no saved flow covers it. When the task is done, \
                 ask the user whether to save it as a flow so next time is one call. Only if they agree: \
                 GET /agent/flow/draft (MCP: phone_flow_draft) gives a ready draft to validate and run once; \
                 publishing to the shared registry needs their explicit OK again.",
                trail.steps.len()
            ),
        }))
    }

    /// The draft for the current trail, or the last finished one.
    pub fn draft(&self) -> Option<Value> {
        let (trail, source) = if self.current.steps.len() >= 2 {
            (&self.current, "current")
        } else {
            (self.previous.as_ref()?, "previous")
        };
        Some(draft_json(trail, source))
    }
}

fn registry_has_flows(key: &str) -> bool {
    let has = |block: Option<Value>| {
        block
            .and_then(|b| b.get("flows").and_then(Value::as_array).map(|f| !f.is_empty()))
            .unwrap_or(false)
    };
    has(registry_block(AppKey::Bundle(key))) || has(registry_block(AppKey::Label(key)))
}

fn draft_json(trail: &Trail, source: &str) -> Value {
    let app_name = trail
        .label
        .clone()
        .or_else(|| trail.bundle.clone())
        .unwrap_or_else(|| "App".into());
    // Going Home at the end is the agent tidying up, not part of the task.
    let is_home = |s: &Value| {
        s.get("kind").and_then(Value::as_str) == Some("shortcut")
            && s.get("name").and_then(Value::as_str) == Some("home")
    };
    let mut recorded: &[Value] = &trail.steps;
    while recorded.last().is_some_and(is_home) {
        recorded = &recorded[..recorded.len() - 1];
    }
    let mut steps = Vec::with_capacity(recorded.len() + 1);
    for step in recorded {
        steps.push(step.clone());
        // Gate the launch on the app actually being in front: the one wait
        // the trail can prove. Later screens need gates a human chooses.
        if step.get("kind").and_then(Value::as_str) == Some("launch_app") {
            if let Some(label) = &trail.label {
                steps.push(json!({
                    "kind": "wait_for",
                    "expect": { "application": label },
                    "timeout_ms": 15000
                }));
            }
        }
    }
    let mut flow = json!({
        "version": 1,
        "name": format!("{app_name} task"),
        "description": "Recorded from an agent session. Rename and describe what it does.",
    });
    if let Some(bundle) = &trail.bundle {
        flow["app"] = json!(bundle);
    }
    if trail.inputs > 0 {
        let inputs: serde_json::Map<String, Value> = (1..=trail.inputs)
            .map(|n| {
                (
                    format!("text{n}"),
                    json!({ "type": "string", "description": format!("text typed at the {} typing step", ordinal(n)) }),
                )
            })
            .collect();
        flow["inputs"] = Value::Object(inputs);
    }
    flow["steps"] = Value::Array(steps);

    let mut todo = vec![
        "Give it a real name and description, and rename text1/text2… inputs to what they mean.".to_string(),
        "Add a wait_for gate after each tap that opens a new screen.".to_string(),
        "Set risk (read_only / navigation / side_effect), category and locale before publishing.".to_string(),
        "Check tap_label / locator labels for personal data (contact names, amounts) and generalize them.".to_string(),
    ];
    if trail.bundle.is_none() {
        todo.push("Start with a launch_app step: the trail never saw the bundle id.".into());
    }
    if trail.steps.iter().any(|s| s.get("kind").and_then(Value::as_str) == Some("tap")) {
        todo.push("Coordinate taps break on other screen sizes; replace them with tap_locator where a label exists.".into());
    }
    todo.extend(trail.todo.iter().cloned());

    json!({
        "ok": true,
        "source": source,
        "app": { "bundle": trail.bundle, "label": trail.label },
        "recorded_steps": trail.steps.len(),
        "flow": flow,
        "todo": todo,
        "next": "Show the user what this flow would do and ask whether to keep it. If yes: write `flow` to a file, \
                 `iphone-use-mcp flow validate <file>`, run it once from the file, then — only with their OK — \
                 `iphone-use-mcp flow publish <file> --as <app>/<name>` (MCP: phone_flow_publish).",
    })
}

fn ordinal(n: usize) -> String {
    match n {
        1 => "first".into(),
        2 => "second".into(),
        3 => "third".into(),
        n => format!("{n}th"),
    }
}

// ---------------------------------------------------------------------------
// Suggestion cooldown: `{ "<bundle or label>": <unix day> }`, nothing else.
// ---------------------------------------------------------------------------

/// Beside the instance state; inside an overridden store when one is set, so
/// tests and development stores never touch the user's real cooldowns.
pub fn cooldown_path() -> PathBuf {
    match std::env::var(STORE_ENV) {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value).join(".suggest-cooldown.json"),
        _ => crate::instance::current().state_dir.join("flow-suggest.json"),
    }
}

pub fn unix_day() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

fn read_cooldown(path: &Path) -> BTreeMap<String, u64> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn cooldown_allows(path: &Path, key: &str, today: u64) -> bool {
    read_cooldown(path)
        .get(key)
        .is_none_or(|day| today.saturating_sub(*day) >= SUGGEST_COOLDOWN_DAYS)
}

fn remember_cooldown(path: &Path, key: &str, today: u64) {
    let mut map = read_cooldown(path);
    map.insert(key.to_string(), today);
    map.retain(|_, day| today.saturating_sub(*day) < SUGGEST_COOLDOWN_DAYS);
    if let Ok(bytes) = serde_json::to_vec(&map) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

// ---------------------------------------------------------------------------
// Background refresh
// ---------------------------------------------------------------------------

/// Whether the store is missing or older than [`AUTO_UPDATE_TTL`].
pub fn store_is_stale() -> bool {
    let Some(path) = index_path() else {
        return false;
    };
    match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(modified) => modified.elapsed().map_or(true, |age| age > AUTO_UPDATE_TTL),
        Err(_) => true,
    }
}

/// `iphone-use-mcp` next to this executable (app bundle and cargo target dir
/// alike). The daemon never reimplements fetching and verification.
pub fn mcp_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("iphone-use-mcp");
    candidate.is_file().then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, label: &str, identifier: Option<&str>) -> crate::wda::ElementRow {
        crate::wda::ElementRow {
            kind: kind.into(),
            label: label.into(),
            identifier: identifier.map(str::to_string),
            ..Default::default()
        }
    }

    fn store(dir: &Path) -> PathBuf {
        let path = dir.join(LOCAL_INDEX_FILE);
        std::fs::write(
            &path,
            json!({
                "version": 1,
                "flows": {
                    "health/export-all": {"source":"official","sha256":"x","name":"Export all","steps":9,
                        "app":"com.apple.Health","risk":"side_effect","verified_on":[{"ios":"26"}]},
                    "health/steps-today": {"source":"official","sha256":"x","name":"Steps today","steps":4,
                        "risk":"read_only","inputs":["day"],"locale":"zh-CN","future_field":1}
                },
                "apps": [{"id":"health","bundle":"com.apple.Health","name":"Health","aliases":["Health","健康"]}]
            })
            .to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn registry_block_matches_by_bundle_and_alias_read_only_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(dir.path());
        for key in [AppKey::Bundle("com.apple.Health"), AppKey::Label("健康")] {
            let block = registry_block_from(Some(&path), key).unwrap();
            let ids: Vec<&str> = block["flows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, ["health/steps-today", "health/export-all"]);
            assert_eq!(block["bundle"], "com.apple.Health");
            assert!(block["next"].as_str().unwrap().contains("flow run"));
        }
        let none = registry_block_from(Some(&path), AppKey::Bundle("com.tencent.xin")).unwrap();
        assert_eq!(none["flows"], json!([]));
        assert!(none["next"].as_str().unwrap().contains("ask the user"));
    }

    #[test]
    fn registry_block_reports_a_missing_store_and_hides_an_unreadable_one() {
        let dir = tempfile::tempdir().unwrap();
        let missing = registry_block_from(Some(&dir.path().join("nope.json")), AppKey::Bundle("a.b")).unwrap();
        assert_eq!(missing["installed"], 0);
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert!(registry_block_from(Some(&bad), AppKey::Bundle("a.b")).is_none());
    }

    #[test]
    fn snapshot_taps_become_locators_and_typed_text_is_dropped() {
        let rows = vec![row("Application", "健康", None), row("Button", "资料", None), row("Cell", "", Some("row-1"))];
        let tap = |i: u64| convert_action(&json!({"type":"tap","element":i,"snapshot":"s"}), Some(&rows));
        let Converted::Step(step) = tap(1) else { panic!() };
        assert_eq!(step, json!({"kind":"tap_locator","locator":{"label":"资料","kind":"Button"}}));
        let Converted::Step(step) = tap(2) else { panic!() };
        assert_eq!(step["locator"], json!({"identifier":"row-1"}), "identifier stands alone");

        let mut trail = FlowTrail::default();
        let now = Instant::now();
        trail.record(convert_action(&json!({"type":"text","text":"secret","clear":true}), None), now);
        let draft = draft_json(&trail.current, "current");
        assert!(!draft.to_string().contains("secret"));
        assert_eq!(draft["flow"]["steps"][0], json!({"kind":"type","input":"text1","clear":true}));
        assert_eq!(draft["flow"]["inputs"]["text1"]["type"], "string");
    }

    #[test]
    fn app_hints_go_out_once_per_app() {
        let mut trail = FlowTrail::default();
        let now = Instant::now();
        assert!(trail.launched("com.apple.Health", now));
        // First tree read after the launch learns the label; no second hint.
        assert!(!trail.saw_app(Some("健康"), now));
        assert!(!trail.saw_app(Some("健康"), now));
        // The user (or a banner) moved to another app.
        assert!(trail.saw_app(Some("微信"), now));
        assert!(!trail.saw_app(Some("微信"), now));
        // The Home screen's blank label is not an app change.
        assert!(!trail.saw_app(Some(" "), now));
        assert!(!trail.saw_app(Some("微信"), now));
    }

    #[test]
    fn suggestion_is_one_shot_and_cooled_down() {
        let dir = tempfile::tempdir().unwrap();
        let cooldown = dir.path().join("flow-suggest.json");
        let now = Instant::now();
        let mut trail = FlowTrail::default();
        trail.launched("com.example.notes", now);
        for _ in 0..SUGGEST_MIN_STEPS - 1 {
            trail.record(Converted::Step(json!({"kind":"back"})), now);
            assert!(trail.suggestion(&cooldown, 100).is_none());
        }
        trail.record(Converted::Step(json!({"kind":"back"})), now);
        let suggestion = trail.suggestion(&cooldown, 100).expect("due at the threshold");
        assert_eq!(suggestion["key"], "com.example.notes");
        assert!(suggestion["message"].as_str().unwrap().contains("ask the user"));
        assert!(trail.suggestion(&cooldown, 100).is_none(), "one-shot per trail");

        // A new trail in the same app inside the cooldown stays quiet...
        let mut again = FlowTrail::default();
        again.launched("com.example.notes", now);
        for _ in 0..SUGGEST_MIN_STEPS {
            again.record(Converted::Step(json!({"kind":"back"})), now);
        }
        assert!(again.suggestion(&cooldown, 100 + SUGGEST_COOLDOWN_DAYS - 1).is_none());
        // ...and speaks again once it has passed.
        let mut later = FlowTrail::default();
        later.launched("com.example.notes", now);
        for _ in 0..SUGGEST_MIN_STEPS {
            later.record(Converted::Step(json!({"kind":"back"})), now);
        }
        assert!(later.suggestion(&cooldown, 100 + SUGGEST_COOLDOWN_DAYS).is_some());
    }

    #[test]
    fn draft_gates_the_launch_and_survives_an_app_switch() {
        let now = Instant::now();
        let mut trail = FlowTrail::default();
        trail.launched("com.apple.Health", now);
        trail.record(convert_action(&json!({"type":"launch_app","bundle":"com.apple.Health"}), None), now);
        trail.saw_app(Some("健康"), now);
        for _ in 0..SUGGEST_MIN_STEPS {
            trail.record(convert_action(&json!({"type":"tap","x":0.5,"y":0.5}), None), now);
        }
        trail.record(convert_action(&json!({"type":"shortcut","name":"home"}), None), now);
        trail.saw_app(Some("微信"), now);
        let draft = trail.draft().expect("the finished trail is kept");
        assert_eq!(draft["flow"]["steps"].as_array().unwrap().len(), 2 + SUGGEST_MIN_STEPS, "trailing Home dropped");
        assert_eq!(draft["source"], "previous");
        let flow = &draft["flow"];
        assert_eq!(flow["app"], "com.apple.Health");
        assert_eq!(flow["steps"][0]["kind"], "launch_app");
        assert_eq!(flow["steps"][1], json!({"kind":"wait_for","expect":{"application":"健康"},"timeout_ms":15000}));
        assert!(draft["todo"].to_string().contains("Coordinate taps"));
    }
}
