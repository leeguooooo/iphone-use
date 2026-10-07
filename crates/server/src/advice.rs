//! `no_progress` advice for plain HTTP / CLI callers, per owner.
//!
//! The same tracker the MCP server uses (`core::progress`), kept here per
//! `X-Phone-Owner`, so a caller that drives `/agent/input?return=delta` with
//! curl gets the same advice an MCP client does. Advice only: nothing is
//! resent or undone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use core::progress::{Action, ProgressGuard, ProgressTracker};
use serde_json::Value;

/// Owners tracked at once; past this the map is cleared (a handful of
/// sessions, not an unbounded set).
const MAX_OWNERS: usize = 64;

type Shared = Arc<Mutex<ProgressTracker>>;

static TRACKERS: OnceLock<Mutex<HashMap<String, Shared>>> = OnceLock::new();

fn tracker(owner: &str) -> Shared {
    let map = TRACKERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.len() >= MAX_OWNERS && !map.contains_key(owner) {
        map.clear();
    }
    map.entry(owner.to_string()).or_default().clone()
}

/// A full screen read for `owner` (an `/agent/elements` body), or `None`
/// for a failed read.
pub fn note_screen(owner: &str, json: Option<&Value>) {
    if let Ok(mut t) = tracker(owner).lock() {
        t.note_screen(json);
    }
}

/// Something changed or replaced the screen without an observation the
/// tracker can judge: forget it.
pub fn reset(owner: &str) {
    if let Ok(mut t) = tracker(owner).lock() {
        t.reset();
    }
}

/// The observed `/agent/input` action this body describes, if the tracker
/// can key it: taps by label, by element + snapshot, by point, and scrolls.
pub fn action_of(body: &Value) -> Option<OwnedAction> {
    let num = |key: &str| body.get(key).and_then(Value::as_f64);
    // Relative targets (a page scroller, an element, a locator) cannot be
    // keyed from the request alone: no key means no advice.
    let relative = ["page", "element", "locator", "snapshot"]
        .iter()
        .any(|key| {
            body.get(*key)
                .is_some_and(|v| !v.is_null() && *v != Value::Bool(false))
        });
    match body.get("type").and_then(Value::as_str)? {
        "scroll" => {
            if relative {
                return None;
            }
            let (dx, dy) = (num("dx"), num("dy"));
            if dx.is_none() && dy.is_none() {
                return None;
            }
            Some(OwnedAction::Scroll(
                num("x")?,
                num("y")?,
                dx.unwrap_or(0.0),
                dy.unwrap_or(0.0),
            ))
        }
        "tap" => {
            if let Some(label) = body.get("label").and_then(Value::as_str) {
                return Some(OwnedAction::Label(label.to_string()));
            }
            if let (Some(index), Some(snapshot)) = (
                body.get("element").and_then(Value::as_u64),
                body.get("snapshot").and_then(Value::as_str),
            ) {
                return Some(OwnedAction::Element(index, snapshot.to_string()));
            }
            Some(OwnedAction::Point(num("x")?, num("y")?))
        }
        _ => None,
    }
}

/// An owned form of [`Action`] that can outlive the request body.
pub enum OwnedAction {
    Label(String),
    Element(u64, String),
    Point(f64, f64),
    Scroll(f64, f64, f64, f64),
}

/// Start judging an observed action for `owner`: the screen and the key are
/// fixed now.
pub fn begin(owner: &str, action: &OwnedAction) -> ProgressGuard {
    let action = match action {
        OwnedAction::Label(label) => Action::TapLabel(label),
        OwnedAction::Element(index, snapshot) => Action::TapElement {
            index: *index,
            snapshot,
        },
        OwnedAction::Point(x, y) => Action::TapPoint { x: *x, y: *y },
        OwnedAction::Scroll(x, y, dx, dy) => Action::Scroll {
            x: *x,
            y: *y,
            dx: *dx,
            dy: *dy,
        },
    };
    ProgressGuard::begin(&tracker(owner), &action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_absolute_targets_are_keyed() {
        assert!(matches!(
            action_of(&json!({"type": "scroll", "x": 0.5, "y": 0.6, "dy": 300})),
            Some(OwnedAction::Scroll(..))
        ));
        // No coordinates, a page scroller, an element scroll: no key.
        assert!(action_of(&json!({"type": "scroll", "dy": 300})).is_none());
        assert!(action_of(&json!({"type": "scroll", "page": true, "dy": 300})).is_none());
        assert!(action_of(
            &json!({"type": "scroll", "element": 3, "snapshot": "S", "x": 0.5, "y": 0.5, "dy": 1})
        )
        .is_none());
        assert!(
            action_of(&json!({"type": "scroll", "x": 0.5, "y": 0.5})).is_none(),
            "no distance"
        );
        assert!(matches!(
            action_of(&json!({"type": "tap", "label": "通用"})),
            Some(OwnedAction::Label(_))
        ));
        assert!(matches!(
            action_of(&json!({"type": "tap", "element": 2, "snapshot": "S"})),
            Some(OwnedAction::Element(2, _))
        ));
        assert!(action_of(&json!({"type": "tap_locator", "locator": {"label": "x"}})).is_none());
        assert!(action_of(&json!({"type": "text", "text": "hi"})).is_none());
    }
}
