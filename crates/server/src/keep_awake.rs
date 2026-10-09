//! Keep the phone awake while someone drives it.
//!
//! An agent that pauses to think for longer than the phone's Auto-Lock lost
//! the phone: it locked, and with a passcode nobody but its owner could open
//! it again. While the phone is in use ([`AppState::phone_in_use`]) the daemon
//! renews a short keep-awake lease on the native runner (`POST
//! /wda/keepawake`), which presses F13 — a key iOS maps to nothing — every
//! 10 s between commands. The deadline lives on the phone: a daemon that goes
//! quiet, crashes or loses the network lets Auto-Lock take over again within
//! [`LEASE_SECS`]. The runner never acts on a locked phone, so a phone its
//! owner locks by hand stays locked.
//!
//! `PHONE_REMOTE_KEEP_AWAKE_SECS` is how long after the last driving request
//! the phone is kept awake (default [`DEFAULT_WINDOW_SECS`]); `0` turns the
//! feature off. A hold lease, an open live view or a live owner lease keep it
//! awake for as long as they last.
//!
//! Each renewal also reports the lock state and whether a passcode is set;
//! [`lock_report`] hands that to the control path, which unlocks a
//! passcode-less phone before the next action (see `agent_input`).

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::http::AppState;
use crate::wda::{KeepAwakeError, KeepAwakeReport};

/// Seconds after the last driving request the phone stays awake.
pub const DEFAULT_WINDOW_SECS: u64 = 120;
/// How long each renewal keeps the phone awake on its own. Three renewals'
/// worth, so one lost request does not let the phone lock.
pub const LEASE_SECS: u64 = 45;
const RENEW_EVERY: Duration = Duration::from_secs(15);
/// A runner without the route (WebDriverAgent, or an older native runner) is
/// asked again this rarely — it may be replaced by a newer one meanwhile.
const UNSUPPORTED_RETRY: Duration = Duration::from_secs(600);

/// What the runner last said about the lock (see [`LockReport`]).
static LAST_LOCK: AtomicU8 = AtomicU8::new(LockReport::Unknown as u8);

/// The lock state the last keep-awake renewal reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LockReport {
    Unknown = 0,
    Unlocked = 1,
    /// Locked, no passcode: a remote unlock can open it.
    LockedNoPasscode = 2,
    /// Locked behind a passcode: only its owner can open it.
    LockedPasscode = 3,
}

impl LockReport {
    fn from_report(report: &KeepAwakeReport) -> Self {
        match (report.locked, report.passcode) {
            (Some(false), _) => Self::Unlocked,
            (Some(true), Some(false)) => Self::LockedNoPasscode,
            (Some(true), Some(true)) => Self::LockedPasscode,
            _ => Self::Unknown,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Unlocked,
            2 => Self::LockedNoPasscode,
            3 => Self::LockedPasscode,
            _ => Self::Unknown,
        }
    }
}

/// The lock state the last renewal reported.
pub fn lock_report() -> LockReport {
    LockReport::from_u8(LAST_LOCK.load(Ordering::Acquire))
}

/// The control path opened the phone.
pub fn note_unlocked() {
    LAST_LOCK.store(LockReport::Unlocked as u8, Ordering::Release);
}

fn record(report: &KeepAwakeReport) {
    LAST_LOCK.store(LockReport::from_report(report) as u8, Ordering::Release);
    crate::lock_readiness::note_runner_report(report);
}

/// Keep-awake as `/agent/status` reports it (see [`crate::lock_readiness`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepAwakeStatus {
    /// Configured: a device runner is managed and the window is not `0`.
    pub enabled: bool,
    /// The runner has the route; `None` until it was asked.
    pub supported: Option<bool>,
    /// The phone is being kept awake right now.
    pub active: bool,
}

/// 0 = not asked yet, 1 = the runner has the route, 2 = it does not.
static SUPPORTED: AtomicU8 = AtomicU8::new(0);
static ENABLED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// What keep-awake is doing now.
pub fn status() -> KeepAwakeStatus {
    KeepAwakeStatus {
        enabled: ENABLED.load(Ordering::Acquire),
        supported: match SUPPORTED.load(Ordering::Acquire) {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        },
        active: ACTIVE.load(Ordering::Acquire),
    }
}

/// Whether the runner answered the keep-awake route (a read counts too).
pub fn note_supported(supported: bool) {
    SUPPORTED.store(if supported { 1 } else { 2 }, Ordering::Release);
}

