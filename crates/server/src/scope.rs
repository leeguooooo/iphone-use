//! Task-scoped views of one fresh element read, and images only when needed.
//!
//! A scoped view keeps the full read's `snapshot` (the hash of the whole
//! tree) and every kept row's original `index`, so taps against it work
//! exactly as against the full read. Nothing is re-read: the view is a filter
//! of the same answer.

use serde_json::{json, Value};

/// Kinds a person can act on.
const INTERACTIVE: &[&str] = &[
    "Button",
    "Cell",
    "TextField",
    "SecureTextField",
    "SearchField",
    "TextView",
    "Switch",
    "Toggle",
    "Slider",
    "Stepper",
    "Link",
    "Tab",
    "Key",
    "Picker",
    "PickerWheel",
    "SegmentedControl",
    "MenuItem",
    "Icon",
];
const SCROLLERS: &[&str] = &["ScrollView", "Table", "CollectionView", "WebView"];

/// Reject a scope the caller cannot get: unknown, or `changed` without the
/// baseline it must be measured against (never guessed).
pub fn validate(scope: Option<&str>, since: Option<&str>) -> Result<(), Value> {
    match scope {
        None | Some("app") | Some("interactive") | Some("focused") => Ok(()),
        Some("changed") if since.is_some_and(|s| !s.is_empty()) => Ok(()),
        Some("changed") => Err(json!({"ok": false, "error": "scope_needs_baseline",
            "hint": "scope=changed needs since=<the snapshot you last read>"})),
        Some(_) => Err(json!({"ok": false, "error": "invalid_scope",
            "hint": "scope is app, interactive, focused, or changed (with since)"})),
    }
}

fn kind(row: &Value) -> &str {
    row.get("kind").and_then(Value::as_str).unwrap_or("")
}

fn truthy(row: &Value, key: &str) -> bool {
    row.get(key) == Some(&Value::Bool(true))
}

/// Replace `elements` with the scoped rows, each carrying its original
/// `index`, and add the context a reader of the scoped view needs.
pub fn apply(json: &mut Value, scope: &str) {
    let Some(rows) = json.get("elements").and_then(Value::as_array).cloned() else {
        return;
    };
    let total = rows.len();
    let focused = rows.iter().position(|row| truthy(row, "focused"));
    let keep: Vec<usize> = match scope {
        // The active app's own subtree (the first Application row), plus any
        // keyboard wherever it sits. A system alert is the top-level
        // `alert` block, which every scope keeps as it is.
        "app" => {
            let depth = |i: usize| rows[i].get("depth").and_then(Value::as_u64).unwrap_or(0);
            let app = rows.iter().position(|row| kind(row) == "Application");
            let app_end = app.map(|a| {
                (a + 1..total)
                    .find(|&i| depth(i) <= depth(a))
                    .unwrap_or(total)
            });
            let mut keep: Vec<usize> = match (app, app_end) {
                (Some(a), Some(end)) => (a..end).collect(),
                _ => (0..total).collect(),
            };
            for k in (0..total).filter(|&i| kind(&rows[i]) == "Keyboard") {
                let end = (k + 1..total)
                    .find(|&i| depth(i) <= depth(k))
                    .unwrap_or(total);
                keep.extend(k..end);
            }
            keep.sort_unstable();
            keep.dedup();
            keep
        }
        "interactive" => (0..total)
            .filter(|&i| INTERACTIVE.contains(&kind(&rows[i])) || Some(i) == focused)
            .collect(),
        "focused" => match focused {
            None => Vec::new(),
            Some(f) => {
                // The focused row's parent and the parent's whole subtree.
                let depth = |i: usize| rows[i].get("depth").and_then(Value::as_u64).unwrap_or(0);
                let parent = (0..f).rev().find(|&i| depth(i) < depth(f)).unwrap_or(f);
                let end = (parent + 1..total)
                    .find(|&i| depth(i) <= depth(parent))
                    .unwrap_or(total);
                (parent..end).collect()
            }
        },
        _ => return,
    };
    let scoped: Vec<Value> = keep
        .iter()
        .map(|&i| {
            let mut row = rows[i].clone();
            row["index"] = json!(i);
            row
        })
        .collect();
    json["elements"] = Value::Array(scoped);
    json["scope"] = json!({
        "name": scope,
        "rows": keep.len(),
        "of": total,
        "focused_index": focused,
        "keyboard": rows.iter().any(|row| kind(row) == "Keyboard"),
        "scroll_containers": rows.iter().filter(|row| SCROLLERS.contains(&kind(row))).count(),
        "note": "indexes refer to the full read with this snapshot",
    });
}

