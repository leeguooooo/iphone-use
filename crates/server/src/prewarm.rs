//! Pre-warm: start bringing an idle-released phone back before an agent
//! needs it.
//!
//! After an idle release the next action pays the whole runner bring-up:
//! ~5 s with the screen on, 10–25 s once iOS has turned the screen off,
//! because `xcodebuild` has to wake it first. That wait cannot be removed
//! (iOS 27 only starts XCTest through CoreDevice), but it can be started
//! earlier — when an MCP session starts, when the model reads status, or when
//! a session takes the owner lease — so it has finished by the time the first
//! action arrives.
//!
//! The risk is the passcode prompt: every runner launch can make iOS ask for
//! the passcode to enable UI automation, so a runner launched for nothing
//! costs the person holding the phone a prompt (the v0.7.4 lesson). Hence the
//! policy below: only for a phone an agent drove recently (except an explicit
//! lease), never on a locked phone or one whose lock state cannot be read,
//! never against a hand-off, a blocker or another session, and at most once
//! per interval per trigger.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What asked for the pre-warm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// An MCP server started (Claude Code starts one per session, used or not).
    SessionStart,
    /// The model read phone status through MCP.
    Status,
    /// A named session took the owner lease on a released phone.
    Lease,
}

impl Trigger {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "session_start" => Some(Self::SessionStart),
            "status" => Some(Self::Status),
            "lease" => Some(Self::Lease),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::Status => "status",
            Self::Lease => "lease",
        }
    }

    /// A lease is an explicit claim on the phone; the other triggers happen
    /// whether or not anyone means to use it, so they need recent use.
    fn needs_recent_activity(self) -> bool {
        !matches!(self, Self::Lease)
    }
}

/// Why a pre-warm did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    Disabled,
    NotManaged,
    NotReleased,
    HumanHandoff,
    LifecycleBusy,
    OwnedByOther,
    Blocked(String),
    RateLimited(u64),
    NoRecentActivity,
    Locked,
    LockUnknown,
}

impl Skip {
    pub fn reason(&self) -> String {
        match self {
            Self::Disabled => "disabled".into(),
            Self::NotManaged => "not_managed".into(),
            Self::NotReleased => "not_released".into(),
            Self::HumanHandoff => "human_handoff".into(),
            Self::LifecycleBusy => "lifecycle_busy".into(),
            Self::OwnedByOther => "owned_by_other".into(),
            Self::Blocked(blocker) => format!("blocked:{blocker}"),
            Self::RateLimited(_) => "rate_limited".into(),
            Self::NoRecentActivity => "no_recent_activity".into(),
            Self::Locked => "locked".into(),
            Self::LockUnknown => "lock_unknown".into(),
        }
    }
}

/// Everything the gate looks at, sampled by the caller.
#[derive(Debug, Clone)]
pub struct Inputs<'a> {
    pub trigger: Trigger,
    pub managed: bool,
    pub released: bool,
    pub human_handoff: bool,
    pub lifecycle_busy: bool,
    pub other_owner: bool,
    /// The setup helper's current blocker (`""` when none).
    pub blocker: &'a str,
    /// Since this trigger last started a pre-warm.
    pub since_last_start: Option<Duration>,
    /// Since an agent last drove the phone in this daemon process; `None`
    /// when it has not yet (a restart is not use).
    pub since_activity: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub enabled: bool,
    /// At most one started pre-warm per trigger per this interval.
    pub min_interval: Duration,
    /// Automatic triggers only for a phone an agent drove within this window.
    pub recent: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: true,
            min_interval: Duration::from_secs(600),
            recent: Duration::from_secs(3600),
        }
    }
}

fn env_secs(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok())
}

