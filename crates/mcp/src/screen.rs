//! The live screen panel: an MCP Apps (SEP-1865, `io.modelcontextprotocol/ui`)
//! view the host renders next to the chat.
//!
//! - one resource, `ui://iphone-use/screen-<version>.html`, served as
//!   `text/html;profile=mcp-app` (a self-contained page, no network);
//! - `phone_screen` and `phone_status` carry `_meta.ui.resourceUri`, so a host
//!   that renders apps opens the panel on them;
//! - the page requests bounded native video batches, with PNG fallback, through
//!   `phone_screen_frame`, which `_meta.ui.visibility: ["app"]`
//!   keeps out of the model's tool list in such hosts.
//!
//! WorkBuddy uses a read-only resource frame transport selected explicitly by
//! `IPHONE_USE_MCP_HOST=workbuddy`; Doubao keeps tools/call and one panel opener.
//! A host without MCP Apps ignores every `_meta` key here; it only sees two
//! more tools.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use rmcp::model::{
    AnnotateAble, CallToolResult, Content, Meta, RawResource, Resource, ResourceContents, Tool,
};
use serde_json::{json, Value};

use crate::types::StatusResponse;

/// The MIME type SEP-1865 requires for an app resource.
pub const MIME: &str = "text/html;profile=mcp-app";
/// The model-visible tool that opens (or reuses) the panel.
pub const SCREEN_TOOL: &str = "phone_screen";
/// The app-only tool the panel polls for frames.
pub const FRAME_TOOL: &str = "phone_screen_frame";
/// Tools whose results the panel renders (`_meta.ui.resourceUri`).
const PANEL_TOOLS: [&str; 2] = [SCREEN_TOOL, "phone_status"];
/// Codex keys panel reuse on this result id: one panel per chat, however often
/// the model opens it.
const WIDGET_SESSION_ID: &str = "iphone-use-screen";
/// The panel's frame size when it does not ask: legible in a sidebar, and a
/// PNG fallback small enough to poll twice a second.
pub const DEFAULT_FRAME_SIDE: u32 = 600;

/// Explicit host adaptation. Generic clients keep the standard tool transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Host {
    #[default]
    Generic,
    WorkBuddy,
    Doubao,
}

impl Host {
    pub fn from_env() -> Self {
        match std::env::var("IPHONE_USE_MCP_HOST")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "generic" => Self::Generic,
            "workbuddy" => Self::WorkBuddy,
            "doubao" => Self::Doubao,
            _ => {
                eprintln!("iphone-use-mcp: unknown IPHONE_USE_MCP_HOST; using generic transport");
                Self::Generic
            }
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::WorkBuddy => "workbuddy",
            Self::Doubao => "doubao",
        }
    }

    pub fn screen_uri(self) -> String {
        if self == Self::Generic {
            return screen_uri();
        }
        format!(
            "ui://iphone-use/screen-{}-{}.html",
            self.name(),
            env!("CARGO_PKG_VERSION")
        )
    }
}

/// No control operation is addressable through this resource namespace. The last
/// component is only a cache nonce for hosts that cache resources by URI.
pub const FRAME_RESOURCE_PREFIX: &str = "ui://iphone-use/frame-v1/";

pub const VIDEO_RESOURCE_PREFIX: &str = "ui://iphone-use/video-v1/";

/// Named frame resources carry their own target, including across concurrent panels.
/// Malformed names and non-frame resources never fall back to the startup daemon.
pub fn resource_route(host: Host, uri: &str) -> Result<(Option<&str>, String), &'static str> {
    let Some(tail) = uri.strip_prefix("ui://iphone-use/instance-v1/") else {
        return Ok((None, uri.to_string()));
    };
    let (name, resource) = tail.split_once('/').ok_or("missing instance resource")?;
    if !crate::instances::valid_name(name) { return Err("invalid instance resource target"); }
    let base = format!("ui://iphone-use/{resource}");
    if resource_video_reset(host, &base).is_none() && resource_frame_side(host, &base).is_none() {
        return Err("only frame resources may select an instance");
    }
    Ok((Some(name), base))
}

pub fn resource_video_reset(host: Host, uri: &str) -> Option<bool> {
    if host != Host::WorkBuddy {return None;}
    let (reset,nonce)=uri.strip_prefix(VIDEO_RESOURCE_PREFIX)?.split_once('/')?;
    if nonce.is_empty() || nonce.len()>20 || !nonce.bytes().all(|v|v.is_ascii_digit()) {return None;}
    nonce.parse::<u64>().ok()?;
    match reset {"0"=>Some(false),"1"=>Some(true),_=>None}
}

