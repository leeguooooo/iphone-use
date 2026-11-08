//! MCP `ServerHandler` implementation — `PhoneHandler`.
//!
//! Each MCP tool maps onto a deliberately supported subset of the daemon's
//! agent API. Calls may select an installed daemon instance; target configuration and destructive maintenance stay outside
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

/// Parameters for [`PhoneHandler::phone_metrics`].
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricsParams {
    /// Latest runs per section (open/recent), default 5, maximum 20.
    /// A shared 12 KiB row budget can reduce the number returned further.
    #[serde(default)]
    #[schemars(range(min = 1, max = 20))]
    pub limit: Option<usize>,
}

/// Parameters for [`phone_tap`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TapParams {
    /// Horizontal position, normalized 0–1 (0 = left edge, 1 = right edge).
    #[schemars(range(min = 0.0, max = 1.0))]
    pub x: f64,
    /// Vertical position, normalized 0–1 (0 = top edge, 1 = bottom edge).
    #[schemars(range(min = 0.0, max = 1.0))]
    pub y: f64,
    /// Default true: reply with the settled screen (delta since your last
    /// look, or the full tree); false = bare action, fastest.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_scroll`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScrollParams {
    /// Horizontal anchor position, normalized 0–1.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub x: f64,
    /// Vertical anchor position, normalized 0–1.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub y: f64,
    /// Horizontal scroll delta. Positive reveals content to the right.
    pub dx: f64,
    /// Vertical scroll delta. **Positive dy reveals content farther down**;
    /// negative dy reveals content above. ~80 ≈ 15% of a screen, ~400 ≈ 75%.
    pub dy: f64,
    /// Default true: reply with the settled screen (delta since your last
    /// look, or the full tree); false = bare action, fastest.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_type`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TypeParams {
    /// Unicode text to start typing, up to 20000 scalars. Omit when continuing.
    #[schemars(length(max = 20000))]
    pub text: Option<String>,
    /// Single-use token from a safe chunk boundary. Pass without text, clear or budget_ms.
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub continue_token: Option<String>,
    /// Optional typing budget, 6000..15000 ms, requiring a named owner and inputContinuation.
    #[schemars(range(min = 6000, max = 15000))]
    pub budget_ms: Option<u64>,
    /// Clear the bound field first. Never combine with continue_token.
    pub clear: Option<bool>,
    /// Default true: reply with the settled screen (delta since your last
    /// look, or the full tree); false = bare action, fastest.
    pub observe: Option<bool>,
}

impl TypeParams {
    fn action(&self) -> Result<serde_json::Value, &'static str> {
        match (&self.text, &self.continue_token) {
            (Some(text), None) if text.chars().count() <= MAX_TEXT_CHARS
                && self.budget_ms.is_none_or(|ms| (6000..=15000).contains(&ms)) => {
                let mut action = serde_json::json!({"type":"text", "text":text});
                if let Some(clear) = self.clear { action["clear"] = clear.into(); }
                if let Some(budget) = self.budget_ms { action["budget_ms"] = budget.into(); }
                Ok(action)
            }
            (None, Some(token)) if self.clear.is_none() && self.budget_ms.is_none()
                && token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) =>
                Ok(serde_json::json!({"type":"text","continue_token":token})),
            _ => Err("Provide text up to 20000 scalars with optional clear/budget_ms (6000..15000), or continue_token alone. Nothing was sent."),
        }
    }
}

/// Select one zero-based home page, or omit to read every page and folder.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HomeLayoutParams {
    pub page: Option<usize>,
    /// Omit to read. `backup` saves the current layout on the Mac; `preview`
    /// shows what writing `file` would change, by app identifier; `apply`
    /// writes `file`; `restore` writes the newest automatic backup (or `file`).
    #[serde(default)]
    pub action: Option<String>,
    /// Absolute path of a layout .plist on the Mac (from a backup).
    #[serde(default)]
    pub file: Option<String>,
    /// apply/restore only: true writes. Without it the preview comes back and
    /// nothing changes; show it to the person and ask first.
    #[serde(default)]
    pub confirm: bool,
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
    /// App Store storefront for the store search, two letters. Default: the
    /// store the phone's apps came from (or IPHONE_USE_STORE_COUNTRY, else the
    /// Region setting); `cn` only when none is known, with a warning. The
    /// answer's `country_source` says which.
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
    /// Show off-screen/not-drawn rows individually in the text, with their
    /// original indexes and hidden state, instead of a five-label summary.
    /// The 200-row / 32 KiB text budget still applies. Scroll them into view
    /// before acting. Default false.
    #[serde(default)]
    pub include_hidden: bool,
    /// Keep only rows whose label, identifier, value or placeholder contains
    /// this text (case-insensitive). Rows keep their original index.
    #[serde(default)]
    pub query: Option<String>,
    /// Literal case-sensitive field filters combined with AND; preserves original snapshot indexes.
    /// Requires literal criteria; optional index0..199 chooses one match in scoped/query tree order before limit.
    /// Row index remains the full snapshot index; incomplete or unknown earlier matches refuse an unproven rank.
    /// Filters AX elements, not separate OCR text.
    #[serde(default)]
    pub selector: Option<iu_core::element_selector::ElementSelector>,
    /// Return at most this many rows; the answer says how many were left out.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Text read from a fresh screenshot (Vision OCR) for screens the tree
    /// cannot describe (games, canvases, video): `auto` adds it only when the
    /// tree has no interactive rows, `force` always, `off` (default) never.
    #[serde(default)]
    pub ocr: Option<String>,
}

/// One snapshot-selected native subtree, read without a global tree walk.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubtreeParams {
    pub snapshot: String,
    pub element: usize,
    /// Relative depth cap (1..64), default 8.
    pub max_depth: Option<u32>,
    /// Node cap (1..5000), default 500.
    pub max_nodes: Option<u32>,
}

/// Parameters for [`phone_tap_label`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TapLabelParams {
    /// The element's visible accessibility label, exactly as shown by
    /// `phone_elements` (e.g. "新备忘录", "Connect").
    pub label: String,
    /// Default true: reply with the settled screen (delta since your last
    /// look, or the full tree); false = bare action, fastest.
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
    /// Default true: reply with the settled screen (delta since your last
    /// look, or the full tree); false = bare action, fastest.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// `phone_screenshot`'s default size: legible, and ~40 % fewer image tokens
/// than a full-resolution capture.
const DEFAULT_SCREENSHOT_MAX_SIDE: u32 = 1200;

/// Longest sides `phone_screenshot` steps down through, largest first, until
/// the base64 image fits [`SCREENSHOT_BUDGET_BYTES`] (the daemon's element
/// read does the same, `scope::IMAGE_SIDES`).
const SCREENSHOT_SIDES: [u32; 3] = [1200, 800, 500];

/// Longest base64 image one `phone_screenshot` reply carries: clients hand
/// the model a result of this size whole.
const SCREENSHOT_BUDGET_BYTES: usize = 1_500_000;

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

/// Select exactly one current field without focusing it or changing the app.
#[derive(Debug,Default,Deserialize,Serialize,JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadValueParams {
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub identifier:Option<String>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub label:Option<String>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub kind:Option<String>,
    /// Maximum UTF-16 units returned (1–65536, default 8192); emoji may use two.
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub max_utf16:Option<u32>,
}

/// Parameters for [`phone_key`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyParams {
    /// Supported names: `return`/`enter`, `escape`, `space`, `tab`,
    /// `delete`/`backspace`, `up`, `down`, `left`, `right`.
    pub name: String,
    /// Default true: reply with the settled screen (delta since your last
    /// look, or the full tree); false = bare action, fastest.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_shortcut`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShortcutParams {
    /// Home, Spotlight, Photos capture (`save_screenshot`), or physical-only `volume_up` / `volume_down`.
    /// App Switcher is unsupported by the device runner.
    pub name: String,
    /// Default true: reply with the settled screen (delta since your last
    /// look, or the full tree); false = bare action, fastest.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// Parameters for [`phone_run_steps`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunStepsParams {
    /// Ordered steps (max 24); validated as a whole first, stops at the
    /// first failure.
    pub steps: Vec<PhoneStep>,
    /// Default true: reply with the screen the batch ended on (`snapshot`,
    /// `elements`, `alert`); false when you will not look at it.
    #[serde(default)]
    pub observe: Option<bool>,
}

/// One stop in a continuous drag. Dwell keeps the same finger down.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhoneDragPoint {
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub dwell_ms: u64,
}

/// One independently placed finger in a simultaneous hold.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhoneHoldPoint {
    pub x: f64,
    pub y: f64,
}

