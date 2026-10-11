//! MCP `ServerHandler` implementation — `PhoneHandler`.
//!
//! Each MCP tool maps onto a deliberately supported subset of the daemon's
//! agent API. Device-target changes and destructive maintenance stay outside
//! this surface.
//!
//! Pattern:
//!   1. `#[tool_router]` on the `impl PhoneHandler` block generates the static
//!      `PhoneHandler::tool_router()` fn.
//!   2. `#[tool_handler]` on the `impl ServerHandler` block fills in
//!      defaults we do not override. Our `call_tool` routes and shapes the
//!      result, then adapts image-bearing results for the initialized peer.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, Content, Implementation, InitializeResult, ProtocolVersion,
        ServerCapabilities,
    },
    schemars, tool, tool_handler, tool_router, ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{client::DaemonClient, types::InputMsg};

// ---------------------------------------------------------------------------
// Parameter types (one struct per tool that takes arguments)
// ---------------------------------------------------------------------------

/// Parameters for [`phone_tap`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TapParams {
    /// Horizontal position, normalized 0–1 (0 = left edge, 1 = right edge).
    pub x: f64,
    /// Vertical position, normalized 0–1 (0 = top edge, 1 = bottom edge).
    pub y: f64,
    /// On by default: the result carries what the screen settled to — what
    /// changed since your last look (`delta`), or the whole tree the first
    /// time — so you rarely need `phone_elements` afterwards. The settled
    /// screen is also captured but NOT sent: `phone_screenshot` returns it at
    /// once if you need to see it. `false` skips both for the fastest bare
    /// action.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_scroll`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScrollParams {
    /// Horizontal anchor position, normalized 0–1.
    pub x: f64,
    /// Vertical anchor position, normalized 0–1.
    pub y: f64,
    /// Horizontal scroll delta. Positive reveals content to the right.
    pub dx: f64,
    /// Vertical scroll delta. **Positive dy reveals content farther down**;
    /// negative dy reveals content above. ~80 ≈ 15% of a screen, ~400 ≈ 75%.
    pub dy: f64,
    /// Default true: return what the screen settled to (as on phone_tap).
    /// `false` for the fastest bare action.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_type`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TypeParams {
    /// Unicode text to send through the device-side input service. Focus the
    /// intended field and verify it before typing. Up to 20000 characters;
    /// text over 200 is typed in chunks (about a minute per 3000 characters),
    /// and a failure part-way reports `characters_confirmed` and
    /// `remaining_text` with retry_safe=false — then type only the rest.
    pub text: String,
    /// Default true: return what the screen settled to (as on phone_tap).
    /// `false` for the fastest bare action.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_apps`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppsParams {
    /// The app's name as a person would say it (微信, WeChat, 招商银行, Settings)
    /// or a bundle id. 1–100 characters.
    pub query: String,
    /// `auto` (default: the phone's own app list, then the bundled catalog,
    /// then the App Store), `installed`, `catalog` (offline), or `apple`.
    #[serde(default)]
    pub source: Option<String>,
    /// App Store storefront for the store search, two letters. Default `cn`.
    #[serde(default)]
    pub country: Option<String>,
    /// 1–30 candidates. Default 10.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters for [`phone_elements`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementsParams {
    /// Keep only rows whose label, identifier, value or placeholder contains
    /// this text (case-insensitive). Rows keep their original index.
    #[serde(default)]
    pub query: Option<String>,
    /// Return at most this many rows; the answer says how many were left out.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Text read from a fresh screenshot (Vision OCR) for screens the tree
    /// cannot describe (games, canvases, video): `auto` adds it only when the
    /// tree has no interactive rows, `force` always, `off` (default) never.
    #[serde(default)]
    pub ocr: Option<String>,
}

/// Parameters for [`phone_tap_label`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TapLabelParams {
    /// The element's visible accessibility label, exactly as shown by
    /// `phone_elements` (e.g. "新备忘录", "Connect").
    pub label: String,
    /// Default true: return what the screen settled to (as on phone_tap).
    /// `false` for the fastest bare action.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_tap_element`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TapElementParams {
    /// Zero-based element index from `phone_elements`.
    pub element: usize,
    /// Snapshot token from the same `phone_elements` response.
    pub snapshot: String,
    /// Default true: return what the screen settled to (as on phone_tap).
    /// `false` for the fastest bare action.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// `phone_screenshot`'s default size: legible, and ~40 % fewer image tokens
/// than a full-resolution capture.
const DEFAULT_SCREENSHOT_MAX_SIDE: u32 = 1200;

/// Parameters for [`phone_screenshot`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotParams {
    /// Longest side of the image in pixels. Default 1200, which reads fine and
    /// costs ~0.9k image tokens instead of ~1.5k at full resolution; ask for
    /// 0 (full resolution) only to read very small text.
    #[serde(default)]
    pub max_side: Option<u32>,
    /// Draw each interactive element's frame and its phone_elements index on
    /// the image, to match what you see with the rows you can tap.
    #[serde(default)]
    pub overlay: Option<bool>,
}

/// Parameters for [`phone_screen_frame`] (called by the screen panel only).
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenFrameParams {
    /// Longest side of the frame in pixels (200–1000, default 600).
    #[serde(default)]
    pub max_side: Option<u32>,
    /// Capture now instead of reusing the settled or live frame.
    #[serde(default)]
    pub fresh: Option<bool>,
}

/// Parameters for [`phone_key`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyParams {
    /// Supported names: `return`/`enter`, `escape`, `space`, `tab`,
    /// `delete`/`backspace`, `up`, `down`, `left`, `right`.
    pub name: String,
    /// Default true: return what the screen settled to (as on phone_tap).
    /// `false` for the fastest bare action.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_shortcut`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShortcutParams {
    /// Supported names: `home` (Home Screen) and `spotlight` (search).
    /// App Switcher is unsupported by the device runner.
    pub name: String,
    /// Default true: return what the screen settled to (as on phone_tap).
    /// `false` for the fastest bare action.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_run_steps`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunStepsParams {
    /// Ordered steps. The daemon validates the whole list before sending the
    /// first action and stops immediately when any action or wait condition
    /// fails.
    pub steps: Vec<PhoneStep>,
    /// Return the screen the batch ended on (`snapshot`, `elements`, `alert`)
    /// in the same reply, so the next decision needs no phone_elements call.
    /// Default true; set false only when you will not look at the result.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// One step in a bounded multi-step sequence.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PhoneStep {
    /// Tap normalized screen coordinates. Prefer `tap_locator` (durable in a
    /// saved flow) whenever the target has a label or identifier.
    Tap {
        x: f64,
        y: f64,
        #[serde(default)]
        after_ms: u64,
    },
    /// Long-press normalized screen coordinates.
    Longpress {
        x: f64,
        y: f64,
        #[serde(default = "default_phone_longpress_ms")]
        duration_ms: u64,
        #[serde(default)]
        after_ms: u64,
    },
    /// Swipe once from one normalized screen point to another.
    Swipe {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        #[serde(default = "default_phone_swipe_ms")]
        duration_ms: u64,
        #[serde(default)]
        after_ms: u64,
    },
    /// Hold, then drag once between two normalized screen points.
    Drag {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        #[serde(default = "default_phone_drag_hold_ms")]
        hold_ms: u64,
        #[serde(default = "default_phone_swipe_ms")]
        duration_ms: u64,
        #[serde(default)]
        after_ms: u64,
    },
    /// Tap one exact, unique accessibility label.
    TapLabel {
        label: String,
        #[serde(default)]
        after_ms: u64,
    },
    /// Tap the one current element matching every supplied semantic locator
    /// field. Zero or multiple matches send no tap.
    TapLocator {
        locator: PhoneElementLocator,
        /// `point`: tap the centre of the element's live frame instead of
        /// XCUIElement's click — for custom controls that ACK a click and do
        /// nothing. Default `element`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<String>,
        /// Pass instead of failing when no element matches — for screens that
        /// appear only sometimes ("Not now", "Skip"). Nothing is sent then.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        if_present: bool,
        #[serde(default)]
        after_ms: u64,
    },
    /// Type Unicode text into the focused field. `clear=true` clears that field
    /// immediately before inserting the text as one compound action.
    Type {
        text: String,
        #[serde(default)]
        clear: bool,
        #[serde(default)]
        after_ms: u64,
    },
    /// Send one supported device-native key.
    Key {
        name: String,
        #[serde(default)]
        after_ms: u64,
    },
    /// Trigger Home or Spotlight.
    Shortcut {
        name: String,
        #[serde(default)]
        after_ms: u64,
    },
    /// Scroll with the same normalized anchor and deltas as `phone_scroll`.
    Scroll {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        #[serde(default)]
        after_ms: u64,
    },
    /// Launch or foreground an installed app: give exactly one of `bundle`
    /// (its exact bundle identifier) or `app` (its exact name — 微信, WeChat,
    /// 招商银行, Settings — which the daemon resolves through phone_apps; an
    /// ambiguous or unknown name is refused with candidates, nothing sent).
    LaunchApp {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bundle: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<String>,
        /// Quit the app first so it opens on its first screen, not the page
        /// it was left on (flows recorded on the phone start there).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        fresh: bool,
        #[serde(default)]
        after_ms: u64,
    },
    /// Open a link (`https://…`, or a deep link such as `weixin://`) with
    /// the app iOS assigns to it. http/https always work; other schemes
    /// (tel, sms, mailto, app deep links) are refused unless the operator
    /// allowed them in IPHONE_USE_ALLOW_URL_SCHEMES. It leaves the current
    /// app, so guard the next screen with `wait_for`.
    OpenUrl {
        url: String,
        #[serde(default)]
        after_ms: u64,
    },
    /// Navigate back inside the current application.
    Back {
        #[serde(default)]
        after_ms: u64,
    },
    /// Press a button on a system alert (UIAlertController) through the device runner's
    /// native alert route — the only path that actually acts on one; taps on
    /// alert buttons are acknowledged without effect. Give the exact button
    /// text, or `action: accept|dismiss` for the default/cancel button. Fails
    /// closed when no alert is showing.
    Alert {
        #[serde(default)]
        button: Option<String>,
        #[serde(default)]
        action: Option<String>,
        /// Pass instead of failing when no alert is up — for prompts that
        /// appear only sometimes ("Allow Paste"). Flows using it need a
        /// client that knows it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        if_present: bool,
        #[serde(default)]
        after_ms: u64,
    },
    /// Select a value in a native picker wheel.
    Picker {
        #[serde(default)]
        column: usize,
        value: String,
        #[serde(default)]
        after_ms: u64,
    },
    /// Invoke a named affordance on a snapshot-bound element: toggle a
    /// switch, increment/decrement or adjust a slider or stepper, and the
    /// other names in phone_capabilities (`perform_actions`). `element` and
    /// `snapshot` come from phone_elements. Snapshot-bound, so not for saved
    /// flows. `value` only with `adjust`; `duration_ms` only with
    /// `menu`/`force_press`. Verify the new `value` with wait_for.
    Perform {
        element: u64,
        snapshot: String,
        action: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default)]
        after_ms: u64,
    },
    /// Poll the current element tree until the semantic expectation holds.
    WaitFor {
        expect: PhoneUiExpectation,
        #[serde(default = "default_phone_wait_ms")]
        timeout_ms: u64,
        #[serde(default = "default_phone_poll_ms")]
        poll_ms: u64,
    },
    /// A short animation pause. Prefer `wait_for` for correctness.
    Pause { ms: u64 },
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhoneUiExpectation {
    /// Exact foreground Application label, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    /// Every locator in this list must match at least one current element.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub present: Vec<PhoneElementLocator>,
    /// Every locator in this list must match no current element.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absent: Vec<PhoneElementLocator>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhoneElementLocator {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
}

fn default_phone_wait_ms() -> u64 {
    15_000
}

fn default_phone_longpress_ms() -> u64 {
    600
}

fn default_phone_swipe_ms() -> u64 {
    300
}

fn default_phone_drag_hold_ms() -> u64 {
    500
}

fn default_phone_poll_ms() -> u64 {
    250
}

/// Parameters for [`phone_flow_list`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowListParams {
    /// Only flows in this registry category (e.g. `health`, `system`, `finance`, `im`).
    #[serde(default)]
    pub category: Option<String>,
    /// Only flows for this app directory (e.g. `health`) or bundle id.
    #[serde(default)]
    pub app: Option<String>,
    /// Only flows with at least one recorded hardware verification.
    #[serde(default)]
    pub verified: bool,
    /// Return every field of every entry (verified_on, app versions, step
    /// counts) as JSON text instead of one compact row per flow.
    #[serde(default)]
    pub detail: bool,
}

/// Parameters for [`phone_flow_info`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowInfoParams {
    /// Registry id such as `health/export-all`.
    pub id: String,
}

/// Parameters for [`phone_flow_run`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowRunParams {
    /// Registry id such as `health/export-all` (see phone_flow_list), or the
    /// path of a flow file (`*.json`) — e.g. a draft saved by phone_flow_draft,
    /// to prove it on the phone before publishing.
    pub id: String,
    /// Runtime string inputs declared by the flow. Values are used for this
    /// run only and never persisted. Never pass passwords, codes, or private
    /// message content.
    #[serde(default)]
    pub inputs: std::collections::BTreeMap<String, String>,
    /// Required for flows declared `risk: side_effect`. Set true only after
    /// the user confirmed the target and inputs.
    #[serde(default)]
    pub confirm: bool,
    /// Run even when compat is `broken` or `incompatible` for this phone.
    #[serde(default)]
    pub force: bool,
    /// Compare what the flow read (its `outputs`) against the recorded result
    /// shape; `verify.ok=false` names what drifted.
    #[serde(default)]
    pub verify: bool,
    /// With verify: record this run's output shape as the fixture (types
    /// only). Only after checking the values are right.
    #[serde(default)]
    pub write_fixture: bool,
}

/// Parameters for [`phone_jev_run`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JevRunParams {
    /// The whole goal in plain language, with every value it needs
    /// ("In Settings, turn on Low Power Mode"). Jev never invents personal data.
    pub goal: String,
    /// Bundle id to launch first, e.g. `com.apple.Preferences`.
    #[serde(default)]
    pub app: Option<String>,
    /// Step budget (default 30).
    #[serde(default)]
    pub max_steps: Option<usize>,
}

/// Parameters for [`phone_flow_draft`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowDraftParams {
    /// Optional path to write the draft flow to (a new `.json` file; existing
    /// files are never overwritten). It is validated after writing.
    #[serde(default)]
    pub save_as: Option<String>,
}

/// Parameters for [`phone_flow_publish`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowPublishParams {
    /// Flow file path, or an id installed with `flow add`.
    pub source: String,
    /// Registry id to publish as, `<app>/<flow>` lowercase slugs.
    pub id: String,
    /// Human app name, used only when the app is new to the registry.
    #[serde(default)]
    pub app_name: Option<String>,
    /// Foreground-app labels per language (e.g. ["Health","健康"]).
    #[serde(default)]
    pub aliases: Vec<String>,
    /// What was verified, where, and anything a reviewer should know.
    #[serde(default)]
    pub note: Option<String>,
    /// Must be true; set only after the user agreed to open a public PR.
    #[serde(default)]
    pub confirm: bool,
}

/// Parameters for [`phone_flow_report`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowReportParams {
    /// Registry id of the flow that failed.
    pub id: String,
    /// One or two sentences: what you expected, what the phone showed.
    #[serde(default)]
    pub note: Option<String>,
    /// Must be true; set only after the user agreed to open a public issue.
    #[serde(default)]
    pub confirm: bool,
}

// ---------------------------------------------------------------------------
// PhoneHandler
// ---------------------------------------------------------------------------

/// MCP server that forwards tool calls to the iphone-use daemon.
#[derive(Clone)]
pub struct PhoneHandler {
    daemon: DaemonClient,
    /// The most recent failed `phone_flow_run`, kept so `phone_flow_report`
    /// can file an issue with the real failure instead of a retelling.
    last_flow_failure: std::sync::Arc<std::sync::Mutex<Option<crate::contrib::ReportContext>>>,
    /// Observed actions this session took, for the `no_progress` advisory.
    progress: std::sync::Arc<std::sync::Mutex<crate::hints::ProgressTracker>>,
    /// Which tools are listed and callable (`IPHONE_USE_MCP_TOOLS`), read
    /// once at start.
    profile: std::sync::Arc<ToolProfile>,
}

/// Parameters for [`PhoneHandler::phone_login`].
#[derive(Debug, Default, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoginParams {
    /// Vault entry name or id, when several entries match the app.
    #[serde(default)]
    pub item: Option<String>,
    /// Username, to pick between entries with the same name.
    #[serde(default)]
    pub user: Option<String>,
    /// Tap the app's Log in button after filling (default true).
    #[serde(default)]
    pub submit: Option<bool>,
    /// Enter a verification code instead of logging in: sms, mail, totp or auto.
    #[serde(default)]
    pub code_via: Option<String>,
    /// Sender, brand or subject fragment of the message with the code.
    #[serde(default)]
    pub code_from: Option<String>,
}

/// Parameters for [`PhoneHandler::phone_run_start`].
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunStartParams {
    /// A short id for this task: 1–64 of A-Z a-z 0-9 . _ : @ -
    pub run_id: String,
    /// Set only if you will pass the id of EVERY model turn of this task to
    /// phone_run_end (including turns that made no tool call); only then are
    /// model round trips reported.
    #[serde(default)]
    pub complete_trace: Option<bool>,
}

/// Parameters for [`PhoneHandler::phone_run_end`].
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunEndParams {
    pub run_id: String,
    /// Every model turn id of the task, when phone_run_start declared a
    /// complete trace; `[]` for a task that used no model turns.
    #[serde(default)]
    pub turn_ids: Option<Vec<String>>,
}

/// Parameters for [`PhoneHandler::phone_collect_list`].
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollectListParams {
    /// A snapshot from an earlier read of the target app: when given, a
    /// different foreground app refuses before any swipe. Not filled in for you.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Element kind of one list row (default `Cell`; e.g. `StaticText`,
    /// `Button`, `Link`). A row without a label is named by its texts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_kind: Option<String>,
    /// Only rows whose centre lies in this normalized `[x, y, w, h]` area,
    /// and swipes inside it (default: the whole screen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Vec<f64>>,
    /// Pages to read, 1–10 (default 6). One swipe between pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<u32>,
    /// Exact label that marks the end of the list ("没有更多了", "No more
    /// results"). Only this makes the result `complete:true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_label: Option<String>,
    /// `down` (default) reveals rows below, `up` rows above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
}

/// Parameters for [`PhoneHandler::phone_scroll_find`].
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScrollFindParams {
    /// A snapshot from an earlier read of the target app: when given, a
    /// different foreground app refuses before any swipe. Not filled in for you.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Exact label to find.
    pub label: String,
    /// Narrow to one element kind (e.g. `Button`) when a text shares the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Swipes allowed when the label is not on screen, 0–5 (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_swipes: Option<u32>,
    /// `down` (default) or `up`: which way to look when the label is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// Swipe inside this normalized `[x, y, w, h]` area (default: whole screen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Vec<f64>>,
}

/// Parameters for [`PhoneHandler::phone_hold`].
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HoldParams {
    /// Seconds to keep the phone (0 clears the hold; at most 14400).
    pub secs: u64,
    /// The person is signing in (passcode, Face ID, a code): nothing reads
    /// the screen until the hold ends or is cleared — screenshots, elements
    /// and live views are refused with `screen_private`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
}

/// Parameters for [`PhoneHandler::phone_logs`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogsParams {
    /// `start` capturing the phone's system log, `read` (default) what was
    /// captured, `stop`, or `clear` the buffer. Capture stops by itself when
    /// nobody reads for a while.
    #[serde(default)]
    pub action: Option<String>,
    /// read: only this process (exact name, case-insensitive).
    #[serde(default)]
    pub process: Option<String>,
    /// read: only messages containing this text (case-insensitive).
    #[serde(default)]
    pub contains: Option<String>,
    /// read: this level or more severe: debug, info, notice, error, fault.
    #[serde(default)]
    pub level: Option<String>,
    /// read: lines after this cursor — the previous read's `next`.
    #[serde(default)]
    pub since: Option<u64>,
    /// read: at most this many lines (default 50).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters for [`PhoneHandler::phone_crashes`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CrashesParams {
    /// Read this report (a `name` from the list). Omit to list reports.
    #[serde(default)]
    pub name: Option<String>,
    /// List: only reports of processes whose name contains this text.
    #[serde(default)]
    pub process: Option<String>,
    /// List: at most this many reports (default 50).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters for [`PhoneHandler::phone_set_location`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetLocationParams {
    /// Latitude in decimal degrees (WGS84), -90..90.
    #[serde(default)]
    pub lat: Option<f64>,
    /// Longitude in decimal degrees (WGS84), -180..180.
    #[serde(default)]
    pub lon: Option<f64>,
    /// `true` (without lat/lon) returns the phone to its real location.
    #[serde(default)]
    pub clear: Option<bool>,
}

/// Parameters for [`PhoneHandler::phone_record`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordParams {
    /// `start` recording the screen to an MP4, `stop` and save it, or
    /// `status`.
    pub action: String,
}

/// Parameters for [`PhoneHandler::phone_install_app`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallAppParams {
    /// Absolute path of an `.ipa` or `.app` on the Mac running the daemon.
    #[serde(default)]
    pub path: Option<String>,
    /// An https URL of an `.ipa` (at most 2 GB); the daemon downloads it.
    #[serde(default)]
    pub url: Option<String>,
    /// Uninstall the app first (its data goes with it), for a clean start.
    #[serde(default)]
    pub fresh: Option<bool>,
}

/// Parameters for [`PhoneHandler::phone_app_notes`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppNotesParams {
    /// The app's bundle id (phone_apps finds it by name).
    pub bundle: String,
    /// Markdown that REPLACES the app's notes (at most 16 KB). Omit to read
    /// them.
    #[serde(default)]
    pub text: Option<String>,
}

