//! What the driving agent did, in plain words, for people watching.
//!
//! Each applied action becomes one line such as `Tapped “Wi-Fi”`, resolved
//! from the screen the action named (its `snapshot`), so a person reading the
//! control page sees what was touched instead of `tap {element: 12}`. Typed
//! text and set values are never kept. In memory only; the
//! last [`CAPACITY`] lines.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::runner::ElementRow;

pub const CAPACITY: usize = 50;
/// Longest label kept in a line, in characters.
const MAX_LABEL: usize = 40;

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    seq: u64,
    at_ms: u64,
    owner: Option<String>,
    text: Line,
}

#[derive(Default)]
struct Log {
    next: u64,
    entries: VecDeque<Entry>,
}

fn log() -> &'static Mutex<Log> {
    static LOG: OnceLock<Mutex<Log>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(Log::default()))
}

/// Append one line.
pub fn record(owner: Option<&str>, text: Line, at_ms: u64) {
    let mut log = log()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    log.next += 1;
    let seq = log.next;
    if log.entries.len() >= CAPACITY {
        log.entries.pop_front();
    }
    log.entries.push_back(Entry {
        seq,
        at_ms,
        owner: owner.map(str::to_string),
        text,
    });
}

/// Lines after `since` (a `seq` the caller already has), oldest first, at
/// most `limit`; `next` is the cursor for the following call.
pub fn recent(since: u64, limit: usize) -> Value {
    let log = log()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let newer: Vec<&Entry> = log.entries.iter().filter(|e| e.seq > since).collect();
    let skip = newer.len().saturating_sub(limit);
    let items: Vec<Value> = newer[skip..].iter().map(|e| entry_json(e)).collect();
    json!({"ok": true, "items": items, "next": log.next})
}

fn entry_json(e: &Entry) -> Value {
    let mut item = json!({
        "seq": e.seq, "at_ms": e.at_ms,
        "text": e.text.en, "text_zh": e.text.zh,
    });
    if let Some(owner) = &e.owner {
        item["owner"] = json!(owner);
    }
    if let Some(action_type) = e.text.action_type {
        item["action_type"] = json!(action_type);
    }
    if let Some(target) = &e.text.target {
        item["target"] = json!(target);
    }
    item
}

/// Geometry alone: never retain the action, locator, row label, or row value.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct Target {
    coordinate_space: CoordinateSpace,
    point: [f64; 2],
    /// Element targets describe a rect center, not the runner's actual hit point.
    point_basis: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    rect: Option<[f64; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    viewport: Option<[f64; 2]>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum CoordinateSpace {
    RunnerPoints,
}

fn quoted(label: &str) -> String {
    let label = label.trim();
    let mut short: String = label.chars().take(MAX_LABEL).collect();
    if label.chars().count() > MAX_LABEL {
        short.push('…');
    }
    format!("“{short}”")
}

/// One line in English (agents, the API) and Chinese (the control page).
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub en: String,
    pub zh: String,
    action_type: Option<&'static str>,
    target: Option<Target>,
}

impl Line {
    /// Supply the authoritative runner window size for this action's screen.
    /// Never use screenshot pixels or infer a viewport from accessibility rows.
    /// Without this hook runner-point targets remain in the feed but are not drawn.
    #[must_use]
    pub fn with_runner_viewport(mut self, width: f64, height: f64) -> Self {
        if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 {
            if let Some(target) = &mut self.target {
                target.viewport = Some([width, height]);
            }
        }
        self
    }
}

fn line(en: impl Into<String>, zh: impl Into<String>) -> Line {
    Line {
        en: en.into(),
        zh: zh.into(),
        action_type: None,
        target: None,
    }
}

/// What an action touched, in both languages: an element of `rows` (its
/// label, identifier or kind), a label, a locator, or a point.
fn key_label(label: &str) -> bool {
    let label = label.trim();
    !label.is_empty() && (label.chars().count() == 1 || label.chars().all(|c| c.is_ascii_digit()))
}

fn sensitive_input(row: &ElementRow) -> bool {
    let kind = row.kind.to_ascii_lowercase();
    kind.contains("textfield")
        || kind.contains("searchfield")
        || kind.contains("textview")
        || kind.contains("securetext")
        || kind == "key"
        || key_label(&row.label)
}

