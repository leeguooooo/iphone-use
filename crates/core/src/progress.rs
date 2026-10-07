//! The no_progress tracker, shared by the MCP server and the daemon so
//! every entry point (MCP, plain HTTP, CLI) gives the same advice from the
//! same rules.

use serde_json::Value;

/// What one tool call did, as the tracker sees it.
pub enum Action<'a> {
    /// Tap by label (the exact-label path is already unique on the screen).
    TapLabel(&'a str),
    /// Tap by element index against the caller's snapshot.
    TapElement { index: u64, snapshot: &'a str },
    /// Tap a normalized point.
    TapPoint { x: f64, y: f64 },
    /// Scroll from an anchor.
    Scroll { x: f64, y: f64, dx: f64, dy: f64 },
}

/// A bounded key: 64 bits, never raw labels.
fn fingerprint(parts: &[&[u8]]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    hasher.finish()
}

/// A snapshot token is the SHA-256 of the whole serialized tree, so it is the
/// screen's identity: equal tokens mean an identical tree (values, focus,
/// rects and all), and any change gives a new token.
fn valid_snapshot(token: &str) -> bool {
    !token.is_empty() && token.len() <= 128 && token.bytes().all(|b| b.is_ascii_graphic())
}

/// Too little in the tree to say anything about it.
fn sparse(json: &Value, rows: &[Value]) -> bool {
    if let Some(stats) = json.get("ax_stats") {
        let interactive = stats.get("n_interactive").and_then(Value::as_u64);
        let container_only = stats.get("container_only") == Some(&Value::Bool(true));
        if interactive == Some(0) || container_only {
            return true;
        }
    }
    rows.len() < 3
}

/// Silence longer than this ends a streak.
pub const PROGRESS_IDLE_MS: u64 = 120_000;

/// Watches observed actions in one MCP session and warns when the same
/// action, on the same screen, keeps settling with nothing changed.
///
/// It speaks only when it can prove its claim: a known, non-sparse screen
/// (from a full read), a key fixed when the action started, and consecutive
/// answers whose baseline is that screen, whose snapshot is still that screen
/// and whose delta lists are present and empty. Anything else resets it, and
/// every reset bumps an epoch so an older completion can never revive state.
#[derive(Debug, Default)]
pub struct ProgressTracker {
    /// Snapshot token of the last full, non-sparse read still known to hold.
    screen: Option<String>,
    /// (action key, consecutive unchanged count).
    streak: Option<(u64, u32)>,
    epoch: u64,
    last_ms: u64,
    /// Observed actions started and not yet finished or abandoned.
    in_flight: u32,
    /// Two actions overlapped: nothing counts until all of them are done.
    overlapped: bool,
}

/// Everything an action's result is judged against, fixed when it started.
#[derive(Debug, Clone)]
pub struct Ticket {
    epoch: u64,
    screen: Option<String>,
    key: Option<u64>,
}

impl ProgressTracker {
    /// Forget everything. Outstanding tickets become void.
    pub fn reset(&mut self) {
        self.epoch += 1;
        self.screen = None;
        self.streak = None;
    }

    /// An action that started but will never be judged (a cancelled call):
    /// it leaves the tracker reset.
    /// A started action that turned out not to be the caller's at all (it was
    /// refused as unauthenticated): release its slot without touching state.
    pub fn withdraw(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if self.in_flight == 0 {
            self.overlapped = false;
        }
    }

    pub fn abandon(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if self.in_flight == 0 {
            self.overlapped = false;
        }
        self.reset();
    }

    /// A full screen read. Anything but a usable, non-sparse tree with a
    /// valid snapshot resets the tracker.
    pub fn note_screen(&mut self, json: Option<&Value>) {
        let usable = json.and_then(|json| {
            let rows = json.get("elements")?.as_array()?;
            let snapshot = json.get("snapshot")?.as_str()?;
            // A spinner on screen means the page may still be loading even
            // when the tree is stable, so it is not a screen to judge on.
            (valid_snapshot(snapshot) && !sparse(json, rows) && !rows.iter().any(is_spinner))
                .then(|| snapshot.to_string())
        });
        match usable {
            Some(snapshot) => {
                if self.screen.as_deref() != Some(snapshot.as_str()) {
                    self.streak = None;
                }
                self.epoch += 1;
                self.screen = Some(snapshot);
            }
            None => self.reset(),
        }
    }

    /// Start an observed action: capture the screen and the action's key
    /// now, and void any ticket still outstanding (overlapping calls).
    pub fn begin(&mut self, action: &Action) -> Ticket {
        self.in_flight += 1;
        if self.in_flight > 1 {
            // Overlapping actions: neither completion can count.
            self.overlapped = true;
            self.streak = None;
        }
        self.epoch += 1;
        let screen = self.screen.clone();
        let key = screen.as_deref().and_then(|screen| {
            let screen = screen.as_bytes();
            Some(match action {
                Action::TapLabel(label) => fingerprint(&[b"label", screen, label.as_bytes()]),
                Action::TapElement { index, snapshot } => {
                    // The index means something only against this screen.
                    if snapshot.as_bytes() != screen {
                        return None;
                    }
                    fingerprint(&[b"element", screen, &index.to_le_bytes()])
                }
                Action::TapPoint { x, y } => {
                    if !x.is_finite() || !y.is_finite() {
                        return None;
                    }
                    let (x, y) = (x.to_bits().to_le_bytes(), y.to_bits().to_le_bytes());
                    fingerprint(&[b"point", screen, &x, &y])
                }
                Action::Scroll { x, y, dx, dy } => {
                    if ![x, y, dx, dy].iter().all(|v| v.is_finite()) {
                        return None;
                    }
                    let bits: Vec<[u8; 8]> = [x, y, dx, dy]
                        .iter()
                        .map(|v| v.to_bits().to_le_bytes())
                        .collect();
                    fingerprint(&[b"scroll", screen, &bits[0], &bits[1], &bits[2], &bits[3]])
                }
            })
        });
        Ticket {
            epoch: self.epoch,
            screen,
            key,
        }
    }

    /// Judge a finished action against its ticket. Returns a `no_progress`
    /// advisory, or `None`. A ticket from before a reset or an overlapping
    /// call changes nothing.
    pub fn finish(&mut self, ticket: &Ticket, json: Option<&Value>, now_ms: u64) -> Option<String> {
        self.in_flight = self.in_flight.saturating_sub(1);
        if self.overlapped {
            if self.in_flight == 0 {
                self.overlapped = false;
            }
            self.reset();
            return None;
        }
        if ticket.epoch != self.epoch {
            return None;
        }
        let idle = self.last_ms != 0 && now_ms.saturating_sub(self.last_ms) > PROGRESS_IDLE_MS;
        self.last_ms = now_ms;
        if idle {
            self.reset();
            return None;
        }
        let (Some(screen), Some(key), Some(json)) = (&ticket.screen, ticket.key, json) else {
            self.reset();
            return None;
        };
        if !unchanged_on(json, screen) {
            // Changed, or not readable as "unchanged on this screen": the
            // screen is no longer known.
            self.reset();
            return None;
        }
        let count = match self.streak {
            Some((last, count)) if last == key => count.saturating_add(1),
            _ => 1,
        };
        self.streak = Some((key, count));
        (count >= 2).then(|| {
            format!(
                "no_progress (advice only): this same action on this same screen has now settled \
                 {count} times in a row and the accessibility tree came back identical, with no \
                 app switch. A change the tree does not show (sound, haptics, pixels only) would \
                 not appear here. Nothing was resent. Re-read the screen with phone_elements, \
                 then try a different control or wait for a specific condition; do not repeat \
                 an action whose outcome was unknown."
            )
        })
    }
}

/// A loading indicator row (`ActivityIndicator` / `ProgressIndicator`), in
/// either the flat row shape or a delta entry's `element`.
fn is_spinner(row: &Value) -> bool {
    let row = row.get("element").unwrap_or(row);
    matches!(
        row.get("kind").and_then(Value::as_str),
        Some("ActivityIndicator" | "ProgressIndicator")
    )
}

/// True only for a settled answer whose baseline is `screen`, whose snapshot
/// is still `screen`, whose delta lists are all present and empty, with no
/// app switch and no delta error.
fn unchanged_on(json: &Value, screen: &str) -> bool {
    let str_of = |key: &str| json.get(key).and_then(Value::as_str);
    let spinner_in = |key: &str| {
        json.get("delta")
            .and_then(|d| d.get(key))
            .and_then(Value::as_array)
            .is_some_and(|rows| rows.iter().any(is_spinner))
    };
    if spinner_in("added") || spinner_in("changed") {
        return false;
    }
    let empty = |key: &str| {
        json.get("delta")
            .and_then(|d| d.get(key))
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    };
    json.get("ok") == Some(&Value::Bool(true))
        && json.get("settle").and_then(|s| s.get("settled")) == Some(&Value::Bool(true))
        && json.get("delta_error").is_none()
        && json.get("app_changed").is_none_or(Value::is_null)
        && str_of("baseline") == Some(screen)
        && str_of("snapshot").is_some_and(|s| valid_snapshot(s) && s == screen)
        && empty("added")
        && empty("removed")
        && empty("changed")
}

/// Holds a [`Ticket`] for an action in progress. Finishing consumes it;
/// dropping it unfinished (the call was cancelled) resets the tracker.
pub struct ProgressGuard {
    tracker: std::sync::Arc<std::sync::Mutex<ProgressTracker>>,
    ticket: Option<Ticket>,
}

impl ProgressGuard {
    pub fn begin(
        tracker: &std::sync::Arc<std::sync::Mutex<ProgressTracker>>,
        action: &Action,
    ) -> Self {
        let ticket = tracker.lock().ok().map(|mut t| t.begin(action));
        Self {
            tracker: tracker.clone(),
            ticket,
        }
    }

    /// The request was not the caller's (refused as unauthenticated).
    pub fn withdraw(mut self) {
        if self.ticket.take().is_some() {
            if let Ok(mut tracker) = self.tracker.lock() {
                tracker.withdraw();
            }
        }
    }

    pub fn finish(mut self, json: Option<&Value>, now_ms: u64) -> Option<String> {
        let ticket = self.ticket.take()?;
        self.tracker
            .lock()
            .ok()
            .and_then(|mut t| t.finish(&ticket, json, now_ms))
    }
}

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        if self.ticket.take().is_some() {
            if let Ok(mut tracker) = self.tracker.lock() {
                tracker.abandon();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A full read in the daemon's `/agent/elements` shape: the snapshot is a
    /// content hash, stable for an identical tree.
    fn screen(snapshot: &str, labels: &[&str]) -> Value {
        let rows: Vec<Value> = labels
            .iter()
            .map(|label| json!({"kind": "Button", "label": label}))
            .collect();
        json!({"snapshot": snapshot, "elements": rows,
               "ax_stats": {"n": rows.len(), "n_interactive": rows.len(), "container_only": false}})
    }

    /// An observed answer: baseline → snapshot, with empty delta lists unless
    /// `changed` is given.
    fn answer(baseline: &str, snapshot: &str, changed: Option<Value>) -> Value {
        let changed = changed.map_or_else(Vec::new, |row| vec![row]);
        let mut body = json!({"ok": true, "transport": "wda", "snapshot": snapshot,
            "baseline": baseline,
            "delta": {"added": [], "removed": [], "changed": changed},
            "settle": {"settled": true, "reason": "stable", "captures": 2, "waited_ms": 600}});
        if baseline == snapshot {
            body["no_visible_change"] = json!(true);
        }
        body
    }

    fn act(t: &mut ProgressTracker, action: Action, json: Value, now: u64) -> Option<String> {
        let ticket = t.begin(&action);
        t.finish(&ticket, Some(&json), now)
    }

    const H: &str = "SHA256-settings-root";

    fn settings() -> ProgressTracker {
        let mut t = ProgressTracker::default();
        t.note_screen(Some(&screen(H, &["通用", "蓝牙", "无障碍"])));
        t
    }

    #[test]
    fn two_identical_answers_on_the_same_screen_warn() {
        let mut t = settings();
        assert!(act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_000).is_none());
        let warn = act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 2_000).unwrap();
        assert!(warn.starts_with("no_progress (advice only)") && warn.contains("2 times"));
        assert!(warn.contains("Nothing was resent"));
    }

    #[test]
    fn a_value_change_or_a_new_hash_breaks_the_streak() {
        let mut t = settings();
        act(&mut t, Action::TapLabel("蓝牙"), answer(H, H, None), 1_000);
        // The switch's value changed: delta non-empty, new hash.
        let toggled = answer(
            H,
            "SHA256-other",
            Some(json!({"index": 1, "element": {"value": "1"}})),
        );
        assert!(act(&mut t, Action::TapLabel("蓝牙"), toggled, 1_100).is_none());
        // The screen is unknown now; even identical answers do not count.
        assert!(act(&mut t, Action::TapLabel("蓝牙"), answer(H, H, None), 1_200).is_none());
        // Empty lists but a different hash (e.g. a rect moved): not unchanged.
        let mut t = settings();
        act(&mut t, Action::TapLabel("蓝牙"), answer(H, H, None), 1_000);
        assert!(act(
            &mut t,
            Action::TapLabel("蓝牙"),
            answer(H, "SHA256-moved", None),
            1_100
        )
        .is_none());
    }

    #[test]
    fn baseline_must_be_the_known_screen() {
        let mut t = settings();
        act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_000);
        assert!(act(
            &mut t,
            Action::TapLabel("通用"),
            answer("SHA256-x", "SHA256-x", None),
            1_100
        )
        .is_none());
    }

    #[test]
    fn duplicate_labels_and_indexes_are_distinct_targets() {
        let mut t = settings();
        let tap = |index| Action::TapElement { index, snapshot: H };
        act(&mut t, tap(1), answer(H, H, None), 1_000);
        assert!(
            act(&mut t, tap(2), answer(H, H, None), 1_100).is_none(),
            "another index resets the count"
        );
        assert!(act(&mut t, tap(2), answer(H, H, None), 1_200).is_some());
        // An index against a snapshot the tracker does not hold has no identity.
        let mut t = settings();
        act(&mut t, tap(1), answer(H, H, None), 1_000);
        let foreign = Action::TapElement {
            index: 1,
            snapshot: "SHA256-foreign",
        };
        assert!(act(&mut t, foreign, answer(H, H, None), 1_100).is_none());
    }

    #[test]
    fn coordinates_use_full_precision() {
        let mut t = settings();
        let point = |x| Action::TapPoint { x, y: 0.5 };
        act(&mut t, point(0.501), answer(H, H, None), 1_000);
        assert!(act(&mut t, point(0.502), answer(H, H, None), 1_100).is_none());
        let scroll = |y| Action::Scroll {
            x: 0.5,
            y,
            dx: 0.0,
            dy: 300.0,
        };
        act(&mut t, scroll(0.5), answer(H, H, None), 1_200);
        assert!(act(&mut t, scroll(0.8), answer(H, H, None), 1_300).is_none());
        assert!(act(&mut t, scroll(0.8), answer(H, H, None), 1_400).is_some());
        assert!(act(
            &mut t,
            Action::TapPoint {
                x: f64::NAN,
                y: 0.5
            },
            answer(H, H, None),
            1_500
        )
        .is_none());
    }

    #[test]
    fn stale_sparse_missing_and_unsettled_answers_reset() {
        let without = |key: &str| {
            let mut a = answer(H, H, None);
            if key == "added" {
                a["delta"].as_object_mut().unwrap().remove("added");
            } else {
                a.as_object_mut().unwrap().remove(key);
            }
            a
        };
        let unsettled = {
            let mut a = answer(H, H, None);
            a["settle"]["settled"] = json!(false);
            a
        };
        let stale = json!({"ok": false, "error": "stale_snapshot"});
        let mut switched = answer(H, H, None);
        switched["app_changed"] = json!({"from": "设置", "to": "微信"});
        for bad in [
            without("settle"),
            without("baseline"),
            without("snapshot"),
            without("added"),
            unsettled,
            stale,
            switched,
        ] {
            let mut t = settings();
            act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_000);
            assert!(act(&mut t, Action::TapLabel("通用"), bad, 1_100).is_none());
            assert!(act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_200).is_none());
        }
        // A sparse full read leaves no known screen.
        let mut t = ProgressTracker::default();
        let mut thin = screen(H, &["通用", "蓝牙", "无障碍"]);
        thin["ax_stats"]["n_interactive"] = json!(0);
        t.note_screen(Some(&thin));
        act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_000);
        assert!(act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_100).is_none());
        // A failed or unparseable read resets too.
        let mut t = settings();
        act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_000);
        t.note_screen(None);
        assert!(act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_100).is_none());
    }

    #[test]
    fn interleaved_flows_and_long_idle_reset() {
        let mut t = settings();
        act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_000);
        t.reset(); // e.g. phone_flow_run / phone_jev_run / phone_reconnect
        t.note_screen(Some(&screen(H, &["通用", "蓝牙", "无障碍"])));
        assert!(act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_100).is_none());

        let mut t = settings();
        act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_000);
        let late = 1_000 + PROGRESS_IDLE_MS + 1;
        assert!(act(&mut t, Action::TapLabel("通用"), answer(H, H, None), late).is_none());
    }

    #[test]
    fn overlapping_and_stale_tickets_change_nothing() {
        let mut t = settings();
        act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_000);
        let older = t.begin(&Action::TapLabel("通用"));
        let newer = t.begin(&Action::TapLabel("通用"));
        // Overlapping calls: neither completion counts, in either order,
        // even though one unchanged answer was already on the streak.
        assert!(t
            .finish(&older, Some(&json!({"ok": false})), 1_100)
            .is_none());
        assert!(t.finish(&newer, Some(&answer(H, H, None)), 1_200).is_none());
        assert!(t.streak.is_none() && t.screen.is_none());
        // Once both are done, a fresh read and two clean answers count again.
        t.note_screen(Some(&screen(H, &["通用", "蓝牙", "无障碍"])));
        act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_250);
        assert!(act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_260).is_some());
        // After a reset an old ticket cannot revive the streak.
        let old = t.begin(&Action::TapLabel("通用"));
        t.reset();
        assert!(t.finish(&old, Some(&answer(H, H, None)), 1_300).is_none());
        assert!(t.streak.is_none() && t.screen.is_none());
    }

    #[test]
    fn loading_screens_never_warn() {
        // A stable tree that still shows a spinner is not a screen to judge.
        let mut t = ProgressTracker::default();
        let mut loading = screen(H, &["通用", "蓝牙", "无障碍"]);
        loading["elements"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind": "ActivityIndicator", "label": ""}));
        t.note_screen(Some(&loading));
        act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_000);
        assert!(act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_100).is_none());

        // A spinner appearing in the delta invalidates the streak.
        let mut t = settings();
        act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_000);
        let mut spinning = answer(H, H, None);
        spinning["delta"]["added"] =
            json!([{"index": 3, "element": {"kind": "ProgressIndicator", "label": ""}}]);
        assert!(act(&mut t, Action::TapLabel("通用"), spinning, 1_100).is_none());
        assert!(act(&mut t, Action::TapLabel("通用"), answer(H, H, None), 1_200).is_none());
    }

    #[test]
    fn a_cancelled_call_resets_through_its_guard() {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(settings()));
        let guard = ProgressGuard::begin(&shared, &Action::TapLabel("通用"));
        assert!(guard.finish(Some(&answer(H, H, None)), 1_000).is_none());
        let guard = ProgressGuard::begin(&shared, &Action::TapLabel("通用"));
        drop(guard); // the future was cancelled before an answer arrived
        let tracker = shared.lock().unwrap();
        assert!(tracker.screen.is_none() && tracker.streak.is_none());
    }
}