impl PhoneHandler {
    pub fn new(daemon: DaemonClient) -> Self {
        Self::with_shared_profile(daemon, std::sync::Arc::new(ToolProfile::from_env()))
    }

    /// One handler per MCP session (`--listen` serves many), sharing the
    /// profile read once at start.
    pub(crate) fn with_shared_profile(
        daemon: DaemonClient,
        profile: std::sync::Arc<ToolProfile>,
    ) -> Self {
        Self {
            daemon,
            last_flow_failure: std::sync::Arc::new(std::sync::Mutex::new(None)),
            progress: std::sync::Arc::default(),
            profile,
        }
    }

    /// The same handler exposing `profile` instead of the environment's.
    #[cfg(test)]
    fn with_profile(mut self, profile: ToolProfile) -> Self {
        self.profile = std::sync::Arc::new(profile);
        self
    }

    /// Start tracking an observed action: the screen and the action's key are
    /// fixed now. Dropping the guard unfinished (a cancelled call) resets.
    fn progress_guard(&self, action: &crate::hints::Action) -> crate::hints::ProgressGuard {
        crate::hints::ProgressGuard::begin(&self.progress, action)
    }

    /// Judge an observed action's result; append a `no_progress` advisory
    /// when the tracker can prove the same action keeps changing nothing.
    /// Advice only: nothing is resent or undone. Any result without usable
    /// evidence (an error, a transport failure) resets the tracker.
    fn note_progress(
        &self,
        guard: crate::hints::ProgressGuard,
        mut result: CallToolResult,
    ) -> CallToolResult {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let line = guard.finish(result.structured_content.as_ref(), now_ms);
        // A daemon that judged the same answer already said so (its
        // `no_progress` is rendered with the observation): say it once.
        let daemon_said = result
            .structured_content
            .as_ref()
            .is_some_and(|json| json.get("no_progress").is_some());
        if let Some(line) = line.filter(|_| !daemon_said) {
            result.content.push(Content::text(line));
        }
        result
    }

    /// Anything that changes or replaces the screen without an observation
    /// the tracker can judge (typing, keys, shortcuts, batches, flows, Jev,
    /// reconnects, released leases): forget the screen.
    fn progress_reset(&self) {
        if let Ok(mut tracker) = self.progress.lock() {
            tracker.reset();
        }
    }

    fn remember_flow_failure(&self, context: crate::contrib::ReportContext) {
        if let Ok(mut slot) = self.last_flow_failure.lock() {
            *slot = Some(context);
        }
    }

    fn take_flow_failure(&self, id: &str) -> Option<crate::contrib::ReportContext> {
        let slot = self.last_flow_failure.lock().ok()?;
        slot.as_ref().filter(|context| context.id == id).cloned()
    }
}