/// One step in a bounded multi-step sequence.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PhoneStep {
    /// Tap normalized coordinates; prefer `tap_locator` when the target has
    /// a label or identifier.
    Tap {
        x: f64,
        y: f64,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Long-press normalized coordinates (duration_ms default 600).
    Longpress {
        x: f64,
        y: f64,
        #[serde(default = "default_phone_longpress_ms")]
        duration_ms: u64,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Hold 2–5 normalized points at once (duration_ms default 1000).
    MultiLongpress {
        points: Vec<PhoneHoldPoint>,
        #[serde(default = "default_phone_multi_longpress_ms")]
        duration_ms: u64,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Swipe from (x1,y1) to (x2,y2), normalized; duration_ms default 300.
    Swipe {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        #[serde(default = "default_phone_swipe_ms")]
        duration_ms: u64,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Hold hold_ms (500), then drag: x1/y1→x2/y2, or 2–16 `points` with a
    /// dwell_ms each; duration_ms (300) is travel per segment; total ≤ 10 s.
    Drag {
        x1: Option<f64>,
        y1: Option<f64>,
        x2: Option<f64>,
        y2: Option<f64>,
        #[serde(default)]
        points: Option<Vec<PhoneDragPoint>>,
        #[serde(default = "default_phone_drag_hold_ms")]
        hold_ms: u64,
        #[serde(default = "default_phone_swipe_ms")]
        duration_ms: u64,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Tap one exact, unique accessibility label.
    TapLabel {
        label: String,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Tap the single element matching every locator field; zero or several
    /// matches send nothing.
    TapLocator {
        locator: PhoneElementLocator,
        /// `point` taps the live frame's centre instead of XCUIElement's
        /// click (custom controls that ACK and do nothing); default `element`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<String>,
        /// Pass instead of failing when nothing matches (screens that appear
        /// only sometimes: "Not now", "Skip").
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        if_present: bool,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Type Unicode text into the focused field; clear=true empties it first.
    Type {
        text: String,
        #[serde(default)]
        clear: bool,
        /// Typing budget, 6000..15000 ms.
        #[schemars(range(min = 6000, max = 15000))]
        budget_ms: Option<u64>,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Continue the unsent suffix of a type step with its single-use token.
    TypeContinue {
        #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
        continue_token: String,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Send one device key (return, escape, space, tab, delete, up, down,
    /// left, right).
    Key {
        name: String,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// `home` or `spotlight`.
    Shortcut {
        name: String,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Scroll as `phone_scroll`: normalized anchor x,y; deltas dx,dy in
    /// points, positive dy reveals content below.
    Scroll {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Launch or foreground an app: exactly one of `bundle` (bundle id) or
    /// `app` (name, resolved as phone_apps does; ambiguous or unknown names
    /// are refused with candidates, nothing sent).
    LaunchApp {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bundle: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<String>,
        /// Quit first so it opens on its first screen (where recorded flows
        /// start).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        fresh: bool,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Open a link with the app iOS assigns it. http/https always; other
    /// schemes only if IPHONE_USE_ALLOW_URL_SCHEMES allows them. Leaves the
    /// current app: guard the next screen with `wait_for`.
    OpenUrl {
        url: String,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Navigate back inside the current app.
    Back {
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Press a system-alert button natively — the only path that acts on one
    /// (taps on alert buttons ACK without effect). Exact `button` text, or
    /// `action: accept|dismiss`. Fails when no alert is showing.
    Alert {
        #[serde(default)]
        button: Option<String>,
        #[serde(default)]
        action: Option<String>,
        /// Pass instead of failing when no alert is up (prompts that appear
        /// only sometimes: "Allow Paste").
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        if_present: bool,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Select a value in a native picker wheel.
    Picker {
        #[serde(default)]
        column: usize,
        value: String,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Invoke an affordance (toggle, increment, decrement, adjust, …; see
    /// phone_capabilities perform_actions) on `element`+`snapshot` from
    /// phone_elements; not for saved flows. `value` only with adjust;
    /// `duration_ms` only with menu/force_press. Verify with wait_for.
    Perform {
        element: u64,
        snapshot: String,
        action: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        duration_ms: Option<u64>,
        /// Pause after this step, ms.
        #[serde(default)]
        after_ms: u64,
    },
    /// Poll until `expect` holds (timeout_ms default 15000, poll_ms 250);
    /// timeout_ms:0 is one 500 ms observation without polling.
    WaitFor {
        expect: PhoneUiExpectation,
        #[serde(default = "default_phone_wait_ms")]
        timeout_ms: u64,
        #[serde(default = "default_phone_poll_ms")]
        poll_ms: u64,
    },
    /// Fixed pause; prefer `wait_for`.
    Pause { ms: u64 },
}

type PhoneWaitSelector = iu_core::wait_selector::WaitSelector<PhoneElementLocator>;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhoneUiExpectation {
    /// Exact foreground Application label, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    /// Every locator in this list must match at least one current element.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub present: Vec<PhoneWaitSelector>,
    /// Every locator in this list must match no current element.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absent: Vec<PhoneWaitSelector>,
    /// Text that must be visible, read by OCR — for canvas screens (games,
    /// Flutter/Unity) with no labels. Case and spaces ignored; not combinable
    /// with application/present/absent; timeout_ms may reach 60000.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ocr_present: Vec<String>,
    /// Text that must not be visible (a loading caption), read by OCR.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ocr_absent: Vec<String>,
    /// Each absent target must first appear, then disappear (loaders that
    /// start after the preceding action returns).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub absent_after_present: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PhoneElementLocator {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Case-sensitive literal substring of the normalized label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 1000))]
    pub label_contains: Option<String>,
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

fn default_phone_multi_longpress_ms() -> u64 {
    1000
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
    instances: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, PhoneHandler>>>,
    /// When this session last saw the phone usable (a status that said so,
    /// or a phone tool that succeeded). Within [`RECONNECT_CHECK_FRESH`] the
    /// on-use reconnect skips its status read: the phone was driven too
    /// recently for the idle release, and this session's own task end
    /// clears it.
    usable_seen: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
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

/// Parameters for [`PhoneHandler::phone_reconnect`].
#[derive(Debug, Default, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReconnectParams {
    /// Wait up to this many seconds (1–300) for the outcome instead of
    /// returning at once: the call returns when the phone is drivable, a
    /// blocker appears (setup_blocked_on, e.g. the passcode), or the time is
    /// up — with the compact final state (a blocker other than the passcode
    /// must hold for 20 s first). A runner rebuild after an upgrade
    /// takes 1–2 min, so 180 covers it. Omit to return immediately.
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

/// Longest `phone_reconnect` may wait.
const RECONNECT_MAX_WAIT_SECS: u64 = 300;
/// How often a waiting `phone_reconnect` reads the status.
const RECONNECT_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// What a waiting reconnect reports: the fields that decide the next step.
fn reconnect_brief(status: &crate::types::StatusResponse) -> serde_json::Value {
    serde_json::json!({
        "drivable": status.drivable,
        "reconnecting": status.reconnecting,
        "released": status.released,
        "runner": status.runner,
        "locked": status.locked,
        "owner": status.owner,
        "setup_phase": status.setup_phase,
        "setup_message": status.setup_message,
        "setup_blocked_on": status.setup_blocked_on,
        "setup_retry": status.extra.get("setup_retry"),
        "hint": status.hint,
    })
}

/// Whether a waiting reconnect is finished, and why.
fn reconnect_wait_outcome(status: &crate::types::StatusResponse) -> Option<&'static str> {
    if ["ended_on_phone", "runner_parked", "human_handoff"].iter()
        .any(|key| status.extra.get(*key).and_then(serde_json::Value::as_bool) == Some(true)) {
        return Some("stopped");
    }

    if status
        .setup_blocked_on
        .as_deref()
        .is_some_and(|blocker| !blocker.trim().is_empty())
    {
        return Some("blocked");
    }
    (status.drivable == Some(true) && status.reconnecting != Some(true)).then_some("drivable")
}

/// How long a blocker other than the passcode must persist, while the daemon
/// is still reconnecting, before a waiting reconnect reports it. Early phases
/// report transient ones: on a Wi-Fi phone `ddi-wait` says `usb` for a moment
/// before the tunnel comes up and the runner build starts.
const RECONNECT_BLOCKER_SETTLE: std::time::Duration = std::time::Duration::from_secs(20);

/// A scheduled retry further out than this ends the wait as `blocked`.
const RECONNECT_RETRY_SOON_SECS: u64 = 60;

/// The supervisor's scheduled automatic retry after a failed setup round, as
/// (attempt, seconds until it runs) from the daemon's `setup_retry`.
fn reconnect_setup_retry(status: &crate::types::StatusResponse) -> Option<(u64, u64)> {
    let retry = status.extra.get("setup_retry")?;
    Some((retry.get("attempt")?.as_u64()?, retry.get("next_in_secs")?.as_u64()?))
}

/// Tools that need the phone itself. Before one runs on a phone released
/// after the last task, the MCP server brings it back (the AI really needs it
/// now) instead of returning the daemon's `released` refusal for the model to
/// work around. Status, inventory, flow files, holds, metrics, logs, the
/// person's live panel and the device-service calls that need no runner
/// (home layout, apps, install, location) never wake a phone.
const RECONNECT_ON_USE_TOOLS: &[&str] = &[
    "phone_screenshot", "phone_tap", "phone_scroll", "phone_type", "phone_key", "phone_shortcut",
    "phone_run_steps", "phone_read_value", "phone_subtree", "phone_elements", "phone_tap_element",
    "phone_tap_label", "phone_collect_list", "phone_scroll_find", "phone_login", "phone_flow_run",
    "phone_jev_run", "phone_record",
];

/// How long a tool call waits for the phone it brings back. A cached runner
/// starts in ~13 s; a rebuild after an upgrade takes 1–2 min.
const RECONNECT_ON_USE_BUDGET: std::time::Duration = std::time::Duration::from_secs(150);

/// See [`PhoneHandler::usable_seen`]. Well under the idle window (300 s).
const RECONNECT_CHECK_FRESH: std::time::Duration = std::time::Duration::from_secs(60);

fn reconnect_on_use(tool: &str, args: Option<&serde_json::Map<String, serde_json::Value>>) -> bool {
    let arg = |key: &str| args.and_then(|args| args.get(key)).and_then(serde_json::Value::as_str);
    let needs_phone = match tool {
        // Only a start needs the runner; status and stop read the recorder.
        "phone_record" => arg("action") == Some("start"),
        // A bad id is refused before anything reaches the phone; do not
        // make the caller wait for (or type a passcode for) a reconnect first.
        "phone_flow_run" => arg("id").is_some_and(|id| {
            id.ends_with(".json") || crate::registry::valid_flow_id(id)
        }),
        _ => RECONNECT_ON_USE_TOOLS.contains(&tool),
    };
    needs_phone && std::env::var("IPHONE_USE_MCP_RECONNECT_ON_USE").map_or(true, |v| v.trim() != "0")
}

/// Whether a tool result is the daemon refusing because the phone is
/// released or coming back: the next call must check status again.
fn refused_as_released(result: &CallToolResult) -> bool {
    result.content.iter().filter_map(|c| c.as_text()).any(|t| {
        ["\"released\"", "\"reconnect_in_progress\"", "\"reconnecting\"", "\"releasing\""]
            .iter()
            .any(|needle| t.text.contains(&format!("\"error\":{needle}")))
    })
}

/// Whether this status is a phone the MCP server may bring back for a tool
/// call: released (or already coming back) by this daemon, with nothing a
/// person decided in the way — a hand-off, a session ended on the phone, a
/// parked runner, a blocker, another session's lease.
fn may_reconnect_for_use(status: &crate::types::StatusResponse, me: &str) -> bool {
    let flag = |key: &str| status.extra.get(key).and_then(serde_json::Value::as_bool) == Some(true);
    status.drivable != Some(true)
        && (status.released == Some(true) || status.reconnecting == Some(true))
        && status.recovery_owner.as_deref() == Some("daemon")
        && !flag("human_handoff")
        && !flag("ended_on_phone")
        && !flag("runner_parked")
        && status.releasing != Some(true)
        // A private hold refuses screen reads anyway; do not start the runner for one.
        && status.extra.get("screen_private_secs").and_then(serde_json::Value::as_u64).unwrap_or(0) == 0
        && status.setup_blocked_on.as_deref().is_none_or(|b| b.trim().is_empty())
        && status.owner.as_deref().is_none_or(|owner| owner == me)
}

/// The tool result when the phone did not come back in time: what happened
/// and what to do, instead of running the tool against a released phone.
fn reconnect_on_use_stopped(tool: &str, wait: &ReconnectWait) -> CallToolResult {
    let blocker = wait
        .status
        .as_ref()
        .and_then(|s| s.setup_blocked_on.clone())
        .unwrap_or_default();
    let advice = if blocker.starts_with("needs_passcode") {
        "starting the device runner may need the passcode on the iPhone to allow automation. If the phone shows that prompt, ask the person to enter it there (never ask for the passcode itself), then call this tool again; if no prompt is showing, check phone_status (hint, setup_message) and the phone's connection instead."
    } else {
        match wait.outcome {
            "retrying" => "the daemon is retrying on its own; call this tool again after wait.status.setup_retry.next_in_secs before asking a person to act.",
            "blocked" => "a person has to clear wait.status.setup_blocked_on (see wait.status.hint) before the phone can be used; tell them, then call this tool again.",
            "stopped" => "the phone was handed to a person or the session was ended on the phone; ask before taking it back.",
            _ => "the phone is still coming back; call phone_status, and call this tool again once drivable=true.",
        }
    };
    let text = serde_json::json!({
        "ok": false,
        "error": "phone_not_ready",
        "outcome": "not_sent",
        "retry_safe": true,
        "tool": tool,
        "note": "the phone was released after the last task; this call started bringing it back and nothing was sent to it",
        "next": advice,
        "wait": wait.json,
    })
    .to_string();
    CallToolResult::error(vec![Content::text(text)])
}

/// What a waiting reconnect ended on.
struct ReconnectWait {
    outcome: &'static str,
    json: serde_json::Value,
    status: Option<crate::types::StatusResponse>,
}

/// One blocker as a waiting reconnect has watched it.
struct BlockerWatch {
    blocker: String,
    since: tokio::time::Instant,
    /// The supervisor's retry attempt when one was first seen scheduled.
    first_retry: Option<u64>,
}

impl BlockerWatch {
    fn observe(watch: Option<Self>, status: &crate::types::StatusResponse) -> Self {
        let blocker = status.setup_blocked_on.clone().unwrap_or_default();
        let retry = reconnect_setup_retry(status).map(|(attempt, _)| attempt);
        match watch {
            Some(mut seen) if seen.blocker == blocker => {
                seen.first_retry = seen.first_retry.or(retry);
                seen
            }
            _ => BlockerWatch { blocker, since: tokio::time::Instant::now(), first_retry: retry },
        }
    }
}

/// What a blocker means for the wait now: `None` keeps waiting, `blocked`
/// ends it, `retrying` ends it with the supervisor's own retry still to come
/// (after the wait would end), which is not a request for a person (#403).
fn reconnect_blocker_final(
    status: &crate::types::StatusResponse,
    watch: &BlockerWatch,
    remaining: std::time::Duration,
) -> Option<&'static str> {
    let held = watch.since.elapsed();
    if status.setup_blocked_on.as_deref().unwrap_or_default().starts_with("needs_passcode") {
        return Some("blocked");
    }
    if let Some((attempt, next_in)) = reconnect_setup_retry(status) {
        if std::time::Duration::from_secs(next_in) >= remaining {
            // A slow backoff (minutes) is not about to fix this on its own.
            return Some(if next_in <= RECONNECT_RETRY_SOON_SECS { "retrying" } else { "blocked" });
        }
        // A retry is due inside the wait: the blocker is final only once it
        // held across at least one of them.
        let retried = watch.first_retry.is_some_and(|first| attempt > first);
        return (retried && held >= RECONNECT_BLOCKER_SETTLE).then_some("blocked");
    }
    (status.reconnecting != Some(true) || held >= RECONNECT_BLOCKER_SETTLE).then_some("blocked")
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
    /// Opt into a bounded, redacted action timeline (no values/screenshots).
    /// Retrieve with phone_run_trace; restart loses it. Default false.
    #[serde(default)]
    pub trace: bool,
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

/// Owner-scoped trace lookup.
#[derive(Debug, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunTraceParams {
    pub run_id: String,
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

    /// When the MCP client goes away, give back the phone leases this process
    /// still holds, so another session does not wait out the rest of the
    /// lease (#414). Only a lease the daemon reports as this process's own is
    /// released, and never under a shared `IPHONE_USE_OWNER` name, which
    /// another process may be using right now.
    pub async fn release_own_leases(&self) {
        if std::env::var("IPHONE_USE_OWNER").is_ok_and(|v| !v.trim().is_empty()) {
            return;
        }
        let mut clients = vec![self.daemon.clone()];
        if let Ok(instances) = self.instances.lock() {
            clients.extend(instances.values().map(|h| h.daemon.clone()));
        }
        for client in clients {
            let release = async {
                let status = client.status().await.ok()?;
                if status.owner.as_deref() != Some(client.owner()) {
                    return None;
                }
                client.release_owner().await.ok()
            };
            if let Ok(Some(_)) = tokio::time::timeout(std::time::Duration::from_secs(5), release).await {
                tracing::info!("released this session's phone lease on exit ({})", client.owner());
            }
        }
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
            instances: std::sync::Arc::default(),
            usable_seen: std::sync::Arc::default(),
        }
    }

    fn note_usable(&self, usable: bool) {
        if let Ok(mut seen) = self.usable_seen.lock() {
            *seen = usable.then(std::time::Instant::now);
        }
    }

    fn usable_recently(&self) -> bool {
        self.usable_seen
            .lock()
            .ok()
            .and_then(|seen| *seen)
            .is_some_and(|at| at.elapsed() < RECONNECT_CHECK_FRESH)
    }

    /// The same handler exposing `profile` instead of the environment's.
    #[cfg(test)]
    fn with_profile(mut self, profile: ToolProfile) -> Self {
        self.profile = std::sync::Arc::new(profile);
        self
    }

    fn for_instance(&self, name: &str) -> anyhow::Result<Self> {
        let daemon = crate::instances::client(name, self.daemon.owner())?;
        let mut instances = self.instances.lock().map_err(|_| anyhow::anyhow!("instance routing unavailable"))?;
        if let Some(handler) = instances.get(name).filter(|handler| handler.daemon.same_target(&daemon)) {
            return Ok(handler.clone());
        }
        let handler = Self::with_shared_profile(daemon, self.profile.clone());
        instances.insert(name.to_string(), handler.clone());
        Ok(handler)
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

    /// Bring a released phone back before a tool that needs it: `Ok(note)`
    /// runs the tool (with a line saying it reconnected), `Err(result)`
    /// returns that instead. Anything unexpected runs the tool unchanged, so
    /// the daemon's own refusal still reaches the model.
    async fn reconnect_for_use(&self, tool: &str) -> Result<Option<String>, CallToolResult> {
        if self.usable_recently() {
            return Ok(None);
        }
        let Ok(status) = self.daemon.status().await else { return Ok(None) };
        if status.drivable == Some(true) {
            self.note_usable(true);
        }
        if !may_reconnect_for_use(&status, self.daemon.owner()) {
            return Ok(None);
        }
        if status.reconnecting != Some(true) {
            if let Err(error) = self.daemon.reconnect().await {
                tracing::debug!("reconnect on use failed: {error:#}");
                return Ok(None);
            }
        }
        self.progress_reset();
        let wait = self.wait_until_drivable(RECONNECT_ON_USE_BUDGET).await;
        if wait.outcome == "drivable" {
            self.note_usable(true);
            return Ok(Some(format!(
                "the phone was released after the last task; brought it back for this call in {} s",
                wait.json["waited_secs"]
            )));
        }
        Err(reconnect_on_use_stopped(tool, &wait))
    }

    /// Poll status until the phone is drivable, a blocker settles, the
    /// supervisor's own retry is still to come, or `budget` runs out
    /// (`phone_reconnect wait_secs`, and the on-use reconnect).
    async fn wait_until_drivable(&self, budget: std::time::Duration) -> ReconnectWait {
        let started = tokio::time::Instant::now();
        let deadline = started + budget;
        let mut last_status = None;
        let mut last_error: Option<String> = None;
        let mut blocked: Option<BlockerWatch> = None;
        let outcome = loop {
            if tokio::time::Instant::now() >= deadline { break "timeout"; }
            let status = match tokio::time::timeout_at(deadline, self.daemon.status()).await {
                Ok(status) => status,
                Err(_) => {
                    last_error = Some("status read exceeded the remaining wait budget".to_string());
                    break "timeout";
                }
            };
            match status {
                Ok(status) => {
                    let done = match reconnect_wait_outcome(&status) {
                        Some("blocked") => {
                            let watch = BlockerWatch::observe(blocked.take(), &status);
                            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                            let done = reconnect_blocker_final(&status, &watch, remaining);
                            blocked = Some(watch);
                            done
                        }
                        Some(done) => Some(done),
                        None => {
                            blocked = None;
                            None
                        }
                    };
                    last_status = Some(status);
                    last_error = None;
                    if let Some(done) = done {
                        break done;
                    }
                }
                Err(e) => last_error = Some(format!("{e:#}")),
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break "timeout";
            }
            tokio::time::sleep(std::cmp::min(RECONNECT_POLL, deadline - now)).await;
        };
        // Out of time with the supervisor still retrying a blocker: say so,
        // rather than a bare timeout or a settled blocker.
        // Only from a status read that succeeded last; a failed or hung read
        // stays a timeout with its status_error.
        let retry = last_status.as_ref().filter(|_| last_error.is_none()).and_then(reconnect_setup_retry);
        let outcome = match (outcome, &blocked, retry) {
            ("timeout", Some(_), Some(_)) => "retrying",
            (outcome, ..) => outcome,
        };
        let mut wait = serde_json::json!({
            "outcome": outcome,
            "waited_secs": started.elapsed().as_secs(),
            "status": last_status.as_ref().map(reconnect_brief),
        });
        if let Some(error) = last_error {
            wait["status_error"] = error.into();
        }
        ReconnectWait { outcome, json: wait, status: last_status }
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
    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "List installed local iPhone instances and their read-only status. This inventory is not a list of paired/connected devices. For recovery_owner=unconfigured, inspect xcrun devicectl list devices and run iphone-use doctor on the daemon host; ask which phone when ambiguous. Does not start, reconnect or claim a phone. Pass instance in any phone_* call to choose one; omitted instance preserves this MCP server's configured daemon. Missing/invalid instances never fall back to another phone."
    )]
    async fn phone_instances(&self) -> CallToolResult {
        let names = match crate::instances::names() {
            Ok(names) => names,
            Err(_) => return CallToolResult::error(vec![Content::text("installed-instance discovery failed")]),
        };
        let mut rows = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        for name in names {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                rows.push(serde_json::json!({"instance":name,"available":false,"error":"status_not_checked_budget"}));
                continue;
            }
            let handler = match self.for_instance(&name) {
                Ok(handler) => handler,
                Err(_) => {
                    rows.push(serde_json::json!({"instance":name,"available":false,"error":"invalid_or_unavailable_configuration"}));
                    continue;
                }
            };
            let status = tokio::time::timeout(remaining.min(std::time::Duration::from_secs(2)), handler.daemon.status()).await;
            rows.push(match status {
                Ok(Ok(status)) => serde_json::json!({"instance":name,"available":true,
                    "drivable":status.drivable,"released":status.released,"owner":status.owner,
                    "target_configured":status.target_configured,"recovery_owner":status.recovery_owner,
                    "recovery":status.extra.get("recovery"),
                    "device":status.extra.get("device"),"human_handoff":status.extra.get("human_handoff")}),
                _ => serde_json::json!({"instance":name,"available":false,"error":"status_unavailable"}),
            });
        }
        let body = serde_json::json!({"instances":rows,"selection":"per_call","implicit_target":"configured_daemon","scope":"installed_daemon_instances",
            "paired_device_discovery_command":"xcrun devicectl list devices"});
        let mut result = CallToolResult::success(vec![Content::text(body.to_string())]);
        result.structured_content = Some(body);
        result
    }


    // -----------------------------------------------------------------------
    // phone_capabilities
    // -----------------------------------------------------------------------

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Supported UI operations and cached readiness: the backend's single-step, batch and perform \
        vocabularies, observation support and accepted modes (phone_status covers \
        device/setup/owner). Read-only: no device connection, wake or lease. supported is static; \
        available is cached, blocked_by names locked, released, reconnecting, human_handoff, \
        owned_by_other or offline; ok:null is unknown, never readiness. UI control only, not app \
        install or device management. If blocked, follow phone_status."
    )]
    async fn phone_capabilities(&self) -> CallToolResult {
        match self.daemon.capabilities().await {
            Ok(response) => daemon_read_result(&response),
            Err(e) => CallToolResult::error(vec![Content::text(format!(
                "capabilities failed: {e:#}"
            ))]),
        }
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Task performance aggregates for this client's X-Phone-Owner on the selected instance: \
        HTTP-call counts, batch/flow usage, stale/unknown outcomes, losses, p50/p95 latency. HTTP \
        calls are not model turns or tokens; model_round_trips stays null without a complete \
        declared trace. Read-only: no connection, runner launch or lease — never reconnect for \
        metrics. Default the latest 5 open and recent runs, max 20 per section, 12 KiB row budget; \
        omitted counts and truncation are explicit."
    )]
    async fn phone_metrics(&self, Parameters(params): Parameters<MetricsParams>) -> CallToolResult {
        let limit = params.limit.unwrap_or(5);
        if !(1..=20).contains(&limit) {
            return CallToolResult::error(vec![Content::text("limit must be between 1 and 20")]);
        }
        let response = match self.daemon.metrics().await {
            Ok(response) => response,
            Err(error) => return CallToolResult::error(vec![Content::text(format!("metrics failed: {error:#}"))]),
        };
        if !response.ok() { return daemon_read_result(&response); }
        let Some(body) = response.json.as_ref().filter(|body| body.get("ok").and_then(serde_json::Value::as_bool) == Some(true)) else {
            return CallToolResult::error(vec![Content::text("metrics response did not contain a valid report")]);
        };
        let owner = self.daemon.owner().trim();
        let mut budget = 12 * 1024;
        let mut report = serde_json::json!({"ok":true,"owner":owner,
            "definitions":"Counts cover recorded HTTP calls, not model turns or token usage. A missing or incomplete model trace is not zero round trips. Only this client's owner is included; global loss counters are excluded.",
            "truncated":false});
        for section in ["open", "recent"] {
            let Some(rows) = body.get(section).and_then(serde_json::Value::as_array) else {
                return CallToolResult::error(vec![Content::text("metrics report is missing its open/recent arrays")]);
            };
            let mut ordered = Vec::with_capacity(rows.len());
            for row in rows {
                if row.pointer("/key/owner").and_then(serde_json::Value::as_str) != Some(owner) {
                    return CallToolResult::error(vec![Content::text("metrics report did not match this client's owner; no run data returned")]);
                }
                let (Some(start), Some(end), Some(incarnation)) = (
                    row.get("started_ms").and_then(serde_json::Value::as_u64),
                    row.get("ended_ms").and_then(serde_json::Value::as_u64),
                    row.pointer("/key/incarnation").and_then(serde_json::Value::as_u64),
                ) else {
                    return CallToolResult::error(vec![Content::text("metrics run is missing its timestamps or identity")]);
                };
                ordered.push(((end.max(start), incarnation), row));
            }
            ordered.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
            let mut selected = Vec::new();
            for (_, row) in ordered.into_iter().take(limit) {
                let bytes = match serde_json::to_vec(row) {
                    Ok(bytes) => bytes.len(),
                    Err(_) => return CallToolResult::error(vec![Content::text("could not encode metrics run")]),
                };
                if bytes > budget { break; }
                budget -= bytes;
                selected.push(row.clone());
            }
            let omitted = rows.len() - selected.len();
            if omitted > 0 { report["truncated"] = serde_json::json!(true); }
            report[format!("{section}_omitted")] = serde_json::json!(omitted);
            report[section] = serde_json::json!(selected);
        }
        let mut result = CallToolResult::success(vec![Content::text(report.to_string())]);
        result.structured_content = Some(report);
        result
    }

    #[tool(
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = true),
        description = "Read all home-screen pages, Dock, folders and widget stacks from the paired iPhone without \
        swiping or screenshots. Needs a physical target over usbmuxd, not drivable=true or a runner: \
        no reconnect. Page indexes are persisted icon state (hidden pages included); null row/col or \
        positions_complete=false means unknown, never drag coordinates. action: backup (plist on the \
        Mac), preview (diff plist vs phone), apply/restore (replace the layout; confirm=true only \
        after the person saw the preview)."
    )]
    async fn phone_home_layout(
        &self,
        Parameters(params): Parameters<HomeLayoutParams>,
    ) -> CallToolResult {
        if let Some(action) = params.action {
            if *self.profile == ToolProfile::ReadOnly {
                return CallToolResult::error(vec![Content::text(format!(
                    "home layout {action} is not available with {TOOLS_ENV}=readonly; call phone_home_layout without action to read"
                ))]);
            }
            let body = serde_json::json!({"action": action, "file": params.file, "confirm": params.confirm});
            return match self.daemon.home_layout_write(body).await {
                Ok(response) => daemon_read_result(&response),
                Err(error) => CallToolResult::error(vec![Content::text(format!("home layout {error:#}"))]),
            };
        }
        match self.daemon.home_layout(params.page).await {
            Ok(response) => daemon_read_result(&response),
            Err(error) => CallToolResult::error(vec![Content::text(format!(
                "home layout failed: {error:#}"
            ))]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_apps
    // -----------------------------------------------------------------------

    #[tool(
        annotations(read_only_hint = true, open_world_hint = true),
        description = "Find an app's bundle id by name. Read-only: no screen access, no owner lease. Searches the \
        phone's installed apps, then a verified catalog of popular apps and Apple's built-ins, then \
        the App Store only when neither matched; exact matches hide partial ones. Candidates carry \
        name, bundle_id, source (installed|catalog|apple), installed_verified and publisher; \
        installed_verified=false with installation_checked=false means unknown, not absent. Then \
        launch_app bundle (or `app` with the exact name)."
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
        description = "One screen as an image/png block, when pixels matter; phone_elements for text/targets, \
        phone_screen for the person's live panel. Reuses the settled frame after an observed action. \
        max_side default 1200 (0 = full resolution; an oversized image is stepped down to fit 1.5 \
        MB); overlay=true draws interactive frames with their phone_elements indexes. Capture only \
        for the current task; private holds or offline devices refuse. An image does not prove an \
        action succeeded; check the state or receipt."
    )]
    async fn phone_screenshot(
        &self,
        Parameters(ScreenshotParams { max_side, overlay }): Parameters<ScreenshotParams>,
    ) -> CallToolResult {
        let requested = match max_side {
            None => Some(DEFAULT_SCREENSHOT_MAX_SIDE),
            Some(0) => None,
            Some(side) => Some(side),
        };
        let overlay = overlay.unwrap_or(false);
        let mut max_side = requested;
        let mut shot = self.daemon.screenshot_with(max_side, overlay).await;
        // Over budget (a dense full-resolution capture): step the long side
        // down rather than hand the client an image it will truncate or drop.
        while let Ok(bytes) = &shot {
            if B64.encode(bytes).len() <= SCREENSHOT_BUDGET_BYTES {
                break;
            }
            let Some(smaller) = SCREENSHOT_SIDES
                .into_iter()
                .find(|&side| max_side.is_none_or(|current| side < current))
            else {
                break;
            };
            max_side = Some(smaller);
            shot = self.daemon.screenshot_with(max_side, overlay).await;
        }
        match shot {
            Ok(bytes) if !bytes.is_empty() => {
                let b64 = B64.encode(&bytes);
                let mut content = vec![Content::image(b64, "image/png")];
                if max_side != requested {
                    content.push(Content::text(format!(
                        "downscaled to max_side={} to fit the {} MB image budget",
                        max_side.unwrap_or_default(),
                        SCREENSHOT_BUDGET_BYTES / 1_000_000
                    )));
                }
                CallToolResult::success(content)
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
        description = "Tap a pixel target using normalized x,y from 0–1, with (0,0) at top-left. Prefer phone_tap_element \
        for an accessible control; use phone_screenshot to locate a pixel-only target. Requires \
        phone_status.drivable=true and current screen evidence: a tap may send, buy, delete or toggle \
        whatever occupies that point, so authorize its actual effect first. observe defaults on and returns \
        the settled screen change. Read outcome/retry_safe; if delivery is unknown, inspect the current \
        state or receipt before considering another tap."
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
        description = "Move the viewport with one device-side swipe (phone_scroll_find to reach a known label, \
        phone_collect_list to read several pages). x,y is a normalized anchor inside the scrollable \
        region — avoid text fields, sliders and screen edges, a misplaced gesture can edit a control \
        or navigate away. Positive dy reveals content below, negative above; dx right/left. Travel \
        is 1.5× each delta in points, clamped to 15–75% of the screen. Inspect the observed result \
        before repeating."
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
        description = "Type Unicode text into the focused field (focus it with phone_tap_element first; phone_key \
        for editing keys). Text may replace a selection or land in the wrong field if focus moved. \
        Long text is chunked; budget_ms (6000..15000) may stop at a safe boundary and return a \
        single-use continue_token: continue with only that token. On partial failure read \
        characters_confirmed and remaining_text; never resend the whole value blindly. Verify the \
        field before submitting; unknown outcome ≠ nothing typed."
    )]
    async fn phone_type(&self, Parameters(params): Parameters<TypeParams>) -> CallToolResult {
        let action = match params.action() {
            Ok(action) => action,
            Err(hint) => return not_sent_result("invalid_text_request", hint.to_string()),
        };
        self.progress_reset();
        let observe = params.observe.unwrap_or(true);
        match self.daemon.input_value_observed(&action, observe).await {
            Ok(response) => daemon_action_result(&response, observe, "ok"),
            Err(error) => unknown_action_result("transport_error", format!("the call failed: {error:#}.")),
        }
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
        description = "Press a navigation or editing key in the foreground app, acting on its current focus. Use phone_type \
        for literal text and phone_tap_element to focus a field first. Check phone_status.drivable and the \
        focused control: delete/backspace can erase content, and return/enter may submit or send it. The \
        schema lists accepted names; unsupported names fail before input is sent. observe defaults on and \
        returns the settled screen change. On an unknown outcome, inspect the screen before another key; \
        repeating a key can change or submit content twice."
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
        description = "System shortcut: home, spotlight, volume_up/volume_down (physical device; the ACK confirms \
        the request, not the level) or save_screenshot (a mutation, unlike phone_screenshot: the \
        receipt means requested, not saved; if iOS shows a preview choose Done/Save there, never \
        take another; check Photos before claiming saved). Never replay an uncertain capture or \
        volume press. App Switcher is unsupported; unknown names fail before input. phone_key for \
        field keys, launch_app for an app."
    )]
    async fn phone_shortcut(
        &self,
        Parameters(ShortcutParams { name, observe }): Parameters<ShortcutParams>,
    ) -> CallToolResult {
        self.progress_reset();
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "home" | "spotlight" | "save_screenshot" | "volume_up" | "volume_down" => {
                send_input_observed(&self.daemon, &InputMsg::Shortcut { name }, observe).await
            }
            "switcher" => CallToolResult::error(vec![Content::text(
                "unsupported shortcut 'switcher': the device runner cannot open \
                 the iOS App Switcher; use home, spotlight, or launch an app by a \
                 supported device action instead",
            )]),
            _ => CallToolResult::error(vec![Content::text(format!(
                "unsupported shortcut '{name}'; supported: home, spotlight, save_screenshot, volume_up, volume_down"
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
        description = "THE DEFAULT WAY TO ACT: run several steps in ONE call and get the screen it ended on back \
        (observe, default on): one turn, not one per tap. The daemon validates the whole batch, \
        holds one lock, stops at the first failure. Single-action tools are for exploring. Guard \
        screen changes with wait_for; never batch an unverified irreversible step. The steps are \
        exactly a saved flow's `steps`: on `flow_suggestion` ask the user about phone_flow_draft. \
        retry_safe=false: DO NOT replay."
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
                .and_then(|mut json| {
                    crate::compact::label_action_steps(&mut json, &request);
                    crate::compact::batch(&json)
                })
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
                .clone()
                .and_then(|mut json| {
                    crate::compact::label_action_steps(&mut json, &request);
                    crate::compact::failed_batch(&json)
                })
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
        description = "Read the phone's state without taking control: owner (another session's owner means do not \
        drive; control calls get phone_owned), device_state, screen_state, runner, locked, drivable, \
        released, hint, next_step, recovery, setup_blocked_on/phase/message, lock_readiness \
        (will_lock_needs_person: a person must unlock after a pause; relay the hint), \
        keep_runner_alive. Act only on drivable=true; otherwise relay next_step and stop. Never ask \
        for a passcode; pause reads while one is entered."
    )]
    async fn phone_status(&self) -> CallToolResult {
        match self.daemon.status().await {
            Ok(s) => {
                // Health checks stay read-only by default. Pre-warming a
                // released phone is an explicit opt-in.
                if s.released == Some(true)
                    && std::env::var("IPHONE_USE_MCP_PREWARM").is_ok_and(|v| v.trim() == "1")
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
        description = "Internal frame transport for the iphone-use MCP Apps panel; models use phone_screenshot for a still \
        image and phone_elements for text. Returns structuredContent with status, frame (mime_type=image/png \
        and base64 data), ok and error. A private hold returns frame=null; an empty or failed capture is \
        reported rather than invented pixels. max_side is clamped to 200–1000; fresh bypasses the reusable \
        frame. The panel calls this with visibility app; do not poll it from model turns or use it for \
        health checks."
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
        // A response body can be revoked after its 200 headers, surfacing as
        // a transport error. Any failed capture must clear the previous pixels;
        // a concurrent status response cannot authorize reusing them.
        let clear_frame = !matches!(&frame, Ok(bytes) if !bytes.is_empty());
        let mut result = crate::screen::frame_result(
            status
                .map(|s| crate::screen::status_summary(&s, self.daemon.owner()))
                .map_err(|e| format!("status failed: {e:#}")),
            frame.map_err(|e| format!("{e:#}")),
        );
        if clear_frame {
            if let Some(body) = result.structured_content.as_mut() { body["clear_frame"] = true.into(); }
        }
        result
    }

    #[tool(
        annotations(read_only_hint=true,open_world_hint=true),
        description="Read one element's AX value without tapping, focusing, waking or a lease. Give exactly one \
        identifier or exact label from a fresh read, optionally kind; ambiguity refuses. Use when \
        phone_elements marks value_truncated or the task needs the full field text. Returns one \
        bounded value with source/completeness; complete means this API did not cut it, not that the \
        app exposes its whole document. A smaller max_utf16 gives truncated:true. Secure fields and \
        runners without ipu:valueRead refuse."
    )]
    async fn phone_read_value(&self, Parameters(params): Parameters<ReadValueParams>) -> CallToolResult {
        let limit = params.max_utf16.unwrap_or(8192);
        let request = serde_json::to_value(params).unwrap_or_else(|_| serde_json::json!({}));
        match self.daemon.read_value(&request).await {
            Ok(response) => {
                if let Some(body) = response.json.as_ref().filter(|body| response.ok() && body["ok"] == true
                    && iu_core::value_read::valid_contract(body, limit)) {
                    // Hosts may show only structuredContent when it exists.
                    // A content-only result keeps the full value visible once
                    // in every output mode, without a competing metadata copy.
                    return CallToolResult::success(vec![Content::text(body.to_string())]);
                }
                if let Some(body) = response.json.as_ref().filter(|body| body["ok"] == false) {
                    let mut safe = body.clone();
                    if let Some(object) = safe.as_object_mut() { object.remove("value"); }
                    let mut result = CallToolResult::error(vec![Content::text(safe.to_string())]);
                    result.structured_content = Some(safe);
                    return result;
                }
                CallToolResult::error(vec![Content::text("full value contract unavailable; no preview was promoted to a complete value")])
            }
            Err(_) => CallToolResult::error(vec![Content::text("value read failed; no input was sent and no preview was promoted to a complete value")]),
        }
    }

    // -----------------------------------------------------------------------
    // phone_elements (L2)
    // -----------------------------------------------------------------------

    #[tool(
        annotations(read_only_hint = true, open_world_hint = true),
        description = "Read one native element's bounded subtree by element+snapshot, serializing only that element (locator lookup may still read the app tree). Requires a unique live semantic target and a runner with ipu:elementSubtree. Returned row positions are READ ONLY, not action refs; use unique locators or fresh phone_elements refs for input. complete=false means descendants may be missing."
    )]
    async fn phone_subtree(&self, Parameters(params): Parameters<SubtreeParams>) -> CallToolResult {
        match self.daemon.subtree(&params.snapshot, params.element, params.max_depth.unwrap_or(8), params.max_nodes.unwrap_or(500)).await {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(error) => CallToolResult::error(vec![Content::text(format!("subtree read failed: {error:#}"))]),
        }
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = true),
        description = "The current screen as text — use instead of screenshots. Rows carry kind, label, identifier, \
        value, rect, state and a `snapshot` token for phone_tap_element, valid for this read only. \
        query/limit/selector narrow rows but keep their indexes. Max 200 rows / 32 KiB; an omission \
        notice means a partial view. `registry` lists saved flows for a newly entered app: prefer \
        phone_flow_run. `alert` = a system alert is up: use an `alert` step, taps on alerts do not \
        act. OCR rows: phone_tap their x/y."
    )]
    async fn phone_elements(
        &self,
        Parameters(ElementsParams {
            query,
            selector,
            limit,
            ocr,
            include_hidden,
        }): Parameters<ElementsParams>,
    ) -> CallToolResult {
        let ocr = match crate::device::ocr_param(ocr.as_deref()) {
            Ok(ocr) => ocr,
            Err(error) => return CallToolResult::error(vec![Content::text(error)]),
        };
        if let Some(selector) = &selector {
            if let Err(hint) = selector.validate() {
                return CallToolResult::error(vec![Content::text(format!("invalid selector: {hint}; nothing was sent to the daemon"))]);
            }
        }
        // A narrowed read is not the whole screen: it must not become the
        // no-progress tracker's baseline.
        let narrowed = query.as_deref().is_some_and(|q| !q.trim().is_empty()) || limit.is_some() || selector.is_some();
        match self
            .daemon
            .elements_with_selector(query.as_deref(), limit, ocr, selector.as_ref())
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
                    .and_then(|json| {
                        if include_hidden {
                            crate::compact::elements_with_hidden(json, true)
                        } else {
                            crate::compact::elements(json)
                        }
                    })
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
                if let Some(error)=e.downcast_ref::<crate::client::DaemonHttpError>() {
                    if error.status==reqwest::StatusCode::CONFLICT {
                        if let Ok(body)=serde_json::from_str::<serde_json::Value>(&error.body) {
                            if body.get("error").and_then(serde_json::Value::as_str)==Some("ordinal_unproven") {
                                let mut result=CallToolResult::error(vec![Content::text(body.to_string())]);
                                result.structured_content=Some(body);
                                return result;
                            }
                        }
                    }
                }
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
        from the SAME phone_elements response. The daemon re-reads the tree; a small \
        change on the same screen (clock, badge, a flipped switch) still finds the row \
        by kind, identifier, label and enclosing rows, while another page, a new alert, \
        or look-alikes it cannot tell apart refuse with nothing sent, so a stale element \
        reference cannot silently tap a different control. Use identifier/kind/label/state fields to \
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
        description = "Start/restart on-device automation; occupies the phone, may need the operator to unlock it. \
        Only to continue a current user-requested task that cannot proceed — never for \
        initialization, health checks, a finished task or to keep the phone ready. Needs \
        released/offline status, recovery_owner=daemon, no blocker or reconnect in progress. Call \
        once; wait_secs blocks until drivable, a blocker or timeout (outcome retrying: the \
        supervisor retries within a minute). Never switches devices."
    )]
    async fn phone_reconnect(
        &self,
        Parameters(params): Parameters<ReconnectParams>,
    ) -> CallToolResult {
        self.progress_reset();
        let wait_secs = match params.wait_secs {
            None | Some(0) => None,
            Some(secs) if secs > RECONNECT_MAX_WAIT_SECS => {
                return CallToolResult::error(vec![Content::text(format!(
                    "wait_secs must be between 1 and {RECONNECT_MAX_WAIT_SECS}; nothing was sent"
                ))]);
            }
            Some(secs) => Some(secs),
        };
        let body = match self.daemon.reconnect().await {
            Ok(body) => body,
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "reconnect failed: {e:#}"
                ))])
            }
        };
        let Some(wait_secs) = wait_secs else {
            return CallToolResult::success(vec![Content::text(body)]);
        };
        let wait = self.wait_until_drivable(std::time::Duration::from_secs(wait_secs)).await;
        let reconnect = serde_json::from_str::<serde_json::Value>(&body)
            .unwrap_or(serde_json::Value::String(body));
        let text = serde_json::json!({"reconnect": reconnect, "wait": wait.json}).to_string();
        CallToolResult::success(vec![Content::text(text)])
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false,
        ),
        description = "Open a task boundary for this session's daemon metrics; pair with phone_run_end, not phone_record \
        (video). Later session calls count toward this run; this does not drive or reserve the phone. \
        Optional: without it, idle gaps infer runs. Use a fresh run_id for each task; starting the same \
        still-open id resumes that run rather than resetting its counts. complete_trace is valid only before \
        its first counted call and commits you to supply every model turn id at run_end. Returns a start \
        acknowledgement; report errors rather than claiming metrics were recorded."
    )]
    async fn phone_run_start(
        &self,
        Parameters(params): Parameters<RunStartParams>,
    ) -> CallToolResult {
        self.progress_reset();
        match self
            .daemon
            .run_start_with_trace(&params.run_id, params.complete_trace.unwrap_or(false), params.trace)
            .await
        {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(e) => CallToolResult::error(vec![Content::text(format!("run start failed: {e:#}"))]),
        }
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = true),
        description = "Read this session's redacted action timeline for a run started with trace=true. Bounded memory only; includes counts and delivery/verification receipts, never typed values or screenshots. It cannot replay actions."
    )]
    async fn phone_run_trace(&self, Parameters(params): Parameters<RunTraceParams>) -> CallToolResult {
        match self.daemon.action_trace(&params.run_id).await {
            Ok(body) => CallToolResult::success(vec![Content::text(body)]),
            Err(error) => CallToolResult::error(vec![Content::text(format!("trace read failed: {error:#}"))]),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false,
        ),
        description = "Close a live task opened with phone_run_start and return its metrics: HTTP calls (not model turns), \
        batches, observed actions, flows, stale/unknown outcomes, failures and p50/p95 call time. Use the \
        same run_id and session; a missing or already-closed run cannot produce a new live summary. Supply \
        turn_ids only for a complete_trace declaration, including every model generation; otherwise \
        model_round_trips remains unknown. This ends accounting only, not a video recording or the owner \
        lease: use phone_record(stop) and phone_release_owner for those."
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
        description = "Read a scrolling list across pages in ONE call: reads rows of one kind (default Cell), \
        swipes a page, repeats until the page repeats (duplicate_page), end_label shows, or \
        max_pages (default 6, max 10). Rows dedupe on (kind, identifier, label, value): look-alike \
        rows without identifiers may collapse, so reconcile counts the user cares about. \
        complete=true ONLY when end_label was seen; duplicate_page is not proof of the end. Leaves \
        the screen on the last page. swipes_uncertain: never replay."
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
        description = "Find one exact label, swiping at most max_swipes (default 1, max 5) when it is off screen. \
        Stops at once — no further swipe — when the label is ambiguous (candidates with index and \
        frame), covered or not drawn. Found: `target` plus a `snapshot` for phone_tap_element; not \
        found: on-screen labels containing the text as `candidates`. Absence after the budget is not \
        proof; look before swiping on. Optional `snapshot` from a read of the target app refuses on \
        app change. swipes_uncertain: never replay."
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
        description = "Keep the phone through a bounded human-in-the-loop pause inside the current task (a PIN, a \
        prompt, a code): the idle watchdog will not release it for `secs` seconds (0 clears; max \
        14400). When the person signs in (passcode, Face ID, a code) pass private=true: the screen \
        is not read or streamed until the hold is cleared; pass private=true again when extending, a \
        hold without it lifts the privacy. Not for initialization, health checks or keeping the \
        phone ready; clear it when done."
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
        description = "Release this session's phone lease when its task ends so another session can drive. Also \
        frees the phone: a daemon-managed runner stops at once (the phone leaves automation mode) \
        unless a live viewer, an active phone_hold or a recording keeps it; the next phone tool \
        brings it back. Unlike phone_hold(secs=0) this gives up ownership. Sends no input, returns \
        owner=null; releasing an unowned phone succeeds; another session's lease returns phone_owned \
        and is never taken over."
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
        description = "Log the app on screen in with the user's OWN account from their password vault \
        (bitwarden-use on the Mac): the daemon reads the entry and types it, so no credential passes \
        through this conversation. Use when an app shows its login page during the user's task. \
        Several matching entries: the answer lists their names; ask the user, then pass item (and \
        user). Existing accounts only — never sign-up, password change or payment. needs_code: call \
        again with code_via (sms|mail) and code_from (max two)."
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
        description = "Installed deterministic per-app flows — check before exploring a task: phone_flow_run \
        replays one without model decisions or screenshots. Filters category, app (registry app dir \
        or bundle id, not a display name) and verified combine. Rows: id, description, inputs, risk, \
        compat with this phone's app version; detail=true for full JSON. verified = a recorded \
        hardware run, not a guarantee. phone_flow_info shows effects. An empty store needs \
        phone_flow_update once; a filter miss ≠ empty store."
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
        description = "Inspect an installed flow before phone_flow_run: returns metadata, declared inputs and step \
        templates with placeholders, never runtime input values. Use an id from phone_flow_list, such as \
        health/export-all: exactly two lowercase app/flow slugs, not a local file path. Check app, locale, \
        verification and side effects before execution. Invalid or missing ids return an error without \
        running or downloading anything; use phone_flow_update if the registry store is empty. To inspect a \
        new local draft, validate its file instead of guessing a registry id."
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
        description = "Run one flow exactly once: an installed registry id or a flow file (*.json, e.g. a draft \
        being proved). Pass its declared inputs; risk=side_effect needs confirm=true; compat \
        broken/incompatible for the installed app version needs force=true. Declared `outputs` come \
        back top-level (verify=true checks them). One lock, whole-sequence validation, stops at the \
        first failed step; a read_only flow missing an element retries its other-language variant. \
        retry_safe=false: DO NOT replay, repair the flow."
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
        description = "Hand ONE whole goal to the fast on-phone agent (TypeSafe Jev picks each step from the \
        element table in ~1–2 s; a small LLM types values). For a clear multi-step goal with no \
        saved flow (check phone_flow_list first). Returns status done|blocked|max_steps with the \
        step history; it answers BLOCKED rather than send, pay, delete or share anything the goal \
        did not ask for — still confirm such goals with the user and verify the end screen. Needs \
        TYPESAFE_API_KEY (+TEXT_MODEL_API_KEY to type)."
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
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false,
        ),
        description = "Turn what you just did on the phone into a flow: the daemon's trail of applied actions in \
        the current app (not flow runs) as a flow v1 document — snapshot taps become \
        label/identifier locators, typed text becomes named inputs and is never included — plus a \
        `todo` list. Call it on `flow_suggestion` or after a multi-step task with no flow, but ASK \
        THE USER before saving. save_as writes and validates the file; fix the todos, prove it with \
        phone_flow_run, publish only with the user's OK."
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
        description = "Publish a working flow to the official registry through the user's GitHub CLI: fork if \
        needed, write the flow and any new app.json, rebuild index.json, push a branch, open a \
        public PR. confirm=true only after the user agreed to that publication. Draft with \
        phone_flow_draft and prove with phone_flow_run first; phone_flow_report is for problems in \
        existing flows. verified_on takes device/iOS/date; unverified flows open a draft PR. Returns \
        id, repo, branch, head, pr_url; inspect it before retrying."
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
        description = "Capture and read the paired phone's live system log for app debugging (phone_crashes for \
        saved crash reports). action: start, then reproduce, then read (default; filters \
        process/contains/level, pass the returned next as since); stop releases the relay, clear \
        drops the buffer. Connection/auth failures are errors, not an empty log. Capture stops \
        itself after 15 min without reads. Lines: #seq time process[pid] level: message. Logs may \
        hold private data: collect only what the task needs."
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
        description = "Simulate GPS for testing location-aware apps. Supply both lat (-90–90) and lon (-180–180), in finite \
        decimal degrees, or clear=true without coordinates to restore real location; combining clear with coordinates is refused. \
        Every app sees the simulated position until cleared; it is not a navigation or address lookup tool. \
        Counts as one daemon action and requires authorized device control. Returns the applied \
        location/clear status; on a transport failure the effect is unknown. Inspect the result before \
        proceeding and clear the simulation when the test ends."
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
        description = "Record the iPhone screen to an MP4 on the daemon's Mac: start, perform the requested task, then stop \
        to get path, duration and size. status checks the recording without starting one. Unlike \
        phone_screenshot (one image) or phone_screen (live panel), this persists video under the instance \
        state directory's runs/recordings. Requires a usable device video stream and a writable output \
        directory. Auto-stops after 10 minutes or 200 MB; ended explains why. Video can contain private \
        screens: record only the authorized task, stop when done, and do not treat the returned Mac path as \
        an upload."
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
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false,
        ),
        description = "Read or replace persistent notes for one bundle id about app navigation and quirks. Resolve bundle \
        with phone_apps; without text, read the existing Markdown. With text, replace it entirely (not \
        append), at most 16 KiB UTF-8; read first to preserve useful notes. An empty text clears the notes. \
        The first screen read in an app also includes them in its registry block. Use phone_flow_draft for \
        executable task replay, not these notes. Writes affect future sessions; never store passwords, codes \
        or personal data. Errors do not prove a write was delivered: read back before retrying."
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
/// `format` tags (`uint`, `double`, …).
/// Every validation constraint — types, enums, required lists and numeric
/// bounds including zero — is kept. Keys under `properties` are property NAMES,
/// never schema keywords, so they are never removed (a parameter may be called
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
            // `Option<T>` comes out as `"type":[T,"null"],"default":null`: an
            // omitted property already means none, so the null arm and the
            // null default only cost bytes (58 and 114 times over tools/list).
            if map.get("default").is_some_and(serde_json::Value::is_null) {
                map.remove("default");
            }
            // `deny_unknown_fields` is enforced by the server on every call,
            // with an error naming the field; stating it 63 times in the
            // listing only costs bytes. A `const` already fixes the type.
            if map.get("additionalProperties") == Some(&serde_json::Value::Bool(false)) {
                map.remove("additionalProperties");
            }
            if map.contains_key("const") {
                map.remove("type");
            }
            if let Some(serde_json::Value::Array(types)) = map.get("type") {
                let kept: Vec<_> = types
                    .iter()
                    .filter(|t| t.as_str() != Some("null"))
                    .cloned()
                    .collect();
                if kept.len() == 1 && kept.len() < types.len() {
                    map.insert("type".into(), kept.into_iter().next().unwrap());
                }
            }
            // `Option<Struct>` is `anyOf:[{$ref},{type:null}]`: keep the ref
            // (its siblings, such as the description, stay).
            let inline = match map.get("anyOf") {
                Some(serde_json::Value::Array(arms)) if arms.len() == 2 => {
                    let mut kept = arms.iter().filter(|arm| {
                        arm.get("type").and_then(serde_json::Value::as_str) != Some("null")
                    });
                    match (kept.next(), kept.next()) {
                        (Some(serde_json::Value::Object(arm)), None) => Some(arm.clone()),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(arm) = inline {
                map.remove("anyOf");
                for (key, value) in arm {
                    map.entry(key).or_insert(value);
                }
            }
            // A doc comment's fenced example (a Rust doctest on a shared core
            // type) is for the crate's reader, not the model.
            if let Some(serde_json::Value::String(text)) = map.get_mut("description") {
                if text.contains("```") {
                    *text = strip_code_fences(text);
                }
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

/// `text` without its fenced code blocks, surrounding blank lines collapsed.
fn strip_code_fences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        out.push_str(&rest[..start]);
        match rest[start + 3..].find("```") {
            Some(end) => rest = &rest[start + 3 + end + 3..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.split("\n\n")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Fewest copies, and smallest copy, worth a `$defs` entry: a `$ref` costs
/// about 27 bytes, so three 40-byte copies already come out ahead.
const HOIST_MIN_COPIES: usize = 3;
const HOIST_MIN_BYTES: usize = 40;

/// Property schemas repeated verbatim across one tool's schema (every
/// `phone_run_steps` kind states `after_ms` the same way) are stated once
/// under `$defs` and referenced from each place. A property whose name is
/// already a definition is left alone.
fn hoist_repeated_properties(root: &mut serde_json::Map<String, serde_json::Value>) {
    fn visit(value: &mut serde_json::Value, f: &mut dyn FnMut(&str, &mut serde_json::Value)) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map.iter_mut() {
                    if key == "properties" {
                        if let serde_json::Value::Object(props) = child {
                            for (name, schema) in props.iter_mut() {
                                f(name, schema);
                                visit(schema, f);
                            }
                        }
                    } else {
                        visit(child, f);
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(|item| visit(item, f)),
            _ => {}
        }
    }
    let mut counts: std::collections::BTreeMap<(String, String), usize> = Default::default();
    let mut whole = serde_json::Value::Object(std::mem::take(root));
    visit(&mut whole, &mut |name, schema| {
        if schema.is_object() && schema.get("$ref").is_none() {
            let canonical = schema.to_string();
            if canonical.len() >= HOIST_MIN_BYTES {
                *counts.entry((name.to_string(), canonical)).or_default() += 1;
            }
        }
    });
    let existing: std::collections::BTreeSet<String> = whole
        .get("$defs")
        .and_then(serde_json::Value::as_object)
        .map(|defs| defs.keys().cloned().collect())
        .unwrap_or_default();
    let mut hoisted: std::collections::BTreeMap<String, (String, serde_json::Value)> = Default::default();
    for ((name, canonical), copies) in counts {
        if copies >= HOIST_MIN_COPIES && !existing.contains(&name) && !hoisted.contains_key(&name) {
            let schema = serde_json::from_str(&canonical).expect("canonical schema");
            hoisted.insert(name, (canonical, schema));
        }
    }
    if !hoisted.is_empty() {
        visit(&mut whole, &mut |name, schema| {
            let Some((canonical, _)) = hoisted.get(name) else { return };
            let rendered = schema.to_string();
            if rendered == *canonical {
                *schema = serde_json::json!({ "$ref": format!("#/$defs/{name}") });
            }
        });
        let defs = whole
            .as_object_mut()
            .unwrap()
            .entry("$defs")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(defs) = defs.as_object_mut() {
            for (name, (_, schema)) in hoisted {
                defs.insert(name, schema);
            }
        }
    }
    if let serde_json::Value::Object(map) = whole {
        *root = map;
    }
}

/// `tools/list` with every input schema slimmed (see [`slim_schema`] and
/// [`hoist_repeated_properties`]).
fn slim_tools(tools: Vec<rmcp::model::Tool>) -> Vec<rmcp::model::Tool> {
    tools
        .into_iter()
        .map(|mut tool| {
            let mut schema = serde_json::Value::Object((*tool.input_schema).clone());
            slim_schema(&mut schema);
            if let serde_json::Value::Object(mut map) = schema {
                hoist_repeated_properties(&mut map);
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
    if tool == "phone_metrics" && result.is_error != Some(true) && matches!(mode, StructuredMode::Slim) {
        // Metrics has its own row and byte bounds; generic slimming loses the
        // per-run counters and the loss/unknown fields needed to interpret them.
        shape_structured(result, StructuredMode::Full)
    } else if tool == crate::screen::FRAME_TOOL {
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
        .map(|mut tool| {
            if *profile == ToolProfile::ReadOnly && tool.name == "phone_flow_draft" {
                let annotations = tool.annotations.as_mut().expect("draft annotations");
                annotations.read_only_hint = Some(true);
                annotations.idempotent_hint = Some(true);
                let mut schema = (*tool.input_schema).clone();
                if let Some(properties) = schema.get_mut("properties").and_then(|v| v.as_object_mut()) {
                    properties.remove("save_as");
                }
                tool.input_schema = std::sync::Arc::new(schema);
                tool.description = Some("Read the current app's applied-action trail as a flow v1 draft and todo list; never executes the flow. Typed values are replaced with named inputs. In this read-only profile no file is written and save_as is unavailable. Inspect and fix the draft before using phone_flow_run in an action-enabled session.".into());
            }
            if *profile == ToolProfile::ReadOnly && tool.name == "phone_home_layout" {
                // Reading stays; this profile hides the write actions.
                let annotations = tool.annotations.as_mut().expect("home layout annotations");
                annotations.read_only_hint = Some(true);
                annotations.destructive_hint = None;
                let mut schema = (*tool.input_schema).clone();
                if let Some(properties) = schema.get_mut("properties").and_then(|v| v.as_object_mut()) {
                    for key in ["action", "file", "confirm"] { properties.remove(key); }
                }
                tool.input_schema = std::sync::Arc::new(schema);
            }
            if tool.name != "phone_instances" {
                let mut schema = (*tool.input_schema).clone();
                let properties = schema.entry("properties").or_insert_with(|| serde_json::json!({}));
                if let Some(properties) = properties.as_object_mut() {
                    // Short on purpose: it is repeated on 42 tools; the
                    // server instructions explain it once.
                    properties.insert("instance".into(), serde_json::json!({"type":"string",
                        "description":"daemon instance name"}));
                }
                tool.input_schema = std::sync::Arc::new(schema);
            }
            tool
        })
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
/// `all` (the default) is the whole surface; `core` is everything but the
/// developer tools ([`DEVELOPER_TOOLS`]); `readonly` keeps the tools
/// annotated `read_only_hint = true` (they never act on the phone); a comma
/// list is an explicit allowlist of tool names. A hidden tool is neither
/// listed nor callable: a call is refused with a message naming the setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolProfile {
    All,
    Core,
    ReadOnly,
    Allow(std::collections::BTreeSet<String>),
}

/// The environment variable that selects a [`ToolProfile`].
const TOOLS_ENV: &str = "IPHONE_USE_MCP_TOOLS";

/// Tools for app debugging and device management rather than driving the
/// phone; `core` leaves them out so an ordinary session does not pay for
/// their schemas.
pub(crate) const DEVELOPER_TOOLS: [&str; 6] = [
    "phone_app_notes",
    "phone_crashes",
    "phone_install_app",
    "phone_logs",
    "phone_record",
    "phone_set_location",
];

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
            "core" => return (Self::Core, Vec::new()),
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
        // Only the allowlist needs this: the panel (phone_screen) fetches
        // its frames through phone_screen_frame, so an operator who lists
        // phone_screen gets a working panel without knowing about the frame
        // tool. `readonly` includes it on its own merits (read_only_hint).
        if allowed.contains(crate::screen::SCREEN_TOOL) {
            allowed.insert(crate::screen::FRAME_TOOL.to_string());
        }
        if allowed.is_empty() {
            warnings.push(format!(
                "{TOOLS_ENV}={raw:?} names no known tool; no tools are exposed \
                 (use all, core, readonly, or a comma list of tool names)"
            ));
        }
        (Self::Allow(allowed), warnings)
    }

    fn allows(&self, tool: &rmcp::model::Tool) -> bool {
        match self {
            Self::All => true,
            Self::Core => !DEVELOPER_TOOLS.contains(&tool.name.as_ref()),
            Self::ReadOnly => {
                // The handler refuses save_as in this profile; discovery below
                // advertises only the side-effect-free draft operation.
                tool.name == "phone_flow_draft"
                    || tool.name == "phone_home_layout"
                    || tool.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true)
            }
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
            Self::Core => format!(
                "{TOOLS_ENV}=core leaves out the developer tools ({})",
                DEVELOPER_TOOLS.join(", ")
            ),
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
             The operator can change {TOOLS_ENV} in the MCP server's environment (all, core, \
             readonly, or a comma list of tool names)."
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
/// Partial input receipts keep their exact unsent suffix separately.
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
                if key == "app_lifecycle" {
                    let receipt: serde_json::Map<String, serde_json::Value> = ["terminate_acknowledged", "launch_attempted"]
                        .into_iter().filter_map(|name| value.get(name).filter(|field| field.is_boolean())
                            .map(|field| (name.to_string(), field.clone()))).collect();
                    if !receipt.is_empty() { slim.insert(key.clone(), serde_json::Value::Object(receipt)); }
                    continue;
                }
                // How far a set_value replacement got (#399): its counts and
                // where it stopped, without the per-segment list.
                if key == "replacement" {
                    let receipt: serde_json::Map<String, serde_json::Value> = ["phase", "segments_total", "segments_delivered",
                        "characters_confirmed", "characters_uncertain", "characters_total", "stopped_at_segment"]
                        .into_iter().filter_map(|name| value.get(name).filter(|field| !field.is_null())
                            .map(|field| (name.to_string(), field.clone()))).collect();
                    if !receipt.is_empty() { slim.insert(key.clone(), serde_json::Value::Object(receipt)); }
                    continue;
                }
                if key == "system_capture" {
                    let receipt: serde_json::Map<String, serde_json::Value> = ["requested", "saved_verified"]
                        .into_iter().filter_map(|name| value.get(name).filter(|field| field.is_boolean())
                            .map(|field| (name.to_string(), field.clone()))).collect();
                    if !receipt.is_empty() { slim.insert(key.clone(), serde_json::Value::Object(receipt)); }
                    continue;
                }
                // A partial input receipt must retain the exact unsent suffix,
                // even when the normal string preview limit would drop it.
                let input_suffix = key == "remaining_text"
                    && full.get("error").and_then(serde_json::Value::as_str)
                        == Some("text_partially_typed")
                    && value.is_string();
                let keep = input_suffix || match value {
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
        mut request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let instance = request.arguments.as_mut().and_then(|args| args.remove("instance"));
        let instance = match instance {
            Some(serde_json::Value::String(name)) if crate::instances::valid_name(&name) => Some(name),
            Some(_) => return Err(rmcp::ErrorData::invalid_params("instance must be a valid installed-instance name", None)),
            None => None,
        };
        let name = request.name.clone();
        if name == "phone_instances" && instance.is_some() {
            return Err(rmcp::ErrorData::invalid_params("phone_instances lists all installed instances and takes no instance argument", None));
        }
        if let Some(refusal) = self.profile.refusal(&name) {
            return Ok(refusal);
        }
        let codex_client = context
            .peer
            .peer_info()
            .is_some_and(|info| is_codex_client(&info.client_info.name));
        let selected = match instance.as_deref() {
            Some(name) => Some(self.for_instance(name).map_err(|_| rmcp::ErrorData::invalid_params(
                "installed instance unavailable or unsafe; no action sent and no other phone selected", None))?),
            None => None,
        };
        let handler = selected.as_ref().unwrap_or(self);
        let mut reconnected = None;
        if reconnect_on_use(&name, request.arguments.as_ref()) {
            match handler.reconnect_for_use(&name).await {
                Ok(note) => reconnected = note,
                Err(stopped) => {
                    let mut result = finish_result(&name, stopped, StructuredMode::from_env());
                    if let Some(instance) = &instance {
                        result.content.insert(0, Content::text(format!("instance: {instance}")));
                    }
                    return Ok(result);
                }
            }
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(handler, request, context);
        let mut result = Self::tool_router().call(tcc).await?;
        // Last, so a result whose text is the JSON body still starts with it.
        if let Some(note) = reconnected {
            result.content.push(Content::text(note));
        }
        if name == "phone_release_owner" || refused_as_released(&result) {
            // The task ended, or the phone was released under this session
            // (another session's task end, an idle release): check again.
            handler.note_usable(false);
        } else if RECONNECT_ON_USE_TOOLS.contains(&name.as_ref())
            && name != "phone_record"
            && result.is_error != Some(true)
        {
            handler.note_usable(true);
        }
        result.meta.get_or_insert_with(rmcp::model::Meta::new).0
            .insert("iphone-use/instance".into(), serde_json::json!(instance));
        if let Some(instance) = &instance {
            if name == "phone_status" || name == "phone_screen" {
                result.meta.as_mut().unwrap().0.insert("openai/widgetSessionId".into(),
                    serde_json::json!(format!("iphone-use-screen-{instance}")));
            }
        }
        let mut result = keep_codex_images(
            finish_result(&name, result, StructuredMode::from_env()),
            codex_client,
        );
        if let Some(instance) = &instance {
            result.content.insert(0, Content::text(format!("instance: {instance}")));
        }
        Ok(result)
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
                 1. If the user names a phone or there are multiple installed phones, use phone_instances \
                 and pass the chosen instance (an installed daemon instance name such as i13 or default) on every \
                 phone_* call. Omitted instance describes only the configured daemon, \
                 never all phones. Explicit unavailable instances fail without fallback. \
                 phone_status — act only when drivable=true, with one exception: released=true (freed after the last task) is \
                 brought back by the next phone tool itself (elements, tap, run_steps, …), which waits for it and answers \
                 phone_not_ready with what to do if a person must act (e.g. the passcode). Otherwise pause screen reads/actions and follow recovery/next_step. \
                 For target_configured=false with recovery_owner=unconfigured, inspect xcrun devicectl list devices and run iphone-use doctor on the daemon host; \
                 ask which device if ambiguous, then IPU_RUNNER_UDID=<UDID> iphone-use setup only for an authorized confirmed target. \
                 Preserve --instance for named instances; external runners recover on their owning host, never via local setup. Do not take over another owner or bypass privacy/handoff. \
                 For trust/passcode blockers, the person acts on the phone; never ask for their passcode, pause screen reads during entry, \
                 then verify status and resume the original task. Do not loop setup or reconnect; \
                 call phone_reconnect once only when the user's current task needs the phone and a tool did not bring it back \
                 (each reconnect may make the user type the passcode), never for health checks.\n\
                 2. Look for a saved flow FIRST. Responses that enter an app (launch_app, the first \
                 phone_elements in a new app) carry a `registry` block: if a listed flow does the task, \
                 phone_flow_run it — one call instead of dozens. phone_flow_list shows all of them. No flow but a \
                 clear goal: use phone_jev_run only when its model credentials are configured and the user \
                 authorized the whole goal; otherwise plan bounded phone_run_steps batches.\n\
                 3. Otherwise read phone_elements and act on what it names (phone_tap_element, \
                 phone_tap_label for a unique label) only to explore; as soon as you know the next few steps, send \
                 them as ONE phone_run_steps batch (wait_for between screens) — it returns the resulting \
                 screen, so a task takes a few turns, not one per tap. \
                 Tool routing: phone_tap_label resolves one unique visible label from a fresh read; \
                 phone_tap_element uses an index and token from the same snapshot for ambiguous controls; \
                 phone_tap is for pixel-only targets. phone_scroll moves one viewport, phone_scroll_find \
                 finds one label, and phone_collect_list reads several pages. phone_screen is the person's \
                 live panel; phone_screenshot is one model-visible image; phone_screen_frame belongs only to \
                 the panel. Screenshots only when pixels matter. Diagnostic/install/recording tools are for \
                 tasks that request those capabilities, not ordinary navigation.\n\
                 4. Verify each step; `retry_safe:false` means never replay.\n\
                 5. A response carrying `flow_suggestion`, or any finished multi-step task with no flow: \
                 ASK THE USER whether to save it. Only if they agree: phone_flow_draft, fix its todo list, \
                 phone_flow_run the file once, then phone_flow_publish(confirm=true) with their OK.\n\
                 Call phone_release_owner when the task is done: it also frees the phone (the device runner stops, so the \
                 phone leaves automation mode); the next task's first phone tool brings it back.",
            )
    }
}

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

fn phone_locator_has_condition(locator: &PhoneElementLocator) -> bool {
    [
        locator.label.as_deref(),
        locator.label_contains.as_deref(),
        locator.identifier.as_deref(),
        locator.kind.as_deref(),
        locator.value.as_deref(),
    ]
    .into_iter()
    .flatten()
    .all(|text| !text.contains('\0'))
        && locator
            .label_contains
            .as_ref()
            .is_none_or(|text| !text.is_empty() && text.chars().count() <= 1000)
        && (locator.label_contains.is_some()
            || locator.label.is_some()
            || locator.identifier.is_some()
            || locator.kind.is_some()
            || locator.value.is_some()
            || locator.focused.is_some()
            || locator.enabled.is_some()
            || locator.visible.is_some())
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
    // An OCR wait (a game's loading screen) may run longer; the daemon checks the same.
    const MAX_OCR_WAIT_MS: u64 = 60_000;
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
            PhoneStep::MultiLongpress {
                points,
                duration_ms,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if !(2..=5).contains(&points.len())
                    || !(1..=10_000).contains(&duration_ms)
                    || points.iter().enumerate().any(|(i, p)| {
                        !p.x.is_finite()
                            || !p.y.is_finite()
                            || !(0.0..=1.0).contains(&p.x)
                            || !(0.0..=1.0).contains(&p.y)
                            || points[..i].iter().any(|q| q.x == p.x && q.y == p.y)
                    })
                {
                    return Err(format!("steps[{index}] multi_longpress needs 2–5 distinct normalized points and duration_ms from 1 to 10000; no action was sent"));
                }
                action_step(
                    serde_json::json!({"type":"multi_longpress", "points":points, "duration_ms":duration_ms}),
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
                points,
                hold_ms,
                duration_ms,
                after_ms,
            } => {
                validate_after(after_ms)?;
                if let Some(points) = points {
                    if [x1, y1, x2, y2].iter().any(Option::is_some) {
                        return Err(format!("steps[{index}] drag needs either points or x1/y1/x2/y2, not both; no action was sent"));
                    }
                    let action = serde_json::json!({"type":"drag","points":points,"hold_ms":hold_ms,"duration_ms":duration_ms});
                    iu_core::drag::Path::parse(&action)
                        .map_err(|error| format!("steps[{index}] {error}; no action was sent"))?;
                    action_step(action, after_ms)
                } else {
                    let (Some(x1), Some(y1), Some(x2), Some(y2)) = (x1, y1, x2, y2) else {
                        return Err(format!("steps[{index}] drag needs points or all four x1/y1/x2/y2 coordinates; no action was sent"));
                    };
                    if ![x1, y1, x2, y2]
                        .into_iter()
                        .all(|v| v.is_finite() && (0.0..=1.0).contains(&v))
                        || hold_ms > 10_000
                        || !(1..=10_000).contains(&duration_ms)
                        || (x1 == x2 && y1 == y2)
                    {
                        return Err(format!(
                            "steps[{index}] drag geometry or timing is invalid; no action was sent"
                        ));
                    }
                    action_step(
                        serde_json::json!({"type":"drag","x1":x1,"y1":y1,"x2":x2,"y2":y2,"hold_ms":hold_ms,"duration_ms":duration_ms}),
                        after_ms,
                    )
                }
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
            PhoneStep::Type { text, clear, budget_ms, after_ms } => {
                validate_after(after_ms)?;
                let action = TypeParams {text:Some(text), clear:Some(clear), budget_ms,
                    continue_token:None, observe:None}.action()
                    .map_err(|error| format!("steps[{index}] {error}; no action was sent"))?;
                action_step(action, after_ms)
            }
            PhoneStep::TypeContinue { continue_token, after_ms } => {
                validate_after(after_ms)?;
                let action = TypeParams {text:None, clear:None, budget_ms:None,
                    continue_token:Some(continue_token), observe:None}.action()
                    .map_err(|error| format!("steps[{index}] {error}; no action was sent"))?;
                action_step(action, after_ms)
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
                if !matches!(name.as_str(), "home" | "spotlight" | "save_screenshot" | "volume_up" | "volume_down") {
                    return Err(format!(
                        "steps[{index}] has unsupported shortcut {name:?}; supported: home, spotlight, save_screenshot, volume_up, volume_down; no action was sent"
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
                let tree = expect.application.is_some()
                    || !expect.present.is_empty()
                    || !expect.absent.is_empty();
                let ocr = !expect.ocr_present.is_empty() || !expect.ocr_absent.is_empty();
                if !tree && !ocr {
                    return Err(format!(
                        "steps[{index}].expect must include application, present, absent, ocr_present, or ocr_absent; no action was sent"
                    ));
                }
                if tree && ocr {
                    return Err(format!(
                        "steps[{index}].expect mixes element locators with ocr_present/ocr_absent; use two wait_for steps; no action was sent"
                    ));
                }
                if expect
                    .ocr_present
                    .iter()
                    .chain(expect.ocr_absent.iter())
                    .any(|text| text.trim().is_empty() || text.chars().count() > 200)
                {
                    return Err(format!(
                        "steps[{index}] ocr_present/ocr_absent entries must be 1 to 200 characters of visible text; no action was sent"
                    ));
                }
                if expect.absent_after_present
                    && (expect.absent.is_empty() && expect.ocr_absent.is_empty() || timeout_ms == 0)
                {
                    return Err(format!("steps[{index}]: absent_after_present needs absent/ocr_absent and a nonzero timeout; no action was sent"));
                }
                let max_wait = if ocr { MAX_OCR_WAIT_MS } else { MAX_WAIT_MS };
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
                if (ocr && timeout_ms == 0) || timeout_ms > max_wait {
                    return Err(format!(
                        "steps[{index}].timeout_ms must be between {} and {max_wait}; zero selects one bounded tree read; no action was sent", if ocr {1} else {0}
                    ));
                }
                if !(50..=1_000).contains(&poll_ms) {
                    return Err(format!(
                        "steps[{index}].poll_ms must be between 50 and 1000; no action was sent"
                    ));
                }
                declared_wait_ms = declared_wait_ms.saturating_add(if timeout_ms == 0 {iu_core::SINGLE_READ_WAIT_BUDGET_MS} else {timeout_ms});
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
        let capture_requested = json.pointer("/system_capture/requested").and_then(serde_json::Value::as_bool) == Some(true)
            || json.get("steps").and_then(serde_json::Value::as_array).is_some_and(|steps|
                steps.iter().any(|step| step.pointer("/system_capture/requested").and_then(serde_json::Value::as_bool) == Some(true)));
        if capture_requested {
            result.content.insert(0, Content::text("System screenshot requested; saving to Photos is not independently verified. If a full-screen preview is open, inspect it and save that existing image with Done/Save to Photos. Check Photos or its picker; do not take another screenshot to finish the pending save."));
        }
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
    let has_flows = serde_json::from_str::<serde_json::Value>(&body).ok().is_some_and(|value| {
        value.get("registry").and_then(|registry| registry.get("flows"))
            .and_then(serde_json::Value::as_array).is_some_and(|flows| !flows.is_empty())
    });
    if !has_flows { return body; }
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
    #[test]
    fn public_type_schema_keeps_budget_bounds_and_token_pattern() {
        let tool = listed_tools(&ToolProfile::All)
            .into_iter()
            .find(|tool| tool.name.as_ref() == "phone_type")
            .unwrap();
        let schema = serde_json::to_value(tool.input_schema.as_ref()).unwrap();
        assert_eq!(
            schema["properties"]["budget_ms"]["minimum"].as_f64(),
            Some(6000.0)
        );
        assert_eq!(
            schema["properties"]["budget_ms"]["maximum"].as_f64(),
            Some(15000.0)
        );
        assert_eq!(
            schema["properties"]["continue_token"]["pattern"],
            "^[0-9a-f]{64}$"
        );
        assert_eq!(schema["properties"]["continue_token"]["minLength"], 64);
        assert_eq!(schema["properties"]["continue_token"]["maxLength"], 64);
    }

    #[test]
    fn text_request_keeps_continuation_separate_from_new_input() {
        let encode = |value| {
            serde_json::from_value::<TypeParams>(value)
                .ok()
                .and_then(|params| params.action().ok())
        };
        let token = "a".repeat(64);
        assert_eq!(
            encode(serde_json::json!({"continue_token":token,"observe":false})).unwrap(),
            serde_json::json!({"type":"text","continue_token":token})
        );
        assert_eq!(
            encode(serde_json::json!({"text":"界","budget_ms":6000,"clear":true})).unwrap(),
            serde_json::json!({"type":"text","text":"界","budget_ms":6000,"clear":true})
        );
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"text":"new","continue_token":token}),
            serde_json::json!({"continue_token":token,"clear":false}),
            serde_json::json!({"continue_token":token,"budget_ms":6000}),
            serde_json::json!({"continue_token":"A".repeat(64)}),
            serde_json::json!({"text":"new","budget_ms":5999}),
            serde_json::json!({"text":"new","budget_ms":15001}),
            serde_json::json!({"text":"new","budget_ms":6000.5}),
        ] {
            assert!(encode(bad.clone()).is_none(), "{bad}");
        }
        let request = phone_steps_request(vec![serde_json::from_value::<PhoneStep>(
            serde_json::json!({"kind":"type_continue","continue_token":token}),
        )
        .unwrap()])
        .unwrap();
        assert_eq!(
            request["steps"][0]["action"],
            serde_json::json!({"type":"text","continue_token":token})
        );
        assert!(serde_json::from_value::<PhoneStep>(
            serde_json::json!({"kind":"type_continue","continue_token":token,"text":"forbidden"})
        )
        .is_err());
    }

    #[test]
    fn multi_hold_steps_validate_all_fingers_before_dispatch() {
        let encode = |v| phone_steps_request(vec![serde_json::from_value::<PhoneStep>(v).unwrap()]);
        let action = serde_json::json!({"kind":"multi_longpress", "points":[
            {"x":0.3,"y":0.4},{"x":0.5,"y":0.4},{"x":0.7,"y":0.4}],"duration_ms":2500});
        let request = encode(action.clone()).unwrap();
        assert_eq!(request["steps"][0]["action"]["type"], "multi_longpress");
        assert_eq!(request["steps"][0]["action"]["duration_ms"], 2500);
        for points in [
            serde_json::json!([]),
            serde_json::json!([{"x":0.3,"y":0.4}]),
            serde_json::json!([{"x":0.3,"y":0.4},{"x":0.3,"y":0.4}]),
            serde_json::json!([{"x":0.3,"y":0.4},{"x":1.1,"y":0.4}]),
        ] {
            let mut bad = action.clone();
            bad["points"] = points;
            assert!(encode(bad).is_err());
        }
        let mut defaulted = action.clone();
        defaulted.as_object_mut().unwrap().remove("duration_ms");
        assert_eq!(
            encode(defaulted).unwrap()["steps"][0]["action"]["duration_ms"],
            1000
        );
        let mut bad = action;
        bad["duration_ms"] = serde_json::json!(10001);
        assert!(encode(bad).is_err());
    }

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

    #[tokio::test]
    async fn exit_releases_only_a_lease_this_process_holds() {
        // #414: a closed client used to leave its lease for the rest of the 300 s.
        if std::env::var("IPHONE_USE_OWNER").is_ok() {
            return;
        }
        let me = DaemonClient::new("http://127.0.0.1:1", None).owner().to_string();
        let ours = format!(r#"{{"ok":true,"owner":"{me}","owner_lease_remaining_secs":180}}"#);
        let (url, server, requests) = crate::client::tests::mock_daemon_sequence(&[
            ("200 OK", &ours),
            ("200 OK", r#"{"ok":true,"owner":null}"#),
        ]);
        PhoneHandler::new(DaemonClient::new(url, None)).release_own_leases().await;
        server.join().unwrap();
        let status = requests.recv().unwrap();
        let release = requests.recv().unwrap();
        assert!(status.starts_with("GET /agent/status"), "{status}");
        assert!(release.starts_with("POST /agent/owner"), "{release}");
        assert!(release.to_ascii_lowercase().contains(&format!("x-phone-owner: {}", me.to_ascii_lowercase())), "{release}");
        assert!(release.contains(r#"{"release":true}"#), "{release}");

        // Another session's lease, or none: read, never release.
        for other in [r#"{"ok":true,"owner":"mcp-1","owner_lease_remaining_secs":180}"#, r#"{"ok":true,"owner":null}"#] {
            let (url, server, requests) = crate::client::tests::mock_daemon_sequence(&[("200 OK", other)]);
            PhoneHandler::new(DaemonClient::new(url, None)).release_own_leases().await;
            server.join().unwrap();
            assert!(requests.recv().unwrap().starts_with("GET /agent/status"));
            assert!(requests.try_recv().is_err(), "must not release a lease it does not hold: {other}");
        }
    }

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

    #[test]
    fn volume_shortcuts_are_sent_to_the_selected_daemon() {
        for name in ["volume_up", "volume_down"] {
            let (url, task) = scripted_daemon("200 OK", br#"{"ok":true}"#.to_vec());
            let result = block(handler_for(&url).phone_shortcut(Parameters(ShortcutParams {
                name: format!("  {}  ", name.to_uppercase()),
                observe: Some(false),
            })));
            assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
            task.join().unwrap();
            let step: PhoneStep =
                serde_json::from_value(serde_json::json!({"kind":"shortcut","name":name})).unwrap();
            let request = phone_steps_request(vec![step]).unwrap();
            assert_eq!(request["steps"][0]["action"]["name"], name);
        }
    }

    fn metrics_run(owner: &str, incarnation: u64, end: u64) -> serde_json::Value {
        serde_json::json!({"key":{"kind":"explicit","owner":owner,
            "run_id":format!("run-{incarnation}"),"incarnation":incarnation},
            "started_ms":1,"ended_ms":end,"tool_calls":3,"stored_calls":2,
            "incomplete":true,"dropped_events":1,"outcome_unknown":1,
            "model_round_trips":null,"failures":{"runner_error":1}})
    }

    fn metrics_reply(body: serde_json::Value, limit: Option<usize>) -> CallToolResult {
        let (url, task) = scripted_daemon("200 OK", serde_json::to_vec(&body).unwrap());
        let handler = PhoneHandler::new(DaemonClient::new(url, None).with_owner("metrics-owner"));
        let result = block(handler.phone_metrics(Parameters(MetricsParams { limit })));
        task.join().unwrap();
        result
    }

    #[test]
    fn metrics_keeps_latest_runs_and_unknown_loss_fields_in_slim_mode() {
        let result = metrics_reply(serde_json::json!({"ok":true,"open":[
            metrics_run("metrics-owner",1,10),metrics_run("metrics-owner",3,30),
            metrics_run("metrics-owner",2,20)],"recent":[],"stats":{"unattributed_events":99}}), Some(1));
        assert_ne!(result.is_error, Some(true));
        let slim = finish_result("phone_metrics", result, StructuredMode::Slim);
        let report = slim.structured_content.unwrap();
        assert_eq!(report["open"][0]["key"]["run_id"], "run-3");
        assert_eq!(report["open"][0]["dropped_events"], 1);
        assert_eq!(report["open"][0]["outcome_unknown"], 1);
        assert!(report["open"][0]["model_round_trips"].is_null());
        assert_eq!(report["open_omitted"], 2);
        assert_eq!(report["truncated"], true);
        assert!(report.get("stats").is_none());
    }

    #[test]
    fn metrics_rejects_other_owner_without_returning_their_run_data() {
        let result = metrics_reply(serde_json::json!({"ok":true,
            "open":[metrics_run("private-other-owner",1,10)],"recent":[]}), None);
        assert_eq!(result.is_error, Some(true));
        assert!(!text_of(&result).contains("private-other-owner"));
        assert!(result.structured_content.is_none());
    }

    #[test]
    fn metrics_reports_byte_budget_omissions_without_forwarding_oversized_data() {
        let mut run = metrics_run("metrics-owner",1,10);
        run["extra"] = serde_json::json!("x".repeat(13 * 1024));
        let result = metrics_reply(serde_json::json!({"ok":true,"open":[run],"recent":[]}), None);
        assert_ne!(result.is_error, Some(true));
        let report = result.structured_content.unwrap();
        assert_eq!(report["open"], serde_json::json!([]));
        assert_eq!(report["open_omitted"], 1);
        assert_eq!(report["truncated"], true);
    }

    #[test]
    fn metrics_rejects_malformed_run_identity_instead_of_inventing_timing() {
        let mut run = metrics_run("metrics-owner",1,10);
        run.as_object_mut().unwrap().remove("started_ms");
        let result = metrics_reply(serde_json::json!({"ok":true,"open":[run],"recent":[]}), None);
        assert_eq!(result.is_error, Some(true));
        assert!(result.structured_content.is_none());
    }

    #[test]
    fn metrics_invalid_limit_is_rejected_before_contact() {
        let handler = handler_for("http://127.0.0.1:1");
        for limit in [0, 21] {
            let result = block(handler.phone_metrics(Parameters(MetricsParams { limit:Some(limit) })));
            assert_eq!(result.is_error, Some(true));
            assert!(text_of(&result).contains("limit must be"));
        }
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
            r#"{{"ok":true,"transport":"runner","tree":"{filler}","snapshot":"snap-1","settle":{{"settled":true,"reason":"stable","captures":2}},"delta":{{"added":["搜索"]}}}}"#
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

    #[test]
    fn system_capture_mcp_preserves_unverified_save_and_explicit_notice() {
        let (url, task) = scripted_daemon("200 OK", br#"{"ok":true,"transport":"runner","system_capture":{"requested":true,"destination":"iPhone Photos","saved_verified":false}}"#.to_vec());
        let handler = handler_for(&url);
        let result = block(handler.phone_shortcut(Parameters(ShortcutParams {
            name: " SAVE_SCREENSHOT ".into(), observe: Some(false),
        })));
        task.join().unwrap();
        assert_ne!(result.is_error, Some(true));
        assert!(text_of(&result).contains("not independently verified"));
        assert_eq!(result.structured_content.unwrap()["system_capture"]["saved_verified"], false);
    }

    #[test]
    fn value_read_unavailability_reason_reaches_every_output_mode_without_a_preview() {
        let body=serde_json::json!({"ok":false,"error":"full_value_unavailable","reason":"ax_attribute_unavailable",
            "hint":"The element did not supply a complete AX attribute; do not infer empty or null.","value":"PREVIEW"});
        let (url,task)=scripted_daemon("501 Not Implemented",body.to_string().into_bytes());
        let result=block(handler_for(&url).phone_read_value(Parameters(ReadValueParams {
            identifier:Some("field-a".into()),label:None,kind:None,max_utf16:None,
        })));
        task.join().unwrap();
        for mode in [StructuredMode::Full,StructuredMode::Slim,StructuredMode::Off] {
            let shaped=shape_structured(result.clone(),mode);
            assert_eq!(shaped.is_error,Some(true));
            let text=shaped.content.iter().filter_map(|c| c.as_text().map(|t|t.text.as_str())).collect::<Vec<_>>().join("\n");
            let value:serde_json::Value=serde_json::from_str(&text).unwrap();
            assert_eq!(value["reason"],"ax_attribute_unavailable");
            assert!(value.get("value").is_none());
            assert!(!text.contains("PREVIEW"));
            if mode != StructuredMode::Off {
                let structured=shaped.structured_content.unwrap();
                assert_eq!(structured["reason"],"ax_attribute_unavailable");
                assert!(structured.get("value").is_none());
            }
        }
    }

    #[test]
    fn system_capture_receipt_survives_default_structured_slim() {
        let (url, task) = scripted_daemon("200 OK", br#"{"ok":true,"system_capture":{"requested":true,"saved_verified":false,"image":"PRIVATE","asset":"PRIVATE"}}"#.to_vec());
        let result = block(handler_for(&url).phone_shortcut(Parameters(ShortcutParams {
            name: "save_screenshot".into(), observe: Some(false),
        })));
        task.join().unwrap();
        let shaped = shape_structured(result, StructuredMode::Slim);
        let structured = shaped.structured_content.unwrap();
        assert_eq!(structured["system_capture"], serde_json::json!({"requested":true,"saved_verified":false}));
        assert!(!structured.to_string().contains("PRIVATE"));
    }

    #[test]
    fn system_capture_batch_validation_accepts_explicit_mutation() {
        let step: PhoneStep = serde_json::from_value(serde_json::json!({"kind":"shortcut","name":"save_screenshot"})).unwrap();
        let request = phone_steps_request(vec![step]).unwrap();
        assert_eq!(request["steps"][0]["action"]["name"], "save_screenshot");
    }

    /// With `observe:false` the result stays the short string callers
    /// already parse, and the request must not have asked for a delta.
    #[test]
    fn an_unobserved_tap_keeps_its_plain_result() {
        let (url, task) = scripted_daemon(
            "200 OK",
            br#"{"ok":true,"transport":"runner"}"#.to_vec(),
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
    fn partial_batch_continuation_is_rendered_without_resending_text() {
        let token="a".repeat(64);
        let body=serde_json::json!({"ok":false,"error":"text_partially_typed","failed_step":0,
            "completed":0,"applied_actions":0,"failed_step_outcome":"partial","batch_outcome":"partially_applied",
            "retry_safe":false,"steps":[],"characters_confirmed":200,"characters_total":500,
            "characters_uncertain":0,"remaining_text":"字".repeat(300),"continue_token":token,"continuation_safe":true});
        let (url,task)=scripted_daemon("502 Bad Gateway",body.to_string().into_bytes());
        let handler=handler_for(&url);
        let result=block(handler.phone_run_steps(Parameters(RunStepsParams{
            steps:vec![PhoneStep::Type{text:"new".into(),clear:false,budget_ms:None,after_ms:0}],
            observe:Some(false)})));
        task.join().unwrap();
        let text=text_of(&result);
        assert!(text.contains(&token),"{text}");
        assert!(text.contains("omit text, clear and budget_ms"),"{text}");
        let result = finish_result("phone_run_steps", result, StructuredMode::Slim);
        assert_eq!(result.structured_content.as_ref().unwrap()["continue_token"],token);
        assert_eq!(result.structured_content.as_ref().unwrap()["remaining_text"], "字".repeat(300));
    }

    #[test]
    fn a_partially_typed_text_reaches_the_model_with_its_counts() {
        let (url, task) = scripted_daemon("502 Bad Gateway", partial_text_body(serde_json::json!({})));
        let handler = handler_for(&url);
        // The request stays small (the scripted daemon reads one 8 KiB
        // buffer); the answer is what is under test.
        let result = block(handler.phone_type(Parameters(TypeParams {
            text: Some("x".repeat(600)),
            continue_token: None, budget_ms: None, clear: None,
            observe: Some(false),
        })));
        task.join().unwrap();
        assert_eq!(result.is_error, Some(true));
        let text = text_of(&result);
        assert!(text.contains("retry_safe=false"), "{text}");
        assert!(text.contains("characters_confirmed"), "{text}");
        let result = finish_result("phone_type", result, StructuredMode::Slim);
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
                budget_ms: None,
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

    #[test]
    fn home_layout_preserves_failures_and_page_index_space() {
        assert!(
            serde_json::from_value::<HomeLayoutParams>(serde_json::json!({"pages":0})).is_err()
        );
        for (status, body, fails) in [
            ("503 Service Unavailable", br#"{"ok":false,"error":"home_layout_unavailable"}"#.to_vec(), true),
            ("200 OK", b"<html>proxy error</html>".to_vec(), true),
            ("200 OK", br#"{"ok":true,"page_index_space":"persisted_icon_state","pages":[{"page":0,"positions_complete":false}]}"#.to_vec(), false),
        ] {
            let (url, task)=scripted_daemon(status,body);
            let result=block(handler_for(&url).phone_home_layout(Parameters(HomeLayoutParams{page:Some(0),action:None,file:None,confirm:false})));
            task.join().unwrap();
            assert_eq!(result.is_error==Some(true),fails,"{}",text_of(&result));
            if !fails { assert_eq!(result.structured_content.unwrap()["page_index_space"],"persisted_icon_state"); }
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

        let expected: Vec<String> = serde_json::from_str(include_str!("../../../scripts/fixtures/mcp-tools.json")).unwrap();
        let expected_refs: Vec<&str> = expected.iter().map(String::as_str).collect();
        assert_eq!(names, expected_refs, "tool contract changed; update the shared fixture, README and skill");
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
    const READ_ONLY_TOOLS: [&str; 17] = [
        "phone_apps",
        "phone_capabilities",
        "phone_crashes",
        "phone_elements",
        "phone_flow_draft",
        "phone_flow_info",
        "phone_flow_list",
        "phone_home_layout",
        "phone_instances",
        "phone_metrics",
        "phone_read_value",
        "phone_run_trace",
        "phone_screen",
        "phone_screen_frame",
        "phone_screenshot",
        "phone_status",
        "phone_subtree",
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
        assert_eq!(
            read_only,
            READ_ONLY_TOOLS
                .into_iter()
                .filter(|name| *name != "phone_flow_draft" && *name != "phone_home_layout")
                .collect::<Vec<_>>()
        );

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
    fn draft_discovery_matches_profile_write_permissions() {
        for (profile, read_only) in [
            (ToolProfile::All, false),
            (ToolProfile::ReadOnly, true),
            (
                ToolProfile::Allow(["phone_flow_draft".to_string()].into()),
                false,
            ),
        ] {
            let tool = listed_tools(&profile)
                .into_iter()
                .find(|tool| tool.name == "phone_flow_draft")
                .expect("draft remains available in each profile");
            let wire = serde_json::to_value(tool).unwrap();
            assert_eq!(wire["annotations"]["readOnlyHint"], read_only);
            assert_eq!(wire["annotations"]["destructiveHint"], false);
            assert_eq!(
                wire["inputSchema"]["properties"].get("save_as").is_none(),
                read_only
            );
            if read_only {
                assert!(wire["description"]
                    .as_str()
                    .unwrap()
                    .contains("no file is written"));
            }
        }
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

    /// `core` is the whole driving surface minus the six developer tools;
    /// a hidden one is refused with a message that names the setting.
    #[test]
    fn the_core_profile_hides_only_the_developer_tools() {
        let (profile, warnings) = ToolProfile::parse(Some("core"));
        assert_eq!(profile, ToolProfile::Core);
        assert!(warnings.is_empty(), "{warnings:?}");
        let all = listed_names(&ToolProfile::All);
        let core = listed_names(&profile);
        assert_eq!(core.len(), all.len() - DEVELOPER_TOOLS.len());
        for name in DEVELOPER_TOOLS {
            assert!(all.contains(&name.to_string()), "{name} is not a tool");
            assert!(!core.contains(&name.to_string()), "{name} listed under core");
            let text = text_of(&profile.refusal(name).expect("hidden"));
            assert!(text.contains("IPHONE_USE_MCP_TOOLS=core"), "{text}");
        }
        for name in ["phone_run_steps", "phone_screenshot", "phone_flow_publish", "phone_screen_frame"] {
            assert!(core.contains(&name.to_string()), "{name} missing from core");
            assert!(profile.refusal(name).is_none());
        }
    }

    /// The generated schema's `Option<T>` noise and repeated property
    /// objects are gone from what a client is handed; the `instance`
    /// parameter is short.
    #[test]
    fn listed_schemas_are_compact() {
        let tools = listed_tools(&ToolProfile::All);
        let wire = serde_json::to_string(&tools).unwrap();
        assert!(!wire.contains("\"default\":null"), "null defaults survived");
        assert!(!wire.contains(",\"null\"]"), "nullable type arrays survived");
        assert!(!wire.contains("```"), "a doctest fence survived");
        let run_steps = tools.iter().find(|t| t.name == "phone_run_steps").unwrap();
        let defs = run_steps.input_schema["$defs"].as_object().unwrap();
        assert!(defs.contains_key("after_ms"), "after_ms was not hoisted: {:?}", defs.keys());
        let text = serde_json::to_string(&run_steps.input_schema).unwrap();
        assert!(text.matches("\"$ref\":\"#/$defs/after_ms\"").count() >= 10, "{text}");
        let tap = tools.iter().find(|t| t.name == "phone_tap").unwrap();
        assert_eq!(tap.input_schema["properties"]["instance"]["description"], "daemon instance name");
        assert_eq!(tap.input_schema["properties"]["observe"]["type"], "boolean");
        let elements = tools.iter().find(|t| t.name == "phone_elements").unwrap();
        // `Option<ElementSelector>` keeps its `$ref` and description, not the null arm.
        let selector = &elements.input_schema["properties"]["selector"];
        assert!(selector.get("anyOf").is_none(), "{selector}");
        assert_eq!(selector["$ref"], "#/$defs/ElementSelector");
        assert!(selector["description"].is_string());
        for tool in &tools {
            let description = tool.description.as_deref().unwrap_or_default();
            assert!(description.len() <= 600, "{} description is {} chars", tool.name, description.len());
        }
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
        assert_eq!(properties["include_hidden"]["type"], "boolean");
        assert!(!serde_json::from_value::<ElementsParams>(serde_json::json!({})).unwrap().include_hidden);
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
                    label_contains: None,
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
                        label_contains: None,
                        label: Some("搜索".to_string()),
                        identifier: None,
                        kind: Some("TextField".to_string()),
                        value: None,
                        focused: Some(true),
                        enabled: None,
                        visible: None,
                    }.into()],
                    absent: vec![],
                    ocr_present: vec![],
                    ocr_absent: vec![],
                absent_after_present: false,
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
    fn drag_waypoints_validate_before_any_batch_is_sent() {
        let valid = serde_json::json!({"kind":"drag","points":[
            {"x":0.3,"y":0.4},{"x":0.13,"y":0.5,"dwell_ms":900},{"x":0.5,"y":0.4}
        ],"hold_ms":400,"duration_ms":300});
        let request =
            phone_steps_request(vec![serde_json::from_value(valid.clone()).unwrap()]).unwrap();
        let expected = valid["points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                let mut p = p.clone();
                if p.get("dwell_ms").is_none() {
                    p["dwell_ms"] = serde_json::json!(0);
                }
                p
            })
            .collect::<Vec<_>>();
        assert_eq!(
            request["steps"][0]["action"]["points"].as_array().unwrap(),
            &expected
        );
        for change in [
            serde_json::json!({"x1":0.1}),
            serde_json::json!({"points":[]}),
            serde_json::json!({"hold_ms":10000}),
        ] {
            let mut step = valid.clone();
            for (key, value) in change.as_object().unwrap() {
                step[key] = value.clone();
            }
            let step = serde_json::from_value(step).unwrap();
            assert!(phone_steps_request(vec![step]).is_err());
        }
        let mut unknown = valid;
        unknown["points"][0]["wait_ms"] = serde_json::json!(1);
        assert!(serde_json::from_value::<PhoneStep>(unknown).is_err());
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
                x1: Some(0.2),
                y1: Some(0.5),
                x2: Some(0.8),
                y2: Some(0.5),
                points: None,
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
            budget_ms: None,
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
    fn wait_schema_exposes_a_flat_bounded_nonnullable_index_only_for_waits() {
        let schema = serde_json::to_value(schemars::schema_for!(PhoneWaitSelector)).unwrap();
        assert_eq!(schema["additionalProperties"], false, "{schema}");
        assert!(schema["properties"].get("label_contains").is_some());
        let field = &schema["properties"]["index"];
        let index = field
            .get("$ref")
            .and_then(serde_json::Value::as_str)
            .map(|reference| schema.pointer(reference.trim_start_matches('#')).unwrap())
            .unwrap_or(field);
        assert_eq!(index["type"], "integer", "{schema}");
        assert_eq!(index["minimum"].as_f64(), Some(0.0));
        assert_eq!(index["maximum"].as_f64(), Some(199.0));
        assert!(!schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|fields| fields.contains(&serde_json::json!("index"))));
        let action = serde_json::to_value(schemars::schema_for!(PhoneElementLocator)).unwrap();
        assert!(action["properties"].get("index").is_none());
    }

    #[test]
    fn wait_index_is_bounded_and_cannot_be_used_as_an_action_locator() {
        let steps:Vec<PhoneStep>=serde_json::from_value(serde_json::json!([{"kind":"wait_for","expect":{"present":[{"label_contains":"Save","index":2}]},"timeout_ms":0}])).unwrap();
        let body = phone_steps_request(steps).unwrap();
        assert_eq!(body["steps"][0]["expect"]["present"][0]["index"], 2);
        for index in [
            serde_json::json!(200),
            serde_json::json!(-1),
            serde_json::json!(true),
            serde_json::Value::Null,
            serde_json::json!(1.5),
        ] {
            let steps = serde_json::json!([{"kind":"wait_for","expect":{"present":[{"label":"Save","index":index}]},"timeout_ms":0}]);
            assert!(serde_json::from_value::<Vec<PhoneStep>>(steps).is_err());
        }
        let steps:Vec<PhoneStep>=serde_json::from_value(serde_json::json!([{"kind":"wait_for","expect":{"present":[{"index":2}]},"timeout_ms":0}])).unwrap();
        assert!(phone_steps_request(steps).is_err());
        let action = serde_json::json!([{"kind":"tap_locator","locator":{"label_contains":"Save","index":1}}]);
        assert!(serde_json::from_value::<Vec<PhoneStep>>(action).is_err());
    }

    #[test]
    fn literal_contains_and_single_read_wait_are_encoded_and_budgeted() {
        let steps:Vec<PhoneStep>=serde_json::from_value(serde_json::json!([{"kind":"wait_for","expect":{"present":[{"label":"保存\n'O\\K'🙂","label_contains":"'O\\K'"}]},"timeout_ms":0,"poll_ms":50}])).unwrap();
        let body = phone_steps_request(steps).unwrap();
        assert_eq!(body["steps"][0]["timeout_ms"], 0);
        assert_eq!(
            body["steps"][0]["expect"]["present"][0]["label_contains"],
            "'O\\K'"
        );
        for fragment in [String::new(), "x".repeat(1001)] {
            let steps:Vec<PhoneStep>=serde_json::from_value(serde_json::json!([{"kind":"wait_for","expect":{"present":[{"identifier":"save","label_contains":fragment}]},"timeout_ms":0}])).unwrap();
            assert!(phone_steps_request(steps).is_err());
        }
        let steps: Vec<PhoneStep> = serde_json::from_value(serde_json::json!([
            {"kind":"wait_for","expect":{"present":[{"label_contains":"Save"}]},"timeout_ms":0},
            {"kind":"wait_for","expect":{"ocr_present":["ready"]},"timeout_ms":60000}
        ]))
        .unwrap();
        assert!(phone_steps_request(steps)
            .unwrap_err()
            .contains("declared waits"));
        let steps:Vec<PhoneStep>=serde_json::from_value(serde_json::json!([{"kind":"wait_for","expect":{"ocr_present":["ready"]},"timeout_ms":0}])).unwrap();
        assert!(phone_steps_request(steps).is_err());
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
                ocr_present: vec![],
                ocr_absent: vec![],
                absent_after_present: false,
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
                ocr_present: vec![],
                ocr_absent: vec![],
                absent_after_present: false,
            },
            timeout_ms: 30_000,
            poll_ms: 100,
        }])
        .expect("the ceiling is a valid wait");

        let empty_locator = phone_steps_request(vec![PhoneStep::WaitFor {
            expect: PhoneUiExpectation {
                application: None,
                present: vec![PhoneElementLocator {
                    label_contains: None,
                    label: None,
                    identifier: None,
                    kind: None,
                    value: None,
                    focused: None,
                    enabled: None,
                    visible: None,
                }.into()],
                absent: vec![],
                ocr_present: vec![],
                ocr_absent: vec![],
                absent_after_present: false,
            },
            timeout_ms: 1_000,
            poll_ms: 100,
        }])
        .unwrap_err();
        assert!(empty_locator.contains("empty element locator"));
    }

    #[test]
    fn a_waiting_reconnect_stops_on_drivable_or_a_blocker() {
        let status = |value: serde_json::Value| -> crate::types::StatusResponse {
            serde_json::from_value(value).unwrap()
        };
        let building = status(serde_json::json!({"ok": true, "drivable": false,
            "reconnecting": true, "setup_phase": "building"}));
        assert_eq!(reconnect_wait_outcome(&building), None);
        // drivable is reported only once the reconnect itself has finished.
        let racing = status(serde_json::json!({"ok": true, "drivable": true, "reconnecting": true}));
        assert_eq!(reconnect_wait_outcome(&racing), None);
        let ready = status(serde_json::json!({"ok": true, "drivable": true, "reconnecting": false}));
        assert_eq!(reconnect_wait_outcome(&ready), Some("drivable"));
        let blocked = status(serde_json::json!({"ok": true, "drivable": false,
            "reconnecting": true, "setup_blocked_on": "needs_passcode_on_phone"}));
        assert_eq!(reconnect_wait_outcome(&blocked), Some("blocked"));
        // The passcode ends the wait at once; a transient early blocker only
        // once it has held, or once the daemon stopped reconnecting.
        let long = std::time::Duration::from_secs(60);
        let fresh = |s: &crate::types::StatusResponse| BlockerWatch::observe(None, s);
        let aged = |s: &crate::types::StatusResponse| {
            let mut watch = BlockerWatch::observe(None, s);
            watch.since -= RECONNECT_BLOCKER_SETTLE;
            watch
        };
        assert_eq!(reconnect_blocker_final(&blocked, &fresh(&blocked), long), Some("blocked"));
        let ddi = status(serde_json::json!({"ok": true, "drivable": false,
            "reconnecting": true, "setup_blocked_on": "usb", "setup_phase": "ddi-wait"}));
        assert_eq!(reconnect_wait_outcome(&ddi), Some("blocked"));
        assert_eq!(reconnect_blocker_final(&ddi, &fresh(&ddi), long), None);
        assert_eq!(reconnect_blocker_final(&ddi, &aged(&ddi), long), Some("blocked"));
        let gave_up = status(serde_json::json!({"ok": true, "drivable": false,
            "reconnecting": false, "setup_blocked_on": "usb"}));
        assert_eq!(reconnect_blocker_final(&gave_up, &fresh(&gave_up), long), Some("blocked"));
        let brief = reconnect_brief(&blocked);
        assert_eq!(brief["setup_blocked_on"], "needs_passcode_on_phone");
        assert_eq!(brief["drivable"], false);
    }

    #[test]
    fn only_a_released_phone_nobody_decided_about_is_brought_back_for_a_tool() {
        let status = |extra: serde_json::Value| -> crate::types::StatusResponse {
            let mut base = serde_json::json!({"ok": true, "drivable": false, "released": true,
                "reconnecting": false, "recovery_owner": "daemon"});
            base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            serde_json::from_value(base).unwrap()
        };
        assert!(may_reconnect_for_use(&status(serde_json::json!({})), "me"));
        assert!(may_reconnect_for_use(&status(serde_json::json!({"owner": "me"})), "me"));
        for refused in [
            serde_json::json!({"drivable": true, "released": false}),
            serde_json::json!({"released": false}),
            serde_json::json!({"human_handoff": true}),
            serde_json::json!({"ended_on_phone": true}),
            serde_json::json!({"runner_parked": true}),
            serde_json::json!({"releasing": true}),
            serde_json::json!({"recovery_owner": "external"}),
            serde_json::json!({"setup_blocked_on": "trust"}),
            serde_json::json!({"owner": "another-session"}),
        ] {
            assert!(!may_reconnect_for_use(&status(refused.clone()), "me"), "{refused}");
        }
        // Already coming back (a released phone whose reconnect is running).
        assert!(may_reconnect_for_use(&status(serde_json::json!({"reconnecting": true})), "me"));
        assert!(RECONNECT_ON_USE_TOOLS.contains(&"phone_elements"));
        for never in ["phone_status", "phone_release_owner", "phone_hold", "phone_flow_list", "phone_screen", "phone_reconnect",
            "phone_home_layout", "phone_apps", "phone_install_app", "phone_set_location"] {
            assert!(!RECONNECT_ON_USE_TOOLS.contains(&never), "{never}");
        }
        let tools: Vec<String> = PhoneHandler::tool_router().list_all().into_iter().map(|t| t.name.to_string()).collect();
        for tool in RECONNECT_ON_USE_TOOLS {
            assert!(tools.iter().any(|t| t == tool), "{tool} is not a tool");
        }
        let passcode = ReconnectWait {
            outcome: "blocked",
            json: serde_json::json!({"outcome": "blocked"}),
            status: Some(status(serde_json::json!({"setup_blocked_on": "needs_passcode_on_phone"}))),
        };
        let text = reconnect_on_use_stopped("phone_tap", &passcode).content[0].as_text().unwrap().text.clone();
        assert!(text.contains("phone_not_ready") && text.contains("never ask for the passcode"), "{text}");
        let args = |v: serde_json::Value| v.as_object().cloned();
        assert!(reconnect_on_use("phone_record", args(serde_json::json!({"action": "start"})).as_ref()));
        assert!(!reconnect_on_use("phone_record", args(serde_json::json!({"action": "status"})).as_ref()));
        assert!(!reconnect_on_use("phone_record", args(serde_json::json!({"action": "stop"})).as_ref()));
        assert!(reconnect_on_use("phone_flow_run", args(serde_json::json!({"id": "health/export"})).as_ref()));
        assert!(!reconnect_on_use("phone_flow_run", args(serde_json::json!({"id": "Not A Flow"})).as_ref()));
        assert!(reconnect_on_use("phone_elements", None));
        assert!(!may_reconnect_for_use(&status(serde_json::json!({"screen_private_secs": 30})), "me"));
        let refused = CallToolResult::error(vec![Content::text(r#"{"ok":false,"error":"released","reconnecting":false}"#)]);
        assert!(refused_as_released(&refused));
        assert!(!refused_as_released(&CallToolResult::success(vec![Content::text(r#"{"released":true}"#)])));
    }

    #[test]
    fn a_waiting_reconnect_waits_out_the_supervisors_own_retry() {
        // #403: a failed prerequisite round with a KeepAlive retry 5 s away.
        let status = |attempt: u64, next_in: u64| -> crate::types::StatusResponse {
            serde_json::from_value(serde_json::json!({"ok": true, "drivable": false,
                "reconnecting": false, "setup_blocked_on": "usb", "setup_phase": "prereq-fail",
                "setup_retry": {"kind": "generic", "attempt": attempt, "next_in_secs": next_in}}))
            .unwrap()
        };
        let long = std::time::Duration::from_secs(30);
        let first = status(1, 5);
        let watch = BlockerWatch::observe(None, &first);
        assert_eq!(reconnect_blocker_final(&first, &watch, long), None, "a retry is coming");
        // The retry ran and failed again, but the blocker has not held long.
        let again = status(2, 10);
        let watch = BlockerWatch::observe(Some(watch), &again);
        assert_eq!(watch.first_retry, Some(1));
        assert_eq!(reconnect_blocker_final(&again, &watch, long), None);
        let mut held = watch;
        held.since -= RECONNECT_BLOCKER_SETTLE;
        assert_eq!(reconnect_blocker_final(&again, &held, long), Some("blocked"));
        // A retry after the wait would end: not settled, and not a request for a person.
        let later = status(1, 45);
        assert_eq!(reconnect_blocker_final(&later, &BlockerWatch::observe(None, &later), long), Some("retrying"));
        assert_eq!(reconnect_brief(&later)["setup_retry"]["next_in_secs"], 45);
        let slow = status(4, 900);
        assert_eq!(reconnect_blocker_final(&slow, &BlockerWatch::observe(None, &slow), long), Some("blocked"));
    }

    #[test]
    fn a_waiting_reconnect_brief_without_retry_is_null() {
        let blocked: crate::types::StatusResponse = serde_json::from_value(serde_json::json!({
            "ok": true, "drivable": false, "reconnecting": true,
            "setup_blocked_on": "needs_passcode_on_phone"})).unwrap();
        assert!(reconnect_brief(&blocked)["setup_retry"].is_null());
    }

    #[test]
    fn an_ocr_wait_reaches_the_daemon_and_may_wait_a_minute() {
        let ocr = |present: Vec<&str>, application: Option<&str>, timeout_ms| {
            phone_steps_request(vec![PhoneStep::WaitFor {
                expect: PhoneUiExpectation {
                    application: application.map(str::to_string),
                    present: vec![],
                    absent: vec![],
                    ocr_present: present.into_iter().map(str::to_string).collect(),
                    ocr_absent: vec![],
                absent_after_present: false,
                },
                timeout_ms,
                poll_ms: 500,
            }])
        };
        let request = ocr(vec!["长按自动"], None, 60_000).expect("an OCR wait up to 60 s");
        assert_eq!(
            request["steps"][0]["expect"],
            serde_json::json!({"ocr_present": ["长按自动"]})
        );
        assert!(ocr(vec!["长按自动"], None, 60_001).unwrap_err().contains("timeout_ms"));
        assert!(ocr(vec!["长按自动"], Some("设置"), 5_000)
            .unwrap_err()
            .contains("two wait_for steps"));
        assert!(ocr(vec![" "], None, 5_000).is_err());
    }

    #[test]
    fn multi_step_request_rejects_excessive_total_declared_wait_offline() {
        let steps = (0..7)
            .map(|_| PhoneStep::WaitFor {
                expect: PhoneUiExpectation {
                    application: Some("设置".to_string()),
                    present: vec![],
                    absent: vec![],
                    ocr_present: vec![],
                    ocr_absent: vec![],
                absent_after_present: false,
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
        assert_eq!(props["count"]["minimum"], 0, "zero is a validation bound, not schema noise");
        assert_eq!(props["pct"]["minimum"], 1, "a meaningful bound is kept");
        assert_eq!(schema["required"], serde_json::json!(["x", "title"]));
    }

    #[test]
    fn schema_slimming_preserves_zero_bounds_in_definitions_and_normalized_ranges() {
        let mut schema=serde_json::json!({"$defs":{"ordinal":{"type":"integer","minimum":0,"maximum":199}},
            "properties":{"ratio":{"type":"number","minimum":0,"maximum":1}}});
        slim_schema(&mut schema);
        assert_eq!(schema["$defs"]["ordinal"]["minimum"],0);
        assert_eq!(schema["$defs"]["ordinal"]["maximum"],199);
        assert_eq!(schema["properties"]["ratio"]["minimum"],0);
        assert_eq!(schema["properties"]["ratio"]["maximum"],1);
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
        let expected: Vec<String> = serde_json::from_str(include_str!("../../../scripts/fixtures/mcp-tools.json")).unwrap();
        assert_eq!(tools.len(), expected.len(), "the whole documented surface");
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
    fn app_lifecycle_receipt_reaches_models_in_all_structured_modes() {
        for (status, launch_attempted, outcome) in [("409 Conflict", Some(false), "partial"), ("409 Conflict", Some(true), "partial"), ("502 Bad Gateway", Some(true), "unknown"), ("504 Gateway Timeout", None, "unknown")] {
            let mut receipt = serde_json::json!({"terminate_acknowledged":true});
            if let Some(sent) = launch_attempted { receipt["launch_attempted"] = sent.into(); }
            let body = serde_json::json!({"ok":false,"error":if outcome == "partial" {"app_launch_interrupted"} else {"outcome_unknown"}, "outcome":outcome,
                "failed_step":0,"completed":0,"applied_actions":0,"failed_step_outcome":outcome,
                "batch_outcome":if outcome == "partial" {"partially_applied"} else {"unknown"},
                "retry_safe":false,"steps":[],"app_lifecycle":receipt});
            let (url, task) = scripted_daemon(status, body.to_string().into_bytes());
            let result = block(handler_for(&url).phone_run_steps(Parameters(RunStepsParams {
                steps:vec![PhoneStep::LaunchApp {bundle:Some("test.fixture".into()),app:None,fresh:true,after_ms:0}],
                observe:Some(false),
            })));
            task.join().unwrap();
            for mode in [StructuredMode::Full, StructuredMode::Slim, StructuredMode::Off] {
                let shaped = shape_structured(result.clone(), mode);
                let text = shaped.content.iter().filter_map(|c| c.as_text().map(|t| t.text.as_str())).collect::<Vec<_>>().join("\n");
                assert!(text.contains("terminate_acknowledged=true"), "{mode:?}: {text}");
                match launch_attempted {
                    Some(sent) => assert!(text.contains(&format!("launch_attempted={sent}")), "{text}"),
                    None => assert!(!text.contains("launch_attempted="), "unknown must stay unknown: {text}"),
                }
                if mode != StructuredMode::Off {
                    assert_eq!(shaped.structured_content.as_ref().unwrap()["app_lifecycle"], receipt);
                }
            }
        }
    }

    #[test]
    fn a_slim_structure_keeps_how_far_a_replacement_got() {
        let mut result = CallToolResult::error(vec![Content::text("FAILED set_value")]);
        result.structured_content = Some(serde_json::json!({
            "ok": false,
            "error": "outcome_unknown",
            "replacement": {"path": "guarded", "phase": "type", "clear": {"status": "cleared"},
                "segments_total": 3, "segments_delivered": 1, "characters_confirmed": 200,
                "characters_uncertain": 200, "characters_total": 450, "stopped_at_segment": 2,
                "segments": [{"segment": 1, "characters": 200, "status": "delivered"}]},
        }));
        let slim = shape_structured(result, StructuredMode::Slim).structured_content.unwrap();
        assert_eq!(slim["replacement"], serde_json::json!({"phase": "type", "segments_total": 3,
            "segments_delivered": 1, "characters_confirmed": 200, "characters_uncertain": 200,
            "characters_total": 450, "stopped_at_segment": 2}));
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
            "remaining_text": "x".repeat(SLIM_STRING_LIMIT + 1),
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
    fn requested_hidden_elements_reach_all_structured_modes_without_an_extra_read() {
        let body = serde_json::json!({"snapshot":"S", "elements":[
            {"index":17,"kind":"Button","label":"Hidden target","identifier":"target-id","visible":false}
        ], "filter":{"query":"target","matched":1,"returned":1,"omitted":0,"of":30}});
        let (result, line, _, _) = device_call("200 OK", &body.to_string(), |h| async move {
            h.phone_elements(Parameters(ElementsParams {
                query: Some("target".into()), include_hidden: true, ..Default::default()
            })).await
        });
        assert_eq!(line, "GET /agent/elements?image=auto&query=target HTTP/1.1");
        assert_ne!(result.is_error, Some(true));
        for mode in [StructuredMode::Slim, StructuredMode::Full, StructuredMode::Off] {
            let shaped = shape_structured(result.clone(), mode);
            let text = text_of(&shaped);
            assert!(text.contains("#17 [Button] \"Hidden target\" id=target-id hidden"), "{text}");
            assert!(text.contains("\"omitted\":0"), "{text}");
            if mode == StructuredMode::Slim {
                assert_eq!(shaped.structured_content.as_ref().unwrap()["summary"], text);
            } else if mode == StructuredMode::Full {
                assert_eq!(shaped.structured_content.as_ref().unwrap()["elements"], body["elements"]);
            }
        }
    }

    #[test]
    fn element_selector_is_encoded_literally_and_preserves_snapshot_indexes() {
        let selector: iu_core::element_selector::ElementSelector = serde_json::from_value(
            serde_json::json!({"kind":"Button", "identifier":"save.primary", "name":"save.primary", "label":"保存\n'O\\K'", "enabled":false,"value_present":false,"index":1})
        ).unwrap();
        let expected = serde_json::to_value(&selector).unwrap();
        let body=serde_json::json!({"snapshot":"S", "elements":[
            {"index":17,"kind":"Button","label":"保存", "enabled":false}
        ], "filter":{"matched":2,"returned":1,"omitted":1,"selector":expected}});
        let (result,line,_,_)=device_call("200 OK", &body.to_string(), |h| async move {
            h.phone_elements(Parameters(ElementsParams {selector:Some(selector),..Default::default()})).await
        });
        let path=line.split_whitespace().nth(1).unwrap();
        let url=reqwest::Url::parse(&format!("http://daemon{path}")).unwrap();
        let encoded=url.query_pairs().find(|(key,_)|key=="selector").unwrap().1.into_owned();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&encoded).unwrap(),expected);
        assert_eq!(result.structured_content.as_ref().unwrap()["snapshot"],"S");
        assert_eq!(result.structured_content.as_ref().unwrap()["elements"][0]["index"],17);
        assert_ne!(result.is_error,Some(true));
    }

    #[test]
    fn element_selector_schema_is_object_only_and_has_strict_field_types() {
        for selector in [serde_json::json!(["Button",null,null,null,null,null]),serde_json::json!(["Button"])] {
            assert!(serde_json::from_value::<ElementsParams>(serde_json::json!({"selector":selector})).is_err());
        }
        let tool=listed_tools(&ToolProfile::All).into_iter().find(|tool|tool.name.as_ref()=="phone_elements").unwrap();
        let schema=serde_json::to_value(tool.input_schema.as_ref()).unwrap();
        fn resolve<'a>(root: &'a serde_json::Value, value: &'a serde_json::Value) -> &'a serde_json::Value {
            value.get("$ref").and_then(|v|v.as_str()).and_then(|reference|reference.strip_prefix('#'))
                .and_then(|pointer|root.pointer(pointer)).unwrap_or(value)
        }
        let selector=resolve(&schema,&schema["properties"]["selector"]);
        let object=selector.get("anyOf").and_then(|a|a.as_array()).and_then(|a|a.iter()
            .map(|v|resolve(&schema,v)).find(|v|v.get("type")==Some(&serde_json::json!("object")))).unwrap_or(selector);
        assert_eq!(object["type"],"object");
        // The listing drops `additionalProperties:false` (serde enforces it,
        // as the refusals above show); the generated schema still states it.
        assert!(object.get("additionalProperties").is_none(),"{object}");
        let raw=serde_json::to_value(rmcp::schemars::schema_for!(iu_core::element_selector::ElementSelector)).unwrap();
        assert_eq!(raw["additionalProperties"],false,"{raw}");
        assert!(object["properties"]["enabled"].to_string().contains("boolean"));
        assert!(object["properties"]["value_present"].to_string().contains("boolean"));
        assert!(object["properties"]["name"].to_string().contains("string"));
        assert_eq!(object["properties"].as_object().unwrap().len(),9);
        let index=resolve(&schema,&object["properties"]["index"]);
        assert_eq!(index["type"],"integer");assert_eq!(index["minimum"],0);assert_eq!(index["maximum"],199);
        assert!(!object["required"].as_array().is_some_and(|fields|fields.iter().any(|v|v=="index")));
        assert!(object["properties"].get("predicate").is_none());
    }

    #[test]
    fn ordinal_unproven_is_structured_error_without_channel_recovery_advice() {
        let body=serde_json::json!({"ok":false,"error":"ordinal_unproven","elements":[],"hint":"refine exact criteria ".repeat(150),"filter":{"ordinal_status":"unproven"}});
        let (result,_,_,_)=device_call("409 Conflict",&body.to_string(),|h|async move {
            h.phone_elements(Parameters(ElementsParams {selector:Some(serde_json::from_value(serde_json::json!({"label":"Save","index":1})).unwrap()),..Default::default()})).await
        });
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.structured_content, Some(body.clone()));
        let text = text_of(&result);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap(), body);
        assert!(!text.contains("phone_status"));
    }

    #[test]
    fn invalid_selector_is_refused_before_contacting_a_daemon() {
        let result=block(handler_for("http://127.0.0.1:9").phone_elements(Parameters(ElementsParams {
            selector:Some(Default::default()),..Default::default()
        })));
        assert!(refused_offline(result).contains("selector needs at least one field"));
    }

    #[test]
    fn filtered_elements_keep_indexes_and_coverage_in_the_model_summary() {
        let body = serde_json::json!({"snapshot":"S", "elements":[
            {"index":17,"kind":"Button","label":"目标"}
        ], "filter":{"query":"目标","matched":3,"returned":1,"omitted":2,"of":30}});
        let (result, line, _, _) = device_call("200 OK", &body.to_string(), |h| async move {
            h.phone_elements(Parameters(ElementsParams {
                query: Some("目标".into()), limit: Some(1), ..Default::default()
            })).await
        });
        assert!(line.contains("limit=1"), "{line}");
        assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
        for mode in [StructuredMode::Slim, StructuredMode::Full, StructuredMode::Off] {
            let shaped = shape_structured(result.clone(), mode);
            let text = text_of(&shaped);
            assert!(text.contains("#17 [Button]"), "{text}");
            assert!(!text.contains("#0 [Button]"), "{text}");
            assert!(text.contains("\"omitted\":2"), "{text}");
            if mode == StructuredMode::Slim {
                assert_eq!(shaped.structured_content.as_ref().unwrap()["summary"], text);
            } else if mode == StructuredMode::Full {
                assert_eq!(shaped.structured_content.as_ref().unwrap()["elements"], body["elements"]);
            }
        }
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
