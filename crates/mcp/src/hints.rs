//! Advisory hints for the model: near-miss labels and "no progress".
//!
//! Both are advice only. Nothing here sends an action, goes back, or replays
//! anything: a hint names what was seen so the next call can be different.

use serde_json::Value;

/// Fold the differences a model gets wrong without meaning anything by them:
/// case, every kind of whitespace (including the ideographic space U+3000),
/// and full-width ASCII (`Ｗｉ－Ｆｉ` → `wi-fi`).
fn fold(label: &str) -> String {
    label
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c as u32 {
            0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            _ => c,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// Labels on screen that differ from `wanted` only by case, whitespace or
/// full-width characters — "did you mean" candidates, at most three, in
/// screen order. An exact match is not a near miss.
pub fn near_misses<'a>(wanted: &str, labels: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let target = fold(wanted);
    if target.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for label in labels {
        if label == wanted || out.iter().any(|seen| seen == label) {
            continue;
        }
        if fold(label) == target {
            out.push(label.to_string());
            if out.len() == 3 {
                break;
            }
        }
    }
    out
}

/// One `did you mean` sentence, or `None` when there is nothing close.
pub fn did_you_mean<'a>(wanted: &str, labels: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let close = near_misses(wanted, labels);
    if close.is_empty() {
        return None;
    }
    let quoted: Vec<String> = close.iter().map(|l| format!("{l:?}")).collect();
    Some(format!(
        "did you mean {}? (differs only by case, spaces or full-width characters)",
        quoted.join(" or ")
    ))
}

/// Keys of operations to stop repeating: the last one if it changed nothing,
/// and any taken three times in the last four. Shared by Jev (which removes
/// them from its menu) and the plain tools (which only warn).
pub fn repeated_without_progress<K: PartialEq + Clone>(history: &[(K, bool)]) -> Vec<K> {
    let mut avoid = Vec::new();
    if let Some((key, changed)) = history.last() {
        if !changed {
            avoid.push(key.clone());
        }
    }
    let recent: Vec<K> = history
        .iter()
        .rev()
        .take(4)
        .map(|(k, _)| k.clone())
        .collect();
    for k in &recent {
        if recent.iter().filter(|r| *r == k).count() >= 3 && !avoid.contains(k) {
            avoid.push(k.clone());
        }
    }
    avoid
}

/// A bounded fingerprint: a 64-bit hash, never the raw labels.
fn fingerprint(parts: impl IntoIterator<Item = String>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    hasher.finish()
}

/// A stable digest of a full screen read: the kind and label of every row,
/// in order. Snapshot tokens change on every read, so they prove freshness
/// but cannot identify a screen.
fn screen_digest(rows: &[Value]) -> u64 {
    fingerprint(rows.iter().map(|row| {
        format!(
            "{}\u{1f}{}",
            row.get("kind").and_then(Value::as_str).unwrap_or(""),
            row.get("label").and_then(Value::as_str).unwrap_or("")
        )
    }))
}

/// Too little in the tree to say anything about it (Mode A territory).
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

