//! Owner-set limits on what a driving agent may do, enforced by the daemon so
//! they hold whatever the agent's own prompt says.
//!
//! - `IPHONE_USE_BLOCKED_APPS`: comma-separated bundle ids or app names
//!   ("Wallet, com.apple.Passwords"). An agent cannot open them, and while one
//!   is in front it can only leave (`home`, `launch_app` of another app).
//! - `IPHONE_USE_MAX_ACTIONS`: actions per run and caller. A run is the span
//!   between `POST /agent/run {"action":"start"}` calls, or, without them, one
//!   that ends after [`crate::metrics::IDLE_GAP_MS`] without an action.
//!
//! Both are off by default. A refusal is `403` with `outcome: not_sent`: no
//! input reached the phone. The human control page is never limited.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::app_lookup::{catalog, normalize, system_app_bundle, valid_bundle};

/// Callers tracked at once; past this the map is cleared.
const MAX_OWNERS: usize = 64;

/// One blocked app: the bundle compared against, and the name shown.
#[derive(Debug, Clone, PartialEq)]
pub struct Blocked {
    pub bundle: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Policy {
    pub blocked: Vec<Blocked>,
    /// `None`: no budget.
    pub max_actions: Option<u32>,
}

/// Why an action was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// `launch_app` of a blocked app.
    BlockedLaunch(String),
    /// A blocked app is in front and the action does not leave it.
    BlockedForeground(String),
    /// The foreground could not be read, so a blocked app cannot be ruled out.
    ForegroundUnknown,
    /// The run's budget would be exceeded.
    Budget { max: u32, used: u32, wanted: u32 },
}

impl Refusal {
    pub fn error(&self) -> &'static str {
        match self {
            Refusal::BlockedLaunch(_) | Refusal::BlockedForeground(_) => "blocked_app",
            Refusal::ForegroundUnknown => "foreground_unverified",
            Refusal::Budget { .. } => "action_budget_exhausted",
        }
    }

    /// What the agent should do next. An error message is an instruction an
    /// agent follows, so it names the next step.
    pub fn hint(&self) -> String {
        match self {
            Refusal::BlockedLaunch(name) => format!(
                "{name} is blocked by the phone owner (IPHONE_USE_BLOCKED_APPS). Do not try to reach it another way; tell the user."
            ),
            Refusal::BlockedForeground(name) => format!(
                "{name} is in front and blocked by the phone owner (IPHONE_USE_BLOCKED_APPS). Only leaving is allowed: {{\"type\":\"home\"}} or launch_app of another app."
            ),
            Refusal::ForegroundUnknown => "the app in front could not be read, and the phone owner blocks some apps (IPHONE_USE_BLOCKED_APPS). Nothing was sent; read the screen and retry.".to_string(),
            Refusal::Budget { max, used, wanted } => format!(
                "this run has used {used} of its {max} actions (IPHONE_USE_MAX_ACTIONS) and this request needs {wanted}. Stop and report progress to the user; a new run (POST /agent/run start) gets a fresh budget."
            ),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut body = serde_json::json!({
            "ok": false,
            "error": self.error(),
            "outcome": "not_sent",
            "retry_safe": matches!(self, Refusal::ForegroundUnknown),
            "hint": self.hint(),
        });
        match self {
            Refusal::BlockedLaunch(name) | Refusal::BlockedForeground(name) => {
                body["app"] = Value::String(name.clone());
            }
            Refusal::Budget { max, used, .. } => {
                body["budget"] = serde_json::json!({"max": max, "used": used});
            }
            Refusal::ForegroundUnknown => {}
        }
        body
    }
}

