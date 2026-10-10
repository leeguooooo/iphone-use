//! Which phone this daemon drives: its name, model and iOS version.
//!
//! Clients used to show the daemon's address (`192.168.0.190:45561`) because
//! `/agent/status` said nothing about the phone itself. The facts come from
//! lockdownd's session-less `GetValue` (`DeviceName`, `ProductType`,
//! `ProductVersion`; [`crate::lockdown::device_info`]) over usbmuxd, which
//! answers over USB and for a Wi-Fi attachment alike, without touching the
//! screen. They are cached in `device.json` in the instance state dir, so a
//! released or unplugged phone keeps its name, and refreshed when the runner
//! comes up (a connect), every half hour, and every minute until a first
//! reading lands. `/agent/status` only reads the cache.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::http::AppState;

/// Refresh at least this often: a renamed phone shows its new name.
const REFRESH_EVERY: Duration = Duration::from_secs(1800);
/// Retry this often while nothing is known yet (the phone was not attached).
const RETRY_EVERY: Duration = Duration::from_secs(60);
/// How often the loop looks for a runner that just came up.
const TICK: Duration = Duration::from_secs(15);
const LOCKDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const STATE_FILE: &str = "device.json";

/// What `/agent/status` reports as `device`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Identity {
    /// The name the owner gave the phone (Settings › General › About).
    pub name: Option<String>,
    /// `iPhone10,3`.
    pub product_type: Option<String>,
    /// `16.5`.
    pub ios: Option<String>,
}

impl Identity {
    fn is_empty(&self) -> bool {
        self.name.is_none() && self.product_type.is_none() && self.ios.is_none()
    }

    /// The marketing name for `product_type`, or the raw identifier when it is
    /// not in the table.
    pub fn model(&self) -> Option<String> {
        let product_type = self.product_type.as_deref()?;
        Some(
            marketing_name(product_type)
                .map(str::to_string)
                .unwrap_or_else(|| product_type.to_string()),
        )
    }

    /// The `device` object of `/agent/status`, or `null` while nothing is known.
    pub fn to_json(&self) -> Value {
        if self.is_empty() {
            return Value::Null;
        }
        json!({
            "name": self.name,
            "model": self.model(),
            "product_type": self.product_type,
            "ios": self.ios,
        })
    }

    fn from_json(value: &Value) -> Self {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        Self {
            name: text("name"),
            product_type: text("product_type"),
            ios: text("ios"),
        }
    }

    /// A reading folded into what was known: a missing value keeps the old one.
    fn merged(&self, reading: Identity) -> Identity {
        Identity {
            name: reading.name.or_else(|| self.name.clone()),
            product_type: reading.product_type.or_else(|| self.product_type.clone()),
            ios: reading.ios.or_else(|| self.ios.clone()),
        }
    }
}

/// `ProductType` → the name on the box. Unknown identifiers return `None`.
pub fn marketing_name(product_type: &str) -> Option<&'static str> {
    Some(match product_type {
        "iPhone8,1" => "iPhone 6s",
        "iPhone8,2" => "iPhone 6s Plus",
        "iPhone8,4" => "iPhone SE",
        "iPhone9,1" | "iPhone9,3" => "iPhone 7",
        "iPhone9,2" | "iPhone9,4" => "iPhone 7 Plus",
        "iPhone10,1" | "iPhone10,4" => "iPhone 8",
        "iPhone10,2" | "iPhone10,5" => "iPhone 8 Plus",
        "iPhone10,3" | "iPhone10,6" => "iPhone X",
        "iPhone11,2" => "iPhone XS",
        "iPhone11,4" | "iPhone11,6" => "iPhone XS Max",
        "iPhone11,8" => "iPhone XR",
        "iPhone12,1" => "iPhone 11",
        "iPhone12,3" => "iPhone 11 Pro",
        "iPhone12,5" => "iPhone 11 Pro Max",
        "iPhone12,8" => "iPhone SE (2nd generation)",
        "iPhone13,1" => "iPhone 12 mini",
        "iPhone13,2" => "iPhone 12",
        "iPhone13,3" => "iPhone 12 Pro",
        "iPhone13,4" => "iPhone 12 Pro Max",
        "iPhone14,2" => "iPhone 13 Pro",
        "iPhone14,3" => "iPhone 13 Pro Max",
        "iPhone14,4" => "iPhone 13 mini",
        "iPhone14,5" => "iPhone 13",
        "iPhone14,6" => "iPhone SE (3rd generation)",
        "iPhone14,7" => "iPhone 14",
        "iPhone14,8" => "iPhone 14 Plus",
        "iPhone15,2" => "iPhone 14 Pro",
        "iPhone15,3" => "iPhone 14 Pro Max",
        "iPhone15,4" => "iPhone 15",
        "iPhone15,5" => "iPhone 15 Plus",
        "iPhone16,1" => "iPhone 15 Pro",
        "iPhone16,2" => "iPhone 15 Pro Max",
        "iPhone17,1" => "iPhone 16 Pro",
        "iPhone17,2" => "iPhone 16 Pro Max",
        "iPhone17,3" => "iPhone 16",
        "iPhone17,4" => "iPhone 16 Plus",
        "iPhone17,5" => "iPhone 16e",
        "iPhone18,1" => "iPhone 17 Pro",
        "iPhone18,2" => "iPhone 17 Pro Max",
        "iPhone18,3" => "iPhone 17",
        "iPhone18,4" => "iPhone Air",
        _ => return None,
    })
}

