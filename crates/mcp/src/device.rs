//! The device tools beyond the screen: logs, crash reports, simulated
//! location, recording, app install and per-app notes. Each builds its daemon
//! request here — checked offline first, so a malformed call sends nothing —
//! and renders the daemon's answer as the short text a model reads; the JSON
//! still rides along as structured content.

use serde_json::Value;

/// Longest message printed per log line; the structured copy keeps it whole.
const LOG_MESSAGE_CHARS: usize = 300;
/// How much of a crash report the model reads (the head holds the exception
/// and the crashed thread); the daemon itself returns up to 2 MiB.
const CRASH_TEXT_BYTES: usize = 48 * 1024;
/// The daemon's cap on one app's notes.
pub const NOTES_MAX_BYTES: usize = 16 * 1024;
/// The daemon's longest install URL.
const MAX_INSTALL_URL_CHARS: usize = 2048;

fn text(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn clip(line: &str, max: usize) -> String {
    let flat = line.replace('\n', " ⏎ ");
    let mut out: String = flat.chars().take(max).collect();
    if flat.chars().count() > max {
        out.push('…');
    }
    out
}

/// A string cut to at most `max` bytes on a char boundary.
fn head(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let cut = (0..=max).rev().find(|i| text.is_char_boundary(*i)).unwrap_or(0);
    &text[..cut]
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// What one `phone_logs` call sends.
#[derive(Debug, PartialEq)]
pub enum LogsCall {
    /// `POST /agent/logs {"action":…}`: start, stop or clear the capture.
    Control(Value),
    /// `GET /agent/logs?…`: read what was captured.
    Read(String),
}

#[derive(Debug, Default)]
pub struct LogsFilter {
    pub process: Option<String>,
    pub contains: Option<String>,
    pub level: Option<String>,
    pub since: Option<u64>,
    pub limit: Option<usize>,
}

impl LogsFilter {
    fn is_empty(&self) -> bool {
        self.process.is_none()
            && self.contains.is_none()
            && self.level.is_none()
            && self.since.is_none()
            && self.limit.is_none()
    }
}

pub fn logs_call(action: Option<&str>, filter: LogsFilter) -> Result<LogsCall, String> {
    let action = action.map(str::trim).unwrap_or("read").to_ascii_lowercase();
    match action.as_str() {
        "read" => {
            let mut params = Vec::new();
            if let Some(since) = filter.since {
                params.push(("since", since.to_string()));
            }
            for (key, value) in [
                ("process", &filter.process),
                ("contains", &filter.contains),
                ("level", &filter.level),
            ] {
                if let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                    params.push((key, value.to_string()));
                }
            }
            if let Some(limit) = filter.limit {
                params.push(("limit", limit.to_string()));
            }
            Ok(LogsCall::Read(crate::client::query_path("/agent/logs", &params)))
        }
        "start" | "stop" | "clear" if filter.is_empty() => {
            Ok(LogsCall::Control(serde_json::json!({ "action": action })))
        }
        "start" | "stop" | "clear" => Err(format!(
            "process, contains, level, since and limit filter action=read only; nothing was sent \
             (call action={action} without them, then read with the filters)"
        )),
        other => Err(format!(
            "unknown action {other:?}; use start, read, stop or clear. Nothing was sent."
        )),
    }
}

/// `GET /agent/crashes?…` (the list) or `GET /agent/crashes/<name>` (one).
pub fn crashes_path(
    name: Option<&str>,
    process: Option<&str>,
    limit: Option<usize>,
) -> Result<String, String> {
    if let Some(name) = name.map(str::trim) {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name == ".." || name == "." {
            return Err(format!(
                "{name:?} is not a crash report name; pass a `name` exactly as the list shows it. \
                 Nothing was sent."
            ));
        }
        if process.is_some() || limit.is_some() {
            return Err(
                "process and limit filter the list; reading one report takes only `name`. \
                 Nothing was sent."
                    .to_string(),
            );
        }
        let mut url = reqwest::Url::parse("http://daemon/agent/crashes").expect("static URL");
        url.path_segments_mut().expect("base URL").push(name);
        return Ok(url.path().to_string());
    }
    let mut params = Vec::new();
    if let Some(process) = process.map(str::trim).filter(|p| !p.is_empty()) {
        params.push(("process", process.to_string()));
    }
    if let Some(limit) = limit {
        params.push(("limit", limit.to_string()));
    }
    Ok(crate::client::query_path("/agent/crashes", &params))
}

/// `POST /agent/location` body: a point, or `{"clear":true}`.
pub fn location_body(lat: Option<f64>, lon: Option<f64>, clear: bool) -> Result<Value, String> {
    match (lat, lon, clear) {
        (None, None, true) => Ok(serde_json::json!({ "clear": true })),
        (Some(_), _, true) | (_, Some(_), true) => Err(
            "pass either lat and lon, or clear=true — not both. Nothing was sent.".to_string(),
        ),
        (Some(lat), Some(lon), false) => {
            if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
                return Err(format!("lat {lat} is outside -90..90. Nothing was sent."));
            }
            if !lon.is_finite() || !(-180.0..=180.0).contains(&lon) {
                return Err(format!("lon {lon} is outside -180..180. Nothing was sent."));
            }
            Ok(serde_json::json!({ "lat": lat, "lon": lon }))
        }
        _ => Err(
            "pass both lat and lon (decimal degrees, WGS84), or clear=true. Nothing was sent."
                .to_string(),
        ),
    }
}