/// The tree cannot be used at all (no interactive rows, containers only).
/// A capture-redacted screen is excluded: its screenshot adds nothing.
pub fn needs_image(json: &Value) -> bool {
    if truthy(json, "capture_redacted") {
        return false;
    }
    let Some(stats) = json.get("ax_stats") else {
        return false;
    };
    stats.get("n_interactive").and_then(Value::as_u64) == Some(0) && truthy(stats, "container_only")
}

/// Longest sides tried, largest first, until the image fits the budget.
pub const IMAGE_SIDES: [u32; 3] = [1200, 800, 500];
/// The whole JSON answer must stay under a standard client's 4 MB read
/// limit, so text is never pushed out by an image.
pub const DEFAULT_IMAGE_BUDGET_BYTES: usize = 3_500_000;

/// The response budget (`IPHONE_USE_IMAGE_BUDGET_BYTES` overrides it).
pub fn image_budget_bytes() -> usize {
    std::env::var("IPHONE_USE_IMAGE_BUDGET_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_IMAGE_BUDGET_BYTES)
}

/// What became of the image for a Mode A read.
pub enum ImageOutcome {
    Attached {
        png: Vec<u8>,
        /// When the screenshot was asked for and when its bytes arrived: the
        /// capture happened somewhere in this interval, never stated as an
        /// exact instant.
        requested_ms: u64,
        received_ms: u64,
        /// `wda-capture`, or the daemon's own source label.
        source: String,
        max_side: u32,
        /// This capture's own `x-capture-redacted`: a wireframe, not pixels.
        redacted: bool,
    },
    TooLarge,
    Unavailable,
}

/// Attach the screenshot (taken after the read, labelled with its real
/// interval and source), or say why there is none. Text is never dropped.
pub fn attach_image(json: &mut Value, outcome: ImageOutcome) {
    use base64::Engine as _;
    match outcome {
        ImageOutcome::Attached {
            png,
            requested_ms,
            received_ms,
            source,
            max_side,
            redacted,
        } => {
            json["image"] = json!({
                "capture_redacted": redacted,
                "png_base64": base64::engine::general_purpose::STANDARD.encode(png),
                "requested_at_ms": requested_ms,
                "received_at_ms": received_ms,
                "source": source,
                "max_side": max_side,
                "relation": "requested after this snapshot was read; not the same instant",
            });
        }
        ImageOutcome::TooLarge => {
            json["image_unavailable"] = json!(true);
            json["image_omitted"] = json!("too_large_for_response_budget");
        }
        ImageOutcome::Unavailable => {
            json["image_unavailable"] = json!(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read() -> Value {
        json!({"snapshot": "H", "elements": [
            {"kind": "Application", "label": "设置", "depth": 0},
            {"kind": "Table", "label": "", "depth": 1},
            {"kind": "Cell", "label": "", "depth": 2},
            {"kind": "Button", "label": "通用", "depth": 3},
            {"kind": "StaticText", "label": "通用", "depth": 4},
            {"kind": "SearchField", "label": "搜索", "depth": 1, "focused": true},
            {"kind": "Keyboard", "label": "", "depth": 1},
            {"kind": "Key", "label": "a", "depth": 2},
        ]})
    }

    #[test]
    fn changed_needs_a_baseline_and_unknown_scopes_are_refused() {
        assert!(validate(Some("changed"), None).is_err());
        assert!(validate(Some("changed"), Some("")).is_err());
        assert!(validate(Some("changed"), Some("H0")).is_ok());
        assert_eq!(
            validate(Some("everything"), None).unwrap_err()["error"],
            "invalid_scope"
        );
        assert!(validate(None, None).is_ok());
    }

    #[test]
    fn interactive_keeps_original_indexes_and_the_snapshot() {
        let mut json = read();
        apply(&mut json, "interactive");
        let indexes: Vec<u64> = json["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["index"].as_u64().unwrap())
            .collect();
        assert_eq!(indexes, vec![2, 3, 5, 7]);
        assert_eq!(json["snapshot"], "H");
        assert_eq!(json["scope"]["of"], 8);
        assert_eq!(json["scope"]["keyboard"], true);
        assert_eq!(json["scope"]["scroll_containers"], 1);
        assert_eq!(json["scope"]["focused_index"], 5);
    }

    #[test]
    fn app_keeps_the_app_subtree_and_the_keyboard() {
        let mut json = json!({"snapshot": "H", "alert": {"text": "允许?"}, "elements": [
            {"kind": "Window", "label": "status", "depth": 0},
            {"kind": "Application", "label": "设置", "depth": 0},
            {"kind": "Button", "label": "通用", "depth": 1},
            {"kind": "Other", "label": "overlay", "depth": 0},
            {"kind": "Keyboard", "label": "", "depth": 0},
            {"kind": "Key", "label": "a", "depth": 1},
        ]});
        apply(&mut json, "app");
        let indexes: Vec<u64> = json["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["index"].as_u64().unwrap())
            .collect();
        assert_eq!(indexes, vec![1, 2, 4, 5]);
        assert_eq!(json["alert"]["text"], "允许?", "the alert block is kept");
        assert!(validate(Some("app"), None).is_ok());
    }

    #[test]
    fn focused_is_the_focused_rows_parent_subtree() {
        let mut json = read();
        apply(&mut json, "focused");
        let indexes: Vec<u64> = json["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["index"].as_u64().unwrap())
            .collect();
        assert_eq!(
            indexes,
            (0..8).collect::<Vec<_>>(),
            "parent is the Application"
        );
        let mut none = json!({"snapshot": "H", "elements": [{"kind": "Button", "depth": 0}]});
        apply(&mut none, "focused");
        assert_eq!(none["elements"].as_array().unwrap().len(), 0);
        assert!(none["scope"]["focused_index"].is_null());
    }

    #[test]
    fn images_only_for_an_unusable_tree_and_never_stale() {
        let mut sparse = json!({"ax_stats": {"n_interactive": 0, "container_only": true}});
        assert!(needs_image(&sparse));
        assert!(!needs_image(
            &json!({"ax_stats": {"n_interactive": 3, "container_only": false}})
        ));
        assert!(!needs_image(&json!({"capture_redacted": true,
            "ax_stats": {"n_interactive": 0, "container_only": true}})));
        attach_image(&mut sparse, ImageOutcome::Unavailable);
        assert_eq!(sparse["image_unavailable"], true);
        assert!(sparse.get("image").is_none());
        let mut ok = json!({});
        attach_image(
            &mut ok,
            ImageOutcome::Attached {
                png: b"png".to_vec(),
                requested_ms: 5,
                received_ms: 7,
                source: "wda-capture".into(),
                max_side: 800,
                redacted: false,
            },
        );
        assert_eq!(
            (
                ok["image"]["requested_at_ms"].as_u64(),
                ok["image"]["received_at_ms"].as_u64()
            ),
            (Some(5), Some(7))
        );
        assert!(
            ok["image"].get("captured_at_ms").is_none(),
            "never a made-up instant"
        );
    }
}