pub fn resource_frame_side(host: Host, uri: &str) -> Option<u32> {
    if host != Host::WorkBuddy {
        return None;
    }
    let (side, nonce) = uri.strip_prefix(FRAME_RESOURCE_PREFIX)?.split_once('/')?;
    if side.is_empty()
        || nonce.is_empty()
        || nonce.len() > 20
        || !side.bytes().all(|v| v.is_ascii_digit())
        || !nonce.bytes().all(|v| v.is_ascii_digit())
    {
        return None;
    }
    nonce.parse::<u64>().ok()?;
    let side = side.parse::<u32>().ok()?;
    (200..=1000).contains(&side).then_some(side)
}

pub fn adapt_tools(mut tools: Vec<Tool>, host: Host) -> Vec<Tool> {
    if host == Host::Generic {
        return tools;
    }
    for tool in &mut tools {
        if PANEL_TOOLS.contains(&tool.name.as_ref()) {
            let meta = tool.meta.get_or_insert_with(Meta::new);
            meta.0.remove("ui");
            meta.0.remove("openai/ui");
            if tool.name == SCREEN_TOOL {
                meta.0
                    .insert("ui".into(), json!({"resourceUri":host.screen_uri()}));
                if host == Host::WorkBuddy {
                    meta.0
                        .insert("workbuddy".into(), json!({"ui":{"launchSurface":"panel"}}));
                    tool.description = Some("Open or reuse the read-only iPhone screen panel. Frame reads take no owner lease and wake nothing. Use normal agent tools in the conversation for Home, refresh or control; the panel does not send control through resources.".into());
                }
            } else if let Some(description) = &mut tool.description {
                description.to_mut().push_str(" To watch the phone, open phone_screen once; status does not open a second panel in this host.");
            }
        }
    }
    tools
}

pub fn resources_for(host: Host) -> Vec<Resource> {
    let mut listed = resources();
    listed[0].raw.uri = host.screen_uri();
    if host == Host::WorkBuddy {
        listed[0].raw.description = Some(
            "Read-only iPhone screen. Control remains in the conversation's normal tools.".into(),
        );
    }
    listed
}

pub fn read_for(host: Host, uri: &str) -> Option<ResourceContents> {
    (uri == host.screen_uri()).then(|| ResourceContents::TextResourceContents {
        uri: uri.to_string(),
        mime_type: Some(MIME.into()),
        text: html_for(host),
        meta: Some(resource_meta()),
    })
}
const HTML: &str = include_str!("screen.html");

/// Versioned so a host that caches resources by URI picks up a new page after
/// an upgrade.
pub fn screen_uri() -> String {
    format!("ui://iphone-use/screen-{}.html", env!("CARGO_PKG_VERSION"))
}

fn meta(value: Value) -> Meta {
    match value {
        Value::Object(map) => Meta(map),
        _ => Meta::new(),
    }
}

/// The page itself, with the build's version and tool name filled in.
pub fn html() -> String { html_for(Host::Generic) }

pub fn html_for(host: Host) -> String {
    HTML.replace("__IU_VERSION__", env!("CARGO_PKG_VERSION"))
        .replace("__IU_FRAME_TOOL__", FRAME_TOOL)
        .replace("__IU_RESOURCE_FRAMES__", if host == Host::WorkBuddy { "true" } else { "false" })
        .replace("__IU_FRAME_RESOURCE__", FRAME_RESOURCE_PREFIX)
        .replace("__IU_VIDEO_RESOURCE__", VIDEO_RESOURCE_PREFIX)
}

/// `_meta` on the resource and its contents. No `csp`: the host default
/// (inline script, `data:` images, no network) is exactly what the page needs.
fn resource_meta() -> Meta {
    meta(json!({ "ui": { "prefersBorder": false } }))
}

/// Add the MCP Apps `_meta` to the listed tools (see the module docs).
pub fn decorate_tools(tools: Vec<Tool>) -> Vec<Tool> {
    tools
        .into_iter()
        .map(|mut tool| {
            let extra = if tool.name == SCREEN_TOOL {
                Some(json!({
                    "ui": { "resourceUri": screen_uri() },
                    "openai/ui": { "entrypoints": [{ "type": "thread" }] },
                }))
            } else if PANEL_TOOLS.contains(&tool.name.as_ref()) {
                Some(json!({ "ui": { "resourceUri": screen_uri() } }))
            } else if tool.name == FRAME_TOOL {
                Some(json!({ "ui": { "visibility": ["app"] } }))
            } else {
                None
            };
            if let Some(Value::Object(extra)) = extra {
                tool.meta.get_or_insert_with(Meta::new).0.extend(extra);
            }
            tool
        })
        .collect()
}

/// `resources/list`.
pub fn resources() -> Vec<Resource> {
    let mut resource = RawResource::new(screen_uri(), "iphone-use-screen");
    resource.title = Some("iPhone screen".into());
    resource.description =
        Some("Live view of the iPhone being driven, with Home and refresh buttons.".into());
    resource.mime_type = Some(MIME.into());
    resource.meta = Some(resource_meta());
    vec![resource.no_annotation()]
}

