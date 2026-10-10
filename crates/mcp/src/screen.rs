//! The live screen panel: an MCP Apps (SEP-1865, `io.modelcontextprotocol/ui`)
//! view the host renders next to the chat.
//!
//! - one resource, `ui://iphone-use/screen-<version>.html`, served as
//!   `text/html;profile=mcp-app` (a self-contained page, no network);
//! - `phone_screen` and `phone_status` carry `_meta.ui.resourceUri`, so a host
//!   that renders apps opens the panel on them;
//! - the page polls `phone_screen_frame`, which `_meta.ui.visibility: ["app"]`
//!   keeps out of the model's tool list in such hosts.
//!
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
/// PNG small enough to poll twice a second.
pub const DEFAULT_FRAME_SIDE: u32 = 600;

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
pub fn html() -> String {
    HTML.replace("__IU_VERSION__", env!("CARGO_PKG_VERSION"))
        .replace("__IU_FRAME_TOOL__", FRAME_TOOL)
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
    let (frame, frame_error) = match frame {
        Ok(bytes) if !bytes.is_empty() => (
            json!({ "mime_type": "image/png", "data": B64.encode(&bytes) }),
            None,
        ),
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
