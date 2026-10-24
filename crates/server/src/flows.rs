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
    with_app_notes(
        registry_block_from(index_path().as_deref(), key),
        key,
        &app_notes_dir(),
    )
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
        "No saved flow for this app; the daemon will offer a draft after several steps."
    } else {
        "If one of these does the task, run it instead of driving by hand: phone_flow_run / `flow run <id>`."
    });
    Some(block)
}

// ---------------------------------------------------------------------------
// App notes
// ---------------------------------------------------------------------------

/// Largest notes file `PUT /agent/app-notes` accepts.
pub const APP_NOTES_MAX_BYTES: usize = 16 * 1024;
/// How much of an app's notes rides along in its `registry` block.
const APP_NOTES_IN_BLOCK: usize = 2 * 1024;

/// Per-app notes (`<bundle>.md`): plain markdown an agent or a person keeps
/// about an app — obstacles, where things are, what not to do. Beside the
/// flow store, never inside it (`flow update` refreshes the store from the
/// registry), so notes survive updates and upgrades.
pub fn app_notes_dir() -> PathBuf {
    store_dir()
        .and_then(|dir| dir.parent().map(|parent| parent.join("app-notes")))
        .unwrap_or_else(|| crate::instance::current().state_dir.join("app-notes"))
}

/// A bundle id safe to use as a file name: reverse-DNS characters only.
pub fn valid_app_notes_bundle(bundle: &str) -> bool {
    !bundle.is_empty()
        && bundle.len() <= 200
        && !bundle.starts_with('.')
        && !bundle.contains("..")
        && bundle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// One app's notes and when they were last written (Unix seconds).
#[derive(Debug, Clone, PartialEq)]
pub struct AppNotes {
    pub text: String,
    pub updated_at: Option<u64>,
}

fn app_notes_path(dir: &Path, bundle: &str) -> Option<PathBuf> {
    valid_app_notes_bundle(bundle).then(|| dir.join(format!("{bundle}.md")))
}

/// The notes for `bundle` in `dir`; `None` when there are none.
pub fn read_app_notes(dir: &Path, bundle: &str) -> Option<AppNotes> {
    let path = app_notes_path(dir, bundle)?;
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > 4 * APP_NOTES_MAX_BYTES as u64 {
        return None;
    }
    let text = String::from_utf8_lossy(&std::fs::read(&path).ok()?).into_owned();
    let updated_at = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|age| age.as_secs());
    Some(AppNotes { text, updated_at })
}

/// Replace `bundle`'s notes; empty (or blank) text deletes them.
pub fn write_app_notes(dir: &Path, bundle: &str, text: &str) -> std::io::Result<()> {
    let path = app_notes_path(dir, bundle).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a bundle id")
    })?;
    if text.trim().is_empty() {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// The first [`APP_NOTES_IN_BLOCK`] bytes, cut at a character boundary.
fn notes_excerpt(text: &str) -> (String, bool) {
    let text = text.trim();
    if text.len() <= APP_NOTES_IN_BLOCK {
        return (text.to_string(), false);
    }
    let mut end = APP_NOTES_IN_BLOCK;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}…", &text[..end]), true)
}