/// `POST /agent/record {"action":"start"|"stop"}`, or `None` for the status
/// read (`GET /agent/record`).
pub fn record_body(action: &str) -> Result<Option<Value>, String> {
    match action.trim().to_ascii_lowercase().as_str() {
        action @ ("start" | "stop") => Ok(Some(serde_json::json!({ "action": action }))),
        "status" => Ok(None),
        other => Err(format!(
            "unknown action {other:?}; use start, stop or status. Nothing was sent."
        )),
    }
}

/// `POST /agent/input {"type":"install",…}` body, checked the way the daemon
/// checks it: exactly one of an absolute `.ipa`/`.app` path on the daemon's
/// Mac, or an https URL.
pub fn install_body(
    path: Option<&str>,
    url: Option<&str>,
    fresh: bool,
) -> Result<Value, String> {
    let path = path.map(str::trim).filter(|p| !p.is_empty());
    let url = url.map(str::trim).filter(|u| !u.is_empty());
    let mut body = match (path, url) {
        (Some(path), None) => {
            let lower = path.trim_end_matches('/').to_ascii_lowercase();
            if !path.starts_with('/') {
                return Err(format!(
                    "path {path:?} must be absolute (a file on the Mac running the daemon). \
                     Nothing was sent."
                ));
            }
            if !(lower.ends_with(".ipa") || lower.ends_with(".app")) {
                return Err(format!(
                    "path {path:?} must name an .ipa file or an .app bundle. Nothing was sent."
                ));
            }
            serde_json::json!({ "type": "install", "path": path })
        }
        (None, Some(url)) => {
            if !url.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://")) {
                return Err(format!(
                    "url {url:?} must be https://. Nothing was sent."
                ));
            }
            if url.chars().count() > MAX_INSTALL_URL_CHARS {
                return Err(format!(
                    "url is longer than {MAX_INSTALL_URL_CHARS} characters. Nothing was sent."
                ));
            }
            serde_json::json!({ "type": "install", "url": url })
        }
        (Some(_), Some(_)) => {
            return Err("pass path or url, not both. Nothing was sent.".to_string())
        }
        (None, None) => {
            return Err(
                "pass path (an .ipa or .app on the daemon's Mac) or url (https). Nothing was sent."
                    .to_string(),
            )
        }
    };
    if fresh {
        body["fresh"] = Value::Bool(true);
    }
    Ok(body)
}

/// `/agent/app-notes?bundle=…`, for a plausible bundle id.
pub fn app_notes_path(bundle: &str) -> Result<String, String> {
    let bundle = bundle.trim();
    let plausible = !bundle.is_empty()
        && bundle.len() <= 255
        && bundle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !bundle.starts_with('.')
        && !bundle.contains("..");
    if !plausible {
        return Err(format!(
            "{bundle:?} is not a bundle id (e.g. com.tencent.xin); phone_apps finds one by name. \
             Nothing was sent."
        ));
    }
    Ok(crate::client::query_path(
        "/agent/app-notes",
        &[("bundle", bundle.to_string())],
    ))
}