/// The keep-awake window from `PHONE_REMOTE_KEEP_AWAKE_SECS`; `None` when it
/// is `0` (off). Unset or unparsable means the default.
pub fn window_from_env(value: Option<&str>) -> Option<Duration> {
    let secs = value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_WINDOW_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Whether this round should ask for a lease: only while the phone is in use
/// and the runner is neither released nor handed to a person.
pub fn wanted(in_use: bool, released: bool, transitioning: bool, handed_off: bool) -> bool {
    in_use && !released && !transitioning && !handed_off
}

/// Start the renewal loop. No-op without a device runner or when switched off.
pub fn spawn(state: Arc<AppState>) {
    let Some(window) = window_from_env(
        std::env::var("PHONE_REMOTE_KEEP_AWAKE_SECS")
            .ok()
            .as_deref(),
    ) else {
        tracing::info!("keep-awake off (PHONE_REMOTE_KEEP_AWAKE_SECS=0): Auto-Lock applies while the phone is driven");
        return;
    };
    let Some(wda) = state.wda.clone() else {
        return;
    };
    ENABLED.store(true, Ordering::Release);
    tracing::info!(
        "keep-awake on: the phone does not auto-lock while it is driven, nor for {}s after the last request",
        window.as_secs()
    );
    tokio::spawn(async move {
        let endpoint = wda.lock().await.keep_awake_endpoint();
        let mut active = false;
        let mut unsupported_until: Option<tokio::time::Instant> = None;
        loop {
            tokio::time::sleep(RENEW_EVERY).await;
            if unsupported_until.is_some_and(|until| tokio::time::Instant::now() < until) {
                continue;
            }
            let want = wanted(
                state.phone_in_use(window),
                state.released.load(Ordering::Acquire),
                state.wda_lifecycle.is_transitioning(),
                crate::http::human_handoff_active(),
            );
            if !want && !active {
                continue;
            }
            match endpoint.renew(if want { LEASE_SECS } else { 0 }).await {
                Ok(report) => {
                    record(&report);
                    note_supported(true);
                    let now_active = want && report.active;
                    if now_active != active {
                        tracing::info!(
                            "keep-awake {}",
                            if now_active {
                                "started: the phone stays awake while it is driven"
                            } else {
                                "ended: Auto-Lock applies again"
                            }
                        );
                    }
                    active = now_active;
                    ACTIVE.store(active, Ordering::Release);
                }
                Err(KeepAwakeError::Unsupported) => {
                    tracing::info!("the device runner has no keep-awake route (WebDriverAgent or an older runner); the phone may auto-lock while it is driven");
                    unsupported_until = Some(tokio::time::Instant::now() + UNSUPPORTED_RETRY);
                    note_supported(false);
                    active = false;
                    ACTIVE.store(false, Ordering::Release);
                }
                Err(KeepAwakeError::Failed(error)) => {
                    // A runner that is down cannot keep the phone awake, and
                    // its lease runs out on its own; the next round retries.
                    tracing::debug!("keep-awake renewal failed: {error:#}");
                    if !want {
                        active = false;
                        ACTIVE.store(false, Ordering::Release);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_defaults_and_zero_turns_it_off() {
        assert_eq!(
            window_from_env(None),
            Some(Duration::from_secs(DEFAULT_WINDOW_SECS))
        );
        assert_eq!(
            window_from_env(Some("bogus")),
            Some(Duration::from_secs(DEFAULT_WINDOW_SECS))
        );
        assert_eq!(
            window_from_env(Some(" 300 ")),
            Some(Duration::from_secs(300))
        );
        assert_eq!(window_from_env(Some("0")), None);
    }

    #[test]
    fn a_released_or_handed_over_phone_is_never_kept_awake() {
        assert!(wanted(true, false, false, false));
        assert!(!wanted(false, false, false, false));
        assert!(!wanted(true, true, false, false));
        assert!(!wanted(true, false, true, false));
        assert!(!wanted(true, false, false, true));
    }

    #[test]
    fn lock_reports_tell_a_passcode_from_none() {
        let report = |locked, passcode| KeepAwakeReport {
            active: true,
            locked,
            passcode,
            auto_lock: None,
        };
        assert_eq!(
            LockReport::from_report(&report(Some(false), Some(true))),
            LockReport::Unlocked
        );
        assert_eq!(
            LockReport::from_report(&report(Some(true), Some(false))),
            LockReport::LockedNoPasscode
        );
        assert_eq!(
            LockReport::from_report(&report(Some(true), Some(true))),
            LockReport::LockedPasscode
        );
        assert_eq!(
            LockReport::from_report(&report(Some(true), None)),
            LockReport::Unknown
        );
        assert_eq!(
            LockReport::from_report(&report(None, None)),
            LockReport::Unknown
        );
        for value in [
            LockReport::Unknown,
            LockReport::Unlocked,
            LockReport::LockedNoPasscode,
            LockReport::LockedPasscode,
        ] {
            assert_eq!(LockReport::from_u8(value as u8), value);
        }
    }
}
