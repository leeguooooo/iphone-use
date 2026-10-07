//! Advisory hints for the model: near-miss labels and "no progress".
//!
//! Both are advice only. Nothing here sends an action, goes back, or replays
//! anything: a hint names what was seen so the next call can be different.

use std::collections::VecDeque;

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

/// Watches observed actions in one MCP session and warns when the same action
/// keeps leaving the screen unchanged.
#[derive(Debug, Default)]
pub struct ProgressTracker {
    /// (app, action signature, changed) for the last few observed actions.
    recent: VecDeque<(String, String, bool)>,
}

const KEEP: usize = 6;

impl ProgressTracker {
    /// Record one observed action and return a `no_progress` line when the
    /// action has stopped making progress. `json` is the daemon's observed
    /// response; an unobserved or unparseable one is ignored (no evidence).
    pub fn record(&mut self, signature: &str, json: Option<&Value>) -> Option<String> {
        let json = json?;
        if json.get("ok") != Some(&Value::Bool(true)) {
            return None;
        }
        // Still loading: an unsettled screen or a spinner is waiting, not
        // stuck, so it neither counts nor warns.
        if still_loading(json) {
            return None;
        }
        let changed = json.get("no_visible_change") != Some(&Value::Bool(true));
        let app = json
            .get("application")
            .or_else(|| json.get("app"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if self
            .recent
            .back()
            .is_some_and(|(last_app, _, _)| *last_app != app)
        {
            self.recent.clear();
        }
        self.recent.push_back((app, signature.to_string(), changed));
        while self.recent.len() > KEEP {
            self.recent.pop_front();
        }
        let history: Vec<(String, bool)> = self
            .recent
            .iter()
            .map(|(_, sig, changed)| (sig.clone(), *changed))
            .collect();
        let unchanged_streak = history
            .iter()
            .rev()
            .take_while(|(sig, changed)| sig == signature && !changed)
            .count();
        let looping = history
            .iter()
            .rev()
            .take(4)
            .filter(|(sig, _)| sig == signature)
            .count()
            >= 3;
        if unchanged_streak >= 2 || (looping && !changed) {
            Some(format!(
                "no_progress: `{signature}` left the screen unchanged {} time(s) in a row — \
                 re-read the screen (phone_elements) and try a different control or wait for \
                 a condition. Do not resend an action whose outcome was unknown.",
                unchanged_streak.max(1)
            ))
        } else {
            None
        }
    }
}

fn still_loading(json: &Value) -> bool {
    if let Some(settle) = json.get("settle") {
        if settle.get("settled") == Some(&Value::Bool(false)) {
            return true;
        }
    }
    let spinner = |rows: Option<&Value>| {
        rows.and_then(Value::as_array).is_some_and(|rows| {
            rows.iter().any(|row| {
                let row = row.get("element").unwrap_or(row);
                matches!(
                    row.get("kind").and_then(Value::as_str),
                    Some("ActivityIndicator" | "ProgressIndicator")
                )
            })
        })
    };
    spinner(json.get("elements"))
        || json
            .get("delta")
            .is_some_and(|d| spinner(d.get("added")) || spinner(d.get("changed")))
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

    fn observed(app: &str, unchanged: bool) -> Value {
        json!({"ok": true, "application": app, "no_visible_change": unchanged,
               "settle": {"settled": true, "reason": "stable"}})
    }

    #[test]
    fn warns_after_two_unchanged_repeats_only() {
        let mut t = ProgressTracker::default();
        assert!(t
            .record("tap 搜索", Some(&observed("设置", true)))
            .is_none());
        let warn = t.record("tap 搜索", Some(&observed("设置", true))).unwrap();
        assert!(warn.starts_with("no_progress:") && warn.contains("2 time"));
        // A different action that changes the screen resets nothing falsely.
        assert!(t
            .record("tap 通用", Some(&observed("设置", false)))
            .is_none());
    }

    #[test]
    fn loading_and_unobserved_never_warn() {
        let mut t = ProgressTracker::default();
        let loading = json!({"ok": true, "application": "a", "no_visible_change": true,
                             "settle": {"settled": false, "reason": "budget"}});
        let spinner = json!({"ok": true, "application": "a", "no_visible_change": true,
                             "delta": {"added": [{"index": 3, "element": {"kind": "ActivityIndicator"}}]}});
        for _ in 0..4 {
            assert!(t.record("tap x", Some(&loading)).is_none());
            assert!(t.record("tap x", Some(&spinner)).is_none());
            assert!(t.record("tap x", None).is_none());
            assert!(t.record("tap x", Some(&json!({"ok": false}))).is_none());
        }
    }

    #[test]
    fn switching_apps_starts_over() {
        let mut t = ProgressTracker::default();
        assert!(t.record("tap x", Some(&observed("a", true))).is_none());
        assert!(t.record("tap x", Some(&observed("b", true))).is_none());
    }
}