/// `phone_elements`' `ocr` argument as the daemon's query value.
pub fn ocr_param(ocr: Option<&str>) -> Result<Option<&'static str>, String> {
    match ocr.map(|o| o.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("off") => Ok(None),
        Some("auto") => Ok(Some("1")),
        Some("force") => Ok(Some("force")),
        Some(other) => Err(format!(
            "ocr {other:?} is not one of auto, force, off"
        )),
    }
}

// ---------------------------------------------------------------------------
// Compact text
// ---------------------------------------------------------------------------

/// `GET /agent/logs`: one line per log line, newest last, and how to continue.
pub fn logs(json: &Value) -> Option<String> {
    let items = json.get("items")?.as_array()?;
    let capturing = json.get("capturing") == Some(&Value::Bool(true));
    let mut headline = format!(
        "{} log lines · next {} (pass as since) · {}",
        items.len(),
        text(json, "next").unwrap_or_else(|| "?".to_string()),
        if capturing { "capturing" } else { "capture off" }
    );
    if let Some(missed) = json.get("missed").and_then(Value::as_u64).filter(|m| *m > 0) {
        headline.push_str(&format!(" · {missed} lines dropped before this read"));
    }
    if let Some(stopped) = text(json, "stopped") {
        headline.push_str(&format!(" · capture stopped: {stopped}"));
    }
    let mut out = vec![headline];
    if items.is_empty() && !capturing {
        out.push(
            "nothing captured: phone_logs action=start, reproduce the problem, then read"
                .to_string(),
        );
    }
    for item in items {
        let process = text(item, "process").unwrap_or_else(|| "?".to_string());
        let pid = text(item, "pid").map(|p| format!("[{p}]")).unwrap_or_default();
        let subsystem = text(item, "subsystem").map(|s| format!(" ({s})")).unwrap_or_default();
        let level = text(item, "level").map(|l| format!(" {l}")).unwrap_or_default();
        out.push(format!(
            "#{} {} {process}{pid}{subsystem}{level}: {}",
            text(item, "seq").unwrap_or_default(),
            text(item, "ts").unwrap_or_default(),
            clip(&text(item, "message").unwrap_or_default(), LOG_MESSAGE_CHARS)
        ));
    }
    Some(out.join("\n"))
}

/// `POST /agent/logs`: the capture's new state.
pub fn logs_control(json: &Value) -> Option<String> {
    let capturing = json.get("capturing")?.as_bool()?;
    Some(if capturing {
        "capturing device logs — reproduce, then phone_logs (action=read)".to_string()
    } else {
        "device log capture is off".to_string()
    })
}

/// `GET /agent/crashes`: one line per report.
pub fn crashes(json: &Value) -> Option<String> {
    let items = json.get("items")?.as_array()?;
    let mut out = vec![if items.is_empty() {
        "no crash reports on the phone match".to_string()
    } else {
        format!(
            "{} crash reports (name · process · time · size); phone_crashes name=… reads one",
            items.len()
        )
    }];
    for item in items {
        let mut line = format!(
            "{} · {} · {} · {} B",
            text(item, "name").unwrap_or_default(),
            text(item, "process").unwrap_or_else(|| "?".to_string()),
            text(item, "stamp")
                .or_else(|| text(item, "time"))
                .unwrap_or_else(|| "?".to_string()),
            text(item, "size").unwrap_or_else(|| "?".to_string())
        );
        if let Some(folder) = text(item, "folder") {
            line.push_str(&format!(" · in {folder}"));
        }
        out.push(line);
    }
    Some(out.join("\n"))
}

/// `GET /agent/crashes/<name>`: a headline, then the report's head.
pub fn crash_report(json: &Value) -> Option<String> {
    let body = json.get("text")?.as_str()?;
    let mut headline = format!(
        "crash report {} · {} bytes",
        text(json, "name").unwrap_or_default(),
        text(json, "size").unwrap_or_else(|| body.len().to_string())
    );
    if json.get("truncated") == Some(&Value::Bool(true)) {
        headline.push_str(" · the daemon cut the file");
    }
    let shown = head(body, CRASH_TEXT_BYTES);
    if shown.len() < body.len() {
        headline.push_str(&format!(
            " · first {} of {} bytes shown",
            shown.len(),
            body.len()
        ));
    }
    Some(format!("{headline}\n{shown}"))
}