// `#[tool_router]` (no `server_handler` flag) generates the static
// `PhoneHandler::tool_router()` fn used by `#[tool_handler]` below.
#[tool_router]
impl PhoneHandler {
    // -----------------------------------------------------------------------
    // phone_capabilities
    // -----------------------------------------------------------------------

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "What this iphone-use build supports, and whether the phone can be \
        driven right now. Read-only: it opens no device connection, wakes nothing, and \
        takes no owner lease, so it is safe to call before deciding what to do. \
        `supported` is static for the configured backend (single-step and batch action \
        vocabularies, the closed `perform` set, whether the element tree and post-action \
        observation exist, and which `mode` values would be accepted). `available` is a \
        cached snapshot with `blocked_by` naming what stands in the way \
        (locked, released, reconnecting, human_handoff, owned_by_other, offline); \
        `ok:null` means the daemon has no evidence either way, not that it is fine. \
        Scope is UI control and observation only — it says nothing about app \
        install/uninstall or other management surfaces."
    )]
    async fn phone_capabilities(&self) -> CallToolResult {
        match self.daemon.capabilities().await {
            Ok(response) => daemon_read_result(&response),
            Err(e) => CallToolResult::error(vec![Content::text(format!(
                "capabilities failed: {e:#}"
            ))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_apps
    // -----------------------------------------------------------------------

    #[tool(
        annotations(read_only_hint = true, open_world_hint = true),
        description = "Find an app's bundle id by name instead of guessing it. Read-only: \
        it never touches the screen and takes no owner lease. Searches the phone's own app \
        list, then a bundled catalog of popular apps (bundle ids verified against Apple's \
        lookup API, plus Apple's built-in apps), then the App Store only when neither \
        matched. Exact name/alias matches hide partial ones. Each candidate carries name, \
        bundle_id, source (installed|catalog|apple), installed_verified, and publisher when \
        known; installed_verified=false with installation_checked=false means unknown, not \
        absent. Pick the candidate whose name and publisher fit, then launch it with \
        launch_app bundle — or give launch_app `app` the exact name and let the daemon \
        resolve it (it refuses ambiguous names with candidates instead of guessing)."
    )]
    async fn phone_apps(
        &self,
        Parameters(AppsParams {
            query,
            source,
            country,
            limit,
        }): Parameters<AppsParams>,
    ) -> CallToolResult {
        match self
            .daemon
            .apps_lookup(&query, source.as_deref(), country.as_deref(), limit)
            .await
        {
            Ok(response) => daemon_read_result(&response),
            Err(e) => CallToolResult::error(vec![Content::text(format!(
                "app lookup failed: {e:#}"
            ))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_screenshot
    // -----------------------------------------------------------------------

    #[tool(
        annotations(read_only_hint = true, open_world_hint = true),
        description = "Capture the current iPhone screen through the device runner and \
        return it as an image/png content block (1200 px on the long side unless max_side \
        says otherwise). Right after an observed action this is instant: the screen as it \
        settled was already captured. overlay=true draws each interactive element's frame and \
        phone_elements index on it. Capture only when a current user-requested task \
        needs phone pixels, never for health checks. If the phone is released or offline, \
        follow phone_status's guidance before reconnecting."
    )]
    async fn phone_screenshot(
        &self,
        Parameters(ScreenshotParams { max_side, overlay }): Parameters<ScreenshotParams>,
    ) -> CallToolResult {
        let max_side = match max_side {
            None => Some(DEFAULT_SCREENSHOT_MAX_SIDE),
            Some(0) => None,
            Some(side) => Some(side),
        };
        match self
            .daemon
            .screenshot_with(max_side, overlay.unwrap_or(false))
            .await
        {
            Ok(bytes) if !bytes.is_empty() => {
                let b64 = B64.encode(&bytes);
                CallToolResult::success(vec![Content::image(b64, "image/png")])
            }
            Ok(_) => CallToolResult::error(vec![Content::text(
                "screenshot returned empty data — inspect phone_status device_state, \
                 screen_state, hint, setup_blocked_on, setup_phase, and setup_message",
            )]),
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("screenshot failed: {e:#}"))])
            }
        }
    }

    // -----------------------------------------------------------------------
    // phone_tap
    // -----------------------------------------------------------------------

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Tap the iPhone screen at a normalized position. \
        x and y are in the range 0–1 where (0,0) is the top-left corner and \
        (1,1) is the bottom-right corner. Prefer phone_elements + \
        phone_tap_element for semantic controls; use phone_screenshot for \
        pixel-only targets."
    )]
    async fn phone_tap(
        &self,
        Parameters(TapParams { x, y, observe }): Parameters<TapParams>,
    ) -> CallToolResult {
        let guard = self.progress_guard(&crate::hints::Action::TapPoint { x, y });
        let result = send_input_observed(&self.daemon, &InputMsg::Tap { x, y }, observe).await;
        self.note_progress(guard, result)
    }

    // -----------------------------------------------------------------------
    // phone_scroll
    // -----------------------------------------------------------------------

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Scroll with a device-side swipe. x,y = normalized anchor (0–1); \
        keep it away from text fields and screen edges. Positive dy reveals content farther \
        down, negative dy content above; dx likewise to the right/left. The swipe travels \
        1.5×|dy| points, clamped to 15–75% of the screen: dy≈80 moves about 15% of a screen, \
        dy≈400 about 75%."
    )]
    async fn phone_scroll(
        &self,
        Parameters(ScrollParams { x, y, dx, dy, observe }): Parameters<ScrollParams>,
    ) -> CallToolResult {
        let guard = self.progress_guard(&crate::hints::Action::Scroll { x, y, dx, dy });
        let result =
            send_input_observed(&self.daemon, &InputMsg::Scroll { x, y, dx, dy }, observe).await;
        self.note_progress(guard, result)
    }

    // -----------------------------------------------------------------------
    // phone_type
    // -----------------------------------------------------------------------

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Type Unicode text into the currently focused iPhone field \
        through the device runner. Check phone_status.drivable and verify the focused element \
        before typing; text lands in whichever field currently owns keyboard focus."
    )]
    async fn phone_type(
        &self,
        Parameters(TypeParams { text, observe }): Parameters<TypeParams>,
    ) -> CallToolResult {
        self.progress_reset();
        send_input_observed(&self.daemon, &InputMsg::Text { text }, observe).await
    }

    // -----------------------------------------------------------------------
    // phone_key
    // -----------------------------------------------------------------------

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Send a device-native named key. Supported names: \
        return/enter, escape, space, tab, delete/backspace, up, down, left, right. \
        Other names return an explicit unsupported error."
    )]
    async fn phone_key(
        &self,
        Parameters(KeyParams { name, observe }): Parameters<KeyParams>,
    ) -> CallToolResult {
        self.progress_reset();
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "return" | "enter" | "escape" | "space" | "tab" | "delete" | "backspace" | "up"
            | "down" | "left" | "right" => {
                send_input_observed(&self.daemon, &InputMsg::Key { name }, observe).await
            }
            _ => CallToolResult::error(vec![Content::text(format!(
                "unsupported key '{name}'; supported: return/enter, escape, space, tab, \
                 delete/backspace, up, down, left, right"
            ))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_shortcut
    // -----------------------------------------------------------------------

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Trigger a supported iOS system shortcut. \
        Supported names: \
        'home' — go to the iOS Home Screen; \
        'spotlight' — open Spotlight search. \
        'switcher' is explicitly unsupported because the device runner cannot synthesize the \
        system App Switcher gesture."
    )]
    async fn phone_shortcut(
        &self,
        Parameters(ShortcutParams { name, observe }): Parameters<ShortcutParams>,
    ) -> CallToolResult {
        self.progress_reset();
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "home" | "spotlight" => {
                send_input_observed(&self.daemon, &InputMsg::Shortcut { name }, observe).await
            }
            "switcher" => CallToolResult::error(vec![Content::text(
                "unsupported shortcut 'switcher': the device runner cannot open \
                 the iOS App Switcher; use home, spotlight, or launch an app by a \
                 supported device action instead",
            )]),
            _ => CallToolResult::error(vec![Content::text(format!(
                "unsupported shortcut '{name}'; supported: home, spotlight"
            ))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_run_steps
    // -----------------------------------------------------------------------

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "THE DEFAULT WAY TO ACT: run a sequence of actions in ONE call and get the \
        screen it ended on back in the same reply (observe, on by default) — one turn instead of one \
        per tap. The daemon validates the whole \
        sequence first, holds one control lock, and stops at the first failure. Use it as soon as a \
        segment is understood; keep single-action tools for exploring an unknown screen. Step kinds: \
        launch_app, open_url, tap_locator, tap_label, tap, type, key, shortcut, scroll, swipe, drag, \
        longpress, back, alert (system alerts; taps on them do not act), picker, perform (toggle a \
        switch, adjust a slider: element+snapshot from phone_elements), wait_for, pause. Guard every \
        screen change with wait_for; never batch an irreversible step you have not verified. These \
        step objects are exactly a saved flow's `steps` — when the response carries \
        `flow_suggestion`, ask the user whether to keep the task as a flow (phone_flow_draft). \
        The result names completed/applied counts and the failed step; retry_safe=false means \
        DO NOT replay."
    )]
    async fn phone_run_steps(
        &self,
        Parameters(RunStepsParams { steps, observe }): Parameters<RunStepsParams>,
    ) -> CallToolResult {
        self.progress_reset();
        let mut request = match phone_steps_request(steps) {
            Ok(request) => request,
            Err(error) => return CallToolResult::error(vec![Content::text(error)]),
        };
        // Observing by default: a batch is how an agent advances, and the
        // screen it ends on is what the agent decides from next.
        if observe.unwrap_or(true) {
            request["observe"] = serde_json::json!(true);
        }
        // The batch entry point answers with the same structured result the
        // CLI and phone_flow_run do. A failing batch carries the evidence a
        // caller needs — `failed_step`, `applied_actions`, `retry_safe`, and
        // each step's `observation` — and flattening that into error prose
        // (bounded to a couple of kilobytes, at that) threw away exactly the
        // part that says whether anyone could see the screen.
        let response = match self.daemon.actions_outcome(&request).await {
            Ok(response) => response,
            // The request left this process; what the phone did is unknown.
            Err(error) => {
                return unknown_action_result("transport_error", format!("{error:#}"));
            }
        };

        // A plain-text `ok` is not a batch result: it carries no per-step
        // outcome, so it can never stand in for one.
        if response.confirms_action() {
            // The daemon adds `registry` after a launch_app and a one-shot
            // `flow_suggestion` once a task is long enough to keep.
            let body = with_flow_compat(&self.daemon, response.body().to_string()).await;
            // The model reads the compact verdict and end screen; the whole
            // JSON still rides along as structured content for programs.
            // Without an observed screen the body stays as it was.
            let text = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|json| crate::compact::batch(&json))
                .unwrap_or(body);
            return with_structure(
                CallToolResult::success(vec![Content::text(text)]),
                &response,
            );
        }

        if response.explicit_refusal() {
            // The model reads the compact verdict (failed step, its evidence,
            // retry_safe, end screen); the daemon's whole result — status,
            // per-step observations, timing — rides along as structured
            // content. Anything that is not a recognisable batch keeps its body.
            let body = response.body().to_string();
            let text = response
                .json
                .as_ref()
                .and_then(crate::compact::failed_batch)
                .unwrap_or(body);
            return with_structure(CallToolResult::error(vec![Content::text(text)]), &response);
        }

        // Neither a confirmation nor a refusal: the request was answered, but
        // nothing in the answer says what happened to the phone.
        unknown_action_result(
            if response.too_large {
                "response_too_large"
            } else if response.json.is_none() {
                "unparseable_response"
            } else {
                "no_verdict_in_response"
            },
            format!("HTTP {}: {}", response.status.as_u16(), response.preview()),
        )
    }

    // -----------------------------------------------------------------------
    // phone_status
    // -----------------------------------------------------------------------

    #[tool(
        annotations(read_only_hint = true, open_world_hint = true),
        description = "Read the phone's status without taking control. `owner` names the \
        session currently driving the phone (this session presents IPHONE_USE_OWNER, \
        else mcp-<pid>); if it is someone else, do not drive the phone — control calls \
        will be refused with phone_owned. The JSON preserves \
        backend, target_configured, managed_wda, managed_wda_pending, recovery_owner, \
        device_state, screen_state, wda, wda_actionable, locked, drivable, released, \
        hint, setup_blocked_on, setup_phase, and setup_message. Gate actions on drivable=true. \
        `lock_readiness` says whether the phone locks on its own when idle (passcode_protected, \
        auto_lock_secs, keep_awake, verdict, hint): will_lock_needs_person means a person must \
        unlock it after a pause; tell the owner the hint instead of retrying. \
        setup_blocked_on=needs_passcode_on_phone means the runner start waits for a person to \
        enter the passcode on the phone: tell the owner next_step and do not retry. \
        `keep_runner_alive` (non-null) means idle release keeps the runner up because its next \
        start would need a person. \
        For status/health checks, report the state and stop. Idle release \
        is intentional: reconnect (phone_reconnect, once) only when a current \
        user-requested task needs the phone, recovery_owner=daemon, and no blocker or \
        reconnect is in progress; then poll until drivable=true."
    )]
    async fn phone_status(&self) -> CallToolResult {
        match self.daemon.status().await {
            Ok(s) => {
                // A model reading status is about to act more often than
                // not; a released phone starts coming back now (the daemon
                // gates this on recent use and an unlocked phone).
                if s.released == Some(true)
                    && std::env::var("IPHONE_USE_MCP_PREWARM").map_or(true, |v| v.trim() != "0")
                {
                    let daemon = self.daemon.clone();
                    tokio::spawn(async move {
                        let _ = daemon.prewarm("status").await;
                    });
                }
                let json =
                    serde_json::to_string(&s).unwrap_or_else(|_| r#"{"ok":true}"#.to_string());
                crate::screen::with_panel_session(CallToolResult::success(vec![Content::text(
                    json,
                )]))
            }
            Err(e) => CallToolResult::error(vec![Content::text(format!("status failed: {e:#}"))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_screen / phone_screen_frame (MCP Apps live panel, see screen.rs)
    // -----------------------------------------------------------------------

    #[tool(
        annotations(read_only_hint = true, open_world_hint = true),
        description = "Open the live iPhone screen panel beside the chat, or reuse the one \
        already open, in hosts that render MCP Apps: the person watches the phone while you \
        work and can press Home or refresh. Read-only: takes no owner lease and wakes \
        nothing. Returns a short status (phone, drivable, owner). Open it once when the \
        person wants to watch; it never replaces phone_elements or phone_screenshot for \
        your own observation. Hosts without MCP Apps show no panel."
    )]
    async fn phone_screen(&self) -> CallToolResult {
        let text = match self.daemon.status().await {
            Ok(status) => {
                let mut summary = crate::screen::status_summary(&status, self.daemon.owner());
                summary["panel"] = serde_json::json!(crate::screen::screen_uri());
                summary.to_string()
            }
            Err(e) => format!("panel opened; status failed: {e:#}"),
        };
        crate::screen::with_panel_session(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = true),
        description = "Internal: one frame for the iphone-use screen panel. Called by the \
        panel itself (MCP Apps visibility [\"app\"]); models use phone_screenshot instead."
    )]
    async fn phone_screen_frame(
        &self,
        Parameters(ScreenFrameParams { max_side, fresh }): Parameters<ScreenFrameParams>,
    ) -> CallToolResult {
        let side = max_side
            .unwrap_or(crate::screen::DEFAULT_FRAME_SIDE)
            .clamp(200, 1000);
        let (status, frame) = tokio::join!(
            self.daemon.status(),
            self.daemon.screen_frame(side, fresh.unwrap_or(false))
        );
        crate::screen::frame_result(
            status
                .map(|s| crate::screen::status_summary(&s, self.daemon.owner()))
                .map_err(|e| format!("status failed: {e:#}")),
            frame.map_err(|e| format!("{e:#}")),
        )
    }

    // -----------------------------------------------------------------------
    // phone_elements (L2)
    // -----------------------------------------------------------------------

    #[tool(
        annotations(read_only_hint = true, open_world_hint = true),
        description = "The current screen as text — use this instead of screenshots (an order \
        of magnitude cheaper). Rows carry kind, label, identifier, value, rect and state, plus a \
        `snapshot` token for phone_tap_element; indexes and snapshot tokens are valid for this read \
        only. `query` keeps only rows containing that text (label, identifier, value, placeholder) \
        and `limit` caps the rows — both keep each row's index and the snapshot, so taps still \
        work. On the first read in a newly entered app a `registry` block lists saved flows for \
        it: if one does the task, phone_flow_run it instead of tapping step by step. An `alert` \
        block means a system alert is up: answer it with an `alert` step, taps on alerts do not act. \
        ocr=auto adds text read from pixels when the tree has no interactive rows (games, \
        canvases), ocr=force always; such rows have no element, tap their x/y with phone_tap. \
        Requires phone_status drivable=true."
    )]
    async fn phone_elements(
        &self,
        Parameters(ElementsParams { query, limit, ocr }): Parameters<ElementsParams>,
    ) -> CallToolResult {
        let ocr = match crate::device::ocr_param(ocr.as_deref()) {
            Ok(ocr) => ocr,
            Err(error) => return CallToolResult::error(vec![Content::text(error)]),
        };
        // A narrowed read is not the whole screen: it must not become the
        // no-progress tracker's baseline.
        let narrowed = query.as_deref().is_some_and(|q| !q.trim().is_empty()) || limit.is_some();
        match self
            .daemon
            .elements_with_image(query.as_deref(), limit, ocr)
            .await
        {
            // The daemon adds `registry` on the first read in a newly
            // entered app; compat per flow is added here.
            Ok(json) => {
                let body = with_flow_compat(&self.daemon, json).await;
                // One line per row for the model; the JSON rides along as
                // structured content for programs.
                let mut parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
                // The daemon decided (image=auto): an image only when the tree
                // is unusable, fresh and within the response budget. Lift it
                // out once as image content; the text and the structured copy
                // keep only its labels, never the base64.
                let png = parsed
                    .as_mut()
                    .and_then(|json| json.get_mut("image"))
                    .and_then(|image| image.as_object_mut())
                    .and_then(|image| image.remove("png_base64"))
                    .and_then(|b64| b64.as_str().map(str::to_string));
                let mut text = parsed
                    .as_ref()
                    .and_then(crate::compact::elements)
                    .unwrap_or_else(|| body.clone());
                // OCR rows ride under the tree, never mixed into its indexes.
                if let Some(ocr) = parsed.as_ref().and_then(crate::device::ocr_block) {
                    text.push('\n');
                    text.push_str(&ocr);
                }
                if let Ok(mut tracker) = self.progress.lock() {
                    tracker.note_screen(parsed.as_ref().filter(|_| !narrowed));
                }
                let mut content = vec![Content::text(text)];
                if let Some(json) = parsed.as_ref() {
                    if let (Some(png), Some(image)) = (png, json.get("image")) {
                        content.push(Content::text(format!(
                            "image: the daemon's auto image policy attached a screenshot — \
                             source {}, requested at {} and received at {} (ms), after this \
                             snapshot was read, not the same instant{}.",
                            image["source"].as_str().unwrap_or("?"),
                            image["requested_at_ms"],
                            image["received_at_ms"],
                            if image["capture_redacted"] == serde_json::Value::Bool(true) {
                                "; the app hides this screen, so it is a wireframe"
                            } else {
                                ""
                            },
                        )));
                        content.push(Content::image(png, "image/png"));
                    } else if json.get("image_omitted").is_some() {
                        content.push(Content::text(
                            "image_omitted: the screenshot would not fit the response budget; \
                             the text above is complete.",
                        ));
                    } else if json.get("image_unavailable").is_some() {
                        content.push(Content::text(
                            "image_unavailable: the auto image policy requested a screenshot \
                             but the capture failed; the text above is still usable.",
                        ));
                    }
                }
                let mut result = CallToolResult::success(content);
                result.structured_content = parsed.filter(serde_json::Value::is_object);
                result
            }
            Err(e) => {
                self.progress_reset();
                CallToolResult::error(vec![Content::text(format!(
                    "elements failed: {e:#}. Call phone_status and follow its `hint`; do not retry \
                     in a loop."
                ))])
            }
        }
    }

    // -----------------------------------------------------------------------
    // phone_tap_element (L2)
    // -----------------------------------------------------------------------

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Tap one element by its zero-based index and snapshot token \
        from the SAME phone_elements response. The daemon re-reads the tree and \
        refuses the action if the UI changed, so a stale element reference cannot \
        silently tap a different control. Use identifier/kind/label/state fields to \
        choose the index. Snapshot refs are ephemeral: never persist them in a flow."
    )]
    async fn phone_tap_element(
        &self,
        Parameters(TapElementParams {
            element,
            snapshot,
            observe,
        }): Parameters<TapElementParams>,
    ) -> CallToolResult {
        let observe = observe.unwrap_or(true);
        // Refused here, before anything is sent: this is the one case where a
        // retry is provably safe, so it is reported as such rather than as an
        // unknown outcome.
        if snapshot.trim().is_empty() {
            self.progress_reset();
            return not_sent_result(
                "missing_snapshot",
                "no request was sent: tap_element needs the snapshot token from the \
                 phone_elements response the index came from."
                    .to_string(),
            );
        }
        // The snapshot is the caller's. This never substitutes one of its own,
        // so the daemon's staleness check runs against the tree the caller
        // actually read.
        let guard = self.progress_guard(&crate::hints::Action::TapElement {
            index: element as u64,
            snapshot: &snapshot,
        });
        let result = match self
            .daemon
            .tap_element_observed(element, &snapshot, observe)
            .await
        {
            Ok(response) => daemon_action_result(
                &response,
                observe,
                &format!("tapped element #{element} from the supplied snapshot"),
            ),
            Err(e) => unknown_action_result(
                "transport_error",
                format!("tap_element #{element} failed: {e:#}."),
            ),
        };
        self.note_progress(guard, result)
    }

    // -----------------------------------------------------------------------
    // phone_tap_label (L2)
    // -----------------------------------------------------------------------

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Tap an iPhone UI element by an EXACT visible label \
        (requires drivable=true in phone_status). Reads a fresh \
        phone_elements snapshot, requires exactly one match, then performs a \
        snapshot-bound tap. Zero or multiple matches return an error and send NO \
        action. For duplicate labels, choose by identifier/kind/state from \
        phone_elements and call phone_tap_element with that response's snapshot."
    )]
    async fn phone_tap_label(
        &self,
        Parameters(TapLabelParams { label, observe }): Parameters<TapLabelParams>,
    ) -> CallToolResult {
        let observe = observe.unwrap_or(true);
        // The snapshot comes from the element read this call performs — never
        // a cached or borrowed baseline.
        let guard = self.progress_guard(&crate::hints::Action::TapLabel(&label));
        let result = match self.daemon.tap_label_observed(&label, observe).await {
            Ok(response) => {
                daemon_action_result(&response, observe, &format!("tapped element: {label}"))
            }
            // `tap_label` resolves the label with reads and only then taps.
            // Which half failed is known structurally, so it is reported
            // exactly — not inferred from the text of an error, and not
            // flattened into "unknown" for both. Note the read is itself a
            // request to the phone: what the first arm can claim is that no
            // MUTATION was dispatched, not that nothing was sent.
            Err(crate::client::TapLabelFailure::BeforeDispatch(e)) => not_sent_result(
                "label_lookup_failed",
                format!(
                    "tap_label '{label}' could not resolve a target: {e:#} No tap was \
                     dispatched (only the element read), so retrying is safe."
                ),
            ),
            Err(crate::client::TapLabelFailure::AfterDispatch(e)) => unknown_action_result(
                "transport_error",
                format!(
                    "tap_label '{label}' failed after the tap was dispatched: {e:#} Whether \
                     the phone acted on it is unknown."
                ),
            ),
        };
        self.note_progress(guard, result)
    }

    // -----------------------------------------------------------------------
    // phone_reconnect
    // -----------------------------------------------------------------------

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Start/restart on-device automation for the configured phone. \
        This occupies the phone and may require the operator to unlock it. \
        Use only to continue a current user-requested phone operation or screen/UI \
        read that cannot proceed; never reconnect for initialization, status/health \
        checks, a completed task, or to keep the phone ready. Idle release is intentional. \
        Require phone_status released/offline and recovery_owner=daemon; inspect \
        hint/setup_blocked_on first. Do not reconnect while releasing/reconnecting \
        or a blocker remains. Once these conditions are met, call once, \
        then poll phone_status until reconnecting=false and drivable=true; while it is \
        reconnecting, report setup_phase/setup_message and obey setup_blocked_on. This tool \
        never accepts a UDID and cannot switch devices. \
        An externally managed runner returns an operator-owned recovery error."
    )]
    async fn phone_reconnect(&self) -> CallToolResult {
        self.progress_reset();
        match self.daemon.reconnect().await {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("reconnect failed: {e:#}"))])
            }
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false,
        ),
        description = "Mark the start of one task for the daemon's task metrics: every \
        later call of this session counts toward this run until phone_run_end. Optional \
        — without it, runs are inferred from idle gaps. Set complete_trace only if you \
        will hand every model turn id to phone_run_end."
    )]
    async fn phone_run_start(
        &self,
        Parameters(params): Parameters<RunStartParams>,
    ) -> CallToolResult {
        self.progress_reset();
        match self
            .daemon
            .run_start(&params.run_id, params.complete_trace.unwrap_or(false))
            .await
        {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(e) => CallToolResult::error(vec![Content::text(format!("run start failed: {e:#}"))]),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false,
        ),
        description = "Close the task opened with phone_run_start and get its summary: \
        tool calls (HTTP calls, not model turns), batches, observed actions, flow calls, \
        stale/unknown outcomes, failures and p50/p95 call time. Pass turn_ids only if \
        phone_run_start declared complete_trace."
    )]
    async fn phone_run_end(&self, Parameters(params): Parameters<RunEndParams>) -> CallToolResult {
        self.progress_reset();
        match self.daemon.run_end(&params.run_id, params.turn_ids).await {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(e) => CallToolResult::error(vec![Content::text(format!("run end failed: {e:#}"))]),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Read a scrolling list across pages in ONE call instead of scroll + read \
        turns: reads the rows of one kind (default Cell) on screen, swipes one page, reads \
        again, and stops when the list stops moving (duplicate_page), a page adds nothing \
        new (no_progress), end_label is on screen, or max_pages (default 6, max 10) is \
        reached. Rows are deduplicated on (kind, label, value): identical-looking rows \
        collapse, so reconcile counts the user cares about. `complete` is true ONLY when \
        end_label was seen; no_progress or duplicate_page is not proof the list ended. Only \
        what the accessibility tree exposes is collected. It swipes the live list, so the \
        screen is left where the last page was; `snapshot` is that last page. \
        Optional `snapshot` (from a read of the target app; never filled in for you): a known \
        foreground app mismatch refuses before any swipe (app_changed, not_sent, retry_safe). \
        Results name the app. \
        Requires phone_status drivable=true."
    )]
    async fn phone_collect_list(
        &self,
        Parameters(params): Parameters<CollectListParams>,
    ) -> CallToolResult {
        self.progress_reset();
        let body = serde_json::to_value(&params).unwrap_or_else(|_| serde_json::json!({}));
        match self.daemon.list_call("/agent/collect", &body).await {
            Ok(response) => list_result(&response, crate::compact::collected_list),
            Err(e) => unknown_action_result("transport_error", format!("{e:#}")),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Find one exact label, swiping at most max_swipes times (default 1, \
        max 5) when it is not on screen. Stops AT ONCE — no further swipe — when the label \
        is ambiguous (several on-screen matches: `candidates` with index and frame), covered \
        by another control, or not drawn. Found: `target` (index, kind, frame) plus a \
        `snapshot` for phone_tap_element. Not found: on-screen labels containing the text \
        as `candidates`. No screenshot is taken; absence after the budget is not proof — \
        look at the screen before swiping further. Optional `snapshot` (from a read of the \
        target app; never filled in for you): a known foreground app mismatch refuses before \
        any swipe (app_changed, not_sent, retry_safe). Results name the app. Requires \
        phone_status drivable=true."
    )]
    async fn phone_scroll_find(
        &self,
        Parameters(params): Parameters<ScrollFindParams>,
    ) -> CallToolResult {
        self.progress_reset();
        let body = serde_json::to_value(&params).unwrap_or_else(|_| serde_json::json!({}));
        match self.daemon.list_call("/agent/scroll_find", &body).await {
            Ok(response) => list_result(&response, crate::compact::found_label),
            Err(e) => unknown_action_result("transport_error", format!("{e:#}")),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false,
        ),
        description = "Keep the phone for a bounded human-in-the-loop pause inside the \
        current user-requested task (the operator types a PIN, approves a prompt, \
        fetches a code): the idle watchdog will not release it for `secs` seconds \
        (0 clears the hold; max 14400). When the person signs in (passcode, Face ID, \
        a code), pass private=true: the screen is not read or streamed until you clear \
        the hold; pass private=true again when extending it, since a hold without it \
        lifts the privacy. Not for initialization, health checks, or \
        keeping the phone ready; clear it when the step is done. Fails with \
        device_release_in_progress if the daemon is already releasing, and with \
        phone_owned if another session holds the phone lease."
    )]
    async fn phone_hold(&self, Parameters(params): Parameters<HoldParams>) -> CallToolResult {
        match self.daemon.hold(params.secs, params.private.unwrap_or(false)).await {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(e) => CallToolResult::error(vec![Content::text(format!("hold failed: {e:#}"))]),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false,
        ),
        description = "Hand the phone lease back when this session's phone task is \
        finished, so another session may drive it without waiting for the lease \
        to lapse. Only the current owner can release."
    )]
    async fn phone_release_owner(&self) -> CallToolResult {
        // A released lease is a run boundary.
        self.progress_reset();
        match self.daemon.release_owner().await {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(e) => CallToolResult::error(vec![Content::text(format!("release failed: {e:#}"))]),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Log the app on screen in with the user's OWN account from their password \
        vault (bitwarden-use on the Mac). The daemon reads the vault entry and types it into the \
        phone, so no password or username passes through this conversation. Use it when an app \
        shows its login page during a task the user asked for. The entry is matched to the app; \
        when several match, the answer lists their names: ask the user, then pass `item` (and \
        `user` when names repeat). Existing-account login only: never sign-up, password changes \
        or payment. When the answer has needs_code, the app sent a verification code: call again \
        with code_via (sms or mail) and code_from (the sender) once it has arrived; at most two \
        code requests per login. A push approval on another device needs the user. Read the \
        screen afterwards to confirm the login."
    )]
    async fn phone_login(&self, Parameters(params): Parameters<LoginParams>) -> CallToolResult {
        let (path, body) = match &params.code_via {
            Some(via) => (
                "/agent/login/code",
                serde_json::json!({ "via": via, "from": params.code_from, "wait_secs": 20 }),
            ),
            None => (
                "/agent/login",
                serde_json::json!({
                    "item": params.item,
                    "user": params.user,
                    "submit": params.submit.unwrap_or(true),
                }),
            ),
        };
        match self
            .daemon
            .request_json(reqwest::Method::POST, path, Some(&body))
            .await
        {
            Ok(response) => daemon_read_result(&response),
            Err(e) => CallToolResult::error(vec![Content::text(format!("login failed: {e:#}"))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_flow_list / phone_flow_info / phone_flow_run / phone_flow_update
    // -----------------------------------------------------------------------

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "List installed registry flows: reviewed, deterministic per-app \
        scripts (id like `health/export-all`) that replay a whole task with NO model \
        and NO screenshots. CHECK THIS FIRST before driving an app step by step: if a \
        flow matches the task, call phone_flow_run instead of exploring. Each row \
        reports id, description, risk (read_only|navigation|side_effect|unknown), \
        verified (has a recorded hardware run), compat against the app version installed on THIS phone \
        (verified|untested-newer|incompatible|broken|needs-verification|draft|unknown), and inputs, \
        one row per flow (detail=true for every field). Empty store: \
        call phone_flow_update once. Filter with `app` for the app you are about to open."
    )]
    async fn phone_flow_list(
        &self,
        Parameters(FlowListParams {
            category,
            app,
            verified,
            detail,
        }): Parameters<FlowListParams>,
    ) -> CallToolResult {
        let filter = crate::registry::ListFilter {
            category,
            app,
            verified_only: verified,
        };
        let installed = crate::compat::installed_apps(&self.daemon).await;
        match crate::registry::list(&filter) {
            Ok((entries, index)) => {
                let json = crate::registry::list_json(&entries, &index, installed.as_ref());
                let text = if detail {
                    json.to_string()
                } else {
                    crate::compact::flow_list(&json).unwrap_or_else(|| json.to_string())
                };
                let mut result = CallToolResult::success(vec![Content::text(text)]);
                result.structured_content = Some(json);
                result
            }
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("flow list failed: {e:#}"))])
            }
        }
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Show one installed registry flow: metadata, declared inputs, and \
        its step templates (tap_label/tap_locator/wait_for/... with input placeholders, \
        never values). Use it to check preconditions (which app, which locale, verified \
        or not) and the exact side effects before phone_flow_run."
    )]
    async fn phone_flow_info(
        &self,
        Parameters(FlowInfoParams { id }): Parameters<FlowInfoParams>,
    ) -> CallToolResult {
        if !crate::registry::valid_flow_id(&id) {
            return CallToolResult::error(vec![Content::text(format!(
                "{id:?} is not a registry id; expected <app>/<flow> lowercase slugs"
            ))]);
        }
        match crate::registry::info(&id) {
            Ok(detail) => CallToolResult::success(vec![Content::text(detail.to_string())]),
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("flow info failed: {e:#}"))])
            }
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Run one flow exactly once — an installed registry id, or a flow file \
        path (*.json, e.g. a phone_flow_draft you are proving before publishing). A flow \
        that declares `outputs` returns what it read as top-level `outputs` (e.g. today's \
        steps); verify=true also checks their shape against the recorded fixture. A \
        read_only/navigation flow that misses an element runs its other-language variant \
        once (`fallback_from` says so): \
        the daemon validates the whole sequence, holds one control lock, and stops at \
        the first failed step. The happy path costs one tool call and zero screenshots. \
        Requires phone_status drivable=true. Pass the flow's declared inputs; a flow \
        declared risk=side_effect is refused unless confirm=true; a flow whose compat is broken or \
        incompatible for the installed app version is refused unless force=true. The result reports \
        completed/applied counts and the failed step; retry_safe=false means DO NOT \
        replay — inspect phone_elements, repair the flow, do not guess. Unverified flows \
        (verified=false in phone_flow_list) may need a checkpoint screenshot afterwards."
    )]
    async fn phone_flow_run(
        &self,
        Parameters(FlowRunParams {
            id,
            inputs,
            confirm,
            force,
            verify,
            write_fixture,
        }): Parameters<FlowRunParams>,
    ) -> CallToolResult {
        self.progress_reset();
        let is_file = id.ends_with(".json");
        if !is_file && !crate::registry::valid_flow_id(&id) {
            return CallToolResult::error(vec![Content::text(format!(
                "{id:?} is neither a registry id (<app>/<flow> lowercase slugs) nor a .json flow file"
            ))]);
        }
        let path = match crate::registry::resolve_target(&id) {
            Ok(path) => path,
            Err(e) => return CallToolResult::error(vec![Content::text(format!("{e:#}"))]),
        };
        let flow = match crate::flow::load_flow(&path) {
            Ok(flow) => flow,
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "flow {id} failed validation: {e:#}"
                ))])
            }
        };
        if let Err(e) = crate::flow::check_input_map(&inputs, &flow.inputs) {
            return CallToolResult::error(vec![Content::text(format!("{e:#}"))]);
        }
        let (compat, _installed) = match crate::flow::compat_gate(&flow, &self.daemon, force).await
        {
            Ok(gate) => gate,
            Err(e) => return CallToolResult::error(vec![Content::text(format!("{e:#}"))]),
        };
        // One shared execution path with the CLI: the daemon's failure body is
        // kept whole and diagnosed, never reverse-parsed out of an error
        // string. An agent driving through MCP sees exactly what a human at
        // the CLI sees.
        let mut run = match crate::flow::execute_and_diagnose(&flow, &inputs, &self.daemon, confirm)
            .await
        {
            Ok(run) => run,
            // Nothing came back at all — a transport failure, or the flow was
            // refused before dispatch. There is no result to diagnose.
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "flow {id} did not run: {e:#}. Nothing was recorded; check phone_status \
                     and do NOT replay blindly."
                ))])
            }
        };
        // A read/navigation flow that missed an element runs its locale
        // variant once (health/export-all ↔ health/export-all-zh-cn).
        let mut ran_id = id.clone();
        let mut ran_flow = None;
        if !run.succeeded {
            if let Some((sibling_id, sibling, sibling_run)) =
                crate::flow::locale_fallback(&id, &flow, &inputs, &run.value, &self.daemon).await
            {
                let failed = std::mem::replace(&mut run, sibling_run).value;
                crate::flow::note_fallback(&mut run.value, &id, &failed);
                ran_id = sibling_id;
                ran_flow = Some(sibling);
            }
        }
        let outputs = run.value.get("outputs").cloned();
        let mut summary = serde_json::json!({
            "flow": ran_id,
            "verified": flow.meta.verified(),
            "risk": flow.meta.risk_label(),
            "compat": compat,
            "result": run.value,
        });
        if let Some(outputs) = outputs {
            summary["outputs"] = outputs;
        }
        if verify || write_fixture {
            let ran = ran_flow.as_ref().unwrap_or(&flow);
            match crate::flow::verify_block(
                &crate::flow::fixture_key(&ran_id),
                ran,
                &summary["result"],
                run.succeeded,
                write_fixture,
            ) {
                Ok(block) => summary["verify"] = block,
                Err(e) => summary["verify"] = serde_json::json!({"ok": false, "error": format!("{e:#}")}),
            }
        }
        if run.succeeded {
            if compat.compat == crate::compat::Compat::UntestedNewer {
                summary["hint"] = serde_json::json!(
                    "the installed app is newer than this flow's last verification — if the \
                     phone ended where the flow promised, publish an updated verified_on \
                     (phone_flow_publish) so others get compat=verified"
                );
            } else if !flow.meta.verified() {
                summary["hint"] = serde_json::json!(
                    "this flow had no hardware verification yet — if the phone is now where \
                     the flow promised, tell the user and offer to add verified_on via \
                     phone_flow_publish"
                );
            }
            return flow_run_result(summary, false);
        }

        // The pre-flight status is already in hand. Asking again costs a
        // round trip on a path that has just failed, and would describe the
        // phone AFTER the failure rather than the run we are reporting.
        let status = Some(run.preflight_status.clone());
        self.remember_flow_failure(crate::contrib::ReportContext {
            id: id.clone(),
            result: Some(summary["result"].clone()),
            status,
            application: None,
            note: None,
        });
        summary["hint"] = serde_json::json!(format!(
            "flow {id} did not complete ({}). `result.diagnosis` says whether the screen could \
             be read and which candidates resemble the locator that failed; candidates are for \
             review only — nothing was retried and the flow was not edited. If the flow itself is \
             wrong (label changed, app updated), call phone_flow_report(id=\"{id}\", confirm=true) \
             with the user's go-ahead. Do NOT replay the flow blindly.",
            match run.status {
                Some(code) => format!("HTTP {code}"),
                None => "no answer from the daemon; the outcome is UNKNOWN, not \"never ran\""
                    .to_string(),
            }
        ));
        flow_run_result(summary, true)
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Hand ONE whole goal to a fast on-phone agent instead of driving it step by \
        step: TypeSafe's Jev picks each next operation and target from the screen's element table \
        (~1–2 s per step instead of a model turn), a small LLM types field values. Use it for a \
        clear multi-step goal with no saved flow (check phone_flow_list first). It stops with \
        status done / blocked / max_steps and the step history; it answers BLOCKED rather than \
        send, pay, delete or share anything the goal did not explicitly ask for — still confirm \
        such goals with the user first. Verify the end screen yourself. Needs TYPESAFE_API_KEY \
        (or ~/.config/typesafe/key) and, for typing, TEXT_MODEL_API_KEY (or ~/.config/openrouter/key)."
    )]
    async fn phone_jev_run(
        &self,
        Parameters(JevRunParams { goal, app, max_steps }): Parameters<JevRunParams>,
    ) -> CallToolResult {
        self.progress_reset();
        let options = crate::jev::Options {
            goal,
            app,
            max_steps: max_steps.unwrap_or(crate::jev::DEFAULT_MAX_STEPS).clamp(1, 80),
        };
        match crate::jev::run(&self.daemon, options).await {
            Ok(report) if report["ok"] == true => {
                CallToolResult::success(vec![Content::text(report.to_string())])
            }
            Ok(report) => CallToolResult::error(vec![Content::text(report.to_string())]),
            Err(e) => CallToolResult::error(vec![Content::text(format!("jev did not run: {e:#}"))]),
        }
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Turn what you just did on the phone into a flow. The daemon records \
        every action it applied in the current app (single actions and phone_run_steps \
        batches, not flow runs); this returns that trail as a flow v1 document (snapshot \
        taps become label/identifier locators, typed text becomes named inputs and is never \
        included) plus a `todo` list of what to fix. Call it when a response carried \
        `flow_suggestion`, or after any multi-step task with no matching flow — but ASK THE \
        USER before saving. With save_as, the draft is written to that new file and \
        validated; fix the todo items, prove it with phone_flow_run(id=<file>), then publish \
        only with the user's OK."
    )]
    async fn phone_flow_draft(
        &self,
        Parameters(FlowDraftParams { save_as }): Parameters<FlowDraftParams>,
    ) -> CallToolResult {
        let response = match self.daemon.flow_draft().await {
            Ok(response) => response,
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "flow draft failed: {e:#}"
                ))])
            }
        };
        let Some(mut draft) = response.json.clone().filter(|_| response.status.is_success()) else {
            return CallToolResult::error(vec![Content::text(response.body().to_string())]);
        };
        if let Some(path) = save_as {
            // Read-only means nothing is written, a local file included.
            if *self.profile == ToolProfile::ReadOnly {
                return CallToolResult::error(vec![Content::text(format!(
                    "save_as is not available with {TOOLS_ENV}=readonly; call phone_flow_draft without save_as to get the draft"
                ))]);
            }
            match crate::flow::save_draft(&path, &draft["flow"]) {
                Ok(validation) => {
                    draft["saved_as"] = serde_json::json!(path);
                    draft["validation"] = validation;
                }
                Err(e) => {
                    return CallToolResult::error(vec![Content::text(format!(
                        "could not save the draft to {path}: {e:#}"
                    ))])
                }
            }
        }
        CallToolResult::success(vec![Content::text(draft.to_string())])
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Contribute a working flow to the official registry: fork if needed, add the \
        file (+ app.json for a new app), rebuild index.json, push a branch, and open a pull \
        request via the user's GitHub CLI login. OUTWARD-FACING: requires confirm=true, which \
        you may set only after the user agreed to publish. Use after a flow you compiled has \
        run successfully on the phone; put device/iOS/date in the file's verified_on first \
        (an unverified file opens as a draft PR). `source` is a flow file path or an id you \
        installed with `flow add`; `aliases` are the foreground-app labels (per language) that \
        should surface this app's flows from phone_elements."
    )]
    async fn phone_flow_publish(
        &self,
        Parameters(FlowPublishParams {
            source,
            id,
            app_name,
            aliases,
            note,
            confirm,
        }): Parameters<FlowPublishParams>,
    ) -> CallToolResult {
        if !confirm {
            return CallToolResult::error(vec![Content::text(
                "phone_flow_publish opens a public pull request; ask the user, then call again with confirm=true",
            )]);
        }
        let path = match crate::contrib::publish_source(&source) {
            Ok(path) => path,
            Err(e) => return CallToolResult::error(vec![Content::text(format!("{e:#}"))]),
        };
        let options = crate::contrib::PublishOptions {
            id,
            app_name,
            aliases,
            note,
            draft: false,
        };
        match tokio::task::spawn_blocking(move || crate::contrib::publish(&path, &options)).await {
            Ok(Ok(report)) => CallToolResult::success(vec![Content::text(
                serde_json::to_string(&report).unwrap_or_default(),
            )]),
            Ok(Err(e)) => {
                CallToolResult::error(vec![Content::text(format!("publish failed: {e:#}"))])
            }
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("publish task failed: {e}"))])
            }
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "File an issue on the official flow registry for an installed flow that \
        failed (label changed, app updated, wrong locale). Uses the failure captured by the last \
        phone_flow_run of that id — failed step, redacted daemon result, daemon version — so you \
        only add a short note. Screen labels, typed text, and element lists are stripped. \
        OUTWARD-FACING: requires confirm=true after the user agreed. Do not file for a phone \
        that was simply locked/offline, or for a flow you ran on the wrong app/locale."
    )]
    async fn phone_flow_report(
        &self,
        Parameters(FlowReportParams { id, note, confirm }): Parameters<FlowReportParams>,
    ) -> CallToolResult {
        if !confirm {
            return CallToolResult::error(vec![Content::text(
                "phone_flow_report opens a public issue; ask the user, then call again with confirm=true",
            )]);
        }
        if !crate::registry::valid_flow_id(&id) {
            return CallToolResult::error(vec![Content::text(format!(
                "{id:?} is not a registry id"
            ))]);
        }
        let mut context =
            self.take_flow_failure(&id)
                .unwrap_or_else(|| crate::contrib::ReportContext {
                    id: id.clone(),
                    ..Default::default()
                });
        if context.result.is_none() && note.as_deref().is_none_or(|n| n.trim().is_empty()) {
            return CallToolResult::error(vec![Content::text(
                "no captured failure for this flow in this session — run it first, or pass a note describing what went wrong",
            )]);
        }
        if note.is_some() {
            context.note = note;
        }
        if context.application.is_none() {
            if let Ok(body) = self.daemon.elements().await {
                context.application = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v["elements"].as_array().and_then(|rows| {
                            rows.iter()
                                .find(|r| r["kind"] == "Application")
                                .and_then(|r| r["label"].as_str().map(String::from))
                        })
                    });
            }
        }
        match tokio::task::spawn_blocking(move || crate::contrib::report(&context)).await {
            Ok(Ok(outcome)) => CallToolResult::success(vec![Content::text(
                serde_json::to_string(&outcome).unwrap_or_default(),
            )]),
            Ok(Err(e)) => {
                CallToolResult::error(vec![Content::text(format!("report failed: {e:#}"))])
            }
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("report task failed: {e}"))])
            }
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true,
        ),
        description = "Mirror the official iphone-use flow registry \
        (github.com/leeguooooo/iphone-use-flows) into the local store. Every file is \
        checksum-verified and strictly validated before it is written; locally added \
        flows are kept. Call once when phone_flow_list reports an empty store, or to \
        pick up new flows. Network only; the phone is not touched."
    )]
    async fn phone_flow_update(&self) -> CallToolResult {
        match crate::registry::update().await {
            Ok(report) => CallToolResult::success(vec![Content::text(
                serde_json::to_string(&report).unwrap_or_default(),
            )]),
            Err(e) => {
                CallToolResult::error(vec![Content::text(format!("flow update failed: {e:#}"))])
            }
        }
    }

    // -----------------------------------------------------------------------
    // Device tools: logs, crashes, location, recording, install, app notes
    // -----------------------------------------------------------------------

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "The phone's system log, for debugging an app: action=start begins \
        capturing, reproduce the problem, then action=read (the default) with process, \
        contains, level (error = errors and faults) and limit; pass the answer's `next` as \
        `since` to read only newer lines. stop ends the capture, clear empties the buffer. \
        One line per log line: #seq time process[pid] level: message."
    )]
    async fn phone_logs(&self, Parameters(params): Parameters<LogsParams>) -> CallToolResult {
        let filter = crate::device::LogsFilter {
            process: params.process,
            contains: params.contains,
            level: params.level,
            since: params.since,
            limit: params.limit,
        };
        let response = match crate::device::logs_call(params.action.as_deref(), filter) {
            Err(error) => return CallToolResult::error(vec![Content::text(error)]),
            Ok(crate::device::LogsCall::Read(path)) => {
                match self.daemon.request_json(reqwest::Method::GET, &path, None).await {
                    Ok(response) => return list_result(&response, crate::device::logs),
                    Err(e) => Err(e),
                }
            }
            Ok(crate::device::LogsCall::Control(body)) => {
                self.daemon
                    .request_json(reqwest::Method::POST, "/agent/logs", Some(&body))
                    .await
            }
        };
        match response {
            Ok(response) => rendered_result(&response, crate::device::logs_control),
            Err(e) => CallToolResult::error(vec![Content::text(format!("logs failed: {e:#}"))]),
        }
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = true),
        description = "Crash reports on the phone: without `name`, list them (name, process, \
        time, size; filter with process); with a `name` from the list, read that report — \
        exception, crashed thread and backtrace. Read-only: it never touches the screen."
    )]
    async fn phone_crashes(
        &self,
        Parameters(CrashesParams { name, process, limit }): Parameters<CrashesParams>,
    ) -> CallToolResult {
        let path = match crate::device::crashes_path(name.as_deref(), process.as_deref(), limit) {
            Ok(path) => path,
            Err(error) => return CallToolResult::error(vec![Content::text(error)]),
        };
        let render = if name.is_some() {
            crate::device::crash_report
        } else {
            crate::device::crashes
        };
        match self.daemon.request_json(reqwest::Method::GET, &path, None).await {
            Ok(response) => list_result(&response, render),
            Err(e) => CallToolResult::error(vec![Content::text(format!("crashes failed: {e:#}"))]),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true,
        ),
        description = "Simulate the phone's GPS location (lat, lon in decimal degrees) for \
        testing location-aware apps; every app sees it until you call again with clear=true, \
        which restores the real location. Counts as one action against the daemon's budget."
    )]
    async fn phone_set_location(
        &self,
        Parameters(SetLocationParams { lat, lon, clear }): Parameters<SetLocationParams>,
    ) -> CallToolResult {
        let body = match crate::device::location_body(lat, lon, clear.unwrap_or(false)) {
            Ok(body) => body,
            Err(error) => return CallToolResult::error(vec![Content::text(error)]),
        };
        match self
            .daemon
            .request_json(reqwest::Method::POST, "/agent/location", Some(&body))
            .await
        {
            Ok(response) => {
                rendered_result(&response, |json| crate::device::location(json, &body))
            }
            Err(e) => CallToolResult::error(vec![Content::text(format!(
                "set location failed: {e:#}; whether the phone took it is unknown"
            ))]),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Record the phone screen to an MP4 on the daemon's Mac: action=start, \
        do the task, action=stop returns the saved file's path, length and size. \
        action=status says whether a recording runs. A recording stops by itself after 10 \
        minutes or 200 MB (`ended` says so)."
    )]
    async fn phone_record(
        &self,
        Parameters(RecordParams { action }): Parameters<RecordParams>,
    ) -> CallToolResult {
        let body = match crate::device::record_body(&action) {
            Ok(body) => body,
            Err(error) => return CallToolResult::error(vec![Content::text(error)]),
        };
        let response = match &body {
            Some(body) => {
                self.daemon
                    .request_json(reqwest::Method::POST, "/agent/record", Some(body))
                    .await
            }
            None => {
                self.daemon
                    .request_json(reqwest::Method::GET, "/agent/record", None)
                    .await
            }
        };
        match response {
            Ok(response) => rendered_result(&response, crate::device::recording),
            Err(e) => CallToolResult::error(vec![Content::text(format!("record failed: {e:#}"))]),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true,
        ),
        description = "Install an app build on the phone: `path` of an .ipa or .app on the \
        Mac running the daemon, or an https `url` of an .ipa (up to 2 GB). fresh=true \
        uninstalls the app first, deleting its data. Apps the operator blocked are refused. \
        Answers with the installed bundle id; it can take minutes. Install only what the user \
        asked for."
    )]
    async fn phone_install_app(
        &self,
        Parameters(InstallAppParams { path, url, fresh }): Parameters<InstallAppParams>,
    ) -> CallToolResult {
        let body = match crate::device::install_body(
            path.as_deref(),
            url.as_deref(),
            fresh.unwrap_or(false),
        ) {
            Ok(body) => body,
            Err(error) => return CallToolResult::error(vec![Content::text(error)]),
        };
        match self.daemon.install(&body).await {
            Ok(response) => rendered_result(&response, crate::device::installed),
            Err(e) => CallToolResult::error(vec![Content::text(format!(
                "install failed: {e:#}. Whether the app got installed is unknown: check with \
                 phone_apps (source=installed) before trying again."
            ))]),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false,
        ),
        description = "Notes about one app that persist across sessions — where things are, \
        quirks, what worked: without `text`, read them; with `text`, replace them (Markdown, \
        at most 16 KB; read first and keep what still holds). The first read in an app also \
        shows them in its `registry` block. Never store passwords or personal data."
    )]
    async fn phone_app_notes(
        &self,
        Parameters(AppNotesParams { bundle, text }): Parameters<AppNotesParams>,
    ) -> CallToolResult {
        let path = match crate::device::app_notes_path(&bundle) {
            Ok(path) => path,
            Err(error) => return CallToolResult::error(vec![Content::text(error)]),
        };
        let Some(text) = text else {
            return match self.daemon.request_json(reqwest::Method::GET, &path, None).await {
                Ok(response) => list_result(&response, crate::device::app_notes),
                Err(e) => {
                    CallToolResult::error(vec![Content::text(format!("app notes failed: {e:#}"))])
                }
            };
        };
        if text.len() > crate::device::NOTES_MAX_BYTES {
            return CallToolResult::error(vec![Content::text(format!(
                "notes are {} bytes; the limit is {} — shorten them. Nothing was saved.",
                text.len(),
                crate::device::NOTES_MAX_BYTES
            ))]);
        }
        let bytes = text.len();
        let bundle = bundle.trim().to_string();
        match self.daemon.put_markdown(&path, &text).await {
            Ok(response) => rendered_result(&response, |json| {
                (json["ok"] == true).then(|| format!("saved {bytes} bytes of notes for {bundle}"))
            }),
            Err(e) => CallToolResult::error(vec![Content::text(format!(
                "saving app notes failed: {e:#}"
            ))]),
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler impl
//
// `#[tool_handler]` supplies defaults for methods we do not override.
// Our call_tool preserves routing and shapes the final wire result.
// ---------------------------------------------------------------------------

/// Drop the parts of a schemars schema a model never needs: the meta-schema
/// URI, generated titles (the property name already says it), numeric
/// `format` tags (`uint`, `double`, …) and `minimum: 0` on unsigned counts.
/// Every other key — types, descriptions, enums, required lists, ranges with
/// meaning — is kept. Keys under `properties` are property NAMES, never
/// schema keywords, so they are never removed (a parameter may be called
/// `title`).
fn slim_schema(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("$schema");
            map.remove("title");
            if map.get("format").is_some_and(|f| {
                f.as_str().is_some_and(|f| {
                    matches!(
                        f,
                        "uint" | "uint8" | "uint16" | "uint32" | "uint64" | "int" | "int8"
                            | "int16" | "int32" | "int64" | "double" | "float"
                    )
                })
            }) {
                map.remove("format");
            }
            if map.get("minimum") == Some(&serde_json::json!(0)) {
                map.remove("minimum");
            }
            for (key, child) in map.iter_mut() {
                if key == "properties" || key == "$defs" || key == "definitions" {
                    if let serde_json::Value::Object(props) = child {
                        for schema in props.values_mut() {
                            slim_schema(schema);
                        }
                    }
                } else {
                    slim_schema(child);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(slim_schema),
        _ => {}
    }
}

/// `tools/list` with every input schema slimmed (see [`slim_schema`]).
fn slim_tools(tools: Vec<rmcp::model::Tool>) -> Vec<rmcp::model::Tool> {
    tools
        .into_iter()
        .map(|mut tool| {
            let mut schema = serde_json::Value::Object((*tool.input_schema).clone());
            slim_schema(&mut schema);
            if let serde_json::Value::Object(map) = schema {
                tool.input_schema = std::sync::Arc::new(map);
            }
            tool
        })
        .collect()
}

/// [`shape_structured`] for every tool but the panel's frame tool: a frame
/// is read by the panel, never the model, and its base64 image must reach
/// `structuredContent` whole.
fn finish_result(tool: &str, result: CallToolResult, mode: StructuredMode) -> CallToolResult {
    if tool == crate::screen::FRAME_TOOL {
        result
    } else {
        shape_structured(result, mode)
    }
}

/// What `tools/list` answers under `profile`: the tools it exposes, with
/// slimmed schemas plus the MCP Apps `_meta`.
fn listed_tools(profile: &ToolProfile) -> Vec<rmcp::model::Tool> {
    let tools = PhoneHandler::tool_router()
        .list_all()
        .into_iter()
        .filter(|tool| profile.allows(tool))
        .collect();
    crate::screen::decorate_tools(slim_tools(tools))
}

/// How many tools the router exposes (for transport tests elsewhere).
#[cfg(test)]
pub(crate) fn tool_count() -> usize {
    PhoneHandler::tool_router().list_all().len()
}

/// Which tools this server lists and accepts (`IPHONE_USE_MCP_TOOLS`).
///
/// `all` (the default) is the whole surface; `readonly` keeps the tools
/// annotated `read_only_hint = true` (they never act on the phone); a comma
/// list is an explicit allowlist of tool names. A hidden tool is neither
/// listed nor callable: a call is refused with a message naming the setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolProfile {
    All,
    ReadOnly,
    Allow(std::collections::BTreeSet<String>),
}

/// The environment variable that selects a [`ToolProfile`].
const TOOLS_ENV: &str = "IPHONE_USE_MCP_TOOLS";

impl ToolProfile {
    /// Read `IPHONE_USE_MCP_TOOLS`; problems with it go to stderr (stdout is
    /// the MCP pipe).
    pub(crate) fn from_env() -> Self {
        let raw = std::env::var(TOOLS_ENV).ok();
        let (profile, warnings) = Self::parse(raw.as_deref());
        for warning in warnings {
            eprintln!("iphone-use-mcp: {warning}");
        }
        profile
    }

    /// Parse a setting against the real tool surface. Unknown names in an
    /// allowlist are dropped with a warning; the panel's frame tool follows
    /// `phone_screen`, since the panel cannot draw without it.
    fn parse(raw: Option<&str>) -> (Self, Vec<String>) {
        let raw = raw.map(str::trim).unwrap_or_default();
        match raw.to_ascii_lowercase().as_str() {
            "" | "all" => return (Self::All, Vec::new()),
            "readonly" | "read-only" | "read_only" => return (Self::ReadOnly, Vec::new()),
            _ => {}
        }
        let known: std::collections::BTreeSet<String> = PhoneHandler::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        let mut warnings = Vec::new();
        let mut allowed = std::collections::BTreeSet::new();
        for name in raw.split(',').map(str::trim).filter(|name| !name.is_empty()) {
            if known.contains(name) {
                allowed.insert(name.to_string());
            } else {
                warnings.push(format!("{TOOLS_ENV}: unknown tool {name:?} ignored"));
            }
        }
        if allowed.contains(crate::screen::SCREEN_TOOL) {
            allowed.insert(crate::screen::FRAME_TOOL.to_string());
        }
        if allowed.is_empty() {
            warnings.push(format!(
                "{TOOLS_ENV}={raw:?} names no known tool; no tools are exposed \
                 (use all, readonly, or a comma list of tool names)"
            ));
        }
        (Self::Allow(allowed), warnings)
    }

    fn allows(&self, tool: &rmcp::model::Tool) -> bool {
        match self {
            Self::All => true,
            Self::ReadOnly => tool
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint)
                == Some(true),
            Self::Allow(names) => names.contains(tool.name.as_ref()),
        }
    }

    /// The refusal for a call to `name`, or `None` when the profile exposes
    /// it. Unknown names fall through to the router's own error.
    fn refusal(&self, name: &str) -> Option<CallToolResult> {
        let router = PhoneHandler::tool_router();
        let tool = router.get(name)?;
        if self.allows(tool) {
            return None;
        }
        let why = match self {
            Self::All => return None,
            Self::ReadOnly => format!(
                "{TOOLS_ENV}=readonly exposes only read-only tools (observation, lookup, flow \
                 list/info/draft)"
            ),
            Self::Allow(names) => format!(
                "{TOOLS_ENV} allows only: {}",
                names.iter().map(String::as_str).collect::<Vec<_>>().join(", ")
            ),
        };
        Some(CallToolResult::error(vec![Content::text(format!(
            "{name} is not available in this session: {why}. Nothing was sent to the phone. \
             The operator can change {TOOLS_ENV} in the MCP server's environment (all, readonly, \
             or a comma list of tool names)."
        ))]))
    }
}

/// How much of a result's JSON goes out as `structuredContent`
/// (`IPHONE_USE_MCP_STRUCTURED`).
///
/// Clients do not show the model what the spec suggests: Claude Code hands
/// the model ONLY `structuredContent` when a successful result has one (the
/// text block is dropped). Codex also sends ONLY non-null structured content
/// as text, dropping every content block, including images (openai/codex,
/// `codex-rs/protocol/src/models.rs`,
/// `CallToolResult::as_function_call_output_payload`, 806d973). So the full
/// JSON — an element tree, every step of a batch —
/// is what those models read, and the compact text is wasted. The default
/// `slim` keeps the verdict a program branches on (the top-level scalars:
/// `ok`, `outcome`, `retry_safe`, `error`, `snapshot`, counts) plus the
/// compact text as `summary`; `full` sends the whole JSON; `off` sends none.
/// After shaping, Codex results carrying images omit structured content and
/// carry any missing verdict fields in text so the model receives the images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructuredMode {
    Slim,
    Full,
    Off,
}

