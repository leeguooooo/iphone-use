//! Owner-set limits on what a driving agent may do, enforced by the daemon so
//! they hold whatever the agent's own prompt says.
//!
//! - `IPHONE_USE_BLOCKED_APPS`: comma-separated bundle ids or app names
//!   ("Wallet, com.apple.Passwords"). An agent cannot open them, and while one
//!   is in front it can only leave (`home`, `launch_app` of another app).
//! - `IPHONE_USE_MAX_ACTIONS`: actions per run and caller. A run is the span
//!   between `POST /agent/run {"action":"start"}` calls, or, without them, one
//!   that ends after [`crate::metrics::IDLE_GAP_MS`] without an action.
//! - `IPHONE_USE_ALLOW_URL_SCHEMES`: comma-separated URL schemes an
//!   `open_url` action may open besides `http`/`https` ("tel, weixin", or
//!   `*` for any). Without it every other scheme is refused: `tel:` dials,
//!   `sms:`/`mailto:` compose, and a custom scheme drives another app.
//!
//! The first two are off by default. A refusal is `403` with `outcome: not_sent`: no
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
    /// Lowercased schemes `open_url` may open besides `http`/`https`.
    pub allowed_url_schemes: Vec<String>,
    /// `IPHONE_USE_ALLOW_URL_SCHEMES=*`: any scheme.
    pub allow_any_url_scheme: bool,
}

/// Longest URL `open_url` accepts, in bytes.
pub const MAX_URL_LEN: usize = 2048;

/// Schemes `open_url` always opens: they only show a page.
const ALWAYS_ALLOWED_SCHEMES: &[&str] = &["http", "https"];

/// The scheme of an `open_url` URL (lowercased) and its host, if it has one.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedUrl {
    pub scheme: String,
    pub host: Option<String>,
}

/// Parse `url` far enough to apply the scheme policy: an RFC 3986 scheme,
/// no whitespace or control characters, at most [`MAX_URL_LEN`] bytes, and a
/// host for `http`/`https`. The error says what is wrong.
pub fn parse_url(url: &str) -> Result<ParsedUrl, &'static str> {
    if url.is_empty() || url.len() > MAX_URL_LEN {
        return Err("url must be 1 to 2048 bytes");
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("url must not contain spaces or control characters (percent-encode them)");
    }
    let Some((scheme, rest)) = url.split_once(':') else {
        return Err("url needs a scheme, such as https://example.com");
    };
    let mut chars = scheme.chars();
    let valid_scheme = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !valid_scheme {
        return Err("url needs a scheme, such as https://example.com");
    }
    let scheme = scheme.to_ascii_lowercase();
    let host = rest.strip_prefix("//").and_then(|authority| {
        let authority = authority.split(['/', '?', '#']).next().unwrap_or_default();
        let host = authority.rsplit('@').next().unwrap_or_default();
        let host = if let Some(bracketed) = host.strip_prefix('[') {
            bracketed.split(']').next().unwrap_or_default()
        } else {
            host.split(':').next().unwrap_or_default()
        };
        (!host.is_empty()).then(|| host.to_ascii_lowercase())
    });
    if ALWAYS_ALLOWED_SCHEMES.contains(&scheme.as_str()) && host.is_none() {
        return Err("an http(s) url needs a host, such as https://example.com/path");
    }
    Ok(ParsedUrl { scheme, host })
}

/// The `url` of an `open_url` action, if `action` is one.
pub fn open_url_target(action: &Value) -> Option<&str> {
    if action.get("type").and_then(Value::as_str) != Some("open_url") {
        return None;
    }
    action.get("url").and_then(Value::as_str)
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
    /// `open_url` of a scheme the owner has not allowed.
    UrlScheme(String),
    /// An app install whose bundle id could not be read while the owner
    /// blocks some apps: it cannot be ruled out.
    InstallUnverified,
}

