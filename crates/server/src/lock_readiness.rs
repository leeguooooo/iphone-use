//! Will this phone get stuck at the lock screen?
//!
//! With many phones the owner wants to see at a glance which ones lock while
//! nobody drives them and then need a person to unlock them. Two settings
//! decide it, and both are read without touching the screen:
//!
//! - whether a passcode is set: lockdown `PasswordProtected` over usbmux (USB
//!   or a Wi-Fi attachment; [`crate::lockdown::passcode_protected`]), or the
//!   runner's `SBGetScreenLockStatus` while it is up. Both say whether a
//!   passcode is required *now*, so a `false` counts only on a locked phone
//!   ([`merge_passcode`]);
//! - the Auto-Lock setting: the device runner reads ManagedConfiguration's
//!   `maxInactivity` in-process and reports it on `GET /wda/keepawake`. No
//!   lockdown domain carries it, so it is known only once a runner that has
//!   the route ran.
//!
//! The facts are cached (and kept in `lock-readiness.json` in the instance
//! state dir, so a released phone still shows the last reading) and refreshed
//! when the runner comes up and every few minutes; `/agent/status` only reads
//! the cache. Passcodes themselves are never stored or typed.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::http::AppState;
use crate::keep_awake::KeepAwakeStatus;
use crate::wda::{KeepAwakeError, KeepAwakeReport};

/// Refresh at least this often.
const REFRESH_EVERY: Duration = Duration::from_secs(300);
/// How often the loop looks for a runner that just came up.
const TICK: Duration = Duration::from_secs(30);
const LOCKDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const STATE_FILE: &str = "lock-readiness.json";
/// No Auto-Lock choice is longer than this; a larger value means Never
/// (ManagedConfiguration stores Never as `INT_MAX`).
const LONGEST_AUTO_LOCK_SECS: i64 = 86_400;

/// The Auto-Lock setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoLock {
    Never,
    Secs(u64),
}

impl AutoLock {
    /// From the runner's `autoLockSecs` / `autoLockNever`.
    pub fn from_runner(secs: Option<i64>, never: Option<bool>) -> Option<Self> {
        if never == Some(true) {
            return Some(Self::Never);
        }
        match secs {
            Some(secs) if secs >= LONGEST_AUTO_LOCK_SECS => Some(Self::Never),
            Some(secs) if secs > 0 => u64::try_from(secs).ok().map(Self::Secs),
            _ => None,
        }
    }

    /// `"never"` or a number of seconds.
    pub fn to_json(self) -> Value {
        match self {
            Self::Never => json!("never"),
            Self::Secs(secs) => json!(secs),
        }
    }

    fn from_json(value: &Value) -> Option<Self> {
        match value {
            Value::String(text) if text == "never" => Some(Self::Never),
            Value::Number(number) => Self::from_runner(number.as_i64(), None),
            _ => None,
        }
    }
}

/// What the owner should take from the two settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Auto-Lock is Never: the phone does not lock on its own.
    Ready,
    /// It locks when idle and has a passcode: a person has to unlock it.
    WillLockNeedsPerson,
    /// It locks when idle but has no passcode: the daemon unlocks it before
    /// the next action.
    WillLockAutoUnlocks,
    Unknown,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::WillLockNeedsPerson => "will_lock_needs_person",
            Self::WillLockAutoUnlocks => "will_lock_auto_unlocks",
            Self::Unknown => "unknown",
        }
    }
}

/// The verdict for a passcode state and an Auto-Lock setting.
///
/// A passcode with an Auto-Lock that could not be read yet still says
/// "needs a person": iOS never ships with Auto-Lock at Never, so a phone
/// nobody changed locks.
pub fn verdict(passcode: Option<bool>, auto_lock: Option<AutoLock>) -> Verdict {
    match (passcode, auto_lock) {
        (_, Some(AutoLock::Never)) => Verdict::Ready,
        (Some(true), _) => Verdict::WillLockNeedsPerson,
        (Some(false), _) => Verdict::WillLockAutoUnlocks,
        (None, _) => Verdict::Unknown,
    }
}