/// What one tool call did, as the tracker sees it.
pub enum Action<'a> {
    /// Tap by label on the current screen.
    TapLabel(&'a str),
    /// Tap by element index against a snapshot.
    TapElement { index: u64, snapshot: &'a str },
    /// Tap a normalized point.
    TapPoint { x: f64, y: f64 },
    /// Scroll from an anchor.
    Scroll { x: f64, y: f64, dx: f64, dy: f64 },
}

/// The screen the tracker last saw in full.
#[derive(Debug, Clone)]
struct Screen {
    digest: u64,
    snapshot: String,
    /// (kind, label) per row, for resolving an element index to a target.
    rows: Vec<(String, String)>,
}

/// Watches observed actions in one MCP session and warns when the same
/// action, on the same screen, keeps settling with nothing changed.
///
/// It only speaks when it can prove its claim: a known screen (from a full
/// read), a target it can name on that screen, and consecutive complete,
/// settled observations with no change and the same app in front. Anything
/// else — an unobserved action, a failure, loading, a missing field, a long
/// idle, a call that overlapped another — resets the streak.
#[derive(Debug, Default)]
pub struct ProgressTracker {
    screen: Option<Screen>,
    /// (target fingerprint, consecutive unchanged count).
    streak: Option<(u64, u32)>,
    last_ms: u64,
    /// Calls started but not yet recorded; >1 means calls overlapped.
    in_flight: u32,
    overlapped: bool,
}

/// Silence longer than this ends a streak.
pub const PROGRESS_IDLE_MS: u64 = 120_000;

impl ProgressTracker {
    /// Forget everything (run boundary, owner released, idle).
    pub fn reset(&mut self) {
        let in_flight = self.in_flight;
        *self = Self::default();
        self.in_flight = in_flight;
    }

    /// A full screen read (`phone_elements`, or an observed answer that
    /// carried the whole tree because there was no baseline).
    pub fn note_screen(&mut self, json: &Value) {
        let Some(rows) = json.get("elements").and_then(Value::as_array) else {
            self.screen = None;
            return;
        };
        let snapshot = json.get("snapshot").and_then(Value::as_str).unwrap_or("");
        if snapshot.is_empty() || sparse(json, rows) {
            self.screen = None;
            self.streak = None;
            return;
        }
        let digest = screen_digest(rows);
        if self.screen.as_ref().is_some_and(|s| s.digest != digest) {
            self.streak = None;
        }
        self.screen = Some(Screen {
            digest,
            snapshot: snapshot.to_string(),
            rows: rows
                .iter()
                .map(|row| {
                    (
                        row.get("kind")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        row.get("label")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    )
                })
                .collect(),
        });
    }

    /// An action that returns no observation (type, key, shortcut, batch,
    /// flow, unobserved tap…): the screen is no longer known.
    pub fn note_unobserved(&mut self) {
        self.screen = None;
        self.streak = None;
    }

    /// Call when an observed action starts.
    pub fn begin(&mut self) {
        self.in_flight += 1;
        if self.in_flight > 1 {
            self.overlapped = true;
        }
    }

    /// The stable target of an action on the known screen, or `None` when it
    /// cannot be named (then no hint is possible).
    fn target(&self, action: &Action) -> Option<u64> {
        let screen = self.screen.as_ref()?;
        let what = match action {
            Action::TapLabel(label) => format!("label\u{1f}{label}"),
            Action::TapElement { index, snapshot } => {
                // The index means something only against the snapshot the
                // tracker saw for this screen.
                if *snapshot != screen.snapshot {
                    return None;
                }
                let (kind, label) = screen.rows.get(usize::try_from(*index).ok()?)?;
                if label.is_empty() {
                    return None;
                }
                format!("element\u{1f}{kind}\u{1f}{label}")
            }
            Action::TapPoint { x, y } => format!("point\u{1f}{:.2}\u{1f}{:.2}", x, y),
            Action::Scroll { x, y, dx, dy } => {
                format!("scroll\u{1f}{:.2}\u{1f}{:.2}\u{1f}{dx}\u{1f}{dy}", x, y)
            }
        };
        Some(fingerprint([screen.digest.to_string(), what]))
    }

    /// Record the daemon's answer to an observed action that started with
    /// [`begin`](Self::begin). Returns a `no_progress` advisory or `None`.
    pub fn record(&mut self, action: Action, json: Option<&Value>, now_ms: u64) -> Option<String> {
        self.in_flight = self.in_flight.saturating_sub(1);
        let overlapped = std::mem::take(&mut self.overlapped) || self.in_flight > 0;
        let idle = self.last_ms != 0 && now_ms.saturating_sub(self.last_ms) > PROGRESS_IDLE_MS;
        self.last_ms = now_ms;
        if overlapped || idle {
            self.reset();
            return None;
        }
        let target = self.target(&action);
        let Some(json) = json else {
            self.note_unobserved();
            return None;
        };
        let unchanged = complete_unchanged(json);
        if json.get("elements").is_some() {
            // No baseline: the whole tree came back. It is a fresh screen,
            // not evidence about this action.
            self.note_screen(json);
            self.streak = None;
            return None;
        }
        let (Some(target), Some(true)) = (target, unchanged) else {
            // A change, or anything we cannot read as "complete and
            // unchanged": the screen is no longer known in full.
            self.note_unobserved();
            return None;
        };
        // Unchanged: the screen digest still holds; refresh the snapshot so
        // a following tap_element against the new token resolves.
        if let (Some(screen), Some(snapshot)) = (
            self.screen.as_mut(),
            json.get("snapshot").and_then(Value::as_str),
        ) {
            screen.snapshot = snapshot.to_string();
        }
        let count = match self.streak {
            Some((last, count)) if last == target => count + 1,
            _ => 1,
        };
        self.streak = Some((target, count));
        (count >= 2).then(|| {
            format!(
                "no_progress (advice only): this same action on this same screen has now settled \
                 {count} times in a row with no change in the accessibility tree and no app \
                 switch. A change the tree does not show (sound, haptics, pixels only) would not \
                 appear here. Nothing was resent. Re-read the screen with phone_elements, then \
                 try a different control or wait for a specific condition; do not repeat an \
                 action whose outcome was unknown."
            )
        })
    }
}

/// `Some(true)` only for a complete, settled delta that reports no visible
/// change and no app switch; `Some(false)` for a complete delta that changed;
/// `None` when the answer is not usable evidence either way.
fn complete_unchanged(json: &Value) -> Option<bool> {
    if json.get("ok") != Some(&Value::Bool(true)) {
        return None;
    }
    let settle = json.get("settle")?;
    if settle.get("settled") != Some(&Value::Bool(true)) {
        return None;
    }
    if json.get("delta_error").is_some() || json.get("snapshot").is_none() {
        return None;
    }
    json.get("delta")?.as_object()?;
    json.get("baseline")?.as_str()?;
    if json.get("app_changed").is_some_and(|v| !v.is_null()) {
        return Some(false);
    }
    Some(json.get("no_visible_change") == Some(&Value::Bool(true)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn near_misses_fold_case_space_and_full_width() {
        let labels = ["Wi-Fi", "通 用", "ＷＩ－ＦＩ", "Bluetooth", "wi fi"];
        assert_eq!(
            near_misses("WiFi", labels),
            vec!["wi fi"],
            "a missing hyphen is not case/space"
        );
        assert_eq!(near_misses("wi-fi", labels), vec!["Wi-Fi", "ＷＩ－ＦＩ"]);
        assert_eq!(near_misses("通用", labels), vec!["通 用"]);
        assert_eq!(
            near_misses("Wi-Fi", labels),
            vec!["ＷＩ－ＦＩ"],
            "the exact label itself is excluded"
        );
        assert!(near_misses("   ", labels).is_empty());
        assert!(did_you_mean("蓝牙", labels).is_none());
        assert!(did_you_mean("通用", labels).unwrap().contains("\"通 用\""));
    }

    #[test]
    fn shared_rule_matches_jev_semantics() {
        assert_eq!(repeated_without_progress(&[("BACK", false)]), vec!["BACK"]);
        let looping = [("A", true), ("A", true), ("B", true), ("A", true)];
        assert_eq!(repeated_without_progress(&looping), vec!["A"]);
        assert!(repeated_without_progress(&[("A", true), ("B", true)]).is_empty());
    }

    /// A full read in the daemon's `/agent/elements` shape.
    fn screen(snapshot: &str, labels: &[&str]) -> Value {
        let rows: Vec<Value> = labels
            .iter()
            .map(|label| json!({"kind": "Button", "label": label}))
            .collect();
        json!({"snapshot": snapshot, "elements": rows,
               "ax_stats": {"n": rows.len(), "n_interactive": rows.len(), "container_only": false}})
    }

    /// An observed action in the daemon's `?return=delta` shape.
    fn delta(snapshot: &str, unchanged: bool) -> Value {
        let mut body = json!({"ok": true, "transport": "wda", "snapshot": snapshot,
            "baseline": "base", "delta": {"added": [], "removed": [], "changed": []},
            "settle": {"settled": true, "reason": "stable", "captures": 2, "waited_ms": 600}});
        if unchanged {
            body["no_visible_change"] = json!(true);
        }
        body
    }

    fn act(t: &mut ProgressTracker, action: Action, json: Value, now: u64) -> Option<String> {
        t.begin();
        t.record(action, Some(&json), now)
    }

    fn settings() -> ProgressTracker {
        let mut t = ProgressTracker::default();
        t.note_screen(&screen("s1", &["通用", "蓝牙", "无障碍"]));
        t
    }

    #[test]
    fn two_consecutive_unchanged_on_the_same_screen_warn() {
        let mut t = settings();
        assert!(act(&mut t, Action::TapLabel("通用"), delta("s2", true), 1_000).is_none());
        let warn = act(&mut t, Action::TapLabel("通用"), delta("s3", true), 2_000).unwrap();
        assert!(warn.starts_with("no_progress (advice only)") && warn.contains("2 times"));
        assert!(warn.contains("Nothing was resent"));
    }

    #[test]
    fn progress_in_between_is_not_a_loop() {
        // A changed, A changed, B changed, A unchanged → one unchanged only.
        let mut t = settings();
        for (label, unchanged) in [("通用", false), ("通用", false), ("蓝牙", false)] {
            assert!(act(
                &mut t,
                Action::TapLabel(label),
                delta("x", unchanged),
                1_000
            )
            .is_none());
            t.note_screen(&screen("s1", &["通用", "蓝牙", "无障碍"]));
        }
        assert!(act(&mut t, Action::TapLabel("通用"), delta("y", true), 2_000).is_none());
    }

    #[test]
    fn unusable_answers_reset_the_streak() {
        let missing_settle = {
            let mut d = delta("s", true);
            d.as_object_mut().unwrap().remove("settle");
            d
        };
        let loading = {
            let mut d = delta("s", true);
            d["settle"]["settled"] = json!(false);
            d
        };
        let no_baseline = {
            let mut d = delta("s", true);
            d.as_object_mut().unwrap().remove("baseline");
            d
        };
        let delta_error = {
            let mut d = delta("s", true);
            d["delta_error"] = json!("tree read failed");
            d
        };
        for bad in [
            missing_settle,
            loading,
            no_baseline,
            delta_error,
            json!({"ok": false}),
        ] {
            let mut t = settings();
            assert!(act(&mut t, Action::TapLabel("通用"), delta("a", true), 1_000).is_none());
            assert!(act(&mut t, Action::TapLabel("通用"), bad, 1_100).is_none());
            // The screen is no longer known, so even a clean unchanged answer
            // cannot be attributed until it is read again.
            assert!(act(&mut t, Action::TapLabel("通用"), delta("b", true), 1_200).is_none());
        }
        // A transport error (no answer) and an unobserved action reset too.
        let mut t = settings();
        act(&mut t, Action::TapLabel("通用"), delta("a", true), 1_000);
        t.begin();
        assert!(t.record(Action::TapLabel("通用"), None, 1_100).is_none());
        t.note_screen(&screen("s1", &["通用", "蓝牙", "无障碍"]));
        act(&mut t, Action::TapLabel("通用"), delta("a", true), 1_200);
        t.note_unobserved(); // e.g. phone_type
        t.note_screen(&screen("s1", &["通用", "蓝牙", "无障碍"]));
        assert!(act(&mut t, Action::TapLabel("通用"), delta("b", true), 1_300).is_none());
    }

    #[test]
    fn same_label_on_a_different_screen_is_a_different_target() {
        let mut t = settings();
        act(&mut t, Action::TapLabel("通用"), delta("a", true), 1_000);
        t.note_screen(&screen("z1", &["通用", "关于本机", "软件更新"]));
        assert!(act(&mut t, Action::TapLabel("通用"), delta("b", true), 1_100).is_none());
    }

    #[test]
    fn element_index_resolves_against_its_snapshot_only() {
        let mut t = settings();
        let tap = |snapshot| Action::TapElement { index: 1, snapshot };
        assert!(act(&mut t, tap("s1"), delta("s2", true), 1_000).is_none());
        // The unchanged answer refreshed the snapshot, so the new token resolves.
        assert!(act(&mut t, tap("s2"), delta("s3", true), 1_100).is_some());
        // A token the tracker never saw cannot be named: no hint, streak ends.
        let mut t = settings();
        act(&mut t, tap("s1"), delta("s2", true), 1_000);
        assert!(act(&mut t, tap("other"), delta("s3", true), 1_100).is_none());
    }

    #[test]
    fn scroll_start_points_are_part_of_the_target() {
        let mut t = settings();
        let scroll = |y| Action::Scroll {
            x: 0.5,
            y,
            dx: 0.0,
            dy: 300.0,
        };
        act(&mut t, scroll(0.5), delta("a", true), 1_000);
        assert!(act(&mut t, scroll(0.8), delta("b", true), 1_100).is_none());
        assert!(act(&mut t, scroll(0.8), delta("c", true), 1_200).is_some());
    }

    #[test]
    fn app_switch_long_idle_and_sparse_trees_never_warn() {
        let mut t = settings();
        let mut switched = delta("a", true);
        switched["app_changed"] = json!({"from": "设置", "to": "微信"});
        act(&mut t, Action::TapLabel("通用"), switched, 1_000);
        assert!(act(&mut t, Action::TapLabel("通用"), delta("b", true), 1_100).is_none());

        let mut t = settings();
        act(&mut t, Action::TapLabel("通用"), delta("a", true), 1_000);
        assert!(act(
            &mut t,
            Action::TapLabel("通用"),
            delta("b", true),
            1_000 + PROGRESS_IDLE_MS + 1
        )
        .is_none());

        let mut t = ProgressTracker::default();
        let mut thin = screen("s1", &["通用", "蓝牙", "无障碍"]);
        thin["ax_stats"]["n_interactive"] = json!(0);
        t.note_screen(&thin);
        act(&mut t, Action::TapLabel("通用"), delta("a", true), 1_000);
        assert!(act(&mut t, Action::TapLabel("通用"), delta("b", true), 1_100).is_none());
    }

    #[test]
    fn overlapping_calls_reset_instead_of_guessing() {
        let mut t = settings();
        act(&mut t, Action::TapLabel("通用"), delta("a", true), 1_000);
        t.begin();
        t.begin(); // a second call started before the first answered
        assert!(t
            .record(Action::TapLabel("通用"), Some(&delta("b", true)), 1_100)
            .is_none());
        assert!(t
            .record(Action::TapLabel("通用"), Some(&delta("c", true)), 1_200)
            .is_none());
    }

    #[test]
    fn no_baseline_answers_are_a_fresh_screen_not_evidence() {
        let mut t = settings();
        act(&mut t, Action::TapLabel("通用"), delta("a", true), 1_000);
        let mut full = screen("f", &["通用", "蓝牙", "无障碍"]);
        full["ok"] = json!(true);
        full["settle"] = json!({"settled": true});
        assert!(act(&mut t, Action::TapLabel("通用"), full, 1_100).is_none());
        assert!(act(&mut t, Action::TapLabel("通用"), delta("b", true), 1_200).is_none());
    }
}
