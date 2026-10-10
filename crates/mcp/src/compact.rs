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
    // The daemon's advice already names itself ("no_progress (advice only): …").
    if let Some(line) = text(json, "no_progress") {
        out.push(line);
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

/// A successful observed batch (`POST /agent/actions` with `observe`) as text:
/// the verdict and step count, one short line for the steps, then the screen
/// the batch ended on — rendered once, exactly as an observed single action
/// is. `None` keeps the daemon's JSON: a failure (its evidence is the whole
/// body), an unobserved batch, or a body without a screen.
///
/// Measured on i13 (Settings → 蓝牙): the batch text was the raw JSON, about
/// 4.3 KB, with the end screen repeated in `structuredContent`. Compact, the
/// batch costs the model fewer bytes than the single calls it replaces.
pub fn batch(json: &Value) -> Option<String> {
    if json.get("ok") != Some(&Value::Bool(true)) || json.get("error").is_some() {
        return None;
    }
    let screen = observed(json)?;
    let mut lines = screen.lines();
    let mut head = lines.next()?.to_string();
    let steps = json.get("steps").and_then(Value::as_array);
    let total = steps.map_or(0, Vec::len);
    let completed = text(json, "completed").unwrap_or_else(|| total.to_string());
    let applied = text(json, "applied_actions").unwrap_or_else(|| "0".to_string());
    head.push_str(&format!(" · {completed}/{total} steps · {applied} applied"));
    let mut out = vec![head];
    if let Some(steps) = steps {
        let list = steps
            .iter()
            .enumerate()
            .map(|(position, step)| {
                let index = text(step, "index").unwrap_or_else(|| (position + 1).to_string());
                let kind = text(step, "kind").unwrap_or_default();
                let mark = if step.get("ok") == Some(&Value::Bool(false)) {
                    format!(" FAILED {}", text(step, "error").unwrap_or_default())
                } else {
                    String::new()
                };
                format!("{index} {kind}{mark}")
            })
            .collect::<Vec<_>>()
            .join(" · ");
        if !list.is_empty() {
            out.push(format!("steps: {list}"));
        }
    }
    out.extend(lines.map(str::to_string));
    Some(out.join("\n"))
}

/// Bounded JSON text for a field a model may need verbatim (outputs, a
/// diagnosis): whole when small, cut on a char boundary with `…` otherwise.
fn bounded_json(value: &Value, limit: usize) -> String {
    let mut rendered = value.to_string();
    if rendered.len() > limit {
        let cut = (0..=limit).rev().find(|i| rendered.is_char_boundary(*i)).unwrap_or(0);
        rendered.truncate(cut);
        rendered.push('…');
    }
    rendered
}

/// `phone_flow_list`, one row per flow: id, risk, compat verdict, whether it
/// was ever verified on hardware, its inputs, and a one-line description.
/// The full entries (steps metadata, verified_on, app versions) stay in the
/// structured copy and behind `detail=true`.
pub fn flow_list(json: &Value) -> Option<String> {
    let flows = json.get("flows")?.as_array()?;
    let mut out = vec![format!(
        "{} flows (id · risk · compat · inputs — description); phone_flow_run(id) runs one",
        flows.len()
    )];
    for flow in flows {
        let id = text(flow, "id").unwrap_or_default();
        let risk = text(flow, "risk").unwrap_or_default();
        let compat = flow
            .get("compat")
            .and_then(|c| text(c, "compat").or_else(|| c.as_str().map(str::to_string)))
            .unwrap_or_default();
        let verified = if flow.get("verified") == Some(&Value::Bool(true)) { "" } else { " (unverified)" };
        let inputs = flow
            .get("inputs")
            .and_then(Value::as_object)
            .map(|inputs| inputs.keys().cloned().collect::<Vec<_>>().join(","))
            .filter(|s| !s.is_empty())
            .map(|s| format!(" · inputs {s}"))
            .unwrap_or_default();
        let mut description = text(flow, "description").unwrap_or_default();
        if description.chars().count() > 80 {
            description = description.chars().take(79).collect::<String>() + "…";
        }
        out.push(format!("{id} · {risk} · {compat}{verified}{inputs} — {description}"));
    }
    Some(out.join("\n"))
}

/// `phone_flow_run`'s summary, compact: verdict, counts, outputs, the
/// failure's diagnosis when it failed, and the hint. The whole summary (every
/// step result) stays in the structured copy.
pub fn flow_run(summary: &Value) -> Option<String> {
    let result = summary.get("result")?;
    let flow = text(summary, "flow").unwrap_or_default();
    let ok = result.get("ok") == Some(&Value::Bool(true));
    let completed = text(result, "completed").unwrap_or_else(|| "?".to_string());
    let applied = text(result, "applied_actions").unwrap_or_else(|| "?".to_string());
    let mut head = if text(result, "outcome").as_deref() == Some("unknown") {
        // Never a count we do not have: the request left, the answer did not
        // say what the phone did.
        let reason = text(result, "reason").unwrap_or_default();
        format!("UNKNOWN outcome for flow {flow} ({reason}): the phone may have acted")
    } else if ok {
        format!("ok · flow {flow} · {completed} steps completed · {applied} applied")
    } else {
        let error = text(result, "error").unwrap_or_else(|| "failed".to_string());
        let failed = text(result, "failed_step").unwrap_or_default();
        format!("FAILED flow {flow}: {error} · failed step {failed} · {completed} completed · {applied} applied")
    };
    match result.get("retry_safe").and_then(Value::as_bool) {
        Some(false) if !ok => head.push_str(" · retry_safe=false: DO NOT replay"),
        Some(true) if !ok => head.push_str(" · retry_safe"),
        _ => {}
    }
    if let Some(compat) = summary.get("compat").and_then(|c| text(c, "compat")) {
        head.push_str(&format!(" · compat {compat}"));
    }
    let mut out = vec![head];
    if let Some(from) = text(result, "fallback_from") {
        out.push(format!("ran the other-language variant after {from} missed"));
    }
    if let Some(outputs) = summary.get("outputs") {
        out.push(format!("outputs: {}", bounded_json(outputs, 2000)));
    }
    if let Some(verify) = summary.get("verify") {
        out.push(format!("verify: {}", bounded_json(verify, 600)));
    }
    if !ok {
        if let Some(diagnosis) = result.get("diagnosis") {
            out.push(format!("diagnosis: {}", bounded_json(diagnosis, 1500)));
        }
    }
    if let Some(hint) = text(summary, "hint") {
        out.push(format!("hint: {hint}"));
    }
    Some(out.join("\n"))
}

/// A failed batch, compact: the verdict line a caller branches on, the step
/// list with the failure marked, the failed step's own evidence (its large
/// observation and timing left to the structured copy), and the screen the
/// batch ended on when the daemon observed one. `None` when the body is not a
/// failed batch, so the caller keeps the raw JSON.
pub fn failed_batch(json: &Value) -> Option<String> {
    if json.get("ok") != Some(&Value::Bool(false)) {
        return None;
    }
    let steps = json.get("steps").and_then(Value::as_array)?;
    let error = text(json, "error").unwrap_or_else(|| "failed".to_string());
    let failed = text(json, "failed_step").unwrap_or_default();
    let completed = text(json, "completed").unwrap_or_default();
    let applied = text(json, "applied_actions").unwrap_or_else(|| "0".to_string());
    let outcome = text(json, "batch_outcome")
        .or_else(|| text(json, "outcome"))
        .unwrap_or_default();
    let retry_safe = json.get("retry_safe").and_then(Value::as_bool);
    let mut head = format!(
        "FAILED {error} · step {failed} of {} · {completed} completed · {applied} applied · {outcome}",
        steps.len()
    );
    match retry_safe {
        Some(true) => head.push_str(" · retry_safe"),
        Some(false) => head.push_str(" · retry_safe=false: DO NOT replay; read the screen first"),
        None => {}
    }
    let mut out = vec![head];
    let list = steps
        .iter()
        .enumerate()
        .map(|(position, step)| {
            let index = text(step, "index").unwrap_or_else(|| (position + 1).to_string());
            let kind = text(step, "kind").unwrap_or_default();
            if step.get("ok") == Some(&Value::Bool(false)) {
                format!("{index} {kind} FAILED")
            } else {
                format!("{index} {kind}")
            }
        })
        .collect::<Vec<_>>()
        .join(" · ");
    if !list.is_empty() {
        out.push(format!("steps: {list}"));
    }
    // The failed step's evidence, minus the bulk a model does not need to
    // decide the next move (its full tree and timing stay structured).
    let failed_step = steps
        .iter()
        .find(|step| step.get("ok") == Some(&Value::Bool(false)))
        .or_else(|| steps.last());
    if let Some(Value::Object(step)) = failed_step {
        let mut detail = serde_json::Map::new();
        for (key, value) in step {
            if matches!(
                key.as_str(),
                "observation" | "timing" | "elements" | "delta" | "screen" | "ax_stats"
            ) {
                continue;
            }
            detail.insert(key.clone(), value.clone());
        }
        let mut rendered = Value::Object(detail).to_string();
        if rendered.len() > 1200 {
            let cut = (0..=1200).rev().find(|i| rendered.is_char_boundary(*i)).unwrap_or(0);
            rendered.truncate(cut);
            rendered.push('…');
        }
        out.push(format!("failed step: {rendered}"));
    }
    // The end screen, when the daemon observed one, rendered like a success.
    let end = json
        .get("observation")
        .or_else(|| failed_step.and_then(|step| step.get("observation")));
    if let Some(Value::Object(observation)) = end {
        let mut view = observation.clone();
        view.insert("ok".to_string(), Value::Bool(true));
        if let Some(screen) = observed(&Value::Object(view)) {
            let mut lines = screen.lines();
            if let Some(first) = lines.next() {
                out.push(format!("end screen: {}", first.trim_start_matches("ok · ")));
            }
            out.extend(lines.map(str::to_string));
        } else if observation.get("read") == Some(&Value::Bool(false)) {
            let hint = text(&Value::Object(observation.clone()), "hint").unwrap_or_default();
            out.push(format!("end screen: unreadable — {hint}"));
        }
    }
    Some(out.join("\n"))
}

/// `phone_collect_list`: the verdict, then one line per collected row.
pub fn collected_list(json: &Value) -> Option<String> {
    let rows = json.get("rows")?.as_array()?;
    let mut out = vec![format!(
        "collected {} rows over {} pages ({} swipes); stop_reason={} complete={}",
        rows.len(),
        json.get("pages").and_then(Value::as_array).map_or(0, Vec::len),
        text(json, "swipes").unwrap_or_else(|| "0".to_string()),
        text(json, "stop_reason").unwrap_or_else(|| "?".to_string()),
        text(json, "complete").unwrap_or_else(|| "false".to_string()),
    )];
    if let Some(coverage) = text(json, "coverage") {
        out.push(format!("coverage: {coverage}"));
    }
    if let Some(error) = text(json, "stop_error") {
        out.push(format!("stopped by: {}", clip(&error)));
    }
    for row in rows {
        let mut line = format!(
            "p{} {} \"{}\"",
            text(row, "page").unwrap_or_default(),
            text(row, "kind").unwrap_or_default(),
            clip(&text(row, "label").unwrap_or_default())
        );
        if let Some(value) = text(row, "value") {
            line.push_str(&format!(" value=\"{}\"", clip(&value)));
        }
        if let Some(identifier) = text(row, "identifier") {
            line.push_str(&format!(" id={identifier}"));
        }
        out.push(line);
    }
    if let Some(snapshot) = text(json, "snapshot") {
        out.push(format!("snapshot (last page): {snapshot}"));
    }
    Some(out.join("\n"))
}

fn found_row(row: &Value) -> String {
    let rect = row
        .get("rect")
        .and_then(Value::as_array)
        .map(|r| r.iter().map(|v| v.as_f64().map_or("?".to_string(), |v| format!("{v:.0}"))).collect::<Vec<_>>().join(","))
        .unwrap_or_default();
    let mut line = format!(
        "[{}] {} \"{}\" @{rect}",
        text(row, "index").unwrap_or_else(|| "?".to_string()),
        text(row, "kind").unwrap_or_default(),
        clip(&text(row, "label").unwrap_or_default()),
    );
    if let Some(value) = text(row, "value") {
        line.push_str(&format!(" value=\"{}\"", clip(&value)));
    }
    line
}

/// `phone_scroll_find`: found or why not, with the target or candidates.
pub fn found_label(json: &Value) -> Option<String> {
    let found = json.get("found")?.as_bool()?;
    let mut out = vec![format!(
        "found={found} stop_reason={} swipes={}{}",
        text(json, "stop_reason").unwrap_or_else(|| "?".to_string()),
        text(json, "swipes").unwrap_or_else(|| "0".to_string()),
        text(json, "error").map_or(String::new(), |e| format!(" error={e}")),
    )];
    if let Some(target) = json.get("target").filter(|t| t.is_object()) {
        out.push(format!("target: {}", found_row(target)));
    }
    if let Some(cover) = json.get("covered_by").filter(|c| c.is_object()) {
        out.push(format!(
            "covered by: {} \"{}\"",
            text(cover, "kind").unwrap_or_default(),
            clip(&text(cover, "label").unwrap_or_default())
        ));
    }
    if let Some(candidates) = json.get("candidates").and_then(Value::as_array).filter(|c| !c.is_empty()) {
        out.push("candidates:".to_string());
        out.extend(candidates.iter().map(|row| format!("  {}", found_row(row))));
    }
    if let Some(hint) = text(json, "hint") {
        out.push(format!("hint: {hint}"));
    }
    if let Some(snapshot) = text(json, "snapshot") {
        out.push(format!("snapshot: {snapshot}"));
    }
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_collected_list_prints_its_verdict_and_one_line_per_row() {
        let body = json!({
            "ok": true, "rows": [
                {"kind": "Cell", "label": "A · 12:00", "page": 1},
                {"kind": "Cell", "label": "B", "value": "3", "page": 2}
            ],
            "pages": [{"page": 1}, {"page": 2}], "swipes": 1,
            "stop_reason": "duplicate_page", "complete": false,
            "coverage": "NOT proven complete", "snapshot": "S1"
        });
        let text = collected_list(&body).unwrap();
        assert!(text.starts_with("collected 2 rows over 2 pages (1 swipes); stop_reason=duplicate_page complete=false"), "{text}");
        assert!(text.contains("p2 Cell \"B\" value=\"3\""), "{text}");
        assert!(text.contains("coverage: NOT proven complete"));
        assert!(text.ends_with("snapshot (last page): S1"));
    }

    #[test]
    fn a_find_verdict_keeps_its_candidates() {
        let body = json!({
            "ok": false, "found": false, "stop_reason": "ambiguous", "swipes": 0,
            "error": "ambiguous_element_label", "snapshot": "S2",
            "candidates": [{"index": 3, "kind": "StaticText", "rect": [16.0, 100.0, 300.0, 40.0]}]
        });
        let text = found_label(&body).unwrap();
        assert!(text.starts_with("found=false stop_reason=ambiguous swipes=0 error=ambiguous_element_label"), "{text}");
        assert!(text.contains("  [3] StaticText \"\" @16,100,300,40"), "{text}");
    }

    #[test]
    fn a_failed_batch_keeps_the_verdict_and_the_failed_step_but_not_the_bulk() {
        let body = json!({
            "ok": false,
            "error": "wait_for_timeout",
            "failed_step": 2,
            "completed": 1,
            "applied_actions": 1,
            "outcome": "not_sent",
            "batch_outcome": "applied_partially",
            "retry_safe": false,
            "steps": [
                {"index": 1, "kind": "tap_label", "ok": true, "timing": {"total_ms": 900}},
                {"index": 2, "kind": "wait_for", "ok": false, "error": "wait_for_timeout",
                 "hint": "关于本机 never appeared", "timing": {"total_ms": 8000},
                 "observation": {"snapshot": "S9", "elements": [{"kind": "Button", "label": "x"}]}}
            ],
            "observation": {"snapshot": "S9", "settle": {"reason": "stable", "waited_ms": 600},
                            "elements": [{"kind": "Application", "label": "设置", "rect": [0,0,390,844], "depth": 0},
                                         {"kind": "Button", "label": "通用", "rect": [16,300,358,44], "depth": 2}]}
        });
        let text = failed_batch(&body).unwrap();
        assert!(text.starts_with("FAILED wait_for_timeout · step 2 of 2 · 1 completed · 1 applied · applied_partially · retry_safe=false"), "{text}");
        assert!(text.contains("steps: 1 tap_label · 2 wait_for FAILED"), "{text}");
        assert!(text.contains("\"hint\":\"关于本机 never appeared\""), "{text}");
        assert!(!text.contains("total_ms"), "timing stays structured: {text}");
        assert!(text.contains("end screen: snapshot S9"), "{text}");
        assert!(text.len() < body.to_string().len(), "{text}");
        // A success is not a failed batch.
        assert!(failed_batch(&json!({"ok": true, "steps": []})).is_none());
    }

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

    /// The shape `POST /agent/actions` answers on i13 for Settings → 蓝牙.
    fn bluetooth_batch() -> Value {
        json!({
            "ok": true,
            "completed": 2,
            "applied_actions": 1,
            "snapshot": "S9",
            "settle": {"reason": "stable", "waited_ms": 812, "captures": 2},
            "timing": {"total_ms": 1900, "wda_ms": 1500, "wda": [{"call": "GET /source", "count": 2, "ms": 400}]},
            "steps": [
                {"index": 1, "kind": "tap_label", "ok": true},
                {"index": 2, "kind": "wait_for", "ok": true, "attempts": 1, "probe_misses": 1,
                 "observation": {"present": [{"label": "蓝牙", "kind": "Switch", "matched": 1}]}}
            ],
            "elements": [
                {"kind": "Application", "label": "设置", "rect": [0, 0, 390, 844], "depth": 0},
                {"kind": "NavigationBar", "label": "蓝牙", "identifier": "_TtGC7SwiftUI32NavigationStackHosting", "rect": [0, 47, 390, 96], "depth": 11},
                {"kind": "Button", "label": "设置", "rect": [16, 55, 44, 44], "depth": 12},
                {"kind": "Switch", "label": "蓝牙", "value": "1", "rect": [16, 200, 358, 52], "depth": 14},
                {"kind": "StaticText", "label": "AirDrop、隔空播放、查找和定位服务使用蓝牙。", "rect": [32, 260, 326, 40], "depth": 14}
            ],
            "flow_suggestion": {"steps": 2, "hint": "keep this as a flow?"}
        })
    }

    #[test]
    fn an_observed_batch_reads_as_its_verdict_and_end_screen() {
        let body = bluetooth_batch();
        let text = batch(&body).unwrap();
        let first = text.lines().next().unwrap();
        assert!(
            first.starts_with("ok · snapshot S9 · settle stable 812ms"),
            "{text}"
        );
        assert!(first.ends_with("· 2/2 steps · 1 applied"), "{text}");
        assert!(text.contains("steps: 1 tap_label · 2 wait_for"), "{text}");
        assert!(text.contains("[Switch] \"蓝牙\" value=1"), "{text}");
        // What the agent decides from next is kept.
        assert!(text.contains("flow_suggestion:"), "{text}");
        // Diagnostics the model does not need stay in the structured copy.
        assert!(!text.contains("total_ms"), "{text}");
        assert!(!text.contains("probe_misses"), "{text}");
        let json_bytes = body.to_string().len();
        assert!(
            text.len() * 2 < json_bytes,
            "compact {} B vs JSON {json_bytes} B:\n{text}",
            text.len()
        );
    }

    #[test]
    fn a_batch_with_a_delta_reads_as_the_change() {
        let mut body = bluetooth_batch();
        body["delta"] = json!({
            "added": [{"index": 3, "element": {"kind": "Switch", "label": "蓝牙", "value": "1", "depth": 14}}],
            "changed": [], "removed": [{"index": 9}], "unchanged": 40
        });
        let text = batch(&body).unwrap();
        assert!(text.contains("+ #3 [Switch] \"蓝牙\" value=1"), "{text}");
        assert!(text.contains("- 1 removed · = 40 unchanged"), "{text}");
        assert!(
            !text.contains("AirDrop"),
            "the delta stands in for the whole tree: {text}"
        );
    }

    #[test]
    fn a_failed_or_unobserved_batch_keeps_its_json() {
        // A failure's evidence is the whole body (failed_step, applied_actions,
        // retry_safe, the failed step's own detail): never summarised away.
        let failure = json!({
            "ok": false, "error": "expectation_timeout", "failed_step": 2, "completed": 1,
            "applied_actions": 1, "outcome": "applied", "retry_safe": false,
            "steps": [{"index": 1, "kind": "tap_label", "ok": true},
                      {"index": 2, "kind": "wait_for", "ok": false, "error": "expectation_timeout"}],
            "observation": {"snapshot": "S2", "elements": []}
        });
        assert_eq!(batch(&failure), None);
        // Not observed: no screen to render, the daemon's answer stands.
        assert_eq!(
            batch(&json!({"ok": true, "completed": 2, "applied_actions": 1,
                          "steps": [{"index": 1, "kind": "tap_label", "ok": true}]})),
            None
        );
    }
}
