//! Flow outputs: a flow that ends on a screen can hand back what is on it.
//!
//! A flow's optional `outputs` map names values to read off the final screen
//! once every step has passed — `{"steps_today": {"locator": {"identifier":
//! "StepCount"}, "type": "number"}}` — so a read task (today's steps, a
//! balance, the newest message) becomes one call that returns JSON, not a
//! replay that only says "done".
//!
//! `flow verify` (and `phone_flow_run verify=true`) records the SHAPE of a good
//! result — each output's type, never its value — and fails a later run whose
//! output went missing, changed type, or came back an empty list: the flow
//! still "passes" after an app update but reads the wrong thing.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const MAX_OUTPUTS: usize = 16;
const MAX_LIST: usize = 50;
const MAX_TEXT_CHARS: usize = 2_000;

/// One named value read from the final screen.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FlowOutput {
    /// Element locator, same fields and matching as `tap_locator` (label,
    /// identifier, kind, value, focused, enabled, visible). May be `{}` when
    /// `label_contains` alone picks the row.
    #[serde(default)]
    pub locator: serde_json::Map<String, serde_json::Value>,
    /// Additionally require the row's label to contain this text — for rows
    /// whose label carries the value ("步数, 8,532 步").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_contains: Option<String>,
    /// Which field holds the value: `value` or `label`. Default: `value` when
    /// the row has one, else `label`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// `string` (default) or `number` (the first number in the text; `,`
    /// grouping and surrounding words are ignored).
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Every matching row as a list, in screen order, instead of the first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

const LOCATOR_FIELDS: [&str; 7] = [
    "label", "identifier", "kind", "value", "focused", "enabled", "visible",
];

pub fn validate(outputs: &BTreeMap<String, FlowOutput>) -> Result<()> {
    if outputs.len() > MAX_OUTPUTS {
        bail!("a flow may declare at most {MAX_OUTPUTS} outputs");
    }
    for (name, output) in outputs {
        let mut chars = name.chars();
        if !(matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
            && name.len() <= 48
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            bail!("output name {name:?} must be an ASCII identifier ([A-Za-z][A-Za-z0-9_]*, ≤48)");
        }
        for (key, value) in &output.locator {
            if !LOCATOR_FIELDS.contains(&key.as_str()) {
                bail!("output {name}: unknown locator field {key:?}");
            }
            let ok = match key.as_str() {
                "focused" | "enabled" | "visible" => value.is_boolean(),
                _ => value.as_str().is_some_and(|s| !s.is_empty()),
            };
            if !ok {
                bail!("output {name}: locator field {key:?} has the wrong type or is empty");
            }
        }
        if output.locator.is_empty()
            && output.label_contains.as_deref().is_none_or(str::is_empty)
        {
            bail!("output {name}: needs a locator or label_contains");
        }
        if let Some(field) = &output.field {
            if field != "value" && field != "label" {
                bail!("output {name}: field must be \"value\" or \"label\"");
            }
        }
        if let Some(kind) = &output.kind {
            if kind != "string" && kind != "number" {
                bail!("output {name}: type must be \"string\" or \"number\"");
            }
        }
    }
    Ok(())
}

/// Same defaults the daemon's matcher applies to the sparse tree.
fn row_matches(row: &serde_json::Value, output: &FlowOutput) -> bool {
    let fields_match = output.locator.iter().all(|(key, expected)| {
        let actual = match row.get(key) {
            Some(value) => Some(value.clone()),
            None => match key.as_str() {
                "enabled" | "visible" => Some(serde_json::Value::Bool(true)),
                "focused" => Some(serde_json::Value::Bool(false)),
                _ => None,
            },
        };
        actual.as_ref() == Some(expected)
    });
    let label_ok = output.label_contains.as_deref().is_none_or(|needle| {
        row.get("label")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|label| label.contains(needle))
    });
    fields_match && label_ok
}

/// The first number in `text`: optional sign, digits with `,` grouping, an
/// optional decimal part. `"步数, 8,532 步"` → 8532.
pub fn first_number(text: &str) -> Option<f64> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let negative = i > 0 && chars[i - 1] == '-';
            let mut digits = String::new();
            let mut seen_dot = false;
            while i < chars.len() {
                let c = chars[i];
                if c.is_ascii_digit() {
                    digits.push(c);
                } else if c == ',' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() && !seen_dot {
                    // grouping separator
                } else if c == '.' && !seen_dot && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
                    seen_dot = true;
                    digits.push(c);
                } else {
                    break;
                }
                i += 1;
            }
            let number: f64 = digits.parse().ok()?;
            return Some(if negative { -number } else { number });
        }
        i += 1;
    }
    None
}