fn duration_zh(secs: u64) -> String {
    if secs < 60 || !secs.is_multiple_of(60) {
        format!("{secs} 秒")
    } else {
        format!("{} 分钟", secs / 60)
    }
}

fn duration_en(secs: u64) -> String {
    match secs {
        60 => "1 minute".to_string(),
        secs if secs >= 60 && secs.is_multiple_of(60) => format!("{} minutes", secs / 60),
        secs => format!("{secs} seconds"),
    }
}

/// A one-line explanation for a person, in Chinese and English.
pub fn hint(
    passcode: Option<bool>,
    auto_lock: Option<AutoLock>,
    keep_awake: KeepAwakeStatus,
) -> (String, String) {
    let keeps_awake = keep_awake.enabled && keep_awake.supported != Some(false);
    match verdict(passcode, auto_lock) {
        Verdict::Ready => {
            if passcode == Some(true) {
                (
                    "自动锁定为永不：这台手机不会自己锁屏。它设了锁屏密码，被手动锁上后仍需有人解锁。".to_string(),
                    "Auto-Lock is Never: this phone does not lock on its own. It has a passcode, so if someone locks it by hand, a person has to unlock it.".to_string(),
                )
            } else {
                (
                    "自动锁定为永不：这台手机不会自己锁屏。".to_string(),
                    "Auto-Lock is Never: this phone does not lock on its own.".to_string(),
                )
            }
        }
        Verdict::WillLockNeedsPerson => {
            let (mut zh, mut en) = match auto_lock {
                Some(AutoLock::Secs(secs)) => (
                    format!(
                        "这台手机设了锁屏密码，自动锁定 {}：没人操作时会锁住，需要有人解锁。建议把自动锁定改为永不。",
                        duration_zh(secs)
                    ),
                    format!(
                        "This phone has a passcode and Auto-Lock {}: when nobody drives it, it locks and a person has to unlock it. Set Auto-Lock to Never.",
                        duration_en(secs)
                    ),
                ),
                _ => (
                    "这台手机设了锁屏密码，自动锁定时间还没读到：除非自动锁定是永不，否则没人操作时会锁住，需要有人解锁。建议把自动锁定改为永不。".to_string(),
                    "This phone has a passcode and its Auto-Lock is not known yet: unless Auto-Lock is Never, it locks when nobody drives it and a person has to unlock it. Set Auto-Lock to Never.".to_string(),
                ),
            };
            if keeps_awake {
                zh.push_str("操作期间 iphone-use 会让它保持唤醒，闲置后仍会锁。");
                en.push_str(" iphone-use keeps it awake while it is driven; it still locks once idle.");
            }
            (zh, en)
        }
        Verdict::WillLockAutoUnlocks => match auto_lock {
            Some(AutoLock::Secs(secs)) => (
                format!(
                    "这台手机没有锁屏密码，自动锁定 {}：锁住后 iphone-use 会在下一个操作前自动解锁。想让屏幕一直亮着，把自动锁定改为永不。",
                    duration_zh(secs)
                ),
                format!(
                    "This phone has no passcode and Auto-Lock {}: when it locks, iphone-use unlocks it before the next action. Set Auto-Lock to Never to keep the screen on.",
                    duration_en(secs)
                ),
            ),
            _ => (
                "这台手机没有锁屏密码：锁住后 iphone-use 会在下一个操作前自动解锁。".to_string(),
                "This phone has no passcode: when it locks, iphone-use unlocks it before the next action.".to_string(),
            ),
        },
        Verdict::Unknown => match auto_lock {
            Some(AutoLock::Secs(secs)) => (
                format!(
                    "自动锁定 {}，但还没读到是否设了锁屏密码：如果设了，没人操作时会锁住，需要有人解锁。建议把自动锁定改为永不。",
                    duration_zh(secs)
                ),
                format!(
                    "Auto-Lock is {}, and whether a passcode is set is not known yet: if one is, the phone locks when nobody drives it and a person has to unlock it. Set Auto-Lock to Never.",
                    duration_en(secs)
                ),
            ),
            _ => (
                "还没读到这台手机的锁屏密码和自动锁定设置：手机要连着这台 Mac，设备运行器要运行过一次。".to_string(),
                "Passcode and Auto-Lock are not known yet: the phone has to be attached to this Mac, and the device runner has to have run once.".to_string(),
            ),
        },
    }
}

