//! Compact text for what a model reads after `phone_elements` and an observed
//! action: one line per row instead of the daemon's JSON. Measured against
//! callstack/agent-device on Settings: its text snapshot was 575 B where our
//! JSON was ~10 KB, for the same screen. The structured JSON still rides along
//! as MCP `structuredContent` for programs; only the text a model reads shrinks.

use serde_json::Value;

/// Rows listed by label in the off-screen summary before it says "…".
const OFFSCREEN_LABELS: usize = 5;
/// Longest label printed before it is cut.
const LABEL_CHARS: usize = 80;

fn text(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn clip(label: &str) -> String {
    let mut out: String = label.chars().take(LABEL_CHARS).collect();
    if label.chars().count() > LABEL_CHARS {
        out.push('…');
    }
    out
}

/// `#4 [Button] "通用" id=… value=… focused disabled`. The identifier is
/// printed only when the label cannot tell the row apart (empty, or shared
/// with another row): it is in the structured content either way.
fn row_line(index: u64, row: &Value, show_id: bool) -> String {
    let kind = text(row, "kind").unwrap_or_default();
    let label = text(row, "label").unwrap_or_default();
    let mut line = format!("#{index} [{kind}] \"{}\"", clip(&label));
    if let Some(id) = text(row, "identifier")
        .filter(|id| (show_id || label.is_empty()) && *id != label && !generated_id(id))
    {
        line.push_str(&format!(" id={id}"));
    }
    if let Some(value) = text(row, "value").filter(|value| *value != label) {
        line.push_str(&format!(" value={}", clip(&value)));
    }
    if text(row, "placeholder").is_some() && text(row, "value").is_none() {
        line.push_str(&format!(
            " placeholder={}",
            text(row, "placeholder").unwrap_or_default()
        ));
    }
    if row.get("focused") == Some(&Value::Bool(true)) {
        line.push_str(" focused");
    }
    if row.get("enabled") == Some(&Value::Bool(false)) {
        line.push_str(" disabled");
    }
    if row.get("selected") == Some(&Value::Bool(true)) {
        line.push_str(" selected");
    }
    if let Some(overlay) = text(row, "overlay") {
        line.push_str(&format!(" overlay={overlay}"));
    }
    line
}

/// Kinds a person taps; their label already names the text and icons inside.
const TAPPABLE: &[&str] = &[
    "Button",
    "Cell",
    "Link",
    "Switch",
    "Slider",
    "TextField",
    "SecureTextField",
    "SearchField",
    "TextView",
    "Tab",
    "MenuItem",
    "SegmentedControl",
    "Stepper",
];

/// Auto-generated identifiers (UUIDs, long dotted paths) name nothing a model
/// can use; the structured content still carries them.
fn generated_id(id: &str) -> bool {
    let uuid_like = id
        .split(|c: char| !c.is_ascii_hexdigit())
        .any(|part| part.len() >= 8)
        && id.matches('-').count() >= 4;
    id.chars().count() > 48 || uuid_like
}

/// Containers that lay rows out: a list, a scroll bar, a toolbar strip.
/// Their rows still carry the controls inside them.
const LAYOUT_KINDS: &[&str] = &[
    "Other",
    "CollectionView",
    "Table",
    "ScrollView",
    "Toolbar",
    "Window",
];

/// An SF Symbol name such as `chevron.forward`: decoration, not content.
fn symbol_name(label: &str) -> bool {
    !label.is_empty()
        && label.contains('.')
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.')
}

/// Rows a model gains nothing from, given the row after it and the tappable
/// rows it sits inside: text or an icon repeating its button's label, an SF
/// Symbol, an unlabeled wrapper cell around a labeled control.
fn is_noise(row: &Value, next: Option<&Value>, tappable_ancestors: &[(u64, String)]) -> bool {
    let kind = text(row, "kind").unwrap_or_default();
    let label = text(row, "label").unwrap_or_default();
    if TAPPABLE.contains(&kind.as_str()) {
        let depth = row.get("depth").and_then(Value::as_u64).unwrap_or(0);
        let wraps_next = next.is_some_and(|next| {
            next.get("depth").and_then(Value::as_u64).unwrap_or(0) > depth
                && !text(next, "label").unwrap_or_default().is_empty()
        });
        return label.is_empty() && kind == "Cell" && wraps_next;
    }
    if symbol_name(&label) || LAYOUT_KINDS.contains(&kind.as_str()) {
        return true;
    }
    if label.is_empty() {
        return false;
    }
    tappable_ancestors
        .last()
        .is_some_and(|(_, outer)| outer.contains(label.as_str()))
}

fn label_counts<'a>(
    rows: impl Iterator<Item = &'a Value>,
) -> std::collections::HashMap<String, usize> {
    let mut counts = std::collections::HashMap::new();
    // Controls only: telling controls apart is what an id is for here.
    for row in rows {
        let tappable = TAPPABLE.contains(&text(row, "kind").unwrap_or_default().as_str());
        if let Some(label) = text(row, "label").filter(|_| tappable) {
            *counts.entry(label).or_insert(0) += 1;
        }
    }
    counts
}