impl StructuredMode {
    fn from_env() -> Self {
        match std::env::var("IPHONE_USE_MCP_STRUCTURED").as_deref() {
            Ok("full") => Self::Full,
            Ok("off") | Ok("0") => Self::Off,
            _ => Self::Slim,
        }
    }
}

/// The longest top-level string a slim payload keeps; longer ones are bulk
/// (a base64 image, a dumped tree) and the summary already covers them.
const SLIM_STRING_LIMIT: usize = 200;

/// Apply [`StructuredMode`] to a finished tool result. Runs after the tool
/// (and its progress tracking) has used the full JSON.
fn shape_structured(mut result: CallToolResult, mode: StructuredMode) -> CallToolResult {
    match mode {
        StructuredMode::Full => result,
        StructuredMode::Off => {
            result.structured_content = None;
            result
        }
        StructuredMode::Slim => {
            let Some(serde_json::Value::Object(full)) = result.structured_content.as_ref() else {
                return result;
            };
            let text = result
                .content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            // A text block that is the body itself (a read's preview) has no
            // compact form to carry; leave such a result whole.
            if text.trim_start().starts_with('{') {
                return result;
            }
            let mut slim = serde_json::Map::new();
            for (key, value) in full {
                let keep = match value {
                    serde_json::Value::String(s) => s.len() <= SLIM_STRING_LIMIT,
                    serde_json::Value::Array(_) | serde_json::Value::Object(_) => false,
                    _ => true,
                };
                if keep {
                    slim.insert(key.clone(), value.clone());
                }
            }
            slim.insert("summary".into(), serde_json::Value::String(text));
            result.structured_content = Some(serde_json::Value::Object(slim));
            result
        }
    }
}