impl Policy {
    /// `PHONE_REMOTE_PREWARM=0` turns pre-warm off;
    /// `PHONE_REMOTE_PREWARM_INTERVAL_SECS` and
    /// `PHONE_REMOTE_PREWARM_RECENT_SECS` tune the limits.
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            enabled: std::env::var("PHONE_REMOTE_PREWARM").map_or(true, |v| v.trim() != "0"),
            min_interval: env_secs("PHONE_REMOTE_PREWARM_INTERVAL_SECS")
                .map_or(default.min_interval, Duration::from_secs),
            recent: env_secs("PHONE_REMOTE_PREWARM_RECENT_SECS")
                .map_or(default.recent, Duration::from_secs),
        }
    }

    /// Everything except the lock state, which costs a device round trip and
    /// is only read once this passes.
    pub fn check(&self, i: &Inputs<'_>) -> Result<(), Skip> {
        if !self.enabled {
            return Err(Skip::Disabled);
        }
        if !i.managed {
            return Err(Skip::NotManaged);
        }
        if !i.released {
            return Err(Skip::NotReleased);
        }
        if i.human_handoff {
            return Err(Skip::HumanHandoff);
        }
        if i.lifecycle_busy {
            return Err(Skip::LifecycleBusy);
        }
        if i.other_owner {
            return Err(Skip::OwnedByOther);
        }
        // A blocker the helper is already backing off on (locked, trust,
        // xcode_too_old, …) is not fixed by another launch.
        if !i.blocker.is_empty() {
            return Err(Skip::Blocked(i.blocker.to_string()));
        }
        if let Some(since) = i.since_last_start {
            if since < self.min_interval {
                return Err(Skip::RateLimited(
                    self.min_interval.saturating_sub(since).as_secs().max(1),
                ));
            }
        }
        if i.trigger.needs_recent_activity()
            && i.since_activity.is_none_or(|since| since > self.recent)
        {
            return Err(Skip::NoRecentActivity);
        }
        Ok(())
    }
}

/// `Some(true)` = passcode required (locked), `Some(false)` = unlocked,
/// `None` = could not tell. Only a clear "unlocked" may launch.
pub fn lock_gate(passcode_required: Option<bool>) -> Result<(), Skip> {
    match passcode_required {
        Some(false) => Ok(()),
        Some(true) => Err(Skip::Locked),
        None => Err(Skip::LockUnknown),
    }
}

// ---------------------------------------------------------------------------
// Process state (one daemon drives one phone)
// ---------------------------------------------------------------------------

static LAST_START: Mutex<Option<HashMap<Trigger, Instant>>> = Mutex::new(None);
/// When an agent last drove the phone. Deliberately not the daemon's idle
/// clock, which a fresh process starts at "now": after a reboot or upgrade a
/// phone nobody has touched for days must not count as recently used.
static LAST_DRIVEN: Mutex<Option<Instant>> = Mutex::new(None);

pub fn note_driven() {
    *LAST_DRIVEN.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
}

pub fn since_driven() -> Option<Duration> {
    LAST_DRIVEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|at| at.elapsed())
}
static WARMING: AtomicBool = AtomicBool::new(false);

fn lock_last_start() -> std::sync::MutexGuard<'static, Option<HashMap<Trigger, Instant>>> {
    LAST_START.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn since_last_start(trigger: Trigger) -> Option<Duration> {
    lock_last_start()
        .as_ref()
        .and_then(|map| map.get(&trigger))
        .map(Instant::elapsed)
}

pub fn note_start(trigger: Trigger) {
    lock_last_start()
        .get_or_insert_with(HashMap::new)
        .insert(trigger, Instant::now());
}

/// True while a reconnect that a pre-warm started is still coming up.
pub fn warming() -> bool {
    WARMING.load(Ordering::Acquire)
}

pub fn set_warming(on: bool) {
    WARMING.store(on, Ordering::Release);
}