fn shared_label(row: &Value, counts: &std::collections::HashMap<String, usize>) -> bool {
    text(row, "label").is_some_and(|label| counts.get(&label).copied().unwrap_or(0) > 1)
}

/// Plain text (not a control) saying what an earlier line already said: a
/// page title repeated under its navigation bar.
fn repeated_text(row: &Value, printed: &std::collections::HashSet<String>) -> bool {
    let kind = text(row, "kind").unwrap_or_default();
    !TAPPABLE.contains(&kind.as_str())
        && text(row, "label").is_some_and(|label| printed.contains(&label))
}

fn is_hidden(row: &Value) -> bool {
    row.get("visible") == Some(&Value::Bool(false))
}

/// Rows of a whole tree: visible rows one per line, hidden ones summarized,
/// decoration (repeated text, icons, wrapper cells) left out. Indexes stay
/// the daemon's, so `#N` is still the element to tap.
fn rows_block(rows: &[Value], out: &mut Vec<String>) {
    let mut hidden = Vec::new();
    let mut omitted = 0;
    let counts = label_counts(rows.iter());
    let mut printed: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Tappable rows enclosing the current one: (depth, label).
    let mut ancestors: Vec<(u64, String)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let depth = row.get("depth").and_then(Value::as_u64).unwrap_or(0);
        while ancestors.last().is_some_and(|(d, _)| *d >= depth) {
            ancestors.pop();
        }
        let noise = is_noise(row, rows.get(index + 1), &ancestors);
        let kind = text(row, "kind").unwrap_or_default();
        let label = text(row, "label").unwrap_or_default();
        if TAPPABLE.contains(&kind.as_str()) && !label.is_empty() {
            ancestors.push((depth, label));
        }
        if is_hidden(row) {
            hidden.push(text(row, "label").unwrap_or_default());
            continue;
        }
        if noise || repeated_text(row, &printed) {
            omitted += 1;
            continue;
        }
        printed.insert(text(row, "label").unwrap_or_default());
        out.push(row_line(index as u64, row, shared_label(row, &counts)));
    }
    hidden_summary(&hidden, out);
    if omitted > 0 {
        out.push(format!("({omitted} decorative/layout rows omitted)"));
    }
}

fn hidden_summary(hidden: &[String], out: &mut Vec<String>) {
    if hidden.is_empty() {
        return;
    }
    let named: Vec<String> = hidden
        .iter()
        .filter(|label| !label.is_empty())
        .take(OFFSCREEN_LABELS)
        .map(|label| format!("\"{}\"", clip(label)))
        .collect();
    let more = if hidden.len() > named.len() {
        ", …"
    } else {
        ""
    };
    out.push(format!(
        "({} rows off-screen or not drawn: {}{more})",
        hidden.len(),
        named.join(", ")
    ));
}