fn is_codex_client(name: &str) -> bool {
    name.get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("codex"))
}

/// Adapt only image-bearing Codex results, after upstream result shaping.
fn keep_codex_images(mut result: CallToolResult, codex_client: bool) -> CallToolResult {
    if !codex_client
        || !result
            .content
            .iter()
            .any(|block| block.as_image().is_some())
    {
        return result;
    }
    let Some(structured) = result.structured_content.take() else {
        return result;
    };
    let verdict = ["ok", "error", "outcome", "retry_safe", "hint"]
        .into_iter()
        .filter_map(|key| {
            let value = structured.get(key)?;
            Some(format!("{key}:{value}"))
        })
        .collect::<Vec<_>>();
    if !verdict.is_empty() {
        result.content.push(Content::text(verdict.join(" ")));
    }
    result
}

#[tool_handler]
impl ServerHandler for PhoneHandler {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let name = request.name.clone();
        if let Some(refusal) = self.profile.refusal(&name) {
            return Ok(refusal);
        }
        let codex_client = context
            .peer
            .peer_info()
            .is_some_and(|info| is_codex_client(&info.client_info.name));
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let result = Self::tool_router().call(tcc).await?;
        Ok(keep_codex_images(
            finish_result(&name, result, StructuredMode::from_env()),
            codex_client,
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        Ok(rmcp::model::ListToolsResult {
            tools: listed_tools(&self.profile),
            meta: None,
            next_cursor: None,
        })
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, rmcp::ErrorData> {
        Ok(rmcp::model::ListResourcesResult::with_all_items(
            crate::screen::resources(),
        ))
    }

    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResult, rmcp::ErrorData> {
        match crate::screen::read(&request.uri) {
            Some(contents) => Ok(rmcp::model::ReadResourceResult::new(vec![contents])),
            None => Err(rmcp::ErrorData::resource_not_found(
                format!("unknown resource {}", request.uri),
                None,
            )),
        }
    }

    fn get_info(&self) -> rmcp::model::ServerInfo {
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::new(
                "iphone-use-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Drive a real iPhone. The loop:\n\
                 1. phone_status — act only when drivable=true. If not, report its `hint` and stop; \
                 call phone_reconnect once only when the user's current task needs the phone \
                 (each reconnect may make the user type the passcode), never for health checks.\n\
                 2. Look for a saved flow FIRST. Responses that enter an app (launch_app, the first \
                 phone_elements in a new app) carry a `registry` block: if a listed flow does the task, \
                 phone_flow_run it — one call instead of dozens. phone_flow_list shows all of them. No flow but a \
                 clear goal: phone_jev_run hands the whole goal to a fast on-phone agent (~1–2 s/step).\n\
                 3. Otherwise read phone_elements and act on what it names (phone_tap_element, \
                 phone_tap_label for a unique label) only to explore; as soon as you know the next few steps, send \
                 them as ONE phone_run_steps batch (wait_for between screens) — it returns the resulting \
                 screen, so a task takes a few turns, not one per tap. \
                 Screenshots only when pixels matter.\n\
                 4. Verify each step; `retry_safe:false` means never replay.\n\
                 5. A response carrying `flow_suggestion`, or any finished multi-step task with no flow: \
                 ASK THE USER whether to save it. Only if they agree: phone_flow_draft, fix its todo list, \
                 phone_flow_run the file once, then phone_flow_publish(confirm=true) with their OK.\n\
                 Call phone_release_owner when the task is done.",
            )
    }
}

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

fn phone_locator_has_condition(locator: &PhoneElementLocator) -> bool {
    locator.label.is_some()
        || locator.identifier.is_some()
        || locator.kind.is_some()
        || locator.value.is_some()
        || locator.focused.is_some()
        || locator.enabled.is_some()
        || locator.visible.is_some()
}

/// Longest text one step (or a whole batch) may type — the daemon's
/// `text_input::MAX_TEXT_CHARS`, in Unicode scalar values.
const MAX_TEXT_CHARS: usize = 20_000;

/// The longest `open_url` the daemon accepts (contract with /agent/actions).
const MAX_URL_CHARS: usize = 2048;

/// Whether `url` starts with an RFC 3986 scheme (`ALPHA *( ALPHA / DIGIT /
/// "+" / "-" / "." ) ":"`). Which schemes may open is the daemon's call.
fn url_has_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

pub(crate) fn phone_steps_request(steps: Vec<PhoneStep>) -> Result<serde_json::Value, String> {
    const MAX_STEPS: usize = 24;
    const MAX_AFTER_MS: u64 = 3_000;
    const MAX_WAIT_MS: u64 = 30_000;
    const MAX_DECLARED_WAIT_MS: u64 = 60_000;

    if steps.is_empty() {
        return Err("steps must contain at least one step; no action was sent".to_string());
    }
    if steps.len() > MAX_STEPS {
        return Err(format!(
            "steps exceeds the maximum of {MAX_STEPS}; no action was sent"
        ));
    }

    let mut encoded = Vec::with_capacity(steps.len());
    let mut declared_wait_ms = 0_u64;
    for (index, step) in steps.into_iter().enumerate() {
        let mut action_step = |action: serde_json::Value, after_ms: u64| {
            declared_wait_ms = declared_wait_ms.saturating_add(after_ms);
            serde_json::json!({
                "kind": "action",
                "action": action,
                "after_ms": after_ms
            })
        };
        let validate_after = |after_ms: u64| -> Result<(), String> {
            if after_ms > MAX_AFTER_MS {
                Err(format!(
                    "steps[{index}].after_ms exceeds {MAX_AFTER_MS}; no action was sent"
                ))
            } else {
                Ok(())
            }
        };
        let encoded_step = match step {
            PhoneStep::Tap { x, y, after_ms } => {
                validate_after(after_ms)?;
                if !x.is_finite()
                    || !y.is_finite()
                    || !(0.0..=1.0).contains(&x)
                    || !(0.0..=1.0).contains(&y)
                {
                    return Err(format!(
                        "steps[{index}] tap coordinates must be finite values from 0 to 1; no action was sent"
                    ));
                }
                action_step(serde_json::json!({"type":"tap","x":x,"y":y}), after_ms)
            }
            PhoneStep::Longpress {
                x,
                y,
                duration_ms,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if !x.is_finite()
                    || !y.is_finite()
                    || !(0.0..=1.0).contains(&x)
                    || !(0.0..=1.0).contains(&y)
                    || !(1..=10_000).contains(&duration_ms)
                {
                    return Err(format!(
                        "steps[{index}] longpress needs coordinates from 0 to 1 and duration_ms from 1 to 10000; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({
                        "type":"longpress",
                        "x":x,
                        "y":y,
                        "duration_ms":duration_ms
                    }),
                    after_ms,
                )
            }
            PhoneStep::Swipe {
                x1,
                y1,
                x2,
                y2,
                duration_ms,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if ![x1, y1, x2, y2].into_iter().all(f64::is_finite)
                    || ![x1, y1, x2, y2]
                        .into_iter()
                        .all(|value| (0.0..=1.0).contains(&value))
                    || !(1..=10_000).contains(&duration_ms)
                    || (x1 == x2 && y1 == y2)
                {
                    return Err(format!(
                        "steps[{index}] swipe needs distinct coordinates from 0 to 1 and duration_ms from 1 to 10000; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({
                        "type":"swipe",
                        "x1":x1,
                        "y1":y1,
                        "x2":x2,
                        "y2":y2,
                        "duration_ms":duration_ms
                    }),
                    after_ms,
                )
            }
            PhoneStep::Drag {
                x1,
                y1,
                x2,
                y2,
                hold_ms,
                duration_ms,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if ![x1, y1, x2, y2].into_iter().all(f64::is_finite)
                    || ![x1, y1, x2, y2]
                        .into_iter()
                        .all(|value| (0.0..=1.0).contains(&value))
                    || hold_ms > 10_000
                    || !(1..=10_000).contains(&duration_ms)
                    || (x1 == x2 && y1 == y2)
                {
                    return Err(format!(
                        "steps[{index}] drag needs distinct coordinates from 0 to 1, hold_ms at most 10000, and duration_ms from 1 to 10000; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({
                        "type":"drag",
                        "x1":x1,
                        "y1":y1,
                        "x2":x2,
                        "y2":y2,
                        "hold_ms":hold_ms,
                        "duration_ms":duration_ms
                    }),
                    after_ms,
                )
            }
            PhoneStep::TapLabel { label, after_ms } => {
                validate_after(after_ms)?;
                if label.trim().is_empty() || label.chars().count() > 500 {
                    return Err(format!(
                        "steps[{index}].label must contain 1 to 500 characters; no action was sent"
                    ));
                }
                action_step(serde_json::json!({"type":"tap","label":label}), after_ms)
            }
            PhoneStep::TapLocator {
                locator,
                via,
                if_present,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if via.as_deref().is_some_and(|v| v != "element" && v != "point") {
                    return Err(format!(
                        "steps[{index}].via must be \"element\" or \"point\"; no action was sent"
                    ));
                }
                if !phone_locator_has_condition(&locator) {
                    return Err(format!(
                        "steps[{index}].locator must include at least one condition; no action was sent"
                    ));
                }
                let locator = serde_json::to_value(locator).map_err(|error| {
                    format!(
                        "steps[{index}] locator serialization failed: {error}; no action was sent"
                    )
                })?;
                let mut action = serde_json::json!({"type":"tap_locator","locator":locator});
                if via.as_deref() == Some("point") {
                    action["via"] = serde_json::json!("point");
                }
                if if_present {
                    action["if_present"] = serde_json::json!(true);
                }
                action_step(action, after_ms)
            }
            PhoneStep::Type {
                text,
                clear,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if text.chars().count() > MAX_TEXT_CHARS {
                    return Err(format!(
                        "steps[{index}].text exceeds {MAX_TEXT_CHARS} characters; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({"type":"text","text":text,"clear":clear}),
                    after_ms,
                )
            }
            PhoneStep::Key { name, after_ms } => {
                validate_after(after_ms)?;
                let name = name.trim().to_ascii_lowercase();
                if !matches!(
                    name.as_str(),
                    "return"
                        | "enter"
                        | "escape"
                        | "space"
                        | "tab"
                        | "delete"
                        | "backspace"
                        | "up"
                        | "down"
                        | "left"
                        | "right"
                        | "dismiss"
                        | "hide"
                ) {
                    return Err(format!(
                        "steps[{index}] has unsupported key {name:?}; no action was sent"
                    ));
                }
                action_step(serde_json::json!({"type":"key","name":name}), after_ms)
            }
            PhoneStep::Shortcut { name, after_ms } => {
                validate_after(after_ms)?;
                let name = name.trim().to_ascii_lowercase();
                if !matches!(name.as_str(), "home" | "spotlight") {
                    return Err(format!(
                        "steps[{index}] has unsupported shortcut {name:?}; supported: home, spotlight; no action was sent"
                    ));
                }
                action_step(serde_json::json!({"type":"shortcut","name":name}), after_ms)
            }
            PhoneStep::Scroll {
                x,
                y,
                dx,
                dy,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if ![x, y, dx, dy].into_iter().all(f64::is_finite)
                    || !(0.0..=1.0).contains(&x)
                    || !(0.0..=1.0).contains(&y)
                    || dx.abs() > 1_000.0
                    || dy.abs() > 1_000.0
                    || (dx == 0.0 && dy == 0.0)
                {
                    return Err(format!(
                        "steps[{index}] has invalid scroll geometry; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({"type":"scroll","x":x,"y":y,"dx":dx,"dy":dy}),
                    after_ms,
                )
            }
            PhoneStep::LaunchApp {
                bundle: None,
                app: Some(app),
                fresh,
                after_ms,
            } => {
                validate_after(after_ms)?;
                let count = app.trim().chars().count();
                if !(1..=100).contains(&count) {
                    return Err(format!(
                        "steps[{index}].app must be an app name of 1 to 100 characters; no action was sent"
                    ));
                }
                let mut action = serde_json::json!({"type":"launch_app","app":app});
                if fresh {
                    action["fresh"] = serde_json::json!(true);
                }
                action_step(action, after_ms)
            }
            PhoneStep::LaunchApp {
                bundle: None,
                app: None,
                ..
            }
            | PhoneStep::LaunchApp {
                bundle: Some(_),
                app: Some(_),
                ..
            } => {
                return Err(format!(
                    "steps[{index}] launch_app needs exactly one of bundle or app; no action was sent"
                ))
            }
            PhoneStep::LaunchApp {
                bundle: Some(bundle),
                app: None,
                fresh,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if bundle.is_empty()
                    || bundle.len() > 200
                    || !bundle.chars().all(|character| {
                        character.is_ascii_alphanumeric() || character == '.' || character == '-'
                    })
                {
                    return Err(format!(
                        "steps[{index}].bundle must be a valid reverse-DNS identifier up to 200 bytes; no action was sent"
                    ));
                }
                {
                    let mut action = serde_json::json!({"type":"launch_app","bundle":bundle});
                    if fresh {
                        action["fresh"] = serde_json::json!(true);
                    }
                    action_step(action, after_ms)
                }
            }
            PhoneStep::OpenUrl { url, after_ms } => {
                validate_after(after_ms)?;
                let url = url.trim().to_string();
                if url.is_empty() || url.chars().count() > MAX_URL_CHARS || !url_has_scheme(&url) {
                    return Err(format!(
                        "steps[{index}].url must be an absolute URL with a scheme (https://…, \
                         weixin://…) of at most {MAX_URL_CHARS} characters; no action was sent"
                    ));
                }
                action_step(serde_json::json!({"type":"open_url","url":url}), after_ms)
            }
            PhoneStep::Back { after_ms } => {
                validate_after(after_ms)?;
                action_step(serde_json::json!({"type":"back"}), after_ms)
            }
            PhoneStep::Alert {
                button,
                action,
                if_present,
                after_ms,
            } => {
                validate_after(after_ms)?;
                let button = button.filter(|b| !b.trim().is_empty());
                let action = action.filter(|a| !a.trim().is_empty());
                let optional = |mut action: serde_json::Value| {
                    if if_present {
                        action["if_present"] = serde_json::json!(true);
                    }
                    action
                };
                match (&button, &action) {
                    (Some(b), None) if b.chars().count() <= 200 => action_step(
                        optional(serde_json::json!({"type":"alert","button":b})),
                        after_ms,
                    ),
                    (None, Some(a)) if a == "accept" || a == "dismiss" => action_step(
                        optional(serde_json::json!({"type":"alert","action":a})),
                        after_ms,
                    ),
                    _ => {
                        return Err(format!(
                            "steps[{index}] alert needs exactly one of button (exact text, ≤200 chars) or action accept|dismiss; no action was sent"
                        ))
                    }
                }
            }
            PhoneStep::Picker {
                column,
                value,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if column > 20 || value.trim().is_empty() || value.chars().count() > 500 {
                    return Err(format!(
                        "steps[{index}] picker needs column 0..20 and a non-empty value up to 500 characters; no action was sent"
                    ));
                }
                action_step(
                    serde_json::json!({"type":"picker","column":column,"value":value}),
                    after_ms,
                )
            }
            PhoneStep::Perform {
                element,
                snapshot,
                action,
                value,
                duration_ms,
                after_ms,
            } => {
                validate_after(after_ms)?;
                const NAMES: [&str; 11] = [
                    "increment", "decrement", "adjust", "toggle", "menu", "double_tap",
                    "two_finger_tap", "scroll_to_visible", "pinch", "rotate", "force_press",
                ];
                if !NAMES.contains(&action.as_str()) {
                    return Err(format!(
                        "steps[{index}] perform action {action:?} is not one of {}; no action was sent",
                        NAMES.join(", ")
                    ));
                }
                if snapshot.trim().is_empty() || snapshot.chars().count() > 200 {
                    return Err(format!(
                        "steps[{index}] perform needs the snapshot token from phone_elements; no action was sent"
                    ));
                }
                if value.is_some() != (action == "adjust") {
                    return Err(format!(
                        "steps[{index}] perform value is required with adjust and accepted only with adjust; no action was sent"
                    ));
                }
                if duration_ms.is_some() && !matches!(action.as_str(), "menu" | "force_press") {
                    return Err(format!(
                        "steps[{index}] perform duration_ms is accepted only with menu or force_press; no action was sent"
                    ));
                }
                let mut request = serde_json::json!({
                    "type": "perform",
                    "element": element,
                    "snapshot": snapshot,
                    "action": action,
                });
                if let Some(value) = value {
                    request["value"] = serde_json::json!(value);
                }
                if let Some(duration) = duration_ms {
                    request["duration_ms"] = serde_json::json!(duration);
                }
                action_step(request, after_ms)
            }
            PhoneStep::WaitFor {
                expect,
                timeout_ms,
                poll_ms,
            } => {
                if expect.application.is_none()
                    && expect.present.is_empty()
                    && expect.absent.is_empty()
                {
                    return Err(format!(
                        "steps[{index}].expect must include application, present, or absent; no action was sent"
                    ));
                }
                if expect
                    .application
                    .as_ref()
                    .is_some_and(|application| application.is_empty())
                {
                    return Err(format!(
                        "steps[{index}].expect.application must not be empty; no action was sent"
                    ));
                }
                if expect
                    .present
                    .iter()
                    .chain(expect.absent.iter())
                    .any(|locator| !phone_locator_has_condition(locator))
                {
                    return Err(format!(
                        "steps[{index}] contains an empty element locator; no action was sent"
                    ));
                }
                if timeout_ms == 0 || timeout_ms > MAX_WAIT_MS {
                    return Err(format!(
                        "steps[{index}].timeout_ms must be between 1 and {MAX_WAIT_MS}; no action was sent"
                    ));
                }
                if !(50..=1_000).contains(&poll_ms) {
                    return Err(format!(
                        "steps[{index}].poll_ms must be between 50 and 1000; no action was sent"
                    ));
                }
                declared_wait_ms = declared_wait_ms.saturating_add(timeout_ms);
                serde_json::json!({
                    "kind": "wait_for",
                    "expect": expect,
                    "timeout_ms": timeout_ms,
                    "poll_ms": poll_ms
                })
            }
            PhoneStep::Pause { ms } => {
                if ms == 0 || ms > MAX_AFTER_MS {
                    return Err(format!(
                        "steps[{index}].ms must be between 1 and {MAX_AFTER_MS}; no action was sent"
                    ));
                }
                declared_wait_ms = declared_wait_ms.saturating_add(ms);
                serde_json::json!({"kind":"pause","ms":ms})
            }
        };
        encoded.push(encoded_step);
    }
    if declared_wait_ms > MAX_DECLARED_WAIT_MS {
        return Err(format!(
            "declared waits exceed the batch maximum of {MAX_DECLARED_WAIT_MS}ms; no action was sent"
        ));
    }
    Ok(serde_json::json!({"steps": encoded}))
}

/// Send a single input event and map daemon errors to MCP tool errors.
/// Attach the parsed JSON as structured content, keeping the text short.
///
/// The JSON goes in whole; the text is only a preview. Putting a large body
/// in the text block is what truncates it — an element tree or a post-action
/// delta is routinely past any display-sized cap, and a client that reads the
/// text would then get unparseable, tail-truncated output.
fn with_structure(
    mut result: CallToolResult,
    response: &crate::client::DaemonResponse,
) -> CallToolResult {
    // Objects only: MCP structured content is an object, so an array or a
    // scalar must not be attached even when it parsed.
    if let Some(json) = response
        .json
        .as_ref()
        .filter(|value| value.is_object())
        .cloned()
    {
        result.structured_content = Some(json);
    }
    result
}

/// `phone_flow_run`'s answer: a compact verdict for the model (counts,
/// outputs, the failure's diagnosis, the hint) and the whole summary — every
/// step result — as structured content for programs.
fn flow_run_result(summary: serde_json::Value, is_error: bool) -> CallToolResult {
    let text = crate::compact::flow_run(&summary).unwrap_or_else(|| summary.to_string());
    let mut result = if is_error {
        CallToolResult::error(vec![Content::text(text)])
    } else {
        CallToolResult::success(vec![Content::text(text)])
    };
    result.structured_content = Some(summary);
    result
}

/// A mutation whose result is not known, in the one form a caller can branch
/// on. Never says "not sent": that is a claim about the phone we cannot make
/// once the request left this process.
fn unknown_action_result(reason: &str, detail: String) -> CallToolResult {
    let mut result = CallToolResult::error(vec![Content::text(format!(
        "outcome unknown: {detail} Do NOT resend automatically — check phone_elements or \
         phone_screenshot to see whether it took effect."
    ))]);
    result.structured_content = Some(serde_json::json!({
        "ok": false,
        "error": "outcome_unknown",
        "outcome": "unknown",
        "retry_safe": false,
        "reason": reason,
    }));
    result
}

/// A mutation this process refused before anything left it. Here — and only
/// here — re-sending is known to be safe, because nothing was sent.
fn not_sent_result(reason: &str, detail: String) -> CallToolResult {
    let mut result = CallToolResult::error(vec![Content::text(detail)]);
    result.structured_content = Some(serde_json::json!({
        "ok": false,
        "error": reason,
        "outcome": "not_sent",
        "retry_safe": true,
        "reason": reason,
    }));
    result
}

/// Render one MUTATION's daemon response.
///
/// Success requires `confirms_action()`, not `ok()`: a 2xx whose body could
/// not be read (unparseable, or past the read limit) proves the request was
/// accepted, never that the phone did anything. Reporting that as a tool
/// success is how an agent concludes a tap landed when nobody knows.
///
/// When there is no usable evidence the result is an error carrying
/// `retry_safe: false` — "we cannot tell" must not read as "nothing was sent",
/// because a blind resend is the one outcome that turns an uncertainty into a
/// duplicate action on a real phone.
fn daemon_action_result(
    response: &crate::client::DaemonResponse,
    observed: bool,
    plain_success: &str,
) -> CallToolResult {
    if response.confirms_action() {
        let text = if observed {
            response
                .json
                .as_ref()
                .and_then(crate::compact::observed)
                .unwrap_or_else(|| response.preview())
        } else {
            plain_success.to_string()
        };
        return with_structure(CallToolResult::success(vec![Content::text(text)]), response);
    }
    // A refusal the daemon spelled out keeps its own fields — it knows what
    // happened and said so. "Spelled out" means a body that explicitly says
    // `ok:false`. Merely parsing is not enough: `500 []`, `500 42` and
    // `500 {"unrelated":1}` all parse, none of them is the daemon telling us
    // the action did not run, and treating them as refusals dropped both the
    // structure (an array cannot be attached) and the unknown contract.
    // Shared with the batch entry point so both agree on what counts.
    if response.explicit_refusal() {
        return with_structure(
            CallToolResult::error(vec![Content::text(response.failure_summary())]),
            response,
        );
    }
    // Everything else — an unparseable 2xx, an oversized body, a 500 with an
    // HTML error page — leaves the fate of the action unknown. A non-2xx does
    // NOT imply the request never reached the phone: it can fail on the way
    // back just as easily as on the way out.
    // Same three reasons the batch entry point reports, so a caller can branch
    // on `reason` without knowing which tool produced it. A body that parsed
    // but carries no verdict is NOT "unparseable": we read it fine, it simply
    // did not say what happened.
    unknown_action_result(
        if response.too_large {
            "response_too_large"
        } else if response.json.is_none() {
            "unparseable_response"
        } else {
            "no_verdict_in_response"
        },
        format!(
            "the daemon answered {} and the result did not say what happened ({}).",
            response.status,
            response.preview()
        ),
    )
}

/// Render one READ-ONLY daemon response.
///
/// A read has no side effect to be uncertain about, so `ok()` is the right
/// question here — but a body that could not be parsed is still not data, so
/// it is reported as a failure rather than handed back as if it were.
fn daemon_read_result(response: &crate::client::DaemonResponse) -> CallToolResult {
    if !response.ok() {
        return with_structure(
            CallToolResult::error(vec![Content::text(response.failure_summary())]),
            response,
        );
    }
    // MCP's structured content is an object. A body that parsed into an array
    // or a scalar is not this endpoint's shape, and attaching it would emit
    // something the protocol does not allow.
    if !response.json.as_ref().is_some_and(serde_json::Value::is_object) {
        return CallToolResult::error(vec![Content::text(format!(
            "the daemon answered {} but the body was not a JSON object: {}",
            response.status,
            response.preview()
        ))]);
    }
    with_structure(
        CallToolResult::success(vec![Content::text(response.preview())]),
        response,
    )
}

/// A list call's answer: compact text for the model, the JSON as structured
/// content. A verdict with `ok:false` (not found, ambiguous) is an error
/// result that still carries its candidates; a refusal or failure keeps the
/// daemon's summary.
fn list_result(
    response: &crate::client::DaemonResponse,
    compact: fn(&serde_json::Value) -> Option<String>,
) -> CallToolResult {
    let text = response.json.as_ref().and_then(compact);
    match text {
        Some(text)
            if response.status.is_success()
                || response
                    .json
                    .as_ref()
                    .and_then(|body| body.get("error"))
                    .and_then(serde_json::Value::as_str)
                    == Some("app_changed") =>
        {
            let result = if response.ok() {
                CallToolResult::success(vec![Content::text(text)])
            } else {
                CallToolResult::error(vec![Content::text(text)])
            };
            with_structure(result, response)
        }
        _ => daemon_read_result(response),
    }
}

/// A device call's answer (logs control, location, recording, install, app
/// notes): `render`'s short text when the daemon said `ok:true`, the JSON as
/// structured content; a refusal keeps the daemon's summary, its hint included.
fn rendered_result(
    response: &crate::client::DaemonResponse,
    render: impl Fn(&serde_json::Value) -> Option<String>,
) -> CallToolResult {
    if response.confirms_action() {
        if let Some(text) = response.json.as_ref().and_then(render) {
            return with_structure(CallToolResult::success(vec![Content::text(text)]), response);
        }
    }
    daemon_read_result(response)
}

/// Add per-flow `compat` to a daemon `registry` block, fetching the installed
/// app inventory only when there is a block to annotate.
async fn with_flow_compat(daemon: &DaemonClient, body: String) -> String {
    if !body.contains("\"registry\"") {
        return body;
    }
    let installed = crate::compat::installed_apps(daemon).await;
    crate::registry::enrich_registry(body, installed.as_ref())
}

async fn send_input_observed(
    daemon: &DaemonClient,
    msg: &InputMsg,
    observe: Option<bool>,
) -> CallToolResult {
    let observe = observe.unwrap_or(true);
    match daemon.input_observed(msg, observe).await {
        Ok(response) => daemon_action_result(&response, observe, "ok"),
        // The request may well have reached the phone before the transport
        // broke, so this is unknown, not "not sent".
        Err(e) => unknown_action_result("transport_error", format!("the call failed: {e:#}.")),
    }
}

#[cfg(test)]
mod tests {

    fn image_verdict_result() -> CallToolResult {
        let mut result = CallToolResult::error(vec![
            Content::text("Screenshot after failure · retry_safe:true"),
            Content::image("cG5n", "image/png"),
        ]);
        result.structured_content = Some(serde_json::json!({
            "ok": false, "error": "x", "outcome": "not_sent",
            "retry_safe": true, "hint": "try again", "elements": ["bulk"],
        }));
        result
    }

    #[test]
    fn codex_image_keeps_image_and_always_appends_verdict() {
        let shaped = finish_result(
            "phone_screenshot",
            image_verdict_result(),
            StructuredMode::Slim,
        );
        let mut expected = shaped.clone();
        expected.structured_content = None;
        expected.content.push(Content::text(
            "ok:false error:\"x\" outcome:\"not_sent\" retry_safe:true hint:\"try again\"",
        ));
        let adapted = keep_codex_images(shaped, is_codex_client("codex-mcp-client"));
        assert!(adapted.structured_content.is_none());
        assert_eq!(adapted.content[1].as_image().unwrap().data, "cG5n");
        assert_eq!(
            serde_json::to_vec(&adapted).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }

    #[test]
    fn codex_image_without_image_is_unchanged() {
        let mut result = image_verdict_result();
        result.content.retain(|block| block.as_image().is_none());
        for mode in [
            StructuredMode::Slim,
            StructuredMode::Full,
            StructuredMode::Off,
        ] {
            let shaped = finish_result("phone_elements", result.clone(), mode);
            assert_eq!(
                serde_json::to_vec(&keep_codex_images(shaped.clone(), true)).unwrap(),
                serde_json::to_vec(&shaped).unwrap(),
            );
        }
    }

    #[test]
    fn codex_image_other_clients_are_unchanged() {
        for mode in [
            StructuredMode::Slim,
            StructuredMode::Full,
            StructuredMode::Off,
        ] {
            let shaped = finish_result("phone_screenshot", image_verdict_result(), mode);
            assert_eq!(
                serde_json::to_vec(&keep_codex_images(shaped.clone(), false)).unwrap(),
                serde_json::to_vec(&shaped).unwrap(),
            );
        }
    }

    #[test]
    fn codex_image_name_matching_is_case_insensitive_prefix_only() {
        for name in ["codex", "Codex-MCP-Client", "CODEX-desktop"] {
            assert!(is_codex_client(name), "{name}");
        }
        for name in ["my-codex", "code", "", " codex", "你好codex"] {
            assert!(!is_codex_client(name), "{name}");
        }
    }

    #[test]
    fn codex_image_device_text_does_not_suppress_verdict() {
        for mode in [StructuredMode::Slim, StructuredMode::Full] {
            let mut result = CallToolResult::success(vec![
                Content::text("#0 [StaticText] \"Status ok:true now\""),
                Content::image("cG5n", "image/png"),
            ]);
            result.structured_content = Some(serde_json::json!({"ok": true}));
            let shaped = finish_result("phone_screenshot", result, mode);
            let mut expected = shaped.clone();
            expected.structured_content = None;
            expected.content.push(Content::text("ok:true"));
            let adapted = keep_codex_images(shaped, true);
            assert_eq!(
                serde_json::to_vec(&adapted).unwrap(),
                serde_json::to_vec(&expected).unwrap()
            );
        }
    }

    #[test]
    fn codex_image_verdict_without_text_and_modes() {
        for mode in [StructuredMode::Slim, StructuredMode::Full] {
            let mut result = image_verdict_result();
            result.content.retain(|block| block.as_image().is_some());
            let adapted = keep_codex_images(finish_result("phone_screenshot", result, mode), true);
            assert!(adapted.structured_content.is_none());
            assert_eq!(
                adapted.content.last().unwrap().as_text().unwrap().text,
                "ok:false error:\"x\" outcome:\"not_sent\" retry_safe:true hint:\"try again\""
            );
        }
        let shaped = finish_result(
            "phone_screenshot",
            image_verdict_result(),
            StructuredMode::Off,
        );
        assert_eq!(
            serde_json::to_vec(&keep_codex_images(shaped.clone(), true)).unwrap(),
            serde_json::to_vec(&shaped).unwrap()
        );
        // A structured body with no verdict keys adds no empty text block.
        let mut bare = image_verdict_result();
        bare.content.retain(|block| block.as_image().is_some());
        bare.structured_content = Some(serde_json::json!({"rows": 3}));
        let adapted = keep_codex_images(bare, true);
        assert!(adapted.structured_content.is_none());
        assert!(adapted.content.iter().all(|block| block.as_text().is_none()));
    }

    /// A misspelled or unsupported argument must fail loudly. Before this,
    /// `phone_tap_label {"label":…, "kind":"Button"}` silently dropped
    /// `kind`, and the agent believed its filter had applied.
    #[test]
    fn tool_arguments_reject_unknown_fields() {
        let err = serde_json::from_value::<TapLabelParams>(
            serde_json::json!({"label": "删除", "kind": "Button"}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field `kind`"), "{err}");
        assert!(serde_json::from_value::<TapLabelParams>(serde_json::json!({"label": "删除"})).is_ok());
        let schema = serde_json::to_value(rmcp::schemars::schema_for!(TapLabelParams)).unwrap();
        assert_eq!(schema["additionalProperties"], serde_json::json!(false), "{schema}");
    }
    use super::*;

    // ---------------------------------------------------------------------
    // Real tool calls against a scripted daemon.
    // ---------------------------------------------------------------------

    /// One-shot HTTP responder. Returns the URL and a join handle so a panic
    /// inside it surfaces in the test.
    fn scripted_daemon(status: &str, body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        // Non-blocking with a deadline: a blocking `accept` that never gets a
        // connection — because the test under it failed early — turns a joined
        // handle into a hung suite.
        listener.set_nonblocking(true).unwrap();
        let status = status.to_string();
        let task = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        // The accept deadline does not cover the socket: a
                        // peer that connects and stays silent would park this
                        // thread in `read`.
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                            .ok();
                        stream
                            .set_write_timeout(Some(std::time::Duration::from_secs(10)))
                            .ok();
                        let mut request = [0_u8; 8_192];
                        let _ = stream.read(&mut request);
                        let head = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&body);
                        return;
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        (format!("http://{address}"), task)
    }

    fn handler_for(url: &str) -> PhoneHandler {
        PhoneHandler::new(DaemonClient::new(url.to_string(), None))
    }

    fn block<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole point of `observe`: a real observation is far past any
    /// display-sized cap, and its TAIL is where `settle` and `delta` live. The
    /// structured content must carry it intact even though the text preview
    /// is trimmed.
    #[test]
    fn an_observed_tap_returns_the_whole_observation_not_a_truncated_preview() {
        let filler = "x".repeat(64 * 1024);
        let body = format!(
            r#"{{"ok":true,"transport":"wda","tree":"{filler}","snapshot":"snap-1","settle":{{"settled":true,"reason":"stable","captures":2}},"delta":{{"added":["搜索"]}}}}"#
        );
        assert!(body.len() > 64 * 1024);
        let (url, task) = scripted_daemon("200 OK", body.into_bytes());
        let handler = handler_for(&url);

        let result = block(handler.phone_tap(Parameters(TapParams {
            x: 0.5,
            y: 0.5,
            observe: Some(true),
        })));
        task.join().unwrap();

        assert_ne!(result.is_error, Some(true), "{:?}", text_of(&result));
        let structured = result
            .structured_content
            .as_ref()
            .expect("an observed action must return structured content");
        assert_eq!(structured["settle"]["reason"], "stable");
        assert_eq!(structured["settle"]["captures"], 2);
        assert_eq!(structured["delta"]["added"][0], "搜索");
        assert_eq!(structured["snapshot"], "snap-1");
    }

    /// With `observe:false` the result stays the short string callers
    /// already parse, and the request must not have asked for a delta.
    #[test]
    fn an_unobserved_tap_keeps_its_plain_result() {
        let (url, task) = scripted_daemon(
            "200 OK",
            br#"{"ok":true,"transport":"wda"}"#.to_vec(),
        );
        let handler = handler_for(&url);

        let result = block(handler.phone_tap(Parameters(TapParams {
            x: 0.5,
            y: 0.5,
            observe: Some(false),
        })));
        task.join().unwrap();

        assert_ne!(result.is_error, Some(true));
        assert_eq!(text_of(&result), "ok");
    }

    /// A refusal must arrive with the fields a caller decides on, not as prose.
    #[test]
    fn a_refused_action_keeps_outcome_and_retry_safety() {
        let (url, task) = scripted_daemon(
            "409 Conflict",
            br#"{"ok":false,"error":"phone_owned","outcome":"not_sent","retry_safe":true}"#.to_vec(),
        );
        let handler = handler_for(&url);

        let result = block(handler.phone_tap(Parameters(TapParams {
            x: 0.5,
            y: 0.5,
            observe: None,
        })));
        task.join().unwrap();

        assert_eq!(result.is_error, Some(true));
        let structured = result
            .structured_content
            .as_ref()
            .expect("a structured refusal must survive");
        assert_eq!(structured["error"], "phone_owned");
        assert_eq!(structured["outcome"], "not_sent");
        assert_eq!(structured["retry_safe"], true);
        let text = text_of(&result);
        assert!(text.contains("outcome=not_sent"), "{text}");
    }

    /// A partial batch must reach the caller as a partial batch.
    ///
    /// The hardware shape: two actions applied, the step that failed never
    /// sent. `outcome` describes that step, so on its own it reads as "nothing
    /// happened" — which is why the batch verdict travels beside it. All three
    /// fields, and `retry_safe`, must arrive intact and unrewritten.
    #[test]
    fn a_partial_batch_keeps_the_batch_verdict_beside_the_step_outcome() {
        let (url, task) = scripted_daemon(
            "409 Conflict",
            br#"{"ok":false,"error":"expectation_timeout","failed_step":2,"completed":2,"applied_actions":2,"outcome":"not_sent","failed_step_outcome":"not_sent","batch_outcome":"partially_applied","retry_safe":false,"steps":[]}"#.to_vec(),
        );
        let handler = handler_for(&url);

        let result = block(handler.phone_run_steps(Parameters(RunStepsParams {
            steps: vec![PhoneStep::Tap {
                x: 0.5,
                y: 0.5,
                after_ms: 0,
            }],
            observe: None,
        })));
        task.join().unwrap();

        assert_eq!(result.is_error, Some(true));
        let structured = result
            .structured_content
            .as_ref()
            .expect("a structured batch refusal must survive");
        assert_eq!(structured["applied_actions"], 2);
        assert_eq!(
            structured["outcome"], "not_sent",
            "the legacy field keeps describing the failed step: {structured}"
        );
        assert_eq!(structured["failed_step_outcome"], "not_sent", "{structured}");
        assert_eq!(
            structured["batch_outcome"], "partially_applied",
            "two actions reached the phone; the batch is not `nothing_applied`: {structured}"
        );
        assert_eq!(
            structured["retry_safe"], false,
            "the only authorisation to replay must survive untouched: {structured}"
        );
    }

    fn partial_text_body(extra: serde_json::Value) -> Vec<u8> {
        let mut body = serde_json::json!({
            "ok": false,
            "error": "text_partially_typed",
            "outcome": "partial",
            "retry_safe": false,
            "characters_confirmed": 400,
            "characters_uncertain": 200,
            "characters_total": 10_600,
            "remaining_text": "字".repeat(10_200),
            "hint": "send only the rest",
        });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        body.to_string().into_bytes()
    }

    /// A long text that stopped part-way: the counts and retry_safe=false must
    /// be readable even though remaining_text alone is far past any preview.
    #[test]
    fn a_partially_typed_text_reaches_the_model_with_its_counts() {
        let (url, task) = scripted_daemon("502 Bad Gateway", partial_text_body(serde_json::json!({})));
        let handler = handler_for(&url);
        // The request stays small (the scripted daemon reads one 8 KiB
        // buffer); the answer is what is under test.
        let result = block(handler.phone_type(Parameters(TypeParams {
            text: "x".repeat(600),
            observe: Some(false),
        })));
        task.join().unwrap();
        assert_eq!(result.is_error, Some(true));
        let text = text_of(&result);
        assert!(text.contains("retry_safe=false"), "{text}");
        assert!(text.contains("characters_confirmed"), "{text}");
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["characters_confirmed"], 400);
        assert_eq!(structured["remaining_text"].as_str().unwrap().chars().count(), 10_200);
    }

    #[test]
    fn a_partially_typed_batch_names_how_far_the_text_got() {
        let (url, task) = scripted_daemon(
            "502 Bad Gateway",
            partial_text_body(serde_json::json!({
                "failed_step": 0, "completed": 0, "applied_actions": 0,
                "failed_step_outcome": "partial", "batch_outcome": "partially_applied",
                "steps": []
            })),
        );
        let handler = handler_for(&url);
        let result = block(handler.phone_run_steps(Parameters(RunStepsParams {
            steps: vec![PhoneStep::Type {
                text: "x".repeat(600),
                clear: false,
                after_ms: 0,
            }],
            observe: Some(false),
        })));
        task.join().unwrap();
        assert_eq!(result.is_error, Some(true));
        let text = text_of(&result);
        assert!(text.contains("400 of 10600 characters typed"), "{text}");
        assert!(text.contains("retry_safe=false"), "{text}");
        assert!(!text.contains(&"字".repeat(50)), "the remaining text stays structured: {text}");
    }

    /// A 400 the daemon spelled out is still a spelled-out refusal.
    ///
    /// `invalid_action` is newer than this renderer. Nothing here keeps a list
    /// of known error codes, and this test is what keeps it that way: an
    /// unfamiliar `ok:false` body must reach the caller with its own
    /// `outcome`/`retry_safe` intact, NOT be flattened into `unknown` — which
    /// would strip the very fact that makes it safe to fix and resend.
    #[test]
    fn an_unfamiliar_refusal_code_is_passed_through_not_downgraded() {
        let (url, task) = scripted_daemon(
            "400 Bad Request",
            br#"{"ok":false,"error":"invalid_action","detail":"unknown action type \"teleport\"","outcome":"not_sent","retry_safe":true}"#.to_vec(),
        );
        let handler = handler_for(&url);

        let result = block(handler.phone_tap(Parameters(TapParams {
            x: 0.5,
            y: 0.5,
            observe: None,
        })));
        task.join().unwrap();

        assert_eq!(result.is_error, Some(true));
        let structured = result
            .structured_content
            .as_ref()
            .expect("a structured refusal must survive");
        assert_eq!(structured["error"], "invalid_action");
        assert_ne!(
            structured["outcome"], "unknown",
            "a spelled-out refusal must not be downgraded: {structured}"
        );
        assert_eq!(structured["outcome"], "not_sent");
        assert_eq!(structured["retry_safe"], true);
    }

    /// The dangerous case: HTTP said yes, the body says nothing. That is not a
    /// success, and above all it is not a licence to resend.
    #[test]
    fn an_unreadable_success_is_reported_unknown_and_never_retry_safe() {
        for body in [
            b"not json at all".to_vec(),
            vec![b'x'; 5 * 1024 * 1024], // past the read limit
        ] {
            let (url, task) = scripted_daemon("200 OK", body);
            let handler = handler_for(&url);

            let result = block(handler.phone_tap(Parameters(TapParams {
                x: 0.5,
                y: 0.5,
                observe: None,
            })));
            task.join().unwrap();

            assert_eq!(
                result.is_error,
                Some(true),
                "an unreadable body was reported as a successful tap: {}",
                text_of(&result)
            );
            let structured = result
                .structured_content
                .as_ref()
                .expect("an unknown outcome must still be structured");
            assert_eq!(structured["outcome"], "unknown");
            assert_eq!(
                structured["retry_safe"], false,
                "an unknown outcome authorised a resend"
            );
            let text = text_of(&result);
            assert!(text.contains("Do NOT resend"), "{text}");
        }
    }

    /// A read that could not be parsed is not data, and must not be handed
    /// back as though it were.
    #[test]
    fn capabilities_reports_an_unparseable_body_as_a_failure() {
        let (url, task) = scripted_daemon("200 OK", b"<html>proxy error</html>".to_vec());
        let handler = handler_for(&url);

        let result = block(handler.phone_capabilities());
        task.join().unwrap();

        assert_eq!(result.is_error, Some(true), "{}", text_of(&result));
        assert!(result.structured_content.is_none());

        let (url, task) = scripted_daemon(
            "200 OK",
            br#"{"ok":true,"backend":"direct","supported":{"element_tree":true}}"#.to_vec(),
        );
        let handler = handler_for(&url);
        let result = block(handler.phone_capabilities());
        task.join().unwrap();
        assert_ne!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["supported"]["element_tree"],
            true
        );
    }

    /// Tool discovery, from the router the macro actually generates — the same
    /// list an MCP client receives. Asserting a documented number in prose
    /// would pass while the server exposed something else.
    #[test]
    fn tool_discovery_matches_the_documented_surface() {
        let router = PhoneHandler::tool_router();
        let tools = router.list_all();
        let mut names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        names.sort_unstable();

        assert_eq!(
            names.len(),
            37,
            "tool count changed; update README, the skill, and the CI assertion: {names:?}"
        );
        for required in [
            "phone_apps",
            "phone_screen",
            "phone_screen_frame",
            "phone_login",
            "phone_jev_run",
            "phone_flow_draft",
            "phone_capabilities",
            "phone_status",
            "phone_run_steps",
            "phone_hold",
            "phone_release_owner",
            "phone_collect_list",
            "phone_scroll_find",
            "phone_logs",
            "phone_crashes",
            "phone_set_location",
            "phone_record",
            "phone_install_app",
            "phone_app_notes",
        ] {
            assert!(names.contains(&required), "{required} missing: {names:?}");
        }

        // Discovery must be free to call: no required arguments.
        let capabilities = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "phone_capabilities")
            .expect("phone_capabilities");
        let required = capabilities
            .input_schema
            .get("required")
            .and_then(|value| value.as_array());
        assert!(
            required.is_none_or(|list| list.is_empty()),
            "capability discovery asks for arguments: {:?}",
            capabilities.input_schema
        );

        // `observe` is accepted (and optional — it defaults on) by every
        // single-step UI tool: in the schema,
        // never required, so calls written before it keep working.
        for name in [
            "phone_tap",
            "phone_scroll",
            "phone_type",
            "phone_key",
            "phone_shortcut",
            "phone_tap_element",
            "phone_tap_label",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .unwrap_or_else(|| panic!("{name} missing"));
            let schema = &tool.input_schema;
            let properties = schema
                .get("properties")
                .and_then(|value| value.as_object())
                .unwrap_or_else(|| panic!("{name} has no properties: {schema:?}"));
            assert!(
                properties.contains_key("observe"),
                "{name} does not accept observe: {schema:?}"
            );
            let required = schema
                .get("required")
                .and_then(|value| value.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|value| value.as_str())
                        .any(|field| field == "observe")
                })
                .unwrap_or(false);
            assert!(!required, "{name} made observe mandatory: {schema:?}");
        }
    }

    /// The tools that only read: no phone input, no outward-facing writes.
    /// `IPHONE_USE_MCP_TOOLS=readonly` exposes exactly these.
    const READ_ONLY_TOOLS: [&str; 11] = [
        "phone_apps",
        "phone_capabilities",
        "phone_crashes",
        "phone_elements",
        "phone_flow_draft",
        "phone_flow_info",
        "phone_flow_list",
        "phone_screen",
        "phone_screen_frame",
        "phone_screenshot",
        "phone_status",
    ];

    /// Every tool states read_only_hint and open_world_hint explicitly (the
    /// spec defaults would call every tool a destructive open-world one), and
    /// the read-only set is exactly the intended one. Acting tools also say
    /// whether they are destructive and idempotent.
    #[test]
    fn every_tool_declares_its_annotations() {
        let tools = PhoneHandler::tool_router().list_all();
        let mut read_only = Vec::new();
        for tool in &tools {
            let name = tool.name.as_ref();
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{name} has no annotations"));
            let ro = annotations
                .read_only_hint
                .unwrap_or_else(|| panic!("{name} does not declare read_only_hint"));
            assert!(
                annotations.open_world_hint.is_some(),
                "{name} does not declare open_world_hint"
            );
            if ro {
                read_only.push(name);
            } else {
                assert!(
                    annotations.destructive_hint.is_some() && annotations.idempotent_hint.is_some(),
                    "{name} acts but does not declare destructive/idempotent hints"
                );
            }
        }
        read_only.sort_unstable();
        assert_eq!(read_only, READ_ONLY_TOOLS);

        // Anything that sends input to the phone or publishes is destructive.
        for name in [
            "phone_tap",
            "phone_run_steps",
            "phone_flow_run",
            "phone_flow_publish",
            "phone_install_app",
        ] {
            let tool = tools.iter().find(|t| t.name.as_ref() == name).unwrap();
            assert_eq!(
                tool.annotations.as_ref().unwrap().destructive_hint,
                Some(true),
                "{name}"
            );
        }
        // The hints reach the wire under their MCP names.
        let listed = serde_json::to_value(listed_tools(&ToolProfile::All)).unwrap();
        let status = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "phone_status")
            .unwrap();
        assert_eq!(status["annotations"]["readOnlyHint"], true);
    }

    fn listed_names(profile: &ToolProfile) -> Vec<String> {
        let mut names: Vec<String> = listed_tools(profile)
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn the_all_profile_is_the_default_and_hides_nothing() {
        for raw in [None, Some(""), Some("all"), Some(" ALL ")] {
            let (profile, warnings) = ToolProfile::parse(raw);
            assert_eq!(profile, ToolProfile::All, "{raw:?}");
            assert!(warnings.is_empty());
        }
        let all = PhoneHandler::tool_router().list_all().len();
        assert_eq!(listed_names(&ToolProfile::All).len(), all);
        assert!(ToolProfile::All.refusal("phone_tap").is_none());
    }

    #[test]
    fn the_readonly_profile_lists_and_accepts_only_read_only_tools() {
        let (profile, warnings) = ToolProfile::parse(Some("readonly"));
        assert_eq!(profile, ToolProfile::ReadOnly);
        assert!(warnings.is_empty());
        assert_eq!(listed_names(&profile), READ_ONLY_TOOLS);

        for name in READ_ONLY_TOOLS {
            assert!(profile.refusal(name).is_none(), "{name} refused");
        }
        let refused = profile.refusal("phone_tap").expect("phone_tap is hidden");
        assert_eq!(refused.is_error, Some(true));
        let text = text_of(&refused);
        assert!(text.contains("IPHONE_USE_MCP_TOOLS=readonly"), "{text}");
        assert!(text.contains("phone_tap"), "{text}");
        // A name that is no tool at all is left to the router's own error.
        assert!(profile.refusal("phone_nonexistent").is_none());
    }

    #[test]
    fn readonly_returns_a_draft_but_never_writes_one() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("draft.json");
        let (url, server) = scripted_daemon(
            "200 OK",
            br#"{"ok":true,"flow":{"version":1,"app":"com.example","steps":[]},"todo":[]}"#.to_vec(),
        );
        let handler = handler_for(&url).with_profile(ToolProfile::ReadOnly);
        let result = block(handler.phone_flow_draft(Parameters(FlowDraftParams {
            save_as: Some(target.display().to_string()),
        })));
        server.join().unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(text_of(&result).contains("IPHONE_USE_MCP_TOOLS=readonly"));
        assert!(!target.exists(), "nothing may be written in readonly mode");
    }

    #[test]
    fn an_allowlist_profile_keeps_named_tools_and_ignores_unknown_ones() {
        let (profile, warnings) =
            ToolProfile::parse(Some("phone_status, phone_tap,phone_bogus,,phone_screen"));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("phone_bogus"), "{warnings:?}");
        // The panel's frame tool follows phone_screen.
        assert_eq!(
            listed_names(&profile),
            ["phone_screen", "phone_screen_frame", "phone_status", "phone_tap"]
        );
        assert!(profile.refusal("phone_tap").is_none());
        let refused = profile.refusal("phone_run_steps").expect("hidden");
        let text = text_of(&refused);
        assert!(text.contains("IPHONE_USE_MCP_TOOLS allows only"), "{text}");
        assert!(text.contains("phone_status"), "{text}");

        let (empty, warnings) = ToolProfile::parse(Some("nope"));
        assert_eq!(empty, ToolProfile::Allow(Default::default()));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(listed_names(&empty).is_empty());
        assert!(handler_for("http://127.0.0.1:9")
            .with_profile(empty)
            .profile
            .refusal("phone_status")
            .is_some());
    }

    #[test]
    fn phone_elements_passes_query_and_limit_through() {
        use crate::client::elements_path;
        assert_eq!(elements_path(None, None, None), "/agent/elements?image=auto");
        assert_eq!(
            elements_path(Some("Wi-Fi & 蓝牙"), Some(5), None),
            "/agent/elements?image=auto&query=Wi-Fi+%26+%E8%93%9D%E7%89%99&limit=5"
        );
        // A blank query is no filter.
        assert_eq!(
            elements_path(Some("  "), Some(0), None),
            "/agent/elements?image=auto&limit=0"
        );
        let tool = PhoneHandler::tool_router()
            .list_all()
            .into_iter()
            .find(|tool| tool.name.as_ref() == "phone_elements")
            .unwrap();
        let properties = tool.input_schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("query") && properties.contains_key("limit"));
        assert!(tool
            .input_schema
            .get("required")
            .and_then(|r| r.as_array())
            .is_none_or(|r| r.is_empty()));
    }

