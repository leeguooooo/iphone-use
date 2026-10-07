//! The phone owner lease, from setup's side.
//!
//! The daemon leases the phone to one session at a time (`X-Phone-Owner`,
//! 300 s, renewed by its requests). Setup used to ignore it, so one session's
//! `setup`, or a supervisor rebuild, could replace the runner another
//! session was driving. Now an interactive setup refuses while another
//! session holds the phone, and the supervisor never replaces a live runner
//! under someone else's lease (recovering a dead one is fine: nobody can be
//! using it).
//!
//! The caller is "that owner" when `PHONE_REMOTE_OWNER` names the lease
//! holder; `--force` (`IPHONE_USE_SETUP_FORCE=1`) overrides on purpose. The
//! refusal never names either: a model reading it would copy the switch
//! instead of waiting (chrome-use #433). They are documented in --help only.

use std::time::Duration;

use super::ctx::Ctx;
use super::sys;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub owner: String,
    pub remaining_secs: u64,
}

/// The lease this instance's daemon reports, if any. `None` too when the
/// daemon cannot be asked (not installed, not running): then nobody can be
/// driving the phone through it.
pub fn current(ctx: &Ctx) -> Option<Lease> {
    if !ctx.daemon_plist.is_file() {
        return None;
    }
    let port = Some(sys::plist_env(&ctx.daemon_plist, "PHONE_REMOTE_PORT"))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "44321".into());
    let mut token = sys::plist_env(&ctx.daemon_plist, "PHONE_REMOTE_AGENT_TOKEN");
    if token.is_empty() {
        token = sys::plist_env(&ctx.daemon_plist, "PHONE_REMOTE_PASSWORD");
    }
    let url = format!("http://127.0.0.1:{port}/agent/status");
    let reply = if token.is_empty() {
        sys::http_get(&url, Duration::from_secs(3))
    } else {
        sys::http_get_auth(&url, Duration::from_secs(3), &token)
    };
    let status: serde_json::Value =
        reply.and_then(|(_, body)| serde_json::from_slice(&body).ok())?;
    lease_in(&status)
}

pub fn lease_in(status: &serde_json::Value) -> Option<Lease> {
    let owner = status.get("owner")?.as_str()?.trim();
    if owner.is_empty() {
        return None;
    }
    Some(Lease {
        owner: owner.to_string(),
        remaining_secs: status
            .get("owner_lease_remaining_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

/// Who is asking (`PHONE_REMOTE_OWNER`), if anyone said.
pub fn caller() -> Option<String> {
    std::env::var("PHONE_REMOTE_OWNER")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// A lease held by a session other than the caller.
pub fn foreign(lease: Option<Lease>, caller: Option<&str>) -> Option<Lease> {
    lease.filter(|lease| Some(lease.owner.as_str()) != caller)
}

pub fn forced() -> bool {
    std::env::var("IPHONE_USE_SETUP_FORCE").is_ok_and(|v| v == "1")
}

/// How long a forced takeover also covers the supervisor that the
/// interactive setup hands its runner to.
const OVERRIDE_SECS: u64 = 300;

fn override_file(ctx: &Ctx) -> std::path::PathBuf {
    ctx.state_dir().join(".owner-override")
}

/// `--force` reaches the supervisor too: an interactive setup that took the
/// phone over on purpose leaves a short-lived marker, so the supervisor it
/// hands off to may replace the runner under the same lease.
fn grant_override(ctx: &Ctx) {
    let until = super::retry::now() + OVERRIDE_SECS;
    let _ = sys::write_atomic(&override_file(ctx), format!("{until}\n").as_bytes(), 0o600);
}

pub fn override_active(ctx: &Ctx) -> bool {
    let file = override_file(ctx);
    sys::marker_file_secure(&file)
        && std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .is_some_and(|until| until > super::retry::now())
}

/// A forced run, or one inside a forced takeover's window.
pub fn overridden(ctx: &Ctx) -> bool {
    forced() || override_active(ctx)
}

pub fn refusal(lease: &Lease, ctx: &Ctx) -> String {
    let _ = ctx;
    format!(
        "the iPhone is being driven by session \"{}\" (owner lease, {}s left); setup would replace the runner it is using. Wait until that session releases the phone, or ask it (or the person running it) to release it, then run setup again.",
        lease.owner, lease.remaining_secs,
    )
}

/// `setup-native owner-check`: exit 1 with the refusal when an interactive
/// setup would take the phone from another session.
pub fn check(ctx: &Ctx) -> i32 {
    if forced() {
        grant_override(ctx);
        return 0;
    }
    let lease = current(ctx);
    match foreign(lease.clone(), caller().as_deref()) {
        Some(lease) => {
            super::term::die_line(&refusal(&lease, ctx));
            1
        }
        None => {
            // The lease holder itself is setting up: its supervisor may
            // replace the runner under that lease too.
            if lease.is_some() {
                grant_override(ctx);
            }
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leases_from_status() {
        let held = serde_json::json!({"owner": "h264-hw", "owner_lease_remaining_secs": 212});
        assert_eq!(
            lease_in(&held),
            Some(Lease {
                owner: "h264-hw".into(),
                remaining_secs: 212
            })
        );
        assert_eq!(lease_in(&serde_json::json!({"owner": null})), None);
        assert_eq!(lease_in(&serde_json::json!({"owner": ""})), None);
        assert_eq!(lease_in(&serde_json::json!({})), None);
    }

    #[test]
    fn only_another_sessions_lease_is_foreign() {
        let lease = Some(Lease {
            owner: "tap-loop".into(),
            remaining_secs: 30,
        });
        assert!(foreign(lease.clone(), None).is_some());
        assert!(foreign(lease.clone(), Some("setup-x")).is_some());
        assert!(
            foreign(lease, Some("tap-loop")).is_none(),
            "the owner may rebuild its own phone"
        );
        assert!(foreign(None, Some("x")).is_none());
    }

    #[test]
    fn a_forced_takeover_covers_the_handoff_for_a_while() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = crate::setup::ctx::tests_support::ctx();
        ctx.instance.state_dir = dir.path().to_path_buf();
        assert!(!override_active(&ctx));
        grant_override(&ctx);
        assert!(override_active(&ctx));
        std::fs::write(override_file(&ctx), "1\n").unwrap();
        assert!(!override_active(&ctx), "an expired override grants nothing");
    }

    #[test]
    fn the_refusal_names_owner_lease_and_ways_out() {
        let ctx = crate::setup::ctx::tests_support::ctx();
        let text = refusal(
            &Lease {
                owner: "prewarm-ab".into(),
                remaining_secs: 120,
            },
            &ctx,
        );
        assert!(text.contains("\"prewarm-ab\""), "{text}");
        assert!(text.contains("120s left"), "{text}");
        assert!(text.contains("release"), "{text}");
        // No bypass in the refusal: a model would copy it instead of waiting.
        for bypass in ["--force", "PHONE_REMOTE_OWNER", "IPHONE_USE_SETUP_FORCE"] {
            assert!(!text.contains(bypass), "{text}");
        }
    }
}