impl Refusal {
    pub fn error(&self) -> &'static str {
        match self {
            Refusal::BlockedLaunch(_) | Refusal::BlockedForeground(_) => "blocked_app",
            Refusal::ForegroundUnknown => "foreground_unverified",
            Refusal::Budget { .. } => "action_budget_exhausted",
            Refusal::UrlScheme(_) => "url_scheme_not_allowed",
            Refusal::InstallUnverified => "install_unverified",
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
            Refusal::UrlScheme(scheme) => format!(
                "{scheme}: links are not opened unless the phone owner allows the scheme (IPHONE_USE_ALLOW_URL_SCHEMES, e.g. IPHONE_USE_ALLOW_URL_SCHEMES={scheme}). http and https links always open. Do not reach it another way; tell the user."
            ),
            Refusal::InstallUnverified => "the app's bundle id could not be read from the .app/.ipa, and the phone owner blocks some apps (IPHONE_USE_BLOCKED_APPS), so it is not installed. Tell the user.".to_string(),
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
            Refusal::UrlScheme(scheme) => {
                body["scheme"] = Value::String(scheme.clone());
            }
            Refusal::ForegroundUnknown | Refusal::InstallUnverified => {}
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
        let mut allowed_url_schemes: Vec<String> = Vec::new();
        let mut allow_any_url_scheme = false;
        for entry in get("IPHONE_USE_ALLOW_URL_SCHEMES")
            .unwrap_or_default()
            .split([',', '，', '\n'])
            .map(|entry| entry.trim().trim_end_matches("://").trim_end_matches(':'))
            .filter(|entry| !entry.is_empty())
        {
            if entry == "*" {
                allow_any_url_scheme = true;
                continue;
            }
            let scheme = entry.to_ascii_lowercase();
            if !allowed_url_schemes.contains(&scheme) {
                allowed_url_schemes.push(scheme);
            }
        }
        Policy {
            blocked,
            max_actions,
            allowed_url_schemes,
            allow_any_url_scheme,
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

    /// An `open_url` whose scheme the owner has not allowed, checked without
    /// touching the phone. Applies with or without the other limits: `http`
    /// and `https` always pass, every other scheme only when listed.
    /// A URL that does not parse is left to the action's validation.
    pub fn check_url(&self, action: &Value) -> Result<(), Refusal> {
        let Some(Ok(parsed)) = open_url_target(action).map(parse_url) else {
            return Ok(());
        };
        if self.allow_any_url_scheme
            || ALWAYS_ALLOWED_SCHEMES.contains(&parsed.scheme.as_str())
            || self.allowed_url_schemes.contains(&parsed.scheme)
        {
            return Ok(());
        }
        Err(Refusal::UrlScheme(parsed.scheme))
    }

    /// An app install of `bundle` (`None`: its bundle id could not be read).
    /// A blocked app is the owner's to put on the phone, not an agent's; an
    /// unreadable bundle cannot be ruled out while anything is blocked.
    pub fn check_install(&self, bundle: Option<&str>) -> Result<(), Refusal> {
        match bundle {
            Some(bundle) => match self.blocked_bundle(bundle) {
                Some(app) => Err(Refusal::BlockedLaunch(app.name.clone())),
                None => Ok(()),
            },
            None if !self.blocked.is_empty() => Err(Refusal::InstallUnverified),
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

/// Actions that only leave the app in front. `open_url` hands the link to
/// another app (Safari, or the scheme's owner), so it leaves too.
fn leaves_app(action: &Value) -> bool {
    match action.get("type").and_then(Value::as_str) {
        Some("home" | "launch_app" | "open_url") => true,
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
        "open_url_schemes": if policy.allow_any_url_scheme {
            serde_json::json!("*")
        } else {
            serde_json::json!(ALWAYS_ALLOWED_SCHEMES
                .iter()
                .map(|s| s.to_string())
                .chain(policy.allowed_url_schemes.iter().cloned())
                .collect::<Vec<_>>())
        },
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

    #[test]
    fn urls_parse_to_a_scheme_and_host() {
        let parsed = parse_url("HTTPS://user:pw@Example.COM:8443/a?q=secret#x").unwrap();
        assert_eq!(parsed.scheme, "https");
        assert_eq!(parsed.host.as_deref(), Some("example.com"));
        assert_eq!(parse_url("tel:+15551234").unwrap().host, None);
        assert_eq!(
            parse_url("weixin://dl/scan").unwrap().host.as_deref(),
            Some("dl")
        );
        assert_eq!(
            parse_url("http://[::1]:80/").unwrap().host.as_deref(),
            Some("::1")
        );
        assert_eq!(parse_url("prefs:root=WIFI").unwrap().scheme, "prefs");
        for bad in [
            "",
            "example.com",
            "://x",
            "1http://x",
            "https://",
            "https:///path",
            "https://a b.com",
            "https://a.com/\n",
        ] {
            assert!(parse_url(bad).is_err(), "{bad:?}");
        }
        assert!(parse_url(&format!("https://a.com/{}", "x".repeat(MAX_URL_LEN))).is_err());
    }

    #[test]
    fn only_web_links_open_unless_the_owner_allows_the_scheme() {
        let open = |url: &str| json!({"type":"open_url","url":url});
        let p = Policy::from_map(|_| None);
        assert!(p.check_url(&open("https://example.com")).is_ok());
        assert!(p.check_url(&open("http://example.com/x")).is_ok());
        for url in [
            "tel:10086",
            "sms:10086",
            "facetime://a@b.c",
            "facetime-audio:1",
            "mailto:a@b.c",
            "itms-services://?action=download-manifest",
            "weixin://dl/scan",
            "prefs:root=WIFI",
            "TEL:1",
        ] {
            let refused = p.check_url(&open(url)).unwrap_err();
            assert_eq!(refused.error(), "url_scheme_not_allowed", "{url}");
            let body = refused.to_json();
            assert_eq!(body["outcome"], "not_sent");
            assert!(body["hint"]
                .as_str()
                .unwrap()
                .contains("IPHONE_USE_ALLOW_URL_SCHEMES"));
        }
        assert!(p.check_url(&json!({"type":"tap","x":1,"y":1})).is_ok());
        assert!(
            !p.is_active(),
            "the scheme policy alone does not switch the other limits on"
        );

        let some = Policy::from_map(|key| {
            (key == "IPHONE_USE_ALLOW_URL_SCHEMES").then(|| "Weixin://, tel:".to_string())
        });
        assert!(some.check_url(&open("weixin://dl/scan")).is_ok());
        assert!(some.check_url(&open("tel:10086")).is_ok());
        assert!(some.check_url(&open("sms:10086")).is_err());
        let any = Policy::from_map(|key| {
            (key == "IPHONE_USE_ALLOW_URL_SCHEMES").then(|| " * ".to_string())
        });
        assert!(any.check_url(&open("sms:10086")).is_ok());
        assert_eq!(describe(&any)["open_url_schemes"], "*");
        assert_eq!(
            describe(&some)["open_url_schemes"],
            json!(["http", "https", "weixin", "tel"])
        );
    }

    #[test]
    fn open_url_leaves_a_blocked_app() {
        let p = policy("com.example.bank", "");
        assert!(!p.needs_foreground(&json!({"type":"open_url","url":"https://example.com"})));
    }

    #[test]
    fn installs_of_blocked_or_unreadable_bundles_are_refused_only_with_a_blocklist() {
        let p = policy("com.example.bank", "");
        let refused = p.check_install(Some("COM.example.bank")).unwrap_err();
        assert_eq!(refused.error(), "blocked_app");
        assert!(p.check_install(Some("com.example.notes")).is_ok());
        let unknown = p.check_install(None).unwrap_err();
        assert_eq!(unknown.to_json()["error"], "install_unverified");
        assert!(Policy::from_map(|_| None).check_install(None).is_ok());
    }
}
