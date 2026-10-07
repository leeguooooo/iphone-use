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
    let mut map = map.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
    match body.get("type").and_then(Value::as_str)? {
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
        "scroll" => Some(OwnedAction::Scroll(
            num("x").unwrap_or(0.5),
            num("y").unwrap_or(0.5),
            num("dx").unwrap_or(0.0),
            num("dy").unwrap_or(0.0),
        )),
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
