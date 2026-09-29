//! `GET /agent/apps` (issue #76): the installed-app inventory, so registry
//! flows can be matched to the app (or, for Apple system apps, iOS) version
//! actually on the phone.
//!
//! The daemon asks CoreDevice (`devicectl device info apps --include-all-apps`
//! plus `device info details`) and maps the result onto the contract the MCP
//! already consumes (`crates/mcp/src/compat.rs::from_daemon_json`). This module
//! holds the pure mapping and the per-target cache; the devicectl calls live
//! beside the other CoreDevice helpers in `http.rs`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// A fresh inventory is reused for this long per target. App installs are
/// rare next to agent polling; `?refresh=1` bypasses it.
pub const CACHE_TTL: Duration = Duration::from_secs(600);

static CACHE: Mutex<Option<HashMap<String, (Instant, Value)>>> = Mutex::new(None);

pub fn cache_get(target: &str) -> Option<Value> {
    let guard = CACHE.lock().ok()?;
    let (at, body) = guard.as_ref()?.get(target)?;
    (at.elapsed() < CACHE_TTL).then(|| body.clone())
}

pub fn cache_put(target: &str, body: &Value) {
    if let Ok(mut guard) = CACHE.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(target.to_string(), (Instant::now(), body.clone()));
    }
}

/// Map `devicectl device info apps --json-output` (and, when it succeeded,
/// `device info details --json-output`) onto the `/agent/apps` body.
///
/// An inventory with no apps is an error, never an empty list: every phone
/// has system apps, so zero means devicectl did not really answer, and an
/// empty list would read as "nothing installed" to a compat check.
pub fn from_devicectl(
    target: &str,
    apps_json: &str,
    details_json: Option<&str>,
    fetched_at: &str,
) -> Result<Value, String> {
    let apps: Value =
        serde_json::from_str(apps_json).map_err(|e| format!("devicectl apps JSON: {e}"))?;
    let mut out: Vec<Value> = apps["result"]["apps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|app| {
            let bundle = app["bundleIdentifier"].as_str().filter(|b| !b.is_empty())?;
            Some(json!({
                "bundle": bundle,
                "name": app["name"].as_str(),
                "version": app["version"].as_str(),
                "bundle_version": app["bundleVersion"].as_str(),
                // `defaultApp` marks Apple's own apps, whose `version` is a
                // placeholder (1.0): consumers compare those against `ios`.
                "system": app["defaultApp"].as_bool().unwrap_or(false),
                "removable": app["removable"].as_bool(),
                "hidden": app["hidden"].as_bool(),
            }))
        })
        .collect();
    if out.is_empty() {
        return Err("devicectl returned no installed apps".to_string());
    }
    out.sort_by(|a, b| a["bundle"].as_str().cmp(&b["bundle"].as_str()));

    // A failed details call only nulls the device fields: the app list is
    // still the useful part.
    let details: Value = details_json
        .and_then(|d| serde_json::from_str(d).ok())
        .unwrap_or(Value::Null);
    let hardware = &details["result"]["hardwareProperties"];
    let props = &details["result"]["deviceProperties"];
    Ok(json!({
        "ok": true,
        "udid": hardware["udid"].as_str().unwrap_or(target),
        "device": {
            "marketing_name": hardware["marketingName"].as_str(),
            "product_type": hardware["productType"].as_str(),
            "ios": props["osVersionNumber"].as_str(),
            "build": props["osBuildUpdate"].as_str(),
        },
        "fetched_at": fetched_at,
        "source": "devicectl",
        "apps": out,
    }))
}

/// `?bundle=<id>`: keep only that entry (possibly none — the app is not
/// installed, which is an answer, not a failure).
pub fn filter_bundle(mut body: Value, bundle: &str) -> Value {
    if let Some(apps) = body["apps"].as_array_mut() {
        apps.retain(|app| app["bundle"].as_str() == Some(bundle));
    }
    body
}