fn row_value(row: &serde_json::Value, output: &FlowOutput) -> Option<serde_json::Value> {
    let text = |key: &str| row.get(key).and_then(serde_json::Value::as_str).map(str::to_string);
    let raw = match output.field.as_deref() {
        Some("label") => text("label"),
        Some(_) => text("value"),
        None => text("value").filter(|v| !v.is_empty()).or_else(|| text("label")),
    }?;
    let raw: String = raw.chars().take(MAX_TEXT_CHARS).collect();
    match output.kind.as_deref() {
        Some("number") => first_number(&raw).map(|n| {
            if n.fract() == 0.0 && n.abs() < 9.0e15 {
                serde_json::json!(n as i64)
            } else {
                serde_json::json!(n)
            }
        }),
        _ => Some(serde_json::json!(raw)),
    }
}

/// Read every output from one `/agent/elements` body. Returns the values and
/// the names that could not be read (each also present as `null`).
pub fn extract(
    outputs: &BTreeMap<String, FlowOutput>,
    elements_body: &serde_json::Value,
) -> (serde_json::Map<String, serde_json::Value>, Vec<String>) {
    let rows: &[serde_json::Value] = elements_body
        .get("elements")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut values = serde_json::Map::new();
    let mut missing = Vec::new();
    for (name, output) in outputs {
        let mut found = rows
            .iter()
            .filter(|row| row_matches(row, output))
            .filter_map(|row| row_value(row, output));
        let value = if output.all {
            let list: Vec<_> = found.by_ref().take(MAX_LIST).collect();
            serde_json::Value::Array(list)
        } else {
            found.next().unwrap_or(serde_json::Value::Null)
        };
        if value.is_null() {
            missing.push(name.clone());
        }
        values.insert(name.clone(), value);
    }
    (values, missing)
}

// ---------------------------------------------------------------------------
// Shapes and fixtures
// ---------------------------------------------------------------------------

fn type_of(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Number(_) => "number".into(),
        serde_json::Value::String(_) => "string".into(),
        serde_json::Value::Bool(_) => "boolean".into(),
        serde_json::Value::Array(items) => match items.first() {
            None => "list<empty>".into(),
            Some(first) => format!("list<{}>", type_of(first)),
        },
        serde_json::Value::Object(_) => "object".into(),
    }
}

/// `{name: type}` — types only, never values (fixtures hold no phone data).
pub fn shape(values: &serde_json::Map<String, serde_json::Value>) -> BTreeMap<String, String> {
    values.iter().map(|(k, v)| (k.clone(), type_of(v))).collect()
}

/// What changed between a recorded good shape and this run's.
pub fn compare(expected: &BTreeMap<String, String>, actual: &BTreeMap<String, String>) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, want) in expected {
        match actual.get(name).map(String::as_str) {
            None | Some("null") => problems.push(format!("output {name} is missing (was {want})")),
            Some("list<empty>") if want.starts_with("list<") && want != "list<empty>" => {
                problems.push(format!("output {name} came back an empty list (was {want})"))
            }
            Some(got) if got != want && !(want == "list<empty>" && got.starts_with("list<")) => {
                problems.push(format!("output {name} changed type: {want} → {got}"))
            }
            _ => {}
        }
    }
    problems
}

/// `~/.iphone-use/flow-fixtures/<key>.json`; `IPHONE_USE_FLOW_FIXTURES_DIR`
/// overrides (tests).
pub fn fixture_path(key: &str) -> Result<PathBuf> {
    let dir = match std::env::var("IPHONE_USE_FLOW_FIXTURES_DIR") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var("HOME").context("HOME is not set")?)
            .join(".iphone-use")
            .join("flow-fixtures"),
    };
    let safe: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    Ok(dir.join(format!("{safe}.json")))
}