fn target(action: &Value, rows: Option<&[ElementRow]>) -> Line {
    let element = action.get("element");
    if let Some(row) = element
        .and_then(Value::as_u64)
        .and_then(|i| rows.and_then(|rows| rows.get(i as usize)))
    {
        // Some input providers repeat entered content as their label.
        if !row.label.trim().is_empty()
            && !sensitive_input(row)
            && row.value.as_deref() != Some(row.label.as_str())
        {
            let q = quoted(&row.label);
            return line(q.clone(), q);
        }
        if let Some(id) = row.identifier.as_deref().filter(|s| {
            !sensitive_input(row) && !s.trim().is_empty() && row.value.as_deref() != Some(*s)
        }) {
            let q = quoted(id);
            return line(q.clone(), q);
        }
        if !row.kind.is_empty() {
            return line(
                format!("a {}", row.kind.to_lowercase()),
                format!("一个 {} 控件", row.kind),
            );
        }
    }
    if element.is_some() {
        return line("an element", "一个控件");
    }
    if action.get("type").and_then(Value::as_str) == Some("set_value") {
        return line("an element", "一个控件");
    }
    let text = action.get("label").and_then(Value::as_str).or_else(|| {
        let locator = action.get("locator")?;
        ["label", "identifier", "label_contains"]
            .iter()
            .find_map(|k| locator.get(*k).and_then(Value::as_str))
    });
    if let Some(text) = text {
        if key_label(text) {
            return line("a keypad key", "一个按键");
        }
        let q = quoted(text);
        return line(q.clone(), q);
    }
    line("the screen", "屏幕")
}

/// The app name for a bundle id, from the catalog; the id when unknown.
fn app_name(bundle: &str) -> String {
    crate::app_lookup::catalog()
        .iter()
        .find(|app| app.bundle_id.eq_ignore_ascii_case(bundle))
        .map_or_else(|| bundle.to_string(), |app| app.name.clone())
}

/// A swipe's direction, from its start and end points.
fn swipe_line(action: &Value) -> Line {
    let get = |k: &str| action.get(k).and_then(Value::as_f64);
    match (get("x1"), get("y1"), get("x2"), get("y2")) {
        (Some(x1), Some(y1), Some(x2), Some(y2)) if (y2 - y1).abs() >= (x2 - x1).abs() => {
            if y2 < y1 {
                line("Swiped up", "向上滑动")
            } else {
                line("Swiped down", "向下滑动")
            }
        }
        (Some(x1), _, Some(x2), _) if x2 < x1 => line("Swiped left", "向左滑动"),
        (Some(_), _, Some(_), _) => line("Swiped right", "向右滑动"),
        _ => line("Swiped", "滑动"),
    }
}

/// One action in plain words. `rows` is the screen its `snapshot` named.
/// Typed text is reduced to its length; set/picker values are omitted.
pub fn describe(action: &Value, rows: Option<&[ElementRow]>) -> Line {
    let mut description = describe_words(action, rows);
    description.action_type = match action.get("type").and_then(Value::as_str) {
        Some("tap") => Some("tap"),
        Some("tap_locator") => Some("tap_locator"),
        Some("longpress") => Some("longpress"),
        _ => None,
    };
    if description.action_type.is_some() {
        description.target = click_target(action, rows);
    }
    description
}

fn click_target(action: &Value, rows: Option<&[ElementRow]>) -> Option<Target> {
    // Locator dispatch resolves internally; this API has no applied row proof.
    if action.get("type").and_then(Value::as_str) == Some("tap_locator") {
        return None;
    }
    // Indexed rows are qualified by the caller's snapshot. Do not resolve a
    // label/locator ourselves: the applied target may differ or be ambiguous.
    if let Some(element) = action.get("element") {
        if action.get("label").is_some()
            || action.get("locator").is_some()
            || action.get("x").is_some()
            || action.get("y").is_some()
        {
            return None;
        }
        let index = usize::try_from(element.as_u64()?).ok()?;
        let row = rows?.get(index)?;
        if sensitive_input(row)
            || row.label.trim().is_empty()
            || row.value.as_deref() == Some(row.label.as_str())
        {
            return None;
        }
        let [x, y, w, h] = row.rect;
        if !row.rect.iter().all(|v| v.is_finite())
            || x < 0.0
            || y < 0.0
            || w <= 0.0
            || h <= 0.0
            || !(x + w).is_finite()
            || !(y + h).is_finite()
        {
            return None;
        }
        return Some(Target {
            coordinate_space: CoordinateSpace::RunnerPoints,
            point: [x + w / 2.0, y + h / 2.0],
            point_basis: "rect_center",
            rect: Some(row.rect),
            viewport: None,
        });
    }
    if action.get("label").is_some() || action.get("locator").is_some() {
        return None;
    }
    // Arbitrary points can encode a PIN/password. They never enter history.
    None
}