/// `/agent/record`: start, status or stop.
pub fn recording(json: &Value) -> Option<String> {
    let object = json.as_object()?;
    let line = |r: &Value| {
        let mut parts = vec![
            text(r, "path").unwrap_or_else(|| "?".to_string()),
            format!("{} s", text(r, "seconds").unwrap_or_else(|| "?".to_string())),
        ];
        // A simulator recording (`via:"simctl"`) has no frame count.
        if let Some(frames) = text(r, "frames") {
            parts.push(format!("{frames} frames"));
        }
        parts.push(format!("{} bytes", text(r, "bytes").unwrap_or_else(|| "?".to_string())));
        if let (Some(w), Some(h)) = (text(r, "width"), text(r, "height")) {
            parts.push(format!("{w}x{h}"));
        }
        if let Some(ended) = text(r, "ended") {
            parts.push(format!("ended by itself: {ended}"));
        }
        parts.join(" · ")
    };
    if let Some(summary) = object.get("summary") {
        return Some(if summary.is_object() {
            format!("saved recording {}", line(summary))
        } else {
            "nothing was recording".to_string()
        });
    }
    match object.get("recording") {
        Some(r) if r.is_object() => Some(format!("recording {}", line(r))),
        Some(_) => Some("not recording".to_string()),
        None => None,
    }
}

/// `POST /agent/location`.
pub fn location(json: &Value, body: &Value) -> Option<String> {
    (json.get("ok") == Some(&Value::Bool(true))).then(|| {
        if body.get("clear").is_some() {
            "simulated location cleared; the phone uses its real location again".to_string()
        } else {
            format!(
                "simulated location set to {}, {} — apps see it until phone_set_location \
                 clear=true",
                body["lat"], body["lon"]
            )
        }
    })
}

/// `POST /agent/input` install.
pub fn installed(json: &Value) -> Option<String> {
    (json.get("ok") == Some(&Value::Bool(true))).then(|| match text(json, "bundle") {
        Some(bundle) => format!("installed {bundle}; launch it with a launch_app step"),
        None => "installed".to_string(),
    })
}

/// `GET /agent/app-notes`.
pub fn app_notes(json: &Value) -> Option<String> {
    let notes = json.get("text")?.as_str()?;
    let bundle = text(json, "bundle").unwrap_or_default();
    Some(if notes.trim().is_empty() {
        format!("no notes for {bundle} yet")
    } else {
        let updated = text(json, "updated_at")
            .map(|u| format!(" (updated {u})"))
            .unwrap_or_default();
        format!("notes for {bundle}{updated}:\n{notes}")
    })
}