pub fn read_fixture(key: &str) -> Result<Option<BTreeMap<String, String>>> {
    let path = fixture_path(key)?;
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| serde_json::from_value(v.get("outputs")?.clone()).ok())
                .with_context(|| format!("unreadable fixture {}", path.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub fn write_fixture(key: &str, shape: &BTreeMap<String, String>) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let path = fixture_path(key)?;
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let body = serde_json::to_vec_pretty(&serde_json::json!({
        "flow": key,
        "outputs": shape,
        "note": "types only — written by `flow verify --write-fixture`",
    }))?;
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&body)?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// The verdict of comparing one run against its fixture.
pub fn verify(
    key: &str,
    values: &serde_json::Map<String, serde_json::Value>,
    write_fixture_too: bool,
) -> Result<serde_json::Value> {
    let current = shape(values);
    if write_fixture_too {
        if let Some((name, _)) = current.iter().find(|(_, t)| t.as_str() == "null") {
            bail!("refusing to record a fixture while output {name} is missing");
        }
        let path = write_fixture(key, &current)?;
        return Ok(serde_json::json!({ "ok": true, "fixture_written": path, "shape": current }));
    }
    match read_fixture(key)? {
        None => Ok(serde_json::json!({
            "ok": true,
            "fixture": null,
            "shape": current,
            "hint": "no fixture yet: when this result is right, record it with `flow verify <flow> --write-fixture`"
        })),
        Some(expected) => {
            let problems = compare(&expected, &current);
            Ok(serde_json::json!({ "ok": problems.is_empty(), "problems": problems, "shape": current }))
        }
    }
}

/// Tests that point `IPHONE_USE_FLOW_FIXTURES_DIR` somewhere hold this, so
/// parallel tests never see each other's directory.
#[cfg(test)]
pub(crate) static FIXTURE_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn outputs(json: serde_json::Value) -> BTreeMap<String, FlowOutput> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn numbers_are_read_out_of_labels() {
        assert_eq!(first_number("步数, 8,532 步"), Some(8532.0));
        assert_eq!(first_number("余额 ¥1,234.56"), Some(1234.56));
        assert_eq!(first_number("-3 °C"), Some(-3.0));
        assert_eq!(first_number("12, 34"), Some(12.0), "a comma then a space ends the number");
        assert_eq!(first_number("none"), None);
    }

    #[test]
    fn extract_reads_values_labels_lists_and_reports_missing() {
        let defs = outputs(serde_json::json!({
            "steps": {"locator": {"identifier": "StepCount"}, "type": "number"},
            "title": {"locator": {"kind": "NavigationBar"}, "field": "label"},
            "chats": {"locator": {"kind": "Cell"}, "field": "label", "all": true},
            "km": {"label_contains": "公里", "type": "number"},
            "absent": {"locator": {"identifier": "Nope"}}
        }));
        validate(&defs).unwrap();
        let body = serde_json::json!({"elements": [
            {"kind": "NavigationBar", "label": "摘要"},
            {"kind": "StaticText", "label": "步数", "identifier": "StepCount", "value": "8,532 步"},
            {"kind": "StaticText", "label": "步行距离 5.4 公里"},
            {"kind": "Cell", "label": "张三"},
            {"kind": "Cell", "label": "李四"},
            {"kind": "Cell", "label": "隐藏", "visible": false}
        ]});
        let (values, missing) = extract(&defs, &body);
        assert_eq!(values["steps"], 8532);
        assert_eq!(values["title"], "摘要");
        assert_eq!(values["chats"], serde_json::json!(["张三", "李四", "隐藏"]));
        assert_eq!(values["km"], 5.4);
        assert_eq!(values["absent"], serde_json::Value::Null);
        assert_eq!(missing, ["absent"]);
    }

    #[test]
    fn validation_rejects_bad_definitions() {
        for bad in [
            serde_json::json!({"1x": {"locator": {"label": "a"}}}),
            serde_json::json!({"x": {}}),
            serde_json::json!({"x": {"locator": {"labels": "a"}}}),
            serde_json::json!({"x": {"locator": {"label": "a"}, "type": "date"}}),
            serde_json::json!({"x": {"locator": {"visible": "yes"}}}),
        ] {
            let parsed: Result<BTreeMap<String, FlowOutput>, _> = serde_json::from_value(bad.clone());
            assert!(parsed.map_or(true, |defs| validate(&defs).is_err()), "{bad}");
        }
        assert!(serde_json::from_value::<BTreeMap<String, FlowOutput>>(
            serde_json::json!({"x": {"locator": {"label": "a"}, "regex": "."}})
        )
        .is_err(), "unknown fields are refused");
    }

    #[test]
    fn shape_comparison_catches_drift_but_not_values() {
        let expected: BTreeMap<String, String> = [
            ("steps".to_string(), "number".to_string()),
            ("chats".to_string(), "list<string>".to_string()),
            ("title".to_string(), "string".to_string()),
        ]
        .into();
        let same = shape(&serde_json::from_value(serde_json::json!({
            "steps": 1, "chats": ["a"], "title": "x"
        })).unwrap());
        assert!(compare(&expected, &same).is_empty());
        let drifted = shape(&serde_json::from_value(serde_json::json!({
            "steps": "8,532", "chats": [], "title": null
        })).unwrap());
        let problems = compare(&expected, &drifted);
        assert_eq!(problems.len(), 3, "{problems:?}");
    }

    #[test]
    fn fixtures_hold_types_only() {
        let _env = FIXTURE_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("IPHONE_USE_FLOW_FIXTURES_DIR", dir.path());
        let values = serde_json::from_value(serde_json::json!({"steps": 8532, "who": "张三"})).unwrap();
        let written = verify("health/steps-today", &values, true).unwrap();
        let text = std::fs::read_to_string(written["fixture_written"].as_str().unwrap()).unwrap();
        assert!(!text.contains("8532") && !text.contains("张三"), "{text}");
        let later = serde_json::from_value(serde_json::json!({"steps": null, "who": "李四"})).unwrap();
        let verdict = verify("health/steps-today", &later, false).unwrap();
        assert_eq!(verdict["ok"], false);
        assert!(verify("health/steps-today", &later, true).is_err(), "no fixture from a broken run");
        std::env::remove_var("IPHONE_USE_FLOW_FIXTURES_DIR");
    }
}