fn describe_words(action: &Value, rows: Option<&[ElementRow]>) -> Line {
    let typ = action.get("type").and_then(Value::as_str).unwrap_or("");
    let name = action.get("name").and_then(Value::as_str);
    let on = |en: &str, zh: &str| {
        let t = target(action, rows);
        line(format!("{en} {}", t.en), format!("{zh}{}", t.zh))
    };
    match typ {
        "tap" | "tap_locator" => on("Tapped", "点按"),
        "longpress" => on("Long-pressed", "长按"),
        "set_value" => on("Filled in", "填写"),
        "text" => {
            let n = action
                .get("text")
                .and_then(Value::as_str)
                .map_or(0, |t| t.chars().count());
            let unit = if n == 1 { "character" } else { "characters" };
            if action.get("clear").and_then(Value::as_bool) == Some(true) {
                line(
                    format!("Cleared the field and typed {n} {unit}"),
                    format!("清空后输入 {n} 个字"),
                )
            } else {
                line(format!("Typed {n} {unit}"), format!("输入 {n} 个字"))
            }
        }
        "key" => match name {
            Some(key) => line(format!("Pressed {key}"), format!("按下 {key}")),
            None => line("Pressed a key", "按下一个键"),
        },
        "keyboard" => line("Dismissed the keyboard", "收起键盘"),
        "home" => line("Went to the Home Screen", "回到主屏幕"),
        "shortcut" => match name {
            Some("home") => line("Went to the Home Screen", "回到主屏幕"),
            Some("spotlight") => line("Opened Spotlight", "打开 Spotlight 搜索"),
            Some(other) => line(
                format!("Used the {other} shortcut"),
                format!("使用快捷操作 {other}"),
            ),
            None => line("Used a shortcut", "使用快捷操作"),
        },
        "back" => line("Went back", "返回"),
        "launch_app" => {
            let app = action
                .get("bundle")
                .and_then(Value::as_str)
                .map(app_name)
                .or_else(|| {
                    ["app", "name"]
                        .iter()
                        .find_map(|k| action.get(*k).and_then(Value::as_str).map(str::to_string))
                });
            match app {
                Some(app) => line(format!("Opened {app}"), format!("打开 {app}")),
                None => line("Opened an app", "打开一个 App"),
            }
        }
        // A web link's host only: a path or query can carry tokens or
        // personal data. Any other scheme is named by its scheme alone.
        "open_url" => match action
            .get("url")
            .and_then(Value::as_str)
            .and_then(|url| crate::guard::parse_url(url).ok())
        {
            Some(crate::guard::ParsedUrl {
                host: Some(host),
                scheme,
            }) if scheme == "http" || scheme == "https" => line(
                format!("Opened a link to {host}"),
                format!("打开链接 {host}"),
            ),
            Some(parsed) => line(
                format!("Opened a {}: link", parsed.scheme),
                format!("打开 {}: 链接", parsed.scheme),
            ),
            None => line("Opened a link", "打开链接"),
        },
        "scroll" if action.get("element").is_some() => on("Scrolled", "滚动"),
        "scroll" => line("Scrolled", "滚动"),
        "swipe" => swipe_line(action),
        "drag" => line("Dragged across the screen", "拖动"),
        "alert" => match (
            action.get("button").and_then(Value::as_str),
            action.get("action").and_then(Value::as_str),
        ) {
            (Some(button), _) => {
                let q = quoted(button);
                line(
                    format!("Answered the alert with {q}"),
                    format!("在弹窗中选择{q}"),
                )
            }
            (None, Some("dismiss")) => line("Dismissed the alert", "关闭弹窗"),
            (None, Some(_)) => line("Accepted the alert", "确认弹窗"),
            (None, None) => line("Answered the alert", "处理弹窗"),
        },
        "picker" => line("Turned a picker", "拨动选择器"),
        "perform" => {
            let verb = action
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("an action")
                .replace('_', " ");
            let t = target(action, rows);
            line(
                format!("Used {verb} on {}", t.en),
                format!("对{}执行 {verb}", t.zh),
            )
        }
        "" => line("Did an action", "执行了一个操作"),
        other => {
            let other = other.replace('_', " ");
            line(format!("Did {other}"), format!("执行 {other}"))
        }
    }
}