    #[test]
    fn open_url_is_a_run_steps_kind() {
        let steps: Vec<PhoneStep> = serde_json::from_value(serde_json::json!([
            {"kind": "open_url", "url": " https://example.com/a?b=1 ", "after_ms": 200}
        ]))
        .unwrap();
        let request = phone_steps_request(steps).unwrap();
        assert_eq!(
            request["steps"][0]["action"],
            serde_json::json!({"type": "open_url", "url": "https://example.com/a?b=1"})
        );
        assert_eq!(request["steps"][0]["after_ms"], 200);

        // Scheme policy is the daemon's; the MCP only refuses what is not a URL.
        let ok: Vec<PhoneStep> =
            serde_json::from_value(serde_json::json!([{"kind": "open_url", "url": "weixin://"}]))
                .unwrap();
        assert!(phone_steps_request(ok).is_ok());
        for bad in ["", "example.com", "://x", "1http://x", &format!("https://{}", "a".repeat(2048))]
        {
            let steps: Vec<PhoneStep> =
                serde_json::from_value(serde_json::json!([{"kind": "open_url", "url": bad}]))
                    .unwrap();
            let error = phone_steps_request(steps).unwrap_err();
            assert!(error.contains("no action was sent"), "{bad}: {error}");
        }
    }

    #[test]
    fn multi_step_request_encodes_actions_and_semantic_waits() {
        let request = phone_steps_request(vec![
            PhoneStep::Shortcut {
                name: "home".to_string(),
                after_ms: 300,
            },
            PhoneStep::TapLabel {
                label: "搜索".to_string(),
                after_ms: 0,
            },
            PhoneStep::TapLocator {
                via: None,
                if_present: false,
                locator: PhoneElementLocator {
                    label: None,
                    identifier: Some("search-field".to_string()),
                    kind: Some("TextField".to_string()),
                    value: None,
                    focused: Some(true),
                    enabled: Some(true),
                    visible: Some(true),
                },
                after_ms: 0,
            },
            PhoneStep::WaitFor {
                expect: PhoneUiExpectation {
                    application: Some("聚焦".to_string()),
                    present: vec![PhoneElementLocator {
                        label: Some("搜索".to_string()),
                        identifier: None,
                        kind: Some("TextField".to_string()),
                        value: None,
                        focused: Some(true),
                        enabled: None,
                        visible: None,
                    }],
                    absent: vec![],
                },
                timeout_ms: 2_000,
                poll_ms: 100,
            },
        ])
        .unwrap();

        assert_eq!(request["steps"][0]["kind"], "action");
        assert_eq!(request["steps"][0]["action"]["type"], "shortcut");
        assert_eq!(request["steps"][1]["action"]["type"], "tap");
        assert_eq!(request["steps"][1]["action"]["label"], "搜索");
        assert_eq!(request["steps"][2]["action"]["type"], "tap_locator");
        assert_eq!(
            request["steps"][2]["action"]["locator"]["identifier"],
            "search-field"
        );
        assert_eq!(request["steps"][3]["kind"], "wait_for");
        assert_eq!(request["steps"][3]["expect"]["application"], "聚焦");
        assert_eq!(request["steps"][3]["expect"]["present"][0]["focused"], true);
    }