/// Ask CoreDevice whether the phone needs its passcode, like setup does
/// before launching the runner (`passcodeRequired` flips the moment the phone
/// locks or unlocks). Bounded at `timeout`; any failure reads as unknown.
pub fn device_passcode_required(udid: &str, timeout: Duration) -> Option<bool> {
    if udid.is_empty() || !udid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return None;
    }
    let out = std::env::temp_dir().join(format!(
        "iphone-use-prewarm-lock-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let mut child = std::process::Command::new("xcrun")
        .args(["devicectl", "device", "info", "lockState", "--device", udid, "-j"])
        .arg(&out)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    let finished = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let text = std::fs::read_to_string(&out).ok();
    let _ = std::fs::remove_file(&out);
    if !finished {
        return None;
    }
    parse_lock_state(&text?)
}

/// `devicectl device info lockState -j` → `result.passcodeRequired`.
pub fn parse_lock_state(json: &str) -> Option<bool> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()?
        .get("result")?
        .get("passcodeRequired")?
        .as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(trigger: Trigger) -> Inputs<'static> {
        Inputs {
            trigger,
            managed: true,
            released: true,
            human_handoff: false,
            lifecycle_busy: false,
            other_owner: false,
            blocker: "",
            since_last_start: None,
            since_activity: Some(Duration::from_secs(400)),
        }
    }

    #[test]
    fn a_recently_used_released_phone_is_warmed() {
        assert_eq!(Policy::default().check(&inputs(Trigger::Status)), Ok(()));
        assert_eq!(Policy::default().check(&inputs(Trigger::SessionStart)), Ok(()));
    }

    #[test]
    fn nothing_to_do_or_not_ours_to_do_is_skipped() {
        let policy = Policy::default();
        let mut i = inputs(Trigger::Status);
        i.released = false;
        assert_eq!(policy.check(&i), Err(Skip::NotReleased));
        let mut i = inputs(Trigger::Status);
        i.managed = false;
        assert_eq!(policy.check(&i), Err(Skip::NotManaged));
        let mut i = inputs(Trigger::Status);
        i.human_handoff = true;
        assert_eq!(policy.check(&i), Err(Skip::HumanHandoff));
        let mut i = inputs(Trigger::Status);
        i.lifecycle_busy = true;
        assert_eq!(policy.check(&i), Err(Skip::LifecycleBusy));
        let mut i = inputs(Trigger::Lease);
        i.other_owner = true;
        assert_eq!(policy.check(&i), Err(Skip::OwnedByOther));
        let disabled = Policy { enabled: false, ..Policy::default() };
        assert_eq!(disabled.check(&inputs(Trigger::Lease)), Err(Skip::Disabled));
    }

    #[test]
    fn a_blocker_the_helper_is_backing_off_on_is_left_alone() {
        let mut i = inputs(Trigger::Lease);
        i.blocker = "xcode_too_old";
        assert_eq!(
            Policy::default().check(&i),
            Err(Skip::Blocked("xcode_too_old".into()))
        );
        assert_eq!(Skip::Blocked("locked".into()).reason(), "blocked:locked");
    }

    #[test]
    fn each_trigger_starts_at_most_once_per_interval() {
        let policy = Policy::default();
        let mut i = inputs(Trigger::SessionStart);
        i.since_last_start = Some(Duration::from_secs(120));
        assert_eq!(policy.check(&i), Err(Skip::RateLimited(480)));
        i.since_last_start = Some(Duration::from_secs(601));
        assert_eq!(policy.check(&i), Ok(()));
    }

    #[test]
    fn automatic_triggers_need_recent_use_but_a_lease_does_not() {
        let policy = Policy::default();
        let mut i = inputs(Trigger::SessionStart);
        i.since_activity = Some(Duration::from_secs(5 * 3600));
        assert_eq!(policy.check(&i), Err(Skip::NoRecentActivity));
        // A daemon that has not been driven since it started (reboot,
        // upgrade) has no recent use, whatever its idle clock says.
        i.since_activity = None;
        assert_eq!(policy.check(&i), Err(Skip::NoRecentActivity));
        i.trigger = Trigger::Status;
        assert_eq!(policy.check(&i), Err(Skip::NoRecentActivity));
        i.trigger = Trigger::Lease;
        assert_eq!(policy.check(&i), Ok(()));
    }

    #[test]
    fn only_a_clear_unlocked_reading_may_launch() {
        assert_eq!(lock_gate(Some(false)), Ok(()));
        assert_eq!(lock_gate(Some(true)), Err(Skip::Locked));
        assert_eq!(lock_gate(None), Err(Skip::LockUnknown));
    }

    #[test]
    fn lock_state_json_is_read_like_setup_reads_it() {
        assert_eq!(
            parse_lock_state(r#"{"result":{"passcodeRequired":true,"unlockedSinceBoot":true}}"#),
            Some(true)
        );
        assert_eq!(parse_lock_state(r#"{"result":{"passcodeRequired":false}}"#), Some(false));
        assert_eq!(parse_lock_state(r#"{"result":{}}"#), None);
        assert_eq!(parse_lock_state("nope"), None);
        assert_eq!(device_passcode_required("bad udid", Duration::from_secs(1)), None);
    }

    #[test]
    fn triggers_round_trip() {
        for t in [Trigger::SessionStart, Trigger::Status, Trigger::Lease] {
            assert_eq!(Trigger::parse(t.as_str()), Some(t));
        }
        assert_eq!(Trigger::parse("other"), None);
    }
}