/// The `ocr` block of `GET /agent/elements?ocr=…`, as lines under the tree:
/// each recognized text with its rect's centre in phone_tap coordinates when
/// the screen size is known. `None` when the read carried neither OCR rows
/// nor an OCR error.
pub fn ocr_block(json: &Value) -> Option<String> {
    if let Some(error) = text(json, "ocr_error") {
        return Some(format!(
            "ocr_error: {} (the element list above is still the answer)",
            clip(&error, 300)
        ));
    }
    let ocr = json.get("ocr")?;
    let rows = ocr.get("rows")?.as_array()?;
    // The Application row spans the screen, in the same points as the rects.
    let screen = json
        .get("elements")
        .and_then(Value::as_array)
        .and_then(|rows| rows.iter().find(|r| r["kind"] == "Application"))
        .and_then(|app| app["rect"].as_array())
        .and_then(|rect| Some((rect.get(2)?.as_f64()?, rect.get(3)?.as_f64()?)))
        .filter(|(w, h)| *w > 0.0 && *h > 0.0);
    let mut out = vec![format!(
        "ocr ({}, {} rows): text read from pixels, no element behind it — tap a row's point \
         with phone_tap",
        text(ocr, "source").unwrap_or_else(|| "vision".to_string()),
        rows.len()
    )];
    for row in rows {
        let label = clip(&text(row, "label").unwrap_or_default(), 80);
        let rect: Option<Vec<f64>> = row["rect"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_f64).collect())
            .filter(|r: &Vec<f64>| r.len() == 4);
        let place = match (rect, screen) {
            (Some(r), Some((w, h))) => format!(
                " x={:.3} y={:.3}",
                (r[0] + r[2] / 2.0) / w,
                (r[1] + r[3] / 2.0) / h
            ),
            (Some(r), None) => format!(" @{},{},{},{}", r[0], r[1], r[2], r[3]),
            _ => String::new(),
        };
        let confidence = text(row, "confidence").map(|c| format!(" ({c})")).unwrap_or_default();
        out.push(format!("\"{label}\"{place}{confidence}"));
    }
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn logs_calls_are_checked_before_anything_is_sent() {
        assert_eq!(
            logs_call(None, LogsFilter::default()).unwrap(),
            LogsCall::Read("/agent/logs".into())
        );
        assert_eq!(
            logs_call(
                Some("read"),
                LogsFilter {
                    process: Some("WeChat".into()),
                    contains: Some("timeout 超时".into()),
                    level: Some("error".into()),
                    since: Some(42),
                    limit: Some(20),
                }
            )
            .unwrap(),
            LogsCall::Read(
                "/agent/logs?since=42&process=WeChat&contains=timeout+%E8%B6%85%E6%97%B6&level=error&limit=20"
                    .into()
            )
        );
        assert_eq!(
            logs_call(Some(" Start "), LogsFilter::default()).unwrap(),
            LogsCall::Control(json!({"action": "start"}))
        );
        let err = logs_call(
            Some("start"),
            LogsFilter { process: Some("x".into()), ..Default::default() },
        )
        .unwrap_err();
        assert!(err.contains("action=read only"), "{err}");
        assert!(logs_call(Some("tail"), LogsFilter::default()).is_err());
    }

    #[test]
    fn crash_paths_encode_the_name_and_refuse_traversal() {
        assert_eq!(crashes_path(None, None, None).unwrap(), "/agent/crashes");
        assert_eq!(
            crashes_path(None, Some("WeChat"), Some(5)).unwrap(),
            "/agent/crashes?process=WeChat&limit=5"
        );
        assert_eq!(
            crashes_path(Some("WeChat-2026-10-11-070102.ips"), None, None).unwrap(),
            "/agent/crashes/WeChat-2026-10-11-070102.ips"
        );
        assert_eq!(
            crashes_path(Some("a b#c.ips"), None, None).unwrap(),
            "/agent/crashes/a%20b%23c.ips"
        );
        for bad in ["../etc/passwd", "Retired/x.ips", "", ".."] {
            assert!(crashes_path(Some(bad), None, None).is_err(), "{bad}");
        }
        assert!(crashes_path(Some("x.ips"), Some("p"), None).is_err());
    }

    #[test]
    fn location_needs_a_valid_point_or_clear() {
        assert_eq!(
            location_body(Some(31.2304), Some(121.4737), false).unwrap(),
            json!({"lat": 31.2304, "lon": 121.4737})
        );
        assert_eq!(location_body(None, None, true).unwrap(), json!({"clear": true}));
        assert!(location_body(Some(1.0), Some(2.0), true).is_err());
        assert!(location_body(Some(1.0), None, false).is_err());
        assert!(location_body(None, None, false).is_err());
        assert!(location_body(Some(91.0), Some(0.0), false).is_err());
        assert!(location_body(Some(0.0), Some(-180.5), false).is_err());
        assert!(location_body(Some(f64::NAN), Some(0.0), false).is_err());
    }

    #[test]
    fn install_takes_one_absolute_package_or_one_https_url() {
        assert_eq!(
            install_body(Some("/tmp/App.ipa"), None, false).unwrap(),
            json!({"type": "install", "path": "/tmp/App.ipa"})
        );
        assert_eq!(
            install_body(None, Some("https://example.com/a.ipa"), true).unwrap(),
            json!({"type": "install", "url": "https://example.com/a.ipa", "fresh": true})
        );
        assert!(install_body(Some("/Build/My.app/"), None, false).is_ok());
        for (path, url) in [
            (Some("App.ipa"), None),
            (Some("/tmp/App.zip"), None),
            (None, Some("http://example.com/a.ipa")),
            (Some("/a.ipa"), Some("https://e.com/a.ipa")),
            (None, None),
        ] {
            assert!(install_body(path, url, false).is_err(), "{path:?} {url:?}");
        }
    }

    #[test]
    fn app_notes_need_a_bundle_id() {
        assert_eq!(
            app_notes_path(" com.tencent.xin ").unwrap(),
            "/agent/app-notes?bundle=com.tencent.xin"
        );
        for bad in ["", "微信", "com/../x", "a b", ".hidden"] {
            assert!(app_notes_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn record_and_ocr_arguments() {
        assert_eq!(record_body("start").unwrap(), Some(json!({"action": "start"})));
        assert_eq!(record_body("STOP").unwrap(), Some(json!({"action": "stop"})));
        assert_eq!(record_body("status").unwrap(), None);
        assert!(record_body("pause").is_err());
        assert_eq!(ocr_param(None).unwrap(), None);
        assert_eq!(ocr_param(Some("off")).unwrap(), None);
        assert_eq!(ocr_param(Some("auto")).unwrap(), Some("1"));
        assert_eq!(ocr_param(Some("Force")).unwrap(), Some("force"));
        assert!(ocr_param(Some("always")).is_err());
    }

    #[test]
    fn log_lines_are_one_per_row_with_the_cursor() {
        let text = logs(&json!({
            "ok": true, "capturing": true, "next": 9, "missed": 3,
            "items": [{"seq": 8, "ts": "07:01:02.5", "process": "WeChat", "pid": 311,
                       "subsystem": "com.tencent.net", "level": "error",
                       "message": "request failed\ncode=-1001"}]
        }))
        .unwrap();
        assert!(text.starts_with("1 log lines · next 9 (pass as since) · capturing · 3 lines dropped"), "{text}");
        assert!(text.contains("#8 07:01:02.5 WeChat[311] (com.tencent.net) error: request failed ⏎ code=-1001"), "{text}");
        let idle = logs(&json!({"ok": true, "capturing": false, "next": 0, "items": []})).unwrap();
        assert!(idle.contains("action=start"), "{idle}");
    }

    #[test]
    fn crash_text_lists_and_reads() {
        let list = crashes(&json!({"ok": true, "items": [
            {"name": "WeChat-2026-10-11-070102.ips", "process": "WeChat",
             "stamp": "2026-10-11 07:01:02", "time": 1, "size": 4096, "folder": "Retired"}
        ]}))
        .unwrap();
        assert!(list.contains("WeChat-2026-10-11-070102.ips · WeChat · 2026-10-11 07:01:02 · 4096 B · in Retired"), "{list}");
        let long = "x".repeat(CRASH_TEXT_BYTES + 10);
        let report = crash_report(&json!({"ok": true, "name": "a.ips", "text": long, "size": long.len(), "truncated": true})).unwrap();
        assert!(report.contains("the daemon cut the file"), "{report}");
        assert!(report.contains(&format!("first {CRASH_TEXT_BYTES} of")), "{}", &report[..200]);
    }

    #[test]
    fn recording_states_read_as_one_line() {
        assert_eq!(
            recording(&json!({"ok": true, "summary": null})).unwrap(),
            "nothing was recording"
        );
        assert_eq!(recording(&json!({"ok": true, "recording": null})).unwrap(), "not recording");
        assert_eq!(
            recording(&json!({"ok": true, "summary": {"path": "/r/s.mp4", "seconds": 4.5, "bytes": 2048, "via": "simctl"}})).unwrap(),
            "saved recording /r/s.mp4 · 4.5 s · 2048 bytes"
        );
        let saved = recording(&json!({"ok": true, "summary": {"path": "/r/a.mp4", "seconds": 12.0,
            "frames": 360, "bytes": 1000, "width": 1170, "height": 2532, "ended": "max_duration"}}))
        .unwrap();
        assert_eq!(
            saved,
            "saved recording /r/a.mp4 · 12.0 s · 360 frames · 1000 bytes · 1170x2532 · ended by itself: max_duration"
        );
    }

    #[test]
    fn ocr_rows_get_tap_coordinates_from_the_screen_size() {
        let text = ocr_block(&json!({
            "elements": [{"kind": "Application", "label": "Game", "rect": [0, 0, 400, 800]}],
            "ocr": {"source": "vision", "rows": [
                {"kind": "OCRText", "label": "Start", "rect": [100, 300, 200, 40], "confidence": 0.98}
            ]}
        }))
        .unwrap();
        assert!(text.contains("\"Start\" x=0.500 y=0.400 (0.98)"), "{text}");
        let failed = ocr_block(&json!({"elements": [], "ocr_error": "vision helper missing"})).unwrap();
        assert!(failed.starts_with("ocr_error: vision helper missing"), "{failed}");
        assert!(ocr_block(&json!({"elements": []})).is_none());
    }
}