/// A screen recording started.
pub fn recording_started() -> Line {
    line("Started recording", "开始录屏")
}

/// A recording saved, with its length in whole seconds.
pub fn recording_saved(seconds: f64) -> Line {
    let n = seconds.max(0.0).round() as u64;
    line(
        format!("Saved a recording ({n} s)"),
        format!("保存录屏（{n} 秒）"),
    )
}

/// The simulated location set or cleared. Never the coordinates: where a
/// person is (or pretends to be) is not for the feed.
pub fn location(cleared: bool) -> Line {
    if cleared {
        line("Cleared the simulated location", "取消模拟定位")
    } else {
        line("Set a simulated location", "设置模拟定位")
    }
}

/// An app installed (`None`: its bundle id was not read).
pub fn installed(bundle: Option<&str>) -> Line {
    match bundle.map(app_name) {
        Some(app) => line(format!("Installed {app}"), format!("安装 {app}")),
        None => line("Installed an app", "安装一个 App"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, label: &str) -> ElementRow {
        ElementRow {
            kind: kind.into(),
            label: label.into(),
            ..ElementRow::default()
        }
    }

    fn en(action: Value, rows: Option<&[ElementRow]>) -> String {
        describe(&action, rows).en
    }

    #[test]
    fn taps_name_the_element_the_snapshot_showed() {
        let rows = vec![row("Window", ""), row("Cell", "Wi-Fi")];
        let tap = json!({"type":"tap","element":1,"snapshot":"S1"});
        let described = describe(&tap, Some(&rows));
        assert_eq!(described.en, "Tapped “Wi-Fi”");
        assert_eq!(described.zh, "点按“Wi-Fi”");
        assert_eq!(en(tap, None), "Tapped an element");
        assert_eq!(
            describe_words(&json!({"type":"tap","element":0}), Some(&rows)),
            line("Tapped a window", "点按一个 Window 控件")
        );
        assert_eq!(
            en(json!({"type":"tap","label":"Done"}), None),
            "Tapped “Done”"
        );
        assert_eq!(
            describe_words(&json!({"type":"tap","x":100.4,"y":220.6}), None),
            line("Tapped the screen", "点按屏幕")
        );
        assert_eq!(
            en(
                json!({"type":"tap_locator","locator":{"identifier":"One"}}),
                None
            ),
            "Tapped “One”"
        );
    }

    #[test]
    fn typed_text_and_values_are_never_kept() {
        let typed = describe(&json!({"type":"text","text":"hunter2 secret"}), None);
        assert_eq!(typed, line("Typed 14 characters", "输入 14 个字"));
        let rows = vec![row("TextField", "Password")];
        let set = describe(
            &json!({"type":"set_value","element":0,"value":"hunter2"}),
            Some(&rows),
        );
        assert_eq!(
            set,
            line("Filled in a textfield", "填写一个 TextField 控件")
        );
        assert!(!format!("{typed:?}{set:?}").contains("hunter2"));
    }

    fn wire(action: Value, rows: Option<&[ElementRow]>) -> Value {
        entry_json(&Entry {
            seq: 1,
            at_ms: 10,
            owner: None,
            text: describe(&action, rows),
        })
    }

    #[test]
    fn arbitrary_click_points_are_never_retained() {
        let item = wire(
            json!({"type":"tap","x":0.25,"y":0.75,"text":"SECRET"}),
            None,
        );
        assert_eq!(item["action_type"], "tap");
        assert!(item.get("target").is_none());
        assert_eq!(item["text"], "Tapped the screen");
        assert!(!item.to_string().contains("SECRET"));
        for action in [
            json!({"type":"tap","x":25,"y":75}),
            json!({"type":"tap","x":-0.1,"y":0.5}),
            json!({"type":"tap","x":0.5}),
            json!({"type":"tap","x":0.5,"y":0.5,"element":99}),
            json!({"type":"tap","x":0.5,"y":0.5,"label":"Done"}),
            json!({"type":"tap_locator","locator":{"label":"Done"}}),
        ] {
            assert!(wire(action, None).get("target").is_none());
        }
        assert!(wire(json!({"type":"longpress","x":0,"y":1}), None)
            .get("target")
            .is_none());
    }

    #[test]
    fn indexed_geometry_is_runner_points_and_never_guesses_a_viewport() {
        let rows = vec![ElementRow {
            rect: [10.0, 20.0, 40.0, 60.0],
            ..row("Button", "Done")
        }];
        let action = json!({"type":"tap","element":0,"snapshot":"S1"});
        let target = wire(action.clone(), Some(&rows))["target"].clone();
        assert_eq!(
            target,
            json!({
                "coordinate_space":"runner_points", "point":[30.0,50.0],
                "point_basis":"rect_center", "rect":[10.0,20.0,40.0,60.0],
            })
        );
        let sized = describe(&action, Some(&rows)).with_runner_viewport(390.0, 844.0);
        assert_eq!(json!(sized.target)["viewport"], json!([390.0, 844.0]));
        for width in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                json!(
                    describe(&action, Some(&rows))
                        .with_runner_viewport(width, 844.0)
                        .target
                ),
                target
            );
        }
        assert!(wire(action.clone(), None).get("target").is_none());
        for rect in [
            [0.0; 4],
            [-1.0, 0.0, 4.0, 4.0],
            [0.0, 0.0, -1.0, 4.0],
            [f64::NAN, 0.0, 4.0, 4.0],
            [f64::MAX, 0.0, f64::MAX, 4.0],
        ] {
            let rows = vec![ElementRow {
                rect,
                ..row("Button", "Done")
            }];
            assert!(wire(action.clone(), Some(&rows)).get("target").is_none());
        }
    }

    #[test]
    fn serialized_activity_never_copies_entered_values_or_input_labels() {
        let rows = vec![ElementRow {
            value: Some("SECRET".into()),
            identifier: Some("SECRET".into()),
            rect: [10.0, 20.0, 40.0, 60.0],
            ..row("TextField", "SECRET")
        }];
        for action in [
            json!({"type":"text","text":"SECRET","x":0.5,"y":0.5}),
            json!({"type":"set_value","element":0,"value":"SECRET"}),
            json!({"type":"picker","element":0,"value":"SECRET"}),
            json!({"type":"tap","element":0,"snapshot":"S1","value":"SECRET"}),
        ] {
            let item = wire(action.clone(), Some(&rows));
            assert!(!item.to_string().contains("SECRET"), "{item}");
            if action["type"] != "tap" {
                assert!(item.get("target").is_none());
            }
        }
        // A provider may expose an input label even when it omits its value.
        let rows = vec![row("SecureTextField", "SECRET")];
        assert!(!wire(json!({"type":"tap","element":0}), Some(&rows))
            .to_string()
            .contains("SECRET"));
        // Geometry-free system activity remains compatible with the same log.
        let entry = Entry {
            seq: 2,
            at_ms: 10,
            owner: Some("agent".into()),
            text: recording_started(),
        };
        assert!(entry_json(&entry).get("target").is_none());
        assert_eq!(entry_json(&entry)["owner"], "agent");
    }

    #[test]
    fn input_identifiers_and_keypad_geometry_are_never_retained() {
        for kind in ["TextField", "SecureTextField", "TextView", "SearchField"] {
            let rows = vec![ElementRow {
                identifier: Some("SECRET".into()),
                rect: [10.0, 20.0, 40.0, 50.0],
                ..row(kind, "SECRET")
            }];
            let value = wire(json!({"type":"tap","element":0}), Some(&rows));
            assert!(!value.to_string().contains("SECRET"));
            assert!(value.get("target").is_none());
        }
        for (kind, label) in [("Button", "7"), ("Key", "Q"), ("Button", "A")] {
            let rows = vec![ElementRow {
                rect: [10.0, 20.0, 40.0, 50.0],
                ..row(kind, label)
            }];
            let value = wire(json!({"type":"tap","element":0}), Some(&rows));
            assert!(value.get("target").is_none());
            assert!(!value["text"]
                .as_str()
                .unwrap()
                .contains(&format!("“{label}”")));
        }
    }

    #[test]
    fn launches_use_the_app_name() {
        assert_eq!(
            describe(
                &json!({"type":"launch_app","bundle":"com.tencent.xin"}),
                None
            ),
            line("Opened 微信", "打开 微信")
        );
        assert_eq!(
            en(
                json!({"type":"launch_app","bundle":"com.example.unknown"}),
                None
            ),
            "Opened com.example.unknown"
        );
    }

    #[test]
    fn gestures_and_system_actions_read_plainly() {
        let up = json!({"type":"swipe","x1":0.5,"y1":0.8,"x2":0.5,"y2":0.2});
        assert_eq!(describe(&up, None), line("Swiped up", "向上滑动"));
        let left = json!({"type":"swipe","x1":0.9,"y1":0.5,"x2":0.1,"y2":0.5});
        assert_eq!(en(left, None), "Swiped left");
        assert_eq!(en(json!({"type":"home"}), None), "Went to the Home Screen");
        assert_eq!(
            describe(&json!({"type":"alert","button":"Allow"}), None),
            line("Answered the alert with “Allow”", "在弹窗中选择“Allow”")
        );
        assert_eq!(en(json!({"type":"back"}), None), "Went back");
    }

    #[test]
    fn long_labels_are_cut() {
        let long = "x".repeat(100);
        let text = en(json!({"type":"tap","label":long}), None);
        assert_eq!(
            text.chars().count(),
            "Tapped “”…".chars().count() + MAX_LABEL
        );
    }

    #[test]
    fn the_log_keeps_the_newest_lines_and_pages_by_cursor() {
        let start = recent(u64::MAX, 0)["next"].as_u64().unwrap();
        for i in 0..(CAPACITY + 5) {
            record(
                Some("t"),
                line(format!("line {i}"), format!("第 {i} 行")),
                i as u64,
            );
        }
        let all = recent(0, CAPACITY * 2);
        let items = all["items"].as_array().unwrap();
        assert_eq!(items.len(), CAPACITY);
        let last = items.last().unwrap();
        assert_eq!(last["text"], format!("line {}", CAPACITY + 4));
        assert_eq!(last["text_zh"], format!("第 {} 行", CAPACITY + 4));
        assert_eq!(last["owner"], "t");
        let next = all["next"].as_u64().unwrap();
        assert!(next >= start + (CAPACITY + 5) as u64);
        assert!(recent(next, 10)["items"].as_array().unwrap().is_empty());
        assert_eq!(recent(next - 2, 10)["items"].as_array().unwrap().len(), 2);
        assert_eq!(recent(0, 3)["items"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_link_shows_only_its_host() {
        let line = describe(
            &json!({"type":"open_url","url":"https://user:pw@Example.com/reset?token=SECRET"}),
            None,
        );
        assert_eq!(line.en, "Opened a link to example.com");
        assert_eq!(line.zh, "打开链接 example.com");
        assert!(!line.en.contains("SECRET") && !line.zh.contains("SECRET"));
        assert_eq!(
            en(json!({"type":"open_url","url":"tel:10086"}), None),
            "Opened a tel: link"
        );
        assert_eq!(
            en(json!({"type":"open_url","url":"weixin://dl/scan"}), None),
            "Opened a weixin: link"
        );
        assert_eq!(en(json!({"type":"open_url"}), None), "Opened a link");
    }

    #[test]
    fn recording_location_and_install_lines() {
        assert_eq!(recording_started().zh, "开始录屏");
        assert_eq!(
            recording_saved(11.6),
            line("Saved a recording (12 s)", "保存录屏（12 秒）")
        );
        assert_eq!(location(false).en, "Set a simulated location");
        assert_eq!(location(true).zh, "取消模拟定位");
        assert_eq!(
            installed(Some("com.example.x")).en,
            "Installed com.example.x"
        );
        assert_eq!(installed(None).en, "Installed an app");
    }
}