/// `resources/read`; `None` for any other URI.
pub fn read(uri: &str) -> Option<ResourceContents> {
    (uri == screen_uri()).then(|| ResourceContents::TextResourceContents {
        uri: uri.to_string(),
        mime_type: Some(MIME.into()),
        text: html(),
        meta: Some(resource_meta()),
    })
}

/// Mark a panel tool's result so a host reuses one panel per chat.
pub fn with_panel_session(mut result: CallToolResult) -> CallToolResult {
    result
        .meta
        .get_or_insert_with(Meta::new)
        .0
        .insert("openai/widgetSessionId".into(), json!(WIDGET_SESSION_ID));
    result
}

/// The few status facts the panel shows: who the phone is, whether it can
/// be driven, and who holds it.
pub fn status_summary(status: &StatusResponse, own_owner: &str) -> Value {
    let device = status.extra.get("device");
    let text = |key: &str| {
        device
            .and_then(|d| d.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let owner = status.owner.as_deref().filter(|o| !o.is_empty());
    // The daemon's one line for a person, e.g. "enter the passcode on the
    // phone" for `needs_passcode_on_phone`: English, plus the Chinese half
    // for a panel whose viewer reads Chinese.
    let step = |lang: &str| {
        status
            .extra
            .get("next_step")
            .and_then(|step| step.get(lang))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let next_step = step("en");
    let next_step_zh = step("zh");
    let blocked_on = status.setup_blocked_on.as_deref().filter(|b| !b.is_empty());
    // A private hold: the person is signing in, so there is no frame to show.
    let private_secs = status
        .extra
        .get("screen_private_secs")
        .and_then(Value::as_u64)
        .filter(|secs| *secs > 0);
    json!({
        "phone": text("name"),
        "model": text("model"),
        "drivable": status.drivable,
        "locked": status.locked,
        "released": status.released,
        "state": status.device_state,
        "owner": owner,
        "mine": owner.map(|o| o == own_owner),
        "hint": status.hint,
        "blocked_on": blocked_on,
        "next_step": next_step,
        "next_step_zh": next_step_zh,
        "private_secs": private_secs,
    })
}

/// One `phone_screen_frame` answer. Always a success: the panel shows what
/// went wrong in its status line instead of the host flagging a tool error.
/// The frame travels in `structuredContent`, which only the panel reads.
pub fn frame_result(
    status: Result<Value, String>,
    frame: Result<Vec<u8>, String>,
) -> CallToolResult {
    let (mut body, status_error) = match status {
        Ok(summary) => (summary, None),
        Err(e) => (json!({}), Some(e)),
    };
    // While the screen is private the refused screenshot is expected, and no
    // frame is shown even if one raced the hold.
    let private = !body["private_secs"].is_null();
    let frame = if private { Ok(Vec::new()) } else { frame };
    let (frame, frame_error) = match frame {
        Ok(bytes) if !bytes.is_empty() => (
            json!({ "mime_type": "image/png", "data": B64.encode(&bytes) }),
            None,
        ),
        Ok(_) if private => (Value::Null, None),
        Ok(_) => (
            Value::Null,
            Some("the screenshot came back empty".to_string()),
        ),
        Err(e) => (Value::Null, Some(e)),
    };
    if let Value::Object(map) = &mut body {
        map.insert("ok".into(), json!(frame_error.is_none()));
        map.insert("frame".into(), frame);
        map.insert("error".into(), json!(status_error.or(frame_error)));
    }
    let mut result = CallToolResult::success(vec![Content::text("iphone-use screen frame")]);
    result.structured_content = Some(body);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_resources_require_valid_explicit_targets() {
        assert_eq!(resource_route(Host::WorkBuddy, "ui://iphone-use/instance-v1/i13/video-v1/1/42").unwrap(),
            (Some("i13"), "ui://iphone-use/video-v1/1/42".into()));
        assert_eq!(resource_route(Host::WorkBuddy, "ui://iphone-use/instance-v1/default/frame-v1/600/42").unwrap().0, Some("default"));
        for suffix in ["runner/video-v1/0/1", "i13/../frame-v1/600/1", "i13/frame-v1/600/no", "i13/screen.html", "i13"] {
            assert!(resource_route(Host::WorkBuddy, &format!("ui://iphone-use/instance-v1/{suffix}")).is_err());
        }
        assert!(resource_route(Host::Generic, "ui://iphone-use/instance-v1/i13/video-v1/0/1").is_err());
    }

    #[test]
    fn native_video_resources_are_read_only_and_bounded() {
        assert_eq!(resource_video_reset(Host::WorkBuddy,"ui://iphone-use/video-v1/1/42"),Some(true));
        assert_eq!(resource_video_reset(Host::WorkBuddy,"ui://iphone-use/video-v1/0/42"),Some(false));
        for host in [Host::Generic,Host::Doubao] {assert_eq!(resource_video_reset(host,"ui://iphone-use/video-v1/0/42"),None);}
        for suffix in ["2/42","0/","0/-1","0/18446744073709551616","0/42/home","0/home","home/42"] {
            assert_eq!(resource_video_reset(Host::WorkBuddy,&format!("ui://iphone-use/video-v1/{suffix}")),None);
        }
    }
    #[test]
    fn a_blocked_phone_says_what_a_person_has_to_do() {
        let status: StatusResponse = serde_json::from_value(json!({
            "ok": true,
            "drivable": false,
            "device_state": "blocked",
            "setup_blocked_on": "needs_passcode_on_phone",
            "next_step": {"zh": "手机设了锁屏密码", "en": "This iPhone has a passcode"},
        }))
        .unwrap();
        let summary = status_summary(&status, "me");
        assert_eq!(summary["blocked_on"], "needs_passcode_on_phone");
        assert_eq!(summary["next_step"], "This iPhone has a passcode");
        assert_eq!(summary["next_step_zh"], "手机设了锁屏密码");
        let ready: StatusResponse =
            serde_json::from_value(json!({"ok": true, "drivable": true, "setup_blocked_on": ""}))
                .unwrap();
        let summary = status_summary(&ready, "me");
        assert!(summary["blocked_on"].is_null() && summary["next_step"].is_null());
        assert!(summary["next_step_zh"].is_null());
    }

    #[test]
    fn a_private_screen_sends_no_frame_and_no_error() {
        let status: StatusResponse = serde_json::from_value(json!({
            "ok": true, "drivable": true, "screen_private_secs": 90,
        }))
        .unwrap();
        let summary = status_summary(&status, "me");
        assert_eq!(summary["private_secs"], 90);
        let result = frame_result(Ok(summary), Ok(vec![1, 2, 3]));
        let body = result.structured_content.unwrap();
        assert!(body["frame"].is_null(), "a frame that raced the hold is dropped");
        assert!(body["error"].is_null());

        let open: StatusResponse =
            serde_json::from_value(json!({"ok": true, "screen_private_secs": 0})).unwrap();
        assert!(status_summary(&open, "me")["private_secs"].is_null());
    }

    #[test]
    fn the_resource_is_an_mcp_app_page() {
        let listed = resources();
        assert_eq!(listed.len(), 1);
        let json = serde_json::to_value(&listed[0]).unwrap();
        assert_eq!(json["uri"], screen_uri());
        assert_eq!(json["mimeType"], MIME);
        assert!(screen_uri().starts_with("ui://iphone-use/screen-"));

        let contents = serde_json::to_value(read(&screen_uri()).unwrap()).unwrap();
        assert_eq!(contents["mimeType"], "text/html;profile=mcp-app");
        assert_eq!(contents["_meta"]["ui"]["prefersBorder"], false);
        let text = contents["text"].as_str().unwrap();
        assert!(text.starts_with("<!doctype html>"));
        assert!(text.contains("ui/initialize"));
        assert!(
            text.contains("\"phone_screen_frame\""),
            "tool name filled in"
        );
        assert!(!text.contains("__IU_"), "no placeholder left");
        // Follows the viewer's language: Chinese for every English line.
        assert!(text.contains("const I18N_ZH = {"));
        assert!(text.contains("\"connecting…\": \"正在连接…\""));
        // Self-contained: nothing fetched from the network.
        assert!(!text.contains("<script src"));
        assert!(!text.contains("https://"));
        assert!(read("ui://iphone-use/other.html").is_none());
    }

    #[test]
    fn a_frame_survives_with_its_status() {
        let status =
            json!({"phone": "Leo's iPhone", "drivable": true, "owner": "mcp-1", "mine": true});
        let result = frame_result(Ok(status), Ok(vec![1, 2, 3]));
        let body = result.structured_content.unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["phone"], "Leo's iPhone");
        assert_eq!(body["frame"]["mime_type"], "image/png");
        assert_eq!(body["frame"]["data"], "AQID");
        assert!(body["error"].is_null());

        let failed = frame_result(Err("daemon down".into()), Err("no frame".into()));
        let body = failed.structured_content.unwrap();
        assert_eq!(body["ok"], false);
        assert!(body["frame"].is_null());
        assert_eq!(body["error"], "daemon down");
        assert_eq!(failed.is_error, Some(false));
    }

    #[test]
    fn panel_results_reuse_one_panel() {
        let result = with_panel_session(CallToolResult::success(vec![Content::text("x")]));
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["_meta"]["openai/widgetSessionId"], WIDGET_SESSION_ID);
    }
}