impl Policy {
    /// The daemon's policy, read from the environment once.
    pub fn get() -> &'static Policy {
        static POLICY: OnceLock<Policy> = OnceLock::new();
        POLICY.get_or_init(|| Policy::from_map(|key| std::env::var(key).ok()))
    }

    pub fn from_map(get: impl Fn(&str) -> Option<String>) -> Policy {
        let mut blocked: Vec<Blocked> = Vec::new();
        for entry in get("IPHONE_USE_BLOCKED_APPS")
            .unwrap_or_default()
            .split([',', '，', '\n'])
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            match resolve(entry) {
                Some(app) if !blocked.iter().any(|b| b.bundle == app.bundle) => blocked.push(app),
                Some(_) => {}
                None => tracing::warn!(
                    "IPHONE_USE_BLOCKED_APPS: {entry:?} is neither a bundle id nor a known app name; ignored"
                ),
            }
        }
        let max_actions = get("IPHONE_USE_MAX_ACTIONS")
            .and_then(|raw| raw.trim().parse::<u32>().ok())
            .filter(|max| *max > 0);
        Policy {
            blocked,
            max_actions,
        }
    }

    pub fn is_active(&self) -> bool {
        !self.blocked.is_empty() || self.max_actions.is_some()
    }

    fn blocked_bundle(&self, bundle: &str) -> Option<&Blocked> {
        self.blocked
            .iter()
            .find(|b| b.bundle.eq_ignore_ascii_case(bundle))
    }

    /// A `launch_app` of a blocked app (after `app` names are resolved to
    /// bundles), checked without touching the phone.
    pub fn check_launch(&self, action: &Value) -> Result<(), Refusal> {
        match launch_target(action).and_then(|bundle| self.blocked_bundle(&bundle)) {
            Some(app) => Err(Refusal::BlockedLaunch(app.name.clone())),
            None => Ok(()),
        }
    }

    /// Whether the foreground must be read before `action`: there is a
    /// blocklist and the action does not leave the current app.
    pub fn needs_foreground(&self, action: &Value) -> bool {
        !self.blocked.is_empty() && !leaves_app(action)
    }

    /// `active`: the bundles iOS reports as active (the foreground app, plus
    /// SpringBoard under a banner).
    pub fn check_foreground(&self, active: &[String]) -> Result<(), Refusal> {
        match active.iter().find_map(|bundle| self.blocked_bundle(bundle)) {
            Some(app) => Err(Refusal::BlockedForeground(app.name.clone())),
            None => Ok(()),
        }
    }
}

/// Bundle id, Apple app name, or catalog name/alias, exactly.
fn resolve(entry: &str) -> Option<Blocked> {
    if let Some(bundle) = system_app_bundle(entry) {
        return Some(Blocked {
            bundle: bundle.to_string(),
            name: entry.to_string(),
        });
    }
    let wanted = normalize(entry);
    if let Some(app) = catalog().iter().find(|app| {
        std::iter::once(&app.name)
            .chain(app.aliases.iter())
            .chain(std::iter::once(&app.bundle_id))
            .any(|name| normalize(name) == wanted)
    }) {
        return Some(Blocked {
            bundle: app.bundle_id.clone(),
            name: app.name.clone(),
        });
    }
    valid_bundle(entry).then(|| Blocked {
        bundle: entry.to_string(),
        name: entry.to_string(),
    })
}

fn launch_target(action: &Value) -> Option<String> {
    if action.get("type").and_then(Value::as_str) != Some("launch_app") {
        return None;
    }
    action
        .get("bundle")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            action
                .get("name")
                .and_then(Value::as_str)
                .and_then(system_app_bundle)
                .map(str::to_string)
        })
}

/// Actions that only leave the app in front.
fn leaves_app(action: &Value) -> bool {
    match action.get("type").and_then(Value::as_str) {
        Some("home" | "launch_app") => true,
        Some("shortcut") => action.get("name").and_then(Value::as_str) == Some("home"),
        _ => false,
    }
}

#[derive(Debug, Clone, Copy)]
struct Spent {
    used: u32,
    last_ms: u64,
}

fn budgets() -> &'static Mutex<HashMap<String, Spent>> {
    static BUDGETS: OnceLock<Mutex<HashMap<String, Spent>>> = OnceLock::new();
    BUDGETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Charge `wanted` actions to `owner`'s run, or refuse with nothing charged.
/// Charged before the actions run: an action that fails still counts.
pub fn charge(policy: &Policy, owner: &str, wanted: u32, now_ms: u64) -> Result<(), Refusal> {
    let Some(max) = policy.max_actions else {
        return Ok(());
    };
    let mut map = budgets()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.len() >= MAX_OWNERS && !map.contains_key(owner) {
        map.clear();
    }
    let spent = map.entry(owner.to_string()).or_insert(Spent {
        used: 0,
        last_ms: now_ms,
    });
    if now_ms.saturating_sub(spent.last_ms) > crate::metrics::IDLE_GAP_MS {
        spent.used = 0;
    }
    if spent.used.saturating_add(wanted) > max {
        return Err(Refusal::Budget {
            max,
            used: spent.used,
            wanted,
        });
    }
    spent.used += wanted;
    spent.last_ms = now_ms;
    Ok(())
}

/// A new run for `owner`: its budget starts over.
pub fn reset(owner: &str) {
    budgets()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(owner);
}

/// Used/max for `owner`, for status displays; `None` without a budget.
pub fn budget_status(policy: &Policy, owner: &str, now_ms: u64) -> Option<Value> {
    let max = policy.max_actions?;
    let used = budgets()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(owner)
        .filter(|spent| now_ms.saturating_sub(spent.last_ms) <= crate::metrics::IDLE_GAP_MS)
        .map_or(0, |spent| spent.used);
    Some(serde_json::json!({"max": max, "used": used}))
}