/// Add the app's notes to its `registry` block (making one when the store
/// gave none), so an agent entering the app reads what earlier runs learned.
fn with_app_notes(block: Option<Value>, key: AppKey<'_>, dir: &Path) -> Option<Value> {
    let bundle = match key {
        AppKey::Bundle(bundle) => Some(bundle.to_string()),
        AppKey::Label(_) => block
            .as_ref()
            .and_then(|block| block.get("bundle"))
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    let Some(bundle) = bundle else {
        return block;
    };
    let Some(notes) = read_app_notes(dir, &bundle).filter(|n| !n.text.trim().is_empty()) else {
        return block;
    };
    let mut block = block.unwrap_or_else(|| json!({ "key": key.as_str(), "bundle": bundle }));
    let (excerpt, truncated) = notes_excerpt(&notes.text);
    block["app_notes"] = json!(excerpt);
    if truncated {
        block["app_notes_truncated"] = json!(true);
    }
    block["app_notes_hint"] = json!(format!(
        "Notes kept for this app by earlier runs or the user; read them before driving. \
         Full text: GET /agent/app-notes?bundle={bundle} (MCP: phone_app_notes); add what you learn the same way."
    ));
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
    /// Single `/agent/input` actions in a row (a batch resets it).
    singles: u32,
    last_single: Option<Instant>,
    /// The batch hint already went out for this streak.
    batch_hinted: bool,
    /// An explicit recording (`POST /agent/flow/record`): every applied step
    /// lands here too, across apps and pauses, until it is stopped or saved.
    recording: Option<Trail>,
    /// Keep the stopped draft until saved or replaced so a lost stop reply
    /// can be recovered by a read-only status request.
    stopped_recording: Option<Value>,
    recording_revision: u64,
}

/// Single actions in a row before the agent is told to batch.
pub const BATCH_HINT_AFTER: u32 = 3;
/// A pause this long between single actions breaks the streak (the agent was
/// thinking about an unknown screen, which is what single actions are for).
const SINGLE_STREAK_GAP: Duration = Duration::from_secs(60);

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
pub fn convert_action(action: &Value, rows: Option<&[crate::runner::ElementRow]>) -> Converted {
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
        "tap_locator" => copy("tap_locator", &["locator", "via", "if_present"]),
        "text" => Converted::Typed {
            clear: action.get("clear").and_then(Value::as_bool).unwrap_or(false),
        },
        "key" => copy("key", &["name"]),
        "shortcut" => copy("shortcut", &["name"]),
        "home" => Converted::Step(json!({ "kind": "shortcut", "name": "home" })),
        "back" => Converted::Step(json!({ "kind": "back" })),
        "launch_app" if action.get("bundle").is_some() => copy("launch_app", &["bundle", "fresh"]),
        "scroll" if action.get("element").is_none() => copy("scroll", &["x", "y", "dx", "dy"]),
        "swipe" => copy("swipe", &["x1", "y1", "x2", "y2", "duration_ms"]),
        "drag" => copy("drag", &["x1", "y1", "x2", "y2", "points", "hold_ms", "duration_ms"]),
        "longpress" => copy("longpress", &["x", "y", "duration_ms"]),
        "multi_longpress" => copy("multi_longpress", &["points", "duration_ms"]),
        "alert" => copy("alert", &["button", "action"]),
        "picker" => copy("picker", &["column", "value"]),
        other => Converted::Unsupported(format!(
            "a `{other}` action, which flows cannot express yet; redo that part with supported steps"
        )),
    }
}

