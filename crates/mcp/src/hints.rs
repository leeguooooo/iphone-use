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

/// Attach a screenshot to a screen read only when the tree is unusable: no
/// interactive rows and nothing but containers (the reference's Mode A
/// "vision" rule). A capture-redacted screen is excluded — its screenshot is
/// blank or a wireframe of the same tree, so it adds nothing.
pub fn needs_image(json: &Value) -> bool {
    if json.get("capture_redacted") == Some(&Value::Bool(true)) {
        return false;
    }
    let Some(stats) = json.get("ax_stats") else {
        return false;
    };
    stats.get("n_interactive").and_then(Value::as_u64) == Some(0)
        && stats.get("container_only") == Some(&Value::Bool(true))
}

pub use iu_core::progress::{Action, ProgressGuard, ProgressTracker};

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
    fn images_only_for_an_unusable_tree() {
        let mode_a = json!({"ax_stats": {"n_interactive": 0, "container_only": true}});
        assert!(needs_image(&mode_a));
        let hybrid = json!({"ax_stats": {"n_interactive": 2, "container_only": false}});
        assert!(!needs_image(&hybrid));
        let no_stats = json!({"elements": []});
        assert!(!needs_image(&no_stats));
        let redacted = json!({"capture_redacted": true,
                              "ax_stats": {"n_interactive": 0, "container_only": true}});
        assert!(!needs_image(&redacted));
    }

    #[test]
    fn shared_rule_matches_jev_semantics() {
        assert_eq!(repeated_without_progress(&[("BACK", false)]), vec!["BACK"]);
        let looping = [("A", true), ("A", true), ("B", true), ("A", true)];
        assert_eq!(repeated_without_progress(&looping), vec!["A"]);
        assert!(repeated_without_progress(&[("A", true), ("B", true)]).is_empty());
    }
}