/// The policy as `/agent/capabilities` reports it.
pub fn describe(policy: &Policy) -> Value {
    serde_json::json!({
        "blocked_apps": policy
            .blocked
            .iter()
            .map(|b| serde_json::json!({"bundle": b.bundle, "name": b.name}))
            .collect::<Vec<_>>(),
        "max_actions_per_run": policy.max_actions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(blocked: &str, max: &str) -> Policy {
        Policy::from_map(|key| match key {
            "IPHONE_USE_BLOCKED_APPS" => Some(blocked.to_string()),
            "IPHONE_USE_MAX_ACTIONS" => Some(max.to_string()),
            _ => None,
        })
    }

    #[test]
    fn default_policy_is_off() {
        let p = Policy::from_map(|_| None);
        assert!(!p.is_active());
        assert!(p
            .check_launch(&json!({"type":"launch_app","bundle":"com.apple.Passbook"}))
            .is_ok());
        assert!(!p.needs_foreground(&json!({"type":"tap","x":1,"y":1})));
        assert!(charge(&p, "default-off", 10_000, 0).is_ok());
    }

    #[test]
    fn entries_resolve_by_bundle_apple_name_and_catalog_name() {
        let p = policy("com.example.bank, Wallet，微信, not a thing", "0");
        let bundles: Vec<_> = p.blocked.iter().map(|b| b.bundle.as_str()).collect();
        assert_eq!(bundles[0], "com.example.bank");
        assert_eq!(bundles[1], system_app_bundle("Wallet").unwrap());
        assert_eq!(bundles[2], "com.tencent.xin");
        assert_eq!(bundles.len(), 3, "unknown names are dropped: {bundles:?}");
        assert_eq!(p.max_actions, None, "0 means no budget");
    }

    #[test]
    fn launch_of_a_blocked_app_is_refused_by_bundle_or_system_name() {
        let p = policy("Wallet", "");
        let wallet = system_app_bundle("Wallet").unwrap();
        let refused = p.check_launch(&json!({"type":"launch_app","bundle":wallet}));
        assert!(matches!(refused, Err(Refusal::BlockedLaunch(ref name)) if name == "Wallet"));
        assert!(p
            .check_launch(&json!({"type":"launch_app","name":"Wallet"}))
            .is_err());
        assert!(p
            .check_launch(&json!({"type":"launch_app","bundle":"com.apple.Preferences"}))
            .is_ok());
        assert!(p.check_launch(&json!({"type":"tap","x":1,"y":1})).is_ok());
    }

    #[test]
    fn inside_a_blocked_app_only_leaving_is_allowed() {
        let p = policy("com.example.bank", "");
        assert!(p.needs_foreground(&json!({"type":"tap","x":1,"y":1})));
        assert!(p.needs_foreground(&json!({"type":"shortcut","name":"spotlight"})));
        assert!(!p.needs_foreground(&json!({"type":"home"})));
        assert!(!p.needs_foreground(&json!({"type":"shortcut","name":"home"})));
        assert!(!p.needs_foreground(&json!({"type":"launch_app","bundle":"com.apple.Preferences"})));
        let refused =
            p.check_foreground(&["com.apple.springboard".into(), "com.example.bank".into()]);
        assert_eq!(refused.unwrap_err().error(), "blocked_app");
        assert!(p
            .check_foreground(&["com.apple.Preferences".into()])
            .is_ok());
    }

    #[test]
    fn budget_refuses_past_the_limit_and_restarts_after_idle_or_reset() {
        let p = policy("", "5");
        let owner = "budget-test";
        assert!(charge(&p, owner, 3, 1_000).is_ok());
        let refused = charge(&p, owner, 3, 2_000).unwrap_err();
        assert_eq!(
            refused,
            Refusal::Budget {
                max: 5,
                used: 3,
                wanted: 3
            }
        );
        assert_eq!(refused.to_json()["outcome"], "not_sent");
        assert!(charge(&p, owner, 2, 3_000).is_ok());
        assert!(charge(&p, owner, 1, 4_000).is_err());
        assert_eq!(budget_status(&p, owner, 4_000).unwrap()["used"], 5);
        // A long pause is a new run.
        assert!(charge(&p, owner, 5, 4_000 + crate::metrics::IDLE_GAP_MS).is_ok());
        reset(owner);
        assert_eq!(budget_status(&p, owner, 0).unwrap()["used"], 0);
        assert!(charge(&p, owner, 5, 10).is_ok());
    }

    #[test]
    fn budgets_are_per_owner() {
        let p = policy("", "1");
        assert!(charge(&p, "owner-a-per", 1, 0).is_ok());
        assert!(charge(&p, "owner-b-per", 1, 0).is_ok());
        assert!(charge(&p, "owner-a-per", 1, 0).is_err());
    }
}