/// The blocks around the rows that a model should still see, compactly.
fn extra_blocks(json: &Value, out: &mut Vec<String>) {
    if let Some(alert) = json.get("alert").filter(|alert| !alert.is_null()) {
        let body = text(alert, "text").unwrap_or_default();
        let buttons = alert
            .get("buttons")
            .and_then(Value::as_array)
            .map(|buttons| {
                buttons
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .unwrap_or_default();
        out.push(format!("alert: \"{}\" buttons: {buttons}", clip(&body)));
    }
    for key in [
        "app_changed",
        "registry",
        "flow_suggestion",
        "agent_focus",
        "batch_hint",
    ] {
        if let Some(block) = json.get(key).filter(|block| !block.is_null()) {
            out.push(format!("{key}: {block}"));
        }
    }
    for key in ["hint", "delta_error"] {
        if let Some(line) = text(json, key) {
            out.push(format!("{key}: {line}"));
        }
    }
    if json.get("capture_redacted") == Some(&Value::Bool(true)) {
        out.push("capture_redacted: the app hides this screen from screenshots".to_string());
    }
}

/// `GET /agent/elements` as text, or `None` when the body is not a tree (an
/// error answer keeps its JSON so every field a caller branches on survives).
pub fn elements(json: &Value) -> Option<String> {
    let rows = json.get("elements")?.as_array()?;
    let snapshot = text(json, "snapshot")?;
    if json.get("error").is_some() {
        return None;
    }
    let mut out = vec![format!(
        "snapshot {snapshot} · {} rows (tap: element=#N + snapshot)",
        rows.len()
    )];
    rows_block(rows, &mut out);
    extra_blocks(json, &mut out);
    Some(out.join("\n"))
}

/// An observed action (`?return=delta`) as text: the verdict, then what
/// changed, or the whole tree when there was no baseline. `None` when the
/// body carries no observation (it keeps the daemon's own text).
pub fn observed(json: &Value) -> Option<String> {
    if json.get("ok") != Some(&Value::Bool(true)) {
        return None;
    }
    let snapshot = text(json, "snapshot")?;
    let mut head = format!("ok · snapshot {snapshot}");
    if let Some(settle) = json.get("settle") {
        let reason = text(settle, "reason").unwrap_or_default();
        let waited = text(settle, "waited_ms").unwrap_or_default();
        head.push_str(&format!(" · settle {reason} {waited}ms"));
    }
    if json.get("no_visible_change") == Some(&Value::Bool(true)) {
        head.push_str(" · NO VISIBLE CHANGE");
    }
    let mut out = vec![head];
    if let Some(delta) = json.get("delta") {
        let indexed = |key: &str| -> Vec<(u64, Value)> {
            delta
                .get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            Some((item.get("index")?.as_u64()?, item.get("element")?.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut hidden = Vec::new();
        let mut omitted = 0;
        // Added and changed rows in tree order, so the decoration rules (text
        // repeating its control, icons, wrapper cells) see their controls.
        let mut rows: Vec<(&str, u64, Value)> = indexed("added")
            .into_iter()
            .map(|(index, row)| ("+", index, row))
            .chain(
                indexed("changed")
                    .into_iter()
                    .map(|(index, row)| ("~", index, row)),
            )
            .collect();
        rows.sort_by_key(|(_, index, _)| *index);
        let counts = label_counts(rows.iter().map(|(_, _, row)| row));
        let mut printed: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut ancestors: Vec<(u64, String)> = Vec::new();
        for (position, (sign, index, row)) in rows.iter().enumerate() {
            let depth = row.get("depth").and_then(Value::as_u64).unwrap_or(0);
            while ancestors.last().is_some_and(|(d, _)| *d >= depth) {
                ancestors.pop();
            }
            let next = rows
                .get(position + 1)
                .filter(|(_, next_index, _)| *next_index == index + 1)
                .map(|(_, _, next)| next);
            let noise = is_noise(row, next, &ancestors);
            let kind = text(row, "kind").unwrap_or_default();
            let label = text(row, "label").unwrap_or_default();
            if TAPPABLE.contains(&kind.as_str()) && !label.is_empty() {
                ancestors.push((depth, label));
            }
            if is_hidden(row) {
                hidden.push(text(row, "label").unwrap_or_default());
            } else if noise || repeated_text(row, &printed) {
                omitted += 1;
            } else {
                printed.insert(text(row, "label").unwrap_or_default());
                out.push(format!(
                    "{sign} {}",
                    row_line(*index, row, shared_label(row, &counts))
                ));
            }
        }
        hidden_summary(&hidden, &mut out);
        if omitted > 0 {
            out.push(format!("({omitted} decorative rows omitted)"));
        }
        let removed = delta
            .get("removed")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let unchanged = delta.get("unchanged").and_then(Value::as_u64).unwrap_or(0);
        out.push(format!("- {removed} removed · = {unchanged} unchanged"));
    } else if let Some(rows) = json.get("elements").and_then(Value::as_array) {
        out.push(format!("{} rows (no baseline: whole screen)", rows.len()));
        rows_block(rows, &mut out);
    } else {
        return None;
    }
    extra_blocks(json, &mut out);
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_tree_reads_as_one_line_per_row_with_hidden_rows_summarized() {
        let body = json!({
            "snapshot": "S1",
            "elements": [
                {"kind": "Application", "label": "设置", "rect": [0, 0, 440, 956], "depth": 0},
                {"kind": "Button", "label": "通用", "identifier": "com.apple.settings.general", "rect": [20, 700, 400, 50], "depth": 3},
                {"kind": "Switch", "label": "飞行模式", "value": "0", "rect": [20, 300, 400, 50], "depth": 3},
                {"kind": "Cell", "label": "隐私", "visible": false, "rect": [20, 1300, 400, 50], "depth": 3}
            ],
            "alert": null,
            "timing": {"total_ms": 900}
        });
        let text = elements(&body).unwrap();
        assert_eq!(
            text,
            "snapshot S1 · 4 rows (tap: element=#N + snapshot)\n\
             #0 [Application] \"设置\"\n\
             #1 [Button] \"通用\"\n\
             #2 [Switch] \"飞行模式\" value=0\n\
             (1 rows off-screen or not drawn: \"隐私\")"
        );
    }

    #[test]
    fn decoration_is_left_out_but_indexes_stay() {
        let body = json!({
            "snapshot": "S",
            "elements": [
                {"kind": "Cell", "label": "", "depth": 4},
                {"kind": "Button", "label": "通用", "identifier": "com.apple.settings.general", "depth": 5},
                {"kind": "StaticText", "label": "通用", "depth": 6},
                {"kind": "Image", "label": "chevron.forward", "depth": 6},
                {"kind": "Button", "label": "建议", "identifier": "a.b.940554BD-3D6C-4A59-B3ED-B00900978BB8", "depth": 5},
                {"kind": "StaticText", "label": "2", "depth": 6}
            ]
        });
        assert_eq!(
            elements(&body).unwrap(),
            "snapshot S · 6 rows (tap: element=#N + snapshot)\n\
             #1 [Button] \"通用\"\n\
             #4 [Button] \"建议\"\n\
             #5 [StaticText] \"2\"\n\
             (3 decorative/layout rows omitted)"
        );
    }

    #[test]
    fn an_error_answer_keeps_its_json() {
        assert_eq!(
            elements(&json!({"elements": [], "error": "wda_source_failed"})),
            None
        );
    }

    #[test]
    fn an_observed_action_reads_as_its_change() {
        let body = json!({
            "ok": true,
            "snapshot": "S2",
            "baseline": "S1",
            "settle": {"reason": "stable", "waited_ms": 420, "captures": 1},
            "delta": {
                "added": [{"index": 5, "element": {"kind": "Cell", "label": "关于本机"}}],
                "changed": [{"index": 2, "element": {"kind": "Switch", "label": "飞行模式", "value": "1"}}],
                "removed": [7, 8],
                "unchanged": 12
            }
        });
        assert_eq!(
            observed(&body).unwrap(),
            "ok · snapshot S2 · settle stable 420ms\n\
             ~ #2 [Switch] \"飞行模式\" value=1\n\
             + #5 [Cell] \"关于本机\"\n\
             - 2 removed · = 12 unchanged"
        );
    }

    #[test]
    fn a_bare_ok_is_not_rewritten() {
        assert_eq!(observed(&json!({"ok": true, "transport": "wda"})), None);
    }
}