static IDENTITY: Mutex<Option<Identity>> = Mutex::new(None);
/// The state file, and the UDID of the phone it describes.
static STATE_PATH: Mutex<Option<(PathBuf, String)>> = Mutex::new(None);

fn current() -> Identity {
    IDENTITY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .unwrap_or_default()
}

/// The `device` object of `/agent/status` (cache only, no I/O).
pub fn current_json() -> Value {
    current().to_json()
}

/// Write `identity` for the phone `udid`.
fn save(path: &Path, udid: &str, identity: &Identity) {
    let mut value = identity.to_json();
    if let Some(object) = value.as_object_mut() {
        object.insert("udid".into(), json!(udid));
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, value.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// The saved identity, if it describes the phone `udid`: an instance pointed
/// at another phone must not name it after the old one.
fn load(path: &Path, udid: &str) -> Option<Identity> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let saved = value.get("udid").and_then(Value::as_str)?;
    if crate::usbmux::normalize_udid(saved) != crate::usbmux::normalize_udid(udid) {
        return None;
    }
    let identity = Identity::from_json(&value);
    (!identity.is_empty()).then_some(identity)
}

/// Fold a reading into the cache and the state file. Returns whether it
/// changed anything.
fn update(reading: Identity) -> bool {
    if reading.is_empty() {
        return false;
    }
    let (changed, snapshot) = {
        let mut slot = IDENTITY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = slot.clone().unwrap_or_default();
        let after = before.merged(reading);
        let changed = after != before;
        *slot = Some(after.clone());
        (changed, after)
    };
    if changed {
        tracing::info!(
            "device: {} ({}), iOS {}",
            snapshot.name.as_deref().unwrap_or("?"),
            snapshot.model().as_deref().unwrap_or("?"),
            snapshot.ios.as_deref().unwrap_or("?")
        );
        let path = STATE_PATH
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some((path, udid)) = path {
            save(&path, &udid, &snapshot);
        }
    }
    changed
}

/// Read the phone once over lockdown. Returns whether a reading landed.
async fn refresh(udid: &str) -> bool {
    match tokio::time::timeout(LOCKDOWN_TIMEOUT, crate::lockdown::device_info(udid)).await {
        Ok(Ok(info)) => {
            let reading = Identity {
                name: info.name.filter(|name| !name.trim().is_empty()),
                product_type: info.product_type,
                ios: info.product_version,
            };
            // lockdown may answer without any of the values: retry that.
            let landed = !reading.is_empty();
            update(reading);
            landed
        }
        Ok(Err(error)) => {
            tracing::debug!("device: lockdown read: {error:#}");
            false
        }
        Err(_) => {
            tracing::debug!("device: lockdown read timed out");
            false
        }
    }
}

/// Whether this tick should read: never read yet, retrying a miss, on
/// schedule, or right after the runner came up (a connect).
pub fn due(since_last: Option<Duration>, last_ok: bool, runner_up: bool, was_up: bool) -> bool {
    match since_last {
        None => true,
        Some(elapsed) => {
            (runner_up && !was_up)
                || elapsed >= REFRESH_EVERY
                || (!last_ok && elapsed >= RETRY_EVERY)
        }
    }
}

fn runner_up(state: &AppState) -> bool {
    state
        .wda_health
        .lock()
        .map(|health| health.up)
        .unwrap_or(false)
}

/// Load the cache and start the refresh loop.
pub fn spawn(state: Arc<AppState>) {
    // Without a target phone there is nothing to name.
    let Some(udid) = state.device_udid.clone() else {
        return;
    };
    let path = crate::instance::current().state_dir.join(STATE_FILE);
    if let Some(saved) = load(&path, &udid) {
        *IDENTITY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(saved);
    }
    *STATE_PATH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((path, udid.clone()));
    tokio::spawn(async move {
        let mut last: Option<tokio::time::Instant> = None;
        let mut last_ok = false;
        let mut was_up = false;
        loop {
            let up = runner_up(&state);
            if due(last.map(|at| at.elapsed()), last_ok, up, was_up) {
                last_ok = refresh(&udid).await;
                last = Some(tokio::time::Instant::now());
            }
            was_up = up;
            tokio::time::sleep(TICK).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marketing_names_for_common_phones() {
        assert_eq!(marketing_name("iPhone10,3"), Some("iPhone X"));
        assert_eq!(marketing_name("iPhone10,6"), Some("iPhone X"));
        assert_eq!(marketing_name("iPhone14,5"), Some("iPhone 13"));
        assert_eq!(marketing_name("iPhone13,1"), Some("iPhone 12 mini"));
        assert_eq!(marketing_name("iPhone14,7"), Some("iPhone 14"));
        assert_eq!(marketing_name("iPhone18,2"), Some("iPhone 17 Pro Max"));
        assert_eq!(marketing_name("iPhone99,1"), None);
        assert_eq!(marketing_name(""), None);
    }

    #[test]
    fn status_json_shape() {
        let identity = Identity {
            name: Some("Leo's iPhone".into()),
            product_type: Some("iPhone10,3".into()),
            ios: Some("16.5".into()),
        };
        assert_eq!(
            identity.to_json(),
            json!({
                "name": "Leo's iPhone",
                "model": "iPhone X",
                "product_type": "iPhone10,3",
                "ios": "16.5",
            })
        );
        // An identifier the table does not know is shown as it is.
        let unknown = Identity {
            product_type: Some("iPhone99,9".into()),
            ..Identity::default()
        };
        assert_eq!(unknown.to_json()["model"], json!("iPhone99,9"));
        assert_eq!(unknown.to_json()["name"], Value::Null);
        assert_eq!(Identity::default().to_json(), Value::Null);
    }

    #[test]
    fn a_partial_reading_keeps_what_was_known() {
        let known = Identity {
            name: Some("A".into()),
            product_type: Some("iPhone14,5".into()),
            ios: Some("18.0".into()),
        };
        let merged = known.merged(Identity {
            name: Some("B".into()),
            ..Identity::default()
        });
        assert_eq!(merged.name.as_deref(), Some("B"));
        assert_eq!(merged.product_type.as_deref(), Some("iPhone14,5"));
        assert_eq!(merged.ios.as_deref(), Some("18.0"));
    }

    #[test]
    fn cache_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("ipu-device-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(STATE_FILE);
        let identity = Identity {
            name: Some("iPhone X".into()),
            product_type: Some("iPhone10,3".into()),
            ios: Some("16.5".into()),
        };
        save(&path, "00008030-001A", &identity);
        assert_eq!(load(&path, "00008030-001A"), Some(identity.clone()));
        assert_eq!(load(&path, "00008030001a"), Some(identity));
        // Another phone on this instance: the old name is not used.
        assert_eq!(load(&path, "63f53bbb05918cbf"), None);
        std::fs::write(&path, r#"{"name":"x"}"#).unwrap();
        assert_eq!(load(&path, "00008030-001A"), None);
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(load(&path, "00008030-001A"), None);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&path, "00008030-001A"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refresh_on_connect_on_schedule_and_until_known() {
        assert!(due(None, false, false, false));
        let soon = Some(Duration::from_secs(20));
        assert!(!due(soon, true, true, true));
        assert!(!due(soon, false, false, false));
        // A connect reads again.
        assert!(due(soon, true, true, false));
        // A miss is retried every minute, a hit every half hour.
        assert!(due(Some(RETRY_EVERY), false, false, false));
        assert!(!due(Some(RETRY_EVERY), true, false, false));
        assert!(due(Some(REFRESH_EVERY), true, false, false));
    }
}