/// A durable locator for a tree row. Never the snapshot index.
///
/// An identifier stands ALONE: `tap_locator` re-resolves through runner, which
/// can query an identifier only by itself (accessibility id) — paired with
/// `kind` it degrades to a kind-only query and every key on a keypad matches
/// (hardware: Calculator `{identifier:"One",kind:"Key"}` → ambiguous).
/// Without one, label + kind.
pub fn locator_for_row(row: &crate::runner::ElementRow) -> Option<Value> {
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
        if finished.steps.len() >= 2 {
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
        self.note_batch();
    }

    /// One single action applied. Returns a `batch_hint` block the first time
    /// a streak reaches [`BATCH_HINT_AFTER`].
    pub fn note_single(&mut self, now: Instant) -> Option<Value> {
        if self
            .last_single
            .is_some_and(|last| now.duration_since(last) > SINGLE_STREAK_GAP)
        {
            self.note_batch();
        }
        self.singles += 1;
        self.last_single = Some(now);
        if self.singles < BATCH_HINT_AFTER || self.batch_hinted {
            return None;
        }
        self.batch_hinted = true;
        Some(json!({
            "single_actions_in_a_row": self.singles,
            "message": "Several single actions in a row: each one costs you a full turn. Send the steps you already \
                        understand as ONE batch (phone_run_steps / POST /agent/actions, wait_for between screens) \
                        with observe:true, which returns the resulting screen in the same reply. Keep single \
                        actions for exploring a screen you have not read yet."
        }))
    }

    /// A batch ran: the streak is over.
    pub fn note_batch(&mut self) {
        self.singles = 0;
        self.last_single = None;
        self.batch_hinted = false;
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
        if let Some(recording) = self.recording.as_mut() {
            if recording.label.is_none() {
                recording.label = Some(label.to_string());
            }
        }
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

    /// Start an explicit recording, dropping one in progress.
    pub fn start_recording(&mut self) {
        self.recording_revision = self.recording_revision.wrapping_add(1);
        self.stopped_recording = None;
        self.recording = Some(Trail::default());
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Steps recorded so far, or `None` when not recording.
    pub fn recording_steps(&self) -> Option<usize> {
        self.recording.as_ref().map(|r| r.steps.len())
    }

    /// End the recording and return its draft (`None` when nothing was
    /// recording or it holds no step).
    pub fn stop_recording(&mut self) -> Option<Value> {
        let recording = self.recording.take()?;
        let draft = (!recording.steps.is_empty()).then(|| draft_json(&recording, "recording"));
        self.stopped_recording = draft.clone();
        draft
    }

    /// The active draft, or the unsaved stopped draft until a new recording starts.
    pub fn recording_draft(&self) -> Option<Value> {
        let Some(recording) = self.recording.as_ref() else {
            return self.stopped_recording.clone();
        };
        (!recording.steps.is_empty()).then(|| draft_json(recording, "recording"))
    }

    /// Saving without an explicit document uses the recording, including a
    /// stopped one whose response was lost, before an unrelated task trail.
    pub fn draft_for_save(&mut self) -> Option<Value> {
        self.stop_recording().or_else(|| self.recording_draft()).or_else(|| self.draft())
    }

    pub fn recording_revision(&self) -> u64 {
        self.recording_revision
    }

    /// A successful save acknowledges only the exact stopped document that
    /// was saved. Another recording started during the write stays intact.
    pub fn acknowledge_recording(&mut self, flow: &Value, revision: u64) {
        if revision == self.recording_revision
            && self.stopped_recording.as_ref().and_then(|draft| draft.get("flow")) == Some(flow)
        {
            self.stopped_recording = None;
        }
    }

    fn record_into_recording(&mut self, converted: &Converted, now: Instant) {
        let Some(recording) = self.recording.as_mut() else {
            return;
        };
        if recording.steps.len() >= TRAIL_MAX_STEPS {
            return;
        }
        recording.last = Some(now);
        match converted {
            Converted::Step(step) => {
                let mut step = step.clone();
                if step.get("kind").and_then(Value::as_str) == Some("launch_app") {
                    if recording.bundle.is_none() {
                        recording.bundle = step.get("bundle").and_then(Value::as_str).map(str::to_string);
                    }
                    // A recording starts its apps fresh, and so does its replay:
                    // an app reopened where it was left would miss the steps.
                    step["fresh"] = json!(true);
                }
                recording.steps.push(step);
            }
            Converted::Typed { clear } => {
                recording.inputs += 1;
                let mut step = json!({ "kind": "type", "input": format!("text{}", recording.inputs) });
                if *clear {
                    step["clear"] = json!(true);
                }
                recording.steps.push(step);
            }
            Converted::Unsupported(note) => recording.todo.push(note.clone()),
        }
    }

    /// Record one applied step.
    pub fn record(&mut self, converted: Converted, now: Instant) {
        self.record_into_recording(&converted, now);
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
                Some(previous) if previous.worth_a_flow() && !previous.suggested => previous,
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

    fn row(kind: &str, label: &str, identifier: Option<&str>) -> crate::runner::ElementRow {
        crate::runner::ElementRow {
            kind: kind.into(),
            label: label.into(),
            identifier: identifier.map(str::to_string),
            ..Default::default()
        }
    }

    fn at(kind: &str, label: &str, rect: [f64; 4]) -> crate::runner::ElementRow {
        crate::runner::ElementRow { rect, ..row(kind, label, None) }
    }

    #[test]
    fn a_persons_tap_records_the_smallest_labelled_element_under_the_finger() {
        let rows = vec![
            at("Cell", "电池", [0.0, 100.0, 400.0, 44.0]),
            at("StaticText", "电池", [16.0, 110.0, 40.0, 20.0]),
            at("Button", "", [300.0, 100.0, 100.0, 44.0]),
            crate::runner::ElementRow { visible: Some(false), ..at("Button", "隐藏", [0.0, 100.0, 10.0, 10.0]) },
        ];
        // On the text: the text (smallest) wins.
        assert_eq!(locator_at(&rows, 20.0, 115.0).unwrap()["label"], "电池");
        // Inside the cell but off the text, and the unlabelled button is skipped.
        let locator = locator_at(&rows, 350.0, 120.0).unwrap();
        assert_eq!(locator["kind"], "Cell", "{locator}");
        assert!(locator_at(&rows, 5.0, 400.0).is_none(), "nothing there: keep the coordinate");
        assert_eq!(locator_at(&rows, 5.0, 105.0).unwrap()["kind"], "Cell", "a hidden row is never the target");
    }

    #[test]
    fn a_recording_spans_apps_and_pauses_and_types_become_inputs() {
        let mut trail = FlowTrail::default();
        let t0 = Instant::now();
        trail.record(Converted::Step(json!({"kind":"shortcut","name":"home"})), t0);
        assert!(trail.recording_draft().is_none(), "nothing before start");
        trail.start_recording();
        trail.launched("com.apple.Preferences", t0);
        trail.record(Converted::Step(json!({"kind":"launch_app","bundle":"com.apple.Preferences"})), t0);
        trail.record(Converted::Step(json!({"kind":"tap_locator","locator":{"label":"电池","kind":"Cell"}})), t0);
        // Long pause and another app: an auto trail would rotate, the recording keeps going.
        let later = t0 + TRAIL_IDLE + Duration::from_secs(5);
        trail.launched("com.apple.mobilesafari", later);
        trail.record(Converted::Step(json!({"kind":"launch_app","bundle":"com.apple.mobilesafari"})), later);
        trail.record(Converted::Typed { clear: true }, later);
        trail.record(Converted::Unsupported("a pinch".into()), later);
        assert_eq!(trail.recording_steps(), Some(4));
        let draft = trail.stop_recording().unwrap();
        let steps = draft["flow"]["steps"].as_array().unwrap();
        assert_eq!(draft["flow"]["app"], "com.apple.Preferences", "{draft}");
        assert!(steps.iter().any(|s| s["kind"] == "type" && s["input"] == "text1" && s["clear"] == true), "{draft}");
        assert!(
            steps.iter().filter(|s| s["kind"] == "launch_app").all(|s| s["fresh"] == true),
            "a recording replays its apps from their first screen: {draft}"
        );
        assert!(draft.to_string().contains("a pinch"), "unsupported steps become todo: {draft}");
        assert!(!trail.is_recording());
        assert_eq!(trail.recording_draft(), Some(draft.clone()), "recover a lost stop reply");
        assert!(trail.stop_recording().is_none());
        assert_eq!(trail.draft_for_save(), Some(draft.clone()), "save the stopped recording, not the recent task");
        assert_eq!(trail.recording_draft(), Some(draft.clone()), "a duplicate stop must not erase it");
        let revision = trail.recording_revision();
        trail.acknowledge_recording(&json!({"steps": []}), revision);
        assert_eq!(trail.recording_draft(), Some(draft.clone()), "saving another document must not discard it");
        trail.acknowledge_recording(&draft["flow"], revision.wrapping_sub(1));
        assert_eq!(trail.recording_draft(), Some(draft.clone()), "an old save must not acknowledge a later recording");
        trail.acknowledge_recording(&draft["flow"], revision);
        assert!(trail.recording_draft().is_none(), "a successfully saved draft is no longer pending");
        trail.start_recording();
        assert!(trail.recording_draft().is_none(), "a new recording clears the old draft");
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
        assert!(none["next"].as_str().unwrap().contains("draft"));
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
    fn a_streak_of_single_actions_is_told_once_to_batch() {
        let now = Instant::now();
        let mut trail = FlowTrail::default();
        assert!(trail.note_single(now).is_none());
        assert!(trail.note_single(now).is_none());
        let hint = trail.note_single(now).expect("third single action");
        assert!(hint["message"].as_str().unwrap().contains("observe:true"));
        assert!(trail.note_single(now).is_none(), "once per streak");
        trail.note_batch();
        assert!(trail.note_single(now).is_none());
        // A long pause (reading an unknown screen) breaks the streak.
        let mut slow = FlowTrail::default();
        for i in 0..5 {
            assert!(slow.note_single(now + SINGLE_STREAK_GAP * (i + 1) * 2).is_none());
        }
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
    fn short_task_draft_survives_returning_to_the_app_without_a_suggestion() {
        let now = Instant::now();
        let dir = tempfile::tempdir().unwrap();
        let mut trail = FlowTrail::default();
        trail.launched("com.apple.Preferences", now);
        trail.record(convert_action(&json!({"type":"launch_app","bundle":"com.apple.Preferences"}), None), now);
        trail.saw_app(Some("设置"), now);
        trail.record(Converted::Step(json!({"kind":"tap_locator","locator":{"label":"电池","kind":"Cell"}})), now);
        trail.launched("com.leeguoo.iphone-use.remote", now);
        trail.record(convert_action(&json!({"type":"launch_app","bundle":"com.leeguoo.iphone-use.remote"}), None), now);
        trail.saw_app(Some("Phone Use Remote"), now);
        let draft = trail.draft().expect("explicit Save as flow retains a short finished task");
        assert_eq!(draft["flow"]["app"], "com.apple.Preferences");
        assert_eq!(draft["flow"]["steps"].as_array().unwrap().len(), 3);
        assert!(trail.suggestion(&dir.path().join("cooldown.json"), 100).is_none(), "short drafts must not trigger automatic suggestions");
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

    #[test]
    fn app_notes_ride_along_in_the_registry_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(dir.path());
        let notes = dir.path().join("app-notes");
        // No notes: the block is untouched.
        let block = with_app_notes(
            registry_block_from(Some(&path), AppKey::Bundle("com.apple.Health")),
            AppKey::Bundle("com.apple.Health"),
            &notes,
        )
        .unwrap();
        assert!(block.get("app_notes").is_none(), "{block}");

        write_app_notes(&notes, "com.apple.Health", "Steps live under Browse.\n").unwrap();
        for key in [AppKey::Bundle("com.apple.Health"), AppKey::Label("健康")] {
            let block = with_app_notes(registry_block_from(Some(&path), key), key, &notes).unwrap();
            assert_eq!(block["app_notes"], "Steps live under Browse.", "{block}");
            assert!(block["flows"].is_array(), "{block}");
        }
        let read = read_app_notes(&notes, "com.apple.Health").unwrap();
        assert_eq!(read.text, "Steps live under Browse.\n");
        assert!(read.updated_at.is_some());

        // An unreadable store still carries the notes; long notes are cut.
        let long = "长".repeat(1000);
        write_app_notes(&notes, "com.example.x", &long).unwrap();
        let block = with_app_notes(None, AppKey::Bundle("com.example.x"), &notes).unwrap();
        let excerpt = block["app_notes"].as_str().unwrap();
        assert!(excerpt.len() <= APP_NOTES_IN_BLOCK + "…".len(), "{}", excerpt.len());
        assert!(excerpt.ends_with('…'));
        assert_eq!(block["app_notes_truncated"], true);
        assert_eq!(block["bundle"], "com.example.x");

        // Blank text deletes; path tricks are refused.
        write_app_notes(&notes, "com.example.x", "  ").unwrap();
        assert!(read_app_notes(&notes, "com.example.x").is_none());
        write_app_notes(&notes, "com.example.x", "").unwrap();
        for bad in ["", "../etc", "a/b", ".hidden", "a..b", "x y"] {
            assert!(!valid_app_notes_bundle(bad), "{bad}");
            assert!(write_app_notes(&notes, bad, "x").is_err(), "{bad}");
        }
    }
}

/// Kinds a person's tap means to hit (the smallest one under the finger wins).
const TAPPABLE: [&str; 15] = [
    "Button", "Cell", "Link", "Key", "Switch", "Toggle", "TextField", "SecureTextField",
    "SearchField", "TextView", "Tab", "Icon", "MenuItem", "SegmentedControl", "StaticText",
];

/// What a person's tap at `(x, y)` (points) hit, as a durable locator: the
/// smallest visible, tappable element under the point that has a label or an
/// identifier. `None` falls back to recording the coordinate.
pub fn locator_at(rows: &[crate::runner::ElementRow], x: f64, y: f64) -> Option<Value> {
    rows.iter()
        .filter(|row| TAPPABLE.contains(&row.kind.as_str()))
        .filter(|row| row.visible != Some(false))
        .filter(|row| {
            let [rx, ry, rw, rh] = row.rect;
            rw > 0.0 && rh > 0.0 && x >= rx && x <= rx + rw && y >= ry && y <= ry + rh
        })
        .filter_map(|row| locator_for_row(row).map(|locator| (row.rect[2] * row.rect[3], locator)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, locator)| locator)
}

#[cfg(test)]
mod drag_path_tests {
    use super::*;
    #[test]
    fn multi_hold_recording_preserves_every_finger_and_duration() {
        let action = json!({"type":"multi_longpress","points":[{"x":0.3,"y":0.4},{"x":0.5,"y":0.4},{"x":0.7,"y":0.4}],"duration_ms":1500});
        let Converted::Step(step) = convert_action(&action, None) else {
            panic!("multi hold must be recordable")
        };
        assert_eq!(step["kind"], "multi_longpress");
        assert_eq!(step["points"], action["points"]);
        assert_eq!(step["duration_ms"], action["duration_ms"]);
    }

    #[test]
    fn drag_recording_preserves_waypoints_and_dwell_for_replay() {
        let action = json!({"type":"drag","points":[{"x":0.3,"y":0.4},{"x":0.13,"y":0.5,"dwell_ms":900},{"x":0.5,"y":0.4}],"hold_ms":400,"duration_ms":300});
        let Converted::Step(step) = convert_action(&action, None) else {
            panic!("drag must be recordable")
        };
        assert_eq!(step["kind"], "drag");
        for key in ["points", "hold_ms", "duration_ms"] {
            assert_eq!(step[key], action[key]);
        }
        assert!(step.get("x1").is_none());
    }
}