    #[test]
    fn multi_step_request_encodes_recordable_gestures() {
        let request = phone_steps_request(vec![
            PhoneStep::Longpress {
                x: 0.4,
                y: 0.5,
                duration_ms: 650,
                after_ms: 100,
            },
            PhoneStep::Swipe {
                x1: 0.5,
                y1: 0.8,
                x2: 0.5,
                y2: 0.2,
                duration_ms: 320,
                after_ms: 0,
            },
            PhoneStep::Drag {
                x1: 0.2,
                y1: 0.5,
                x2: 0.8,
                y2: 0.5,
                hold_ms: 500,
                duration_ms: 400,
                after_ms: 0,
            },
        ])
        .unwrap();

        assert_eq!(request["steps"][0]["action"]["type"], "longpress");
        assert_eq!(request["steps"][0]["action"]["duration_ms"], 650);
        assert_eq!(request["steps"][1]["action"]["type"], "swipe");
        assert_eq!(request["steps"][1]["action"]["y2"], 0.2);
        assert_eq!(request["steps"][2]["action"]["type"], "drag");
        assert_eq!(request["steps"][2]["action"]["hold_ms"], 500);
    }

    #[test]
    fn multi_step_request_rejects_every_invalid_step_before_sending() {
        let error = phone_steps_request(vec![
            PhoneStep::TapLabel {
                label: "搜索".to_string(),
                after_ms: 0,
            },
            PhoneStep::Shortcut {
                name: "switcher".to_string(),
                after_ms: 0,
            },
        ])
        .unwrap_err();
        assert!(error.contains("steps[1]"));
        assert!(error.contains("no action was sent"));
    }

    #[test]
    fn multi_step_alert_encodes_button_or_action_and_rejects_both_or_neither() {
        let ok = phone_steps_request(vec![
            PhoneStep::Alert {
                if_present: false,
                button: Some("导出".into()),
                action: None,
                after_ms: 100,
            },
            PhoneStep::Alert {
                if_present: false,
                button: None,
                action: Some("dismiss".into()),
                after_ms: 0,
            },
        ])
        .unwrap();
        assert_eq!(
            ok["steps"][0]["action"],
            serde_json::json!({"type":"alert","button":"导出"})
        );
        assert_eq!(
            ok["steps"][1]["action"],
            serde_json::json!({"type":"alert","action":"dismiss"})
        );
        let optional = phone_steps_request(vec![PhoneStep::Alert {
            if_present: true,
            button: Some("允许粘贴".into()),
            action: None,
            after_ms: 0,
        }])
        .unwrap();
        assert_eq!(
            optional["steps"][0]["action"],
            serde_json::json!({"type":"alert","button":"允许粘贴","if_present":true})
        );
        for step in [
            PhoneStep::Alert {
                if_present: false,
                button: None,
                action: None,
                after_ms: 0,
            },
            PhoneStep::Alert {
                if_present: false,
                button: Some("OK".into()),
                action: Some("accept".into()),
                after_ms: 0,
            },
            PhoneStep::Alert {
                if_present: false,
                button: None,
                action: Some("yes".into()),
                after_ms: 0,
            },
        ] {
            assert!(phone_steps_request(vec![step])
                .unwrap_err()
                .contains("alert needs"));
        }
    }

    #[test]
    fn multi_step_launch_app_requires_a_valid_bundle_identifier() {
        let request = phone_steps_request(vec![PhoneStep::LaunchApp {
            bundle: Some("com.example.SampleApp".to_string()),
            app: None,
            fresh: false,
            after_ms: 500,
        }])
        .unwrap();
        assert_eq!(request["steps"][0]["action"]["type"], "launch_app");
        assert!(request["steps"][0]["action"].get("fresh").is_none(), "only when asked");
        let fresh = phone_steps_request(vec![PhoneStep::LaunchApp {
            bundle: Some("com.example.SampleApp".to_string()),
            app: None,
            fresh: true,
            after_ms: 0,
        }])
        .unwrap();
        assert_eq!(fresh["steps"][0]["action"]["fresh"], true);
        assert_eq!(
            request["steps"][0]["action"]["bundle"],
            "com.example.SampleApp"
        );

        let error = phone_steps_request(vec![PhoneStep::LaunchApp {
            bundle: Some("not a bundle".to_string()),
            app: None,
            fresh: false,
            after_ms: 0,
        }])
        .unwrap_err();
        assert!(error.contains("reverse-DNS"));
        assert!(error.contains("no action was sent"));
    }

    #[test]
    fn multi_step_launch_app_by_name_passes_the_name_to_the_daemon() {
        // Saved flows and phone_run_steps both deserialize this shape.
        let step: PhoneStep =
            serde_json::from_value(serde_json::json!({"kind":"launch_app","app":"微信"})).unwrap();
        let request = phone_steps_request(vec![step]).unwrap();
        assert_eq!(
            request["steps"][0]["action"],
            serde_json::json!({"type":"launch_app","app":"微信"})
        );
        // The old shape still parses.
        let old: PhoneStep = serde_json::from_value(
            serde_json::json!({"kind":"launch_app","bundle":"com.apple.Health"}),
        )
        .unwrap();
        assert!(phone_steps_request(vec![old]).is_ok());

        for bad in [
            serde_json::json!({"kind":"launch_app"}),
            serde_json::json!({"kind":"launch_app","bundle":"com.apple.Health","app":"健康"}),
            serde_json::json!({"kind":"launch_app","app":"  "}),
        ] {
            let step: PhoneStep = serde_json::from_value(bad.clone()).unwrap();
            let error = phone_steps_request(vec![step]).unwrap_err();
            assert!(error.contains("no action was sent"), "{bad}: {error}");
        }
    }

    #[test]
    fn multi_step_text_accepts_long_text_up_to_the_cap() {
        let long = |n: usize| PhoneStep::Type {
            text: "字".repeat(n),
            clear: false,
            after_ms: 0,
        };
        assert!(phone_steps_request(vec![long(MAX_TEXT_CHARS)]).is_ok());
        let error = phone_steps_request(vec![long(MAX_TEXT_CHARS + 1)]).unwrap_err();
        assert!(error.contains("20000"), "{error}");
    }

    #[test]
    fn long_text_requests_wait_for_the_daemon_to_finish_typing() {
        use crate::client::{actions_text_allowance, text_allowance};
        assert_eq!(text_allowance("short"), std::time::Duration::ZERO);
        assert_eq!(text_allowance(&"a".repeat(200)), std::time::Duration::ZERO);
        // Always longer than the daemon's own allowance for the same text.
        let n = 3000;
        let daemon = std::time::Duration::from_secs_f64(n as f64 / 60.0 * 1.5)
            + std::time::Duration::from_secs((n / 200) as u64);
        assert!(text_allowance(&"a".repeat(n)) > daemon);
        let body = serde_json::json!({"steps":[
            {"action":{"type":"text","text":"a".repeat(n)}},
            {"action":{"type":"tap","x":0.5,"y":0.5}},
            {"wait_for":{}}
        ]});
        assert_eq!(actions_text_allowance(&body), text_allowance(&"a".repeat(n)));
    }

    #[test]
    fn multi_step_request_rejects_invalid_waits_and_empty_locators_offline() {
        // The ceiling is 30s — sized for an element read that can cost six or
        // seven seconds on real hardware. One millisecond over is still
        // refused, and refused before anything is sent.
        let invalid_timeout = phone_steps_request(vec![PhoneStep::WaitFor {
            expect: PhoneUiExpectation {
                application: Some("设置".to_string()),
                present: vec![],
                absent: vec![],
            },
            timeout_ms: 30_001,
            poll_ms: 100,
        }])
        .unwrap_err();
        assert!(invalid_timeout.contains("timeout_ms"));
        assert!(invalid_timeout.contains("no action was sent"));

        // And the ceiling itself is accepted.
        phone_steps_request(vec![PhoneStep::WaitFor {
            expect: PhoneUiExpectation {
                application: Some("设置".to_string()),
                present: vec![],
                absent: vec![],
            },
            timeout_ms: 30_000,
            poll_ms: 100,
        }])
        .expect("the ceiling is a valid wait");

        let empty_locator = phone_steps_request(vec![PhoneStep::WaitFor {
            expect: PhoneUiExpectation {
                application: None,
                present: vec![PhoneElementLocator {
                    label: None,
                    identifier: None,
                    kind: None,
                    value: None,
                    focused: None,
                    enabled: None,
                    visible: None,
                }],
                absent: vec![],
            },
            timeout_ms: 1_000,
            poll_ms: 100,
        }])
        .unwrap_err();
        assert!(empty_locator.contains("empty element locator"));
    }

    #[test]
    fn multi_step_request_rejects_excessive_total_declared_wait_offline() {
        let steps = (0..7)
            .map(|_| PhoneStep::WaitFor {
                expect: PhoneUiExpectation {
                    application: Some("设置".to_string()),
                    present: vec![],
                    absent: vec![],
                },
                timeout_ms: 10_000,
                poll_ms: 100,
            })
            .collect();
        let error = phone_steps_request(steps).unwrap_err();
        assert!(error.contains("batch maximum of 60000ms"));
        assert!(error.contains("no action was sent"));
    }

