//! Keep the device runner up on a phone whose next start needs a person.
//!
//! Idle release stops the runner after a quiet spell and starts it again on
//! the next request. That is free on most phones, but two kinds of phone
//! cannot be started again without someone standing next to them:
//!
//! - a phone with a lock-screen passcode: every runner launch makes iOS ask
//!   for the passcode on the phone before it enables UI automation. With
//!   nobody there, the start ends in `automation_mode_disabled` (iOS 17+) or
//!   a runner that never starts (the iOS 15/16 path), and a remote user who
//!   let the phone idle cannot get it back;
//! - a phone that refuses to start the runner over Wi-Fi
//!   (`wifi_automation_refused`) while the runner is up over the Wi-Fi tunnel:
//!   starting it again needs the cable.
//!
//! For those the idle-release watchdog keeps the runner process alive
//! (`keep_runner_alive` in `/agent/status`). It only keeps the *process*:
//! the screen is not kept awake while idle (keep-awake leases follow use, see
//! [`crate::keep_awake`]), so Auto-Lock still locks the phone, and the phone
//! stays in automation mode ("Automation Running") between uses. The reason
//! idle release exists — a runner that is always up gets relaunched by
//! KeepAlive every time iOS kills it, and each relaunch asks for the passcode —
//! is handled by keeping only a runner that is *up*: a runner that is already
//! down is still released, which stops the relaunches.
//!
//! `IPHONE_USE_IDLE_RELEASE=force` turns the exception off: the owner would
//! rather have the phone released when idle than kept in automation mode.

use std::path::Path;

use serde_json::{json, Value};

/// State-dir marker written by setup when a runner start failed because
/// nobody entered the passcode on the phone (`needs_passcode_on_phone`). It
/// stands in for the passcode fact until the daemon has read one (a phone
/// whose lockdown answer never came back).
pub const RELAUNCH_NEEDS_PERSON_FILE: &str = "relaunch-needs-person";

/// The setup blocker for a runner start that waited on the passcode prompt.
pub const NEEDS_PASSCODE_BLOCKER: &str = "needs_passcode_on_phone";

/// The one `keep_runner_alive.reason` value.
pub const REASON: &str = "relaunch_needs_person";

/// Why a start of the runner needs a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Because {
    /// The phone has a passcode: iOS asks for it on every runner launch.
    Passcode,
    /// The phone refuses to start the runner over Wi-Fi and the runner is up
    /// over Wi-Fi: starting it again needs the cable.
    WifiStartRefused,
}

impl Because {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passcode => "passcode",
            Self::WifiStartRefused => "wifi_start_refused",
        }
    }

    /// Does this reason also keep a runner the probe saw *down*? Only the
    /// Wi-Fi one: a Wi-Fi probe that timed out is no proof the runner is
    /// gone. A passcode phone whose runner is down is released as before, so
    /// KeepAlive stops relaunching it — and asking for the passcode — while
    /// nobody uses it.
    pub fn keeps_down_runner(self) -> bool {
        matches!(self, Self::WifiStartRefused)
    }

    /// One line for a person, Chinese and English.
    pub fn hint(self) -> (&'static str, &'static str) {
        match self {
            Self::Passcode => (
                "这台手机设了锁屏密码，每次启动设备服务都要有人在手机上输入密码，所以空闲时不停掉设备服务：手机会一直处在自动化模式，屏幕照常自动锁定。想让它空闲时照常释放，设置 IPHONE_USE_IDLE_RELEASE=force。",
                "This phone has a passcode and every start of the device service asks for it on the phone, so the service is kept running while idle: the phone stays in automation mode, and the screen still locks as usual. Set IPHONE_USE_IDLE_RELEASE=force to release it when idle anyway.",
            ),
            Self::WifiStartRefused => (
                "这台手机不允许通过 Wi‑Fi 启动设备服务，空闲时停掉的话要插线才能再启动，所以设备服务一直保持运行。想让它空闲时照常释放，设置 IPHONE_USE_IDLE_RELEASE=force。",
                "This phone will not start the device service over Wi-Fi, so stopping it while idle would need the cable to undo; it is kept running. Set IPHONE_USE_IDLE_RELEASE=force to release it when idle anyway.",
            ),
        }
    }
}

/// What the policy looks at.
#[derive(Debug, Clone, Copy, Default)]
pub struct Signals<'a> {
    /// Setup recorded `wifi_automation_refused` for this phone.
    pub wifi_start_refused: bool,
    /// How control reaches the runner (`usb`, `wifi-tunnel`, `wifi`, …).
    pub transport: &'a str,
    /// `lock_readiness.passcode_protected` (cached).
    pub passcode_protected: Option<bool>,
    /// Setup left [`RELAUNCH_NEEDS_PERSON_FILE`].
    pub passcode_marker: bool,
    /// `IPHONE_USE_IDLE_RELEASE=force`.
    pub force_release: bool,
}

/// Should idle release keep the runner, and why.
pub fn reason(signals: Signals<'_>) -> Option<Because> {
    if signals.force_release {
        return None;
    }
    if signals.wifi_start_refused && matches!(signals.transport, "wifi-tunnel" | "wifi") {
        return Some(Because::WifiStartRefused);
    }
    match signals.passcode_protected {
        Some(true) => Some(Because::Passcode),
        // A passcode read as off (on a locked phone) outranks the marker: the
        // owner removed the passcode since.
        Some(false) => None,
        None => signals.passcode_marker.then_some(Because::Passcode),
    }
}

/// `IPHONE_USE_IDLE_RELEASE=force`: release idle runners even when the next
/// start needs a person.
pub fn force_release_from_env(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case("force"))
}