/// UTC `YYYY-MM-DDTHH:MM:SSZ` without a date-time dependency.
pub fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rfc3339(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPS: &str = r#"{"info":{"outcome":"success"},"result":{"apps":[
        {"bundleIdentifier":"com.tencent.xin","name":"WeChat","version":"8.0.76",
         "bundleVersion":"8.0.76.36","defaultApp":false,"removable":true,"hidden":false},
        {"bundleIdentifier":"com.apple.Health","name":"Health","version":"1.0",
         "bundleVersion":"7027.0.72.2.7","defaultApp":true,"removable":false,"hidden":false},
        {"name":"no bundle id is skipped"}
    ]}}"#;
    const DETAILS: &str = r#"{"result":{
        "hardwareProperties":{"marketingName":"iPhone 17 Pro Max","productType":"iPhone18,2",
                              "udid":"00008150-000000000000001E"},
        "deviceProperties":{"osVersionNumber":"27.0","osBuildUpdate":"24A5424a"}}}"#;

    #[test]
    fn maps_devicectl_onto_the_issue_contract() {
        let body = from_devicectl("core-id", APPS, Some(DETAILS), "2026-09-05T11:02:00Z").unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["udid"], "00008150-000000000000001E");
        assert_eq!(body["source"], "devicectl");
        assert_eq!(body["fetched_at"], "2026-09-05T11:02:00Z");
        assert_eq!(
            body["device"],
            json!({"marketing_name":"iPhone 17 Pro Max","product_type":"iPhone18,2",
                   "ios":"27.0","build":"24A5424a"})
        );
        let apps = body["apps"].as_array().unwrap();
        // Sorted by bundle; the entry without an id is dropped.
        assert_eq!(apps.len(), 2);
        assert_eq!(
            apps[0],
            json!({"bundle":"com.apple.Health","name":"Health","version":"1.0",
                   "bundle_version":"7027.0.72.2.7","system":true,"removable":false,"hidden":false})
        );
        assert_eq!(apps[1]["bundle"], "com.tencent.xin");
        assert_eq!(apps[1]["version"], "8.0.76");
        assert_eq!(apps[1]["system"], false);
    }

    #[test]
    fn a_failed_details_call_only_nulls_the_device_fields() {
        let body = from_devicectl("00008150-AAAA", APPS, None, "t").unwrap();
        assert_eq!(body["udid"], "00008150-AAAA");
        assert!(body["device"]["ios"].is_null());
        assert!(body["device"]["marketing_name"].is_null());
        assert_eq!(body["apps"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn an_empty_or_unreadable_inventory_is_an_error_not_an_empty_list() {
        assert!(from_devicectl("x", r#"{"result":{"apps":[]}}"#, None, "t").is_err());
        assert!(from_devicectl("x", r#"{"result":{}}"#, None, "t").is_err());
        assert!(from_devicectl("x", "not json", None, "t").is_err());
    }

    #[test]
    fn bundle_filter_keeps_one_entry_or_none() {
        let body = from_devicectl("x", APPS, None, "t").unwrap();
        let one = filter_bundle(body.clone(), "com.apple.Health");
        assert_eq!(one["apps"].as_array().unwrap().len(), 1);
        assert_eq!(one["apps"][0]["bundle"], "com.apple.Health");
        let none = filter_bundle(body, "com.example.missing");
        assert_eq!(none["ok"], true);
        assert!(none["apps"].as_array().unwrap().is_empty());
    }

    #[test]
    fn cache_is_per_target() {
        let body = json!({"ok":true,"apps":[{"bundle":"a"}]});
        cache_put("cache-test-target", &body);
        assert_eq!(cache_get("cache-test-target"), Some(body));
        assert_eq!(cache_get("cache-test-other"), None);
    }

    #[test]
    fn rfc3339_is_utc_civil_time() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_788_606_120), "2026-09-05T11:02:00Z");
    }
}