    #[test]
    fn a_perform_step_reaches_the_daemon_as_a_perform_action() {
        let steps: Vec<PhoneStep> = serde_json::from_value(serde_json::json!([
            {"kind": "perform", "element": 7, "snapshot": "S1", "action": "toggle"},
            {"kind": "perform", "element": 3, "snapshot": "S1", "action": "adjust", "value": "0.5"}
        ]))
        .unwrap();
        let request = phone_steps_request(steps).unwrap();
        let actions: Vec<_> = request["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["action"].clone())
            .collect();
        assert_eq!(
            actions[0],
            serde_json::json!({"type": "perform", "element": 7, "snapshot": "S1", "action": "toggle"})
        );
        assert_eq!(actions[1]["value"], "0.5");

        for (bad, expect) in [
            (serde_json::json!({"kind": "perform", "element": 1, "snapshot": "S", "action": "explode"}), "is not one of"),
            (serde_json::json!({"kind": "perform", "element": 1, "snapshot": "S", "action": "adjust"}), "required with adjust"),
            (serde_json::json!({"kind": "perform", "element": 1, "snapshot": "S", "action": "toggle", "value": "1"}), "only with adjust"),
            (serde_json::json!({"kind": "perform", "element": 1, "snapshot": "", "action": "toggle"}), "snapshot token"),
        ] {
            let steps: Vec<PhoneStep> = serde_json::from_value(serde_json::json!([bad])).unwrap();
            let error = phone_steps_request(steps).unwrap_err();
            assert!(error.contains(expect) && error.contains("no action was sent"), "{error}");
        }
    }

    #[test]
    fn schema_noise_goes_but_meaning_and_property_names_stay() {
        let mut schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "TapParams",
            "type": "object",
            "required": ["x", "title"],
            "properties": {
                "x": {"type": "number", "format": "double", "description": "0–1"},
                "title": {"type": "string", "title": "Title"},
                "count": {"type": "integer", "format": "uint32", "minimum": 0},
                "pct": {"type": "integer", "minimum": 1, "maximum": 100}
            }
        });
        slim_schema(&mut schema);
        assert!(schema.get("$schema").is_none() && schema.get("title").is_none());
        let props = &schema["properties"];
        assert!(props.get("title").is_some(), "a property named title survives: {schema}");
        assert!(props["title"].get("title").is_none());
        assert!(props["x"].get("format").is_none());
        assert_eq!(props["x"]["description"], "0–1");
        assert!(props["count"].get("minimum").is_none());
        assert_eq!(props["pct"]["minimum"], 1, "a meaningful bound is kept");
        assert_eq!(schema["required"], serde_json::json!(["x", "title"]));
    }

    #[test]
    fn the_listed_tools_are_slimmed() {
        let tools = slim_tools(PhoneHandler::tool_router().list_all());
        let text = serde_json::to_string(&tools).unwrap();
        assert!(!text.contains("\"$schema\""), "no meta-schema URIs left");
        assert_eq!(tools.len(), PhoneHandler::tool_router().list_all().len());
    }

    #[test]
    fn tools_list_carries_the_mcp_apps_meta() {
        let tools = serde_json::to_value(listed_tools(&ToolProfile::All)).unwrap();
        let tools = tools.as_array().unwrap();
        let find = |name: &str| {
            tools
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("{name} listed"))
        };
        let uri = crate::screen::screen_uri();
        for name in ["phone_screen", "phone_status"] {
            let tool = find(name);
            assert_eq!(tool["_meta"]["ui"]["resourceUri"], uri, "{name}: {tool}");
            assert!(
                tool["_meta"]["ui"].get("visibility").is_none(),
                "{name} stays model-visible"
            );
        }
        assert_eq!(
            find("phone_screen")["_meta"]["openai/ui"]["entrypoints"][0]["type"],
            "thread"
        );
        let frame = find("phone_screen_frame");
        assert_eq!(
            frame["_meta"]["ui"]["visibility"],
            serde_json::json!(["app"])
        );
        assert!(frame["_meta"]["ui"].get("resourceUri").is_none());
        // Nothing else gains MCP Apps metadata.
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            if !["phone_screen", "phone_status", "phone_screen_frame"].contains(&name) {
                assert!(tool.get("_meta").is_none(), "{name} has no _meta: {tool}");
            }
        }
        assert_eq!(tools.len(), 37, "the whole documented surface");
    }

    #[test]
    fn a_panel_frame_skips_structured_shaping() {
        let status = serde_json::json!({"phone": "x", "drivable": true});
        let frame = crate::screen::frame_result(Ok(status), Ok(vec![7; 600]));
        let kept = finish_result("phone_screen_frame", frame.clone(), StructuredMode::Slim);
        let data = kept.structured_content.unwrap()["frame"]["data"].clone();
        assert!(
            data.as_str().unwrap().len() > SLIM_STRING_LIMIT,
            "base64 frame intact"
        );
        // Any other tool would have lost it.
        let shaped = finish_result("phone_screenshot", frame, StructuredMode::Slim);
        assert!(shaped.structured_content.unwrap().get("frame").is_none());
    }

    #[test]
    fn the_server_advertises_resources() {
        let info = serde_json::to_value(handler_for("http://127.0.0.1:9").get_info()).unwrap();
        assert!(info["capabilities"]["resources"].is_object(), "{info}");
        assert!(info["capabilities"]["tools"].is_object(), "{info}");
    }

    #[test]
    fn a_slim_structure_keeps_the_verdict_and_the_summary_only() {
        let mut result = CallToolResult::error(vec![Content::text("FAILED x · step 2 of 2")]);
        result.structured_content = Some(serde_json::json!({
            "ok": false,
            "retry_safe": false,
            "failed_step": 1,
            "error": "expectation_timeout",
            "steps": [{"ok": true}, {"ok": false}],
            "observation": {"tree": "…"},
            "blob": "x".repeat(SLIM_STRING_LIMIT + 1),
        }));
        let full = shape_structured(result.clone(), StructuredMode::Full);
        assert_eq!(full.structured_content, result.structured_content);
        let off = shape_structured(result.clone(), StructuredMode::Off);
        assert!(off.structured_content.is_none());
        let slim = shape_structured(result, StructuredMode::Slim)
            .structured_content
            .unwrap();
        assert_eq!(
            slim,
            serde_json::json!({
                "ok": false,
                "retry_safe": false,
                "failed_step": 1,
                "error": "expectation_timeout",
                "summary": "FAILED x · step 2 of 2",
            })
        );
    }

    #[test]
    fn a_result_whose_text_is_the_body_keeps_its_structure() {
        let mut result = CallToolResult::success(vec![Content::text(r#"{"ok":true,"a":{"b":1}}"#)]);
        result.structured_content = Some(serde_json::json!({"ok": true, "a": {"b": 1}}));
        let slim = shape_structured(result.clone(), StructuredMode::Slim);
        assert_eq!(slim.structured_content, result.structured_content);
    }

    // ---------------------------------------------------------------------
    // Device tools against a daemon that records the request it got.
    // ---------------------------------------------------------------------

    /// [`scripted_daemon`] that also hands back the whole request it read
    /// (request line, headers, body), so a test can check what was sent.
    fn recording_daemon(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (status, body) = (status.to_string(), body.to_string());
        let task = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(_) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => return String::new(),
                }
            };
            stream.set_nonblocking(false).ok();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .ok();
            let mut raw = Vec::new();
            let mut chunk = [0_u8; 8_192];
            loop {
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if rest.len() >= length {
                        break;
                    }
                }
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => raw.extend_from_slice(&chunk[..read]),
                }
            }
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            String::from_utf8_lossy(&raw).to_string()
        });
        (format!("http://{address}"), task)
    }

    /// The request line, the lowercased headers and the body of a recorded
    /// request.
    fn request_parts(raw: &str) -> (String, String, String) {
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
        let (line, headers) = head.split_once("\r\n").unwrap_or((head, ""));
        (line.to_string(), headers.to_ascii_lowercase(), body.to_string())
    }

    /// A tool call against a recording daemon: the result and what was sent.
    fn device_call<F, Fut>(
        status: &str,
        body: &str,
        call: F,
    ) -> (CallToolResult, String, String, String)
    where
        F: FnOnce(PhoneHandler) -> Fut,
        Fut: std::future::Future<Output = CallToolResult>,
    {
        let (url, daemon) = recording_daemon(status, body);
        let result = block(call(handler_for(&url)));
        let (line, headers, sent) = request_parts(&daemon.join().unwrap());
        (result, line, headers, sent)
    }

    /// Argument mistakes are refused here: nothing reaches the daemon (the
    /// address below has no listener, so a request would fail differently).
    fn refused_offline(result: CallToolResult) -> String {
        assert_eq!(result.is_error, Some(true));
        let text = text_of(&result);
        assert!(text.contains("Nothing was sent") || text.contains("nothing was sent"), "{text}");
        text
    }

    #[test]
    fn phone_logs_reads_with_filters_and_controls_the_capture() {
        let (result, line, _, _) = device_call(
            "200 OK",
            r#"{"ok":true,"capturing":true,"next":43,"items":[{"seq":42,"ts":"07:01:02","process":"WeChat","pid":311,"level":"error","message":"net timeout"}]}"#,
            |h| async move {
                h.phone_logs(Parameters(LogsParams {
                    process: Some("WeChat".into()),
                    level: Some("error".into()),
                    since: Some(40),
                    limit: Some(5),
                    ..Default::default()
                }))
                .await
            },
        );
        assert_eq!(line, "GET /agent/logs?since=40&process=WeChat&level=error&limit=5 HTTP/1.1");
        assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
        let text = text_of(&result);
        assert!(text.starts_with("1 log lines · next 43"), "{text}");
        assert!(text.contains("#42 07:01:02 WeChat[311] error: net timeout"), "{text}");
        assert_eq!(result.structured_content.as_ref().unwrap()["next"], 43);

        let (result, line, headers, body) = device_call(
            "200 OK",
            r#"{"ok":true,"capturing":true}"#,
            |h| async move {
                h.phone_logs(Parameters(LogsParams {
                    action: Some("start".into()),
                    ..Default::default()
                }))
                .await
            },
        );
        assert_eq!(line, "POST /agent/logs HTTP/1.1");
        assert!(headers.contains("x-phone-control: 1"), "{headers}");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(), serde_json::json!({"action": "start"}));
        assert!(text_of(&result).contains("capturing device logs"), "{}", text_of(&result));

        refused_offline(block(handler_for("http://127.0.0.1:9").phone_logs(Parameters(LogsParams {
            action: Some("stop".into()),
            contains: Some("x".into()),
            ..Default::default()
        }))));
    }

    #[test]
    fn phone_crashes_lists_and_reads_one_report() {
        let (result, line, headers, _) = device_call(
            "200 OK",
            r#"{"ok":true,"items":[{"name":"WeChat-2026-10-11-070102.ips","process":"WeChat","time":1760166062,"stamp":"2026-10-11 07:01:02","size":5120}]}"#,
            |h| async move {
                h.phone_crashes(Parameters(CrashesParams {
                    process: Some("WeChat".into()),
                    ..Default::default()
                }))
                .await
            },
        );
        assert_eq!(line, "GET /agent/crashes?process=WeChat HTTP/1.1");
        assert!(!headers.contains("x-phone-control"), "a read takes no control: {headers}");
        let text = text_of(&result);
        assert!(text.contains("WeChat-2026-10-11-070102.ips · WeChat · 2026-10-11 07:01:02 · 5120 B"), "{text}");
        assert_eq!(result.structured_content.as_ref().unwrap()["items"][0]["size"], 5120);

        let (result, line, _, _) = device_call(
            "200 OK",
            r#"{"ok":true,"name":"WeChat-2026-10-11-070102.ips","text":"Exception Type: EXC_CRASH (SIGABRT)","size":35,"truncated":false}"#,
            |h| async move {
                h.phone_crashes(Parameters(CrashesParams {
                    name: Some("WeChat-2026-10-11-070102.ips".into()),
                    ..Default::default()
                }))
                .await
            },
        );
        assert_eq!(line, "GET /agent/crashes/WeChat-2026-10-11-070102.ips HTTP/1.1");
        let text = text_of(&result);
        assert!(text.starts_with("crash report WeChat-2026-10-11-070102.ips · 35 bytes"), "{text}");
        assert!(text.contains("EXC_CRASH (SIGABRT)"), "{text}");

        // The daemon's refusal of a bad name keeps its hint.
        let (result, _, _, _) = device_call(
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid_name","hint":"pass a name from the list"}"#,
            |h| async move {
                h.phone_crashes(Parameters(CrashesParams {
                    name: Some("nope".into()),
                    ..Default::default()
                }))
                .await
            },
        );
        assert_eq!(result.is_error, Some(true));
        assert!(text_of(&result).contains("pass a name from the list"), "{}", text_of(&result));
    }

    #[test]
    fn phone_set_location_sends_a_point_or_a_clear() {
        let (result, line, headers, body) = device_call("200 OK", r#"{"ok":true}"#, |h| async move {
            h.phone_set_location(Parameters(SetLocationParams {
                lat: Some(31.2304),
                lon: Some(121.4737),
                clear: None,
            }))
            .await
        });
        assert_eq!(line, "POST /agent/location HTTP/1.1");
        assert!(headers.contains("x-phone-control: 1"), "{headers}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"lat": 31.2304, "lon": 121.4737})
        );
        assert_ne!(result.is_error, Some(true));
        assert!(text_of(&result).contains("set to 31.2304, 121.4737"), "{}", text_of(&result));

        let (result, _, _, body) = device_call("200 OK", r#"{"ok":true}"#, |h| async move {
            h.phone_set_location(Parameters(SetLocationParams {
                clear: Some(true),
                ..Default::default()
            }))
            .await
        });
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(), serde_json::json!({"clear": true}));
        assert!(text_of(&result).contains("cleared"), "{}", text_of(&result));

        refused_offline(block(handler_for("http://127.0.0.1:9").phone_set_location(Parameters(
            SetLocationParams { lat: Some(95.0), lon: Some(0.0), clear: None },
        ))));
    }

    #[test]
    fn phone_record_starts_stops_and_reports_status() {
        let (result, line, _, body) = device_call(
            "200 OK",
            r#"{"ok":true,"recording":{"path":"/s/runs/recordings/a.mp4","seconds":0.0,"frames":0,"bytes":0}}"#,
            |h| async move { h.phone_record(Parameters(RecordParams { action: "start".into() })).await },
        );
        assert_eq!(line, "POST /agent/record HTTP/1.1");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(), serde_json::json!({"action": "start"}));
        assert!(text_of(&result).starts_with("recording /s/runs/recordings/a.mp4"), "{}", text_of(&result));

        let (result, _, _, body) = device_call(
            "200 OK",
            r#"{"ok":true,"summary":{"path":"/s/a.mp4","seconds":12.0,"frames":360,"bytes":900000,"width":1170,"height":2532}}"#,
            |h| async move { h.phone_record(Parameters(RecordParams { action: "stop".into() })).await },
        );
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(), serde_json::json!({"action": "stop"}));
        assert_eq!(
            text_of(&result),
            "saved recording /s/a.mp4 · 12.0 s · 360 frames · 900000 bytes · 1170x2532"
        );
        assert_eq!(result.structured_content.as_ref().unwrap()["summary"]["frames"], 360);

        let (result, line, _, _) = device_call(
            "200 OK",
            r#"{"ok":true,"recording":null}"#,
            |h| async move { h.phone_record(Parameters(RecordParams { action: "status".into() })).await },
        );
        assert_eq!(line, "GET /agent/record HTTP/1.1");
        assert_eq!(text_of(&result), "not recording");
    }

    #[test]
    fn phone_install_app_sends_an_install_action() {
        let (result, line, headers, body) = device_call(
            "200 OK",
            r#"{"ok":true,"bundle":"com.example.app","installed":true}"#,
            |h| async move {
                h.phone_install_app(Parameters(InstallAppParams {
                    url: Some("https://example.com/build/App.ipa".into()),
                    fresh: Some(true),
                    ..Default::default()
                }))
                .await
            },
        );
        assert_eq!(line, "POST /agent/input HTTP/1.1");
        assert!(headers.contains("x-phone-control: 1"), "{headers}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"type": "install", "url": "https://example.com/build/App.ipa", "fresh": true})
        );
        assert!(text_of(&result).starts_with("installed com.example.app"), "{}", text_of(&result));
        assert_eq!(result.structured_content.as_ref().unwrap()["installed"], true);

        let (result, _, _, _) = device_call(
            "403 Forbidden",
            r#"{"ok":false,"error":"app_blocked","hint":"com.example.app is in IPHONE_USE_BLOCKED_APPS"}"#,
            |h| async move {
                h.phone_install_app(Parameters(InstallAppParams {
                    path: Some("/Users/me/App.ipa".into()),
                    ..Default::default()
                }))
                .await
            },
        );
        assert_eq!(result.is_error, Some(true));
        assert!(text_of(&result).contains("IPHONE_USE_BLOCKED_APPS"), "{}", text_of(&result));

        refused_offline(block(handler_for("http://127.0.0.1:9").phone_install_app(Parameters(
            InstallAppParams { url: Some("http://example.com/a.ipa".into()), ..Default::default() },
        ))));
    }

    #[test]
    fn phone_app_notes_reads_and_replaces() {
        let (result, line, headers, _) = device_call(
            "200 OK",
            r#"{"ok":true,"bundle":"com.tencent.xin","text":"- 发现 tab: bottom bar, 3rd","updated_at":"2026-10-11T07:00:00Z"}"#,
            |h| async move {
                h.phone_app_notes(Parameters(AppNotesParams {
                    bundle: "com.tencent.xin".into(),
                    text: None,
                }))
                .await
            },
        );
        assert_eq!(line, "GET /agent/app-notes?bundle=com.tencent.xin HTTP/1.1");
        assert!(!headers.contains("x-phone-control"), "{headers}");
        let text = text_of(&result);
        assert!(text.starts_with("notes for com.tencent.xin (updated 2026-10-11T07:00:00Z):"), "{text}");
        assert!(text.contains("发现 tab"), "{text}");

        let notes = "# WeChat\n- 发现 is the 3rd tab";
        let (result, line, headers, body) = device_call("200 OK", r#"{"ok":true}"#, |h| async move {
            h.phone_app_notes(Parameters(AppNotesParams {
                bundle: "com.tencent.xin".into(),
                text: Some(notes.into()),
            }))
            .await
        });
        assert_eq!(line, "PUT /agent/app-notes?bundle=com.tencent.xin HTTP/1.1");
        assert!(headers.contains("content-type: text/markdown"), "{headers}");
        assert!(headers.contains("x-phone-control: 1"), "{headers}");
        assert_eq!(body, notes);
        assert_eq!(
            text_of(&result),
            format!("saved {} bytes of notes for com.tencent.xin", notes.len())
        );

        let long = block(handler_for("http://127.0.0.1:9").phone_app_notes(Parameters(
            AppNotesParams {
                bundle: "com.tencent.xin".into(),
                text: Some("x".repeat(crate::device::NOTES_MAX_BYTES + 1)),
            },
        )));
        assert_eq!(long.is_error, Some(true));
        assert!(text_of(&long).contains("Nothing was saved"), "{}", text_of(&long));
        refused_offline(block(handler_for("http://127.0.0.1:9").phone_app_notes(Parameters(
            AppNotesParams { bundle: "微信".into(), text: None },
        ))));
    }

    #[test]
    fn phone_screenshot_asks_for_the_overlay() {
        let (result, line, _, _) = device_call("200 OK", "PNGDATA", |h| async move {
            h.phone_screenshot(Parameters(ScreenshotParams {
                max_side: None,
                overlay: Some(true),
            }))
            .await
        });
        assert_eq!(line, "GET /agent/screenshot?max_side=1200&overlay=elements HTTP/1.1");
        assert_ne!(result.is_error, Some(true));
        assert!(result.content[0].as_image().is_some());
    }

    #[test]
    fn phone_elements_asks_for_ocr_and_shows_its_rows() {
        let (result, line, _, _) = device_call(
            "200 OK",
            r#"{"snapshot":"s1","elements":[{"kind":"Application","label":"Game","rect":[0,0,400,800],"depth":0}],
                "ocr":{"source":"vision","rows":[{"kind":"OCRText","label":"Start","rect":[100,300,200,40],"depth":0,"source":"ocr","confidence":0.97}]}}"#,
            |h| async move {
                h.phone_elements(Parameters(ElementsParams {
                    ocr: Some("force".into()),
                    ..Default::default()
                }))
                .await
            },
        );
        assert_eq!(line, "GET /agent/elements?image=auto&ocr=force HTTP/1.1");
        let text = text_of(&result);
        assert!(text.contains("\"Start\" x=0.500 y=0.400 (0.97)"), "{text}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["ocr"]["rows"][0]["label"],
            "Start"
        );

        let (_, line, _, _) = device_call("200 OK", r#"{"snapshot":"s","elements":[]}"#, |h| async move {
            h.phone_elements(Parameters(ElementsParams {
                ocr: Some("auto".into()),
                ..Default::default()
            }))
            .await
        });
        assert_eq!(line, "GET /agent/elements?image=auto&ocr=1 HTTP/1.1");
        let bad = block(handler_for("http://127.0.0.1:9").phone_elements(Parameters(ElementsParams {
            ocr: Some("always".into()),
            ..Default::default()
        })));
        assert_eq!(bad.is_error, Some(true));
    }

    /// The new tools follow the profile like every other: readonly keeps
    /// phone_crashes and hides the rest; an allowlist can name them.
    #[test]
    fn profiles_cover_the_device_tools() {
        let readonly = listed_names(&ToolProfile::ReadOnly);
        assert!(readonly.contains(&"phone_crashes".to_string()));
        for name in [
            "phone_logs",
            "phone_set_location",
            "phone_record",
            "phone_install_app",
            "phone_app_notes",
        ] {
            assert!(!readonly.contains(&name.to_string()), "{name}");
            assert!(ToolProfile::ReadOnly.refusal(name).is_some(), "{name}");
        }
        let (allow, warnings) = ToolProfile::parse(Some("phone_logs,phone_app_notes"));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(listed_names(&allow), ["phone_app_notes", "phone_logs"]);
    }
}