pub fn force_release() -> bool {
    force_release_from_env(std::env::var("IPHONE_USE_IDLE_RELEASE").ok().as_deref())
}

pub fn passcode_marker(state_dir: &Path) -> bool {
    state_dir.join(RELAUNCH_NEEDS_PERSON_FILE).exists()
}

/// Setup saw a start that needed the passcode on the phone.
pub fn write_passcode_marker(state_dir: &Path) {
    let _ = std::fs::write(
        state_dir.join(RELAUNCH_NEEDS_PERSON_FILE),
        format!("{NEEDS_PASSCODE_BLOCKER}\n"),
    );
}

/// The policy for this daemon right now (cache and marker files, no device
/// I/O). `wifi_start_refused` is the existing Wi-Fi marker.
pub fn current(state_dir: &Path, wifi_start_refused: bool, transport: &str) -> Option<Because> {
    reason(Signals {
        wifi_start_refused,
        transport,
        passcode_protected: crate::lock_readiness::passcode(),
        passcode_marker: passcode_marker(state_dir),
        force_release: force_release(),
    })
}

/// `keep_runner_alive` in `/agent/status`: `null`, or why idle release keeps
/// the runner.
pub fn status_json(because: Option<Because>) -> Value {
    match because {
        None => Value::Null,
        Some(because) => {
            let (zh, en) = because.hint();
            json!({
                "reason": REASON,
                "because": because.as_str(),
                "hint": { "zh": zh, "en": en },
            })
        }
    }
}

/// The blocker to publish for a failed runner start. A refusal to enable UI
/// automation on a phone known to have a passcode is the passcode prompt
/// nobody answered, so say that; anything else passes through.
pub fn person_blocker(raw: &str, passcode_protected: Option<bool>) -> &str {
    match raw {
        "automation_mode_disabled" | "automation_not_allowed"
            if passcode_protected == Some(true) =>
        {
            NEEDS_PASSCODE_BLOCKER
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> Signals<'static> {
        Signals {
            transport: "usb",
            ..Signals::default()
        }
    }

    #[test]
    fn a_passcode_phone_keeps_its_runner_on_any_transport() {
        for transport in ["usb", "wifi-tunnel", "wifi", "unknown"] {
            let s = Signals {
                transport,
                passcode_protected: Some(true),
                ..signals()
            };
            assert_eq!(reason(s), Some(Because::Passcode), "{transport}");
        }
    }

    #[test]
    fn a_phone_without_a_passcode_is_released() {
        assert_eq!(reason(signals()), None);
        let off = Signals {
            passcode_protected: Some(false),
            passcode_marker: true,
            ..signals()
        };
        assert_eq!(
            reason(off),
            None,
            "a passcode read as off outranks an old marker"
        );
    }

    #[test]
    fn the_setup_marker_stands_in_for_an_unknown_passcode() {
        let s = Signals {
            passcode_marker: true,
            ..signals()
        };
        assert_eq!(reason(s), Some(Because::Passcode));
    }

    #[test]
    fn wifi_start_refused_keeps_only_a_wifi_runner() {
        let wifi = Signals {
            wifi_start_refused: true,
            transport: "wifi-tunnel",
            ..signals()
        };
        assert_eq!(reason(wifi), Some(Because::WifiStartRefused));
        let usb = Signals {
            wifi_start_refused: true,
            ..signals()
        };
        assert_eq!(reason(usb), None, "over USB the runner restarts on demand");
        assert!(Because::WifiStartRefused.keeps_down_runner());
        assert!(
            !Because::Passcode.keeps_down_runner(),
            "a down runner on a passcode phone is released so KeepAlive stops prompting"
        );
    }

    #[test]
    fn force_turns_every_exception_off() {
        let s = Signals {
            wifi_start_refused: true,
            transport: "wifi",
            passcode_protected: Some(true),
            passcode_marker: true,
            force_release: true,
        };
        assert_eq!(reason(s), None);
        assert!(force_release_from_env(Some("force")));
        assert!(force_release_from_env(Some(" FORCE\n")));
        assert!(!force_release_from_env(Some("1")));
        assert!(!force_release_from_env(None));
    }

    #[test]
    fn the_marker_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!passcode_marker(dir.path()));
        write_passcode_marker(dir.path());
        assert!(passcode_marker(dir.path()));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(RELAUNCH_NEEDS_PERSON_FILE)).unwrap(),
            "needs_passcode_on_phone\n"
        );
    }

    #[test]
    fn status_names_the_reason_and_tells_a_person() {
        assert_eq!(status_json(None), Value::Null);
        let v = status_json(Some(Because::Passcode));
        assert_eq!(v["reason"], "relaunch_needs_person");
        assert_eq!(v["because"], "passcode");
        assert!(v["hint"]["zh"]
            .as_str()
            .unwrap()
            .contains("IPHONE_USE_IDLE_RELEASE=force"));
        assert!(v["hint"]["en"]
            .as_str()
            .unwrap()
            .contains("screen still locks"));
        let v = status_json(Some(Because::WifiStartRefused));
        assert_eq!(v["because"], "wifi_start_refused");
    }

    #[test]
    fn an_unanswered_automation_prompt_on_a_passcode_phone_is_named() {
        for raw in ["automation_mode_disabled", "automation_not_allowed"] {
            assert_eq!(person_blocker(raw, Some(true)), "needs_passcode_on_phone");
            assert_eq!(
                person_blocker(raw, None),
                raw,
                "unknown passcode keeps the raw blocker"
            );
            assert_eq!(person_blocker(raw, Some(false)), raw);
        }
        assert_eq!(person_blocker("trust", Some(true)), "trust");
        assert_eq!(person_blocker("", Some(true)), "");
    }
}