/// The cached facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Facts {
    pub passcode: Option<bool>,
    pub auto_lock: Option<AutoLock>,
    /// Unix seconds of the last reading that changed or confirmed a fact.
    pub checked_at: Option<u64>,
}

impl Facts {
    fn to_file_json(self) -> Value {
        json!({
            "passcode_protected": self.passcode,
            "auto_lock_secs": self.auto_lock.map_or(Value::Null, AutoLock::to_json),
            "checked_at": self.checked_at,
        })
    }

    fn from_file_json(value: &Value) -> Self {
        Self {
            passcode: value.get("passcode_protected").and_then(Value::as_bool),
            auto_lock: value.get("auto_lock_secs").and_then(AutoLock::from_json),
            checked_at: value.get("checked_at").and_then(Value::as_u64),
        }
    }
}

static FACTS: Mutex<Facts> = Mutex::new(Facts {
    passcode: None,
    auto_lock: None,
    checked_at: None,
});
static STATE_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

fn facts() -> Facts {
    *FACTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Fold a passcode reading into what is known.
///
/// Both sources answer "is a passcode required right now" rather than "is
/// one set": on a 12 mini (iOS 15.4.1) with a passcode, lockdown
/// `PasswordProtected` read `true` while the phone was locked and `false`
/// while it was unlocked, and the runner's `SBGetScreenLockStatus` reported
/// `passcodeEnabled:false` on the unlocked phone while `/wda/unlock` on the
/// locked one answered `passcode_required`. So a `true` always counts, and a
/// `false` only when the phone was known to be locked when it was read; an
/// unlocked `false` keeps what was known.
pub fn merge_passcode(
    known: Option<bool>,
    reading: Option<bool>,
    locked: Option<bool>,
) -> Option<bool> {
    match (reading, locked) {
        (Some(true), _) => Some(true),
        (Some(false), Some(true)) => Some(false),
        _ => known,
    }
}

/// Merge a reading into the cache; a `None` keeps what was known.
/// `locked` is the lock state when `passcode` was read (see [`merge_passcode`]).
fn update(passcode: Option<bool>, locked: Option<bool>, auto_lock: Option<AutoLock>) {
    if passcode.is_none() && auto_lock.is_none() {
        return;
    }
    let (changed, snapshot) = {
        let mut facts = FACTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = *facts;
        facts.passcode = merge_passcode(facts.passcode, passcode, locked);
        if auto_lock.is_some() {
            facts.auto_lock = auto_lock;
        }
        facts.checked_at = Some(now_secs());
        (
            before.passcode != facts.passcode || before.auto_lock != facts.auto_lock,
            *facts,
        )
    };
    if changed {
        tracing::info!(
            "lock readiness: passcode={:?} auto_lock={:?} verdict={}",
            snapshot.passcode,
            snapshot.auto_lock,
            verdict(snapshot.passcode, snapshot.auto_lock).as_str()
        );
    }
    let path = STATE_PATH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(path) = path {
        save(&path, snapshot);
    }
}

fn save(path: &Path, facts: Facts) {
    let tmp = path.with_extension("json.tmp");
    let body = facts.to_file_json().to_string();
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn load(path: &Path) -> Option<Facts> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(Facts::from_file_json(&value))
}

/// A keep-awake renewal or read told us about the phone.
pub fn note_runner_report(report: &KeepAwakeReport) {
    update(report.passcode, report.locked, report.auto_lock);
}

/// The `lock_readiness` object of `/agent/status`.
pub fn status_json(facts: Facts, keep_awake: KeepAwakeStatus) -> Value {
    let (zh, en) = hint(facts.passcode, facts.auto_lock, keep_awake);
    json!({
        "passcode_protected": facts.passcode,
        "auto_lock_secs": facts.auto_lock.map_or(Value::Null, AutoLock::to_json),
        "keep_awake": {
            "enabled": keep_awake.enabled,
            "supported": keep_awake.supported,
            "active": keep_awake.active,
        },
        "verdict": verdict(facts.passcode, facts.auto_lock).as_str(),
        "hint": { "zh": zh, "en": en },
        "checked_at": facts.checked_at,
    })
}

/// The cached `passcode_protected` (no I/O).
pub fn passcode() -> Option<bool> {
    facts().passcode
}

/// `passcode_protected` as the daemon last saved it in `state_dir` — for
/// setup, which runs in its own process.
pub fn saved_passcode(state_dir: &Path) -> Option<bool> {
    load(&state_dir.join(STATE_FILE)).and_then(|facts| facts.passcode)
}

/// `lock_readiness` for this daemon right now (cache only, no I/O).
pub fn current_json() -> Value {
    status_json(facts(), crate::keep_awake::status())
}

/// Read both facts once: lockdown for the passcode, the runner for both.
async fn refresh(state: &AppState) {
    if let Some(udid) = state.device_udid.as_deref() {
        match tokio::time::timeout(LOCKDOWN_TIMEOUT, crate::lockdown::passcode_protected(udid))
            .await
        {
            Ok(Ok(passcode)) => update(Some(passcode), known_lock_state(state), None),
            Ok(Err(error)) => tracing::debug!("lock readiness: lockdown passcode read: {error:#}"),
            Err(_) => tracing::debug!("lock readiness: lockdown passcode read timed out"),
        }
    }
    if !runner_readable(state) {
        return;
    }
    let Some(wda) = state.wda.clone() else {
        return;
    };
    let endpoint = wda.lock().await.keep_awake_endpoint();
    match endpoint.read().await {
        Ok(report) => {
            crate::keep_awake::note_supported(true);
            note_runner_report(&report);
        }
        Err(KeepAwakeError::Unsupported) => crate::keep_awake::note_supported(false),
        Err(KeepAwakeError::Failed(error)) => {
            tracing::debug!("lock readiness: runner read: {error:#}")
        }
    }
}

/// The lock state the runner last reported, if it is up.
fn known_lock_state(state: &AppState) -> Option<bool> {
    if !runner_readable(state) {
        return None;
    }
    state
        .wda_health
        .lock()
        .ok()
        .and_then(|health| health.locked)
}

/// Only ask a runner that is up and not being started, stopped or handed over.
fn runner_readable(state: &AppState) -> bool {
    !state.managed_wda_pending
        && !state.released.load(Ordering::Acquire)
        && !state.wda_lifecycle.is_transitioning()
        && state
            .wda_health
            .lock()
            .map(|health| health.up)
            .unwrap_or(false)
}

/// Whether this tick should read: on schedule, or right after the runner
/// came up (a connect).
pub fn due(since_last: Option<Duration>, runner_up: bool, was_up: bool) -> bool {
    match since_last {
        None => true,
        Some(elapsed) => elapsed >= REFRESH_EVERY || (runner_up && !was_up),
    }
}

/// Load the cache and start the refresh loop.
pub fn spawn(state: Arc<AppState>) {
    let path = crate::instance::current().state_dir.join(STATE_FILE);
    if let Some(saved) = load(&path) {
        *FACTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = saved;
    }
    *STATE_PATH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut last: Option<tokio::time::Instant> = None;
        let mut was_up = false;
        loop {
            let up = runner_readable(&state);
            if due(last.map(|at| at.elapsed()), up, was_up) {
                refresh(&state).await;
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

    const ON: KeepAwakeStatus = KeepAwakeStatus {
        enabled: true,
        supported: Some(true),
        active: false,
    };
    const OFF: KeepAwakeStatus = KeepAwakeStatus {
        enabled: false,
        supported: None,
        active: false,
    };

    #[test]
    fn runner_values_map_to_auto_lock() {
        assert_eq!(
            AutoLock::from_runner(Some(2_147_483_647), Some(true)),
            Some(AutoLock::Never)
        );
        assert_eq!(
            AutoLock::from_runner(Some(2_147_483_647), None),
            Some(AutoLock::Never)
        );
        assert_eq!(
            AutoLock::from_runner(Some(30), Some(false)),
            Some(AutoLock::Secs(30))
        );
        assert_eq!(AutoLock::from_runner(Some(0), None), None);
        assert_eq!(AutoLock::from_runner(Some(-1), Some(false)), None);
        assert_eq!(AutoLock::from_runner(None, None), None);
        assert_eq!(
            AutoLock::from_runner(None, Some(true)),
            Some(AutoLock::Never)
        );
    }

    #[test]
    fn verdict_table() {
        use AutoLock::*;
        use Verdict::*;
        let cases = [
            (Some(true), Some(Never), Ready),
            (Some(false), Some(Never), Ready),
            (None, Some(Never), Ready),
            (Some(true), Some(Secs(30)), WillLockNeedsPerson),
            (Some(true), None, WillLockNeedsPerson),
            (Some(false), Some(Secs(60)), WillLockAutoUnlocks),
            (Some(false), None, WillLockAutoUnlocks),
            (None, Some(Secs(30)), Unknown),
            (None, None, Unknown),
        ];
        for (passcode, auto_lock, want) in cases {
            assert_eq!(
                verdict(passcode, auto_lock),
                want,
                "{passcode:?} {auto_lock:?}"
            );
        }
        assert_eq!(WillLockNeedsPerson.as_str(), "will_lock_needs_person");
        assert_eq!(WillLockAutoUnlocks.as_str(), "will_lock_auto_unlocks");
    }

    #[test]
    fn hints_name_the_setting_and_the_fix() {
        let (zh, en) = hint(Some(true), Some(AutoLock::Secs(30)), OFF);
        assert_eq!(
            zh,
            "这台手机设了锁屏密码，自动锁定 30 秒：没人操作时会锁住，需要有人解锁。建议把自动锁定改为永不。"
        );
        assert!(en.contains("Auto-Lock 30 seconds") && en.contains("Set Auto-Lock to Never"));
        let (zh, en) = hint(Some(true), Some(AutoLock::Secs(120)), ON);
        assert!(zh.contains("自动锁定 2 分钟") && zh.contains("保持唤醒"));
        assert!(en.contains("2 minutes") && en.contains("keeps it awake"));
        let (zh, _) = hint(Some(false), Some(AutoLock::Secs(60)), ON);
        assert!(zh.contains("没有锁屏密码") && zh.contains("1 分钟"));
        let (zh, en) = hint(Some(false), Some(AutoLock::Never), ON);
        assert_eq!(zh, "自动锁定为永不：这台手机不会自己锁屏。");
        assert!(en.starts_with("Auto-Lock is Never"));
        // Keep-awake that the runner lacks is not promised.
        let unsupported = KeepAwakeStatus {
            supported: Some(false),
            ..ON
        };
        assert!(!hint(Some(true), None, unsupported).0.contains("保持唤醒"));
        assert_eq!(duration_en(90), "90 seconds");
        assert_eq!(duration_zh(300), "5 分钟");
    }

    #[test]
    fn status_json_shape() {
        let facts = Facts {
            passcode: Some(true),
            auto_lock: Some(AutoLock::Secs(30)),
            checked_at: Some(1_791_000_000),
        };
        let value = status_json(facts, ON);
        assert_eq!(value["passcode_protected"], json!(true));
        assert_eq!(value["auto_lock_secs"], json!(30));
        assert_eq!(value["verdict"], json!("will_lock_needs_person"));
        assert_eq!(
            value["keep_awake"],
            json!({"enabled": true, "supported": true, "active": false})
        );
        assert!(value["hint"]["zh"].as_str().unwrap().contains("30 秒"));
        assert!(value["hint"]["en"].is_string());
        assert_eq!(value["checked_at"], json!(1_791_000_000u64));

        let never = status_json(
            Facts {
                passcode: Some(false),
                auto_lock: Some(AutoLock::Never),
                checked_at: None,
            },
            OFF,
        );
        assert_eq!(never["auto_lock_secs"], json!("never"));
        assert_eq!(never["verdict"], json!("ready"));
        assert_eq!(never["keep_awake"]["supported"], Value::Null);

        let unknown = status_json(Facts::default(), OFF);
        assert_eq!(unknown["passcode_protected"], Value::Null);
        assert_eq!(unknown["auto_lock_secs"], Value::Null);
        assert_eq!(unknown["verdict"], json!("unknown"));
        assert_eq!(unknown["checked_at"], Value::Null);
    }

    #[test]
    fn cache_file_round_trips() {
        for facts in [
            Facts {
                passcode: Some(true),
                auto_lock: Some(AutoLock::Secs(30)),
                checked_at: Some(5),
            },
            Facts {
                passcode: Some(false),
                auto_lock: Some(AutoLock::Never),
                checked_at: Some(6),
            },
            Facts::default(),
        ] {
            assert_eq!(Facts::from_file_json(&facts.to_file_json()), facts);
        }
        let dir = std::env::temp_dir().join(format!("ipu-lock-readiness-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(STATE_FILE);
        let facts = Facts {
            passcode: Some(true),
            auto_lock: Some(AutoLock::Never),
            checked_at: Some(7),
        };
        assert_eq!(saved_passcode(&dir), None, "no file yet");
        save(&path, facts);
        assert_eq!(load(&path), Some(facts));
        assert_eq!(saved_passcode(&dir), Some(true), "setup reads what the daemon saved");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unlocked_false_does_not_hide_a_passcode() {
        // Locked phone, passcode required: set.
        assert_eq!(merge_passcode(None, Some(true), Some(true)), Some(true));
        assert_eq!(merge_passcode(Some(false), Some(true), None), Some(true));
        // The 12 mini case: unlocked, both sources say false — keep `true`.
        assert_eq!(
            merge_passcode(Some(true), Some(false), Some(false)),
            Some(true)
        );
        assert_eq!(merge_passcode(Some(true), Some(false), None), Some(true));
        // Nothing known yet: an unlocked or unknown-state false proves nothing.
        assert_eq!(merge_passcode(None, Some(false), Some(false)), None);
        assert_eq!(merge_passcode(None, Some(false), None), None);
        // Locked and no passcode required: there is none (also after removal).
        assert_eq!(merge_passcode(None, Some(false), Some(true)), Some(false));
        assert_eq!(
            merge_passcode(Some(true), Some(false), Some(true)),
            Some(false)
        );
        // No reading keeps what was known.
        assert_eq!(merge_passcode(Some(true), None, Some(true)), Some(true));
    }

    #[test]
    fn refresh_on_schedule_and_on_connect() {
        assert!(due(None, false, false));
        assert!(!due(Some(Duration::from_secs(10)), false, false));
        assert!(!due(Some(Duration::from_secs(10)), true, true));
        assert!(due(Some(Duration::from_secs(10)), true, false));
        assert!(due(Some(REFRESH_EVERY), false, false));
    }
}
