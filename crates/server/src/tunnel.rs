//! The CoreDevice tunnel to a Wi-Fi iPhone.
//!
//! usbmuxd's network attachment of a Wi-Fi phone only reaches lockdownd
//! (62078); a `Connect` to any other port, such as the runner's 8100, never
//! answers. CoreDevice (what Xcode and `devicectl` use) keeps an encrypted
//! tunnel to the phone instead, and while `tunnelState` is `connected` the
//! phone answers on its `tunnelIPAddress` (an IPv6 address that only this Mac
//! routes) on every port. That is how a runner launched over USB keeps
//! working after the cable is pulled, without exposing it on the LAN the way
//! the old `WDA_ALLOW_LAN` socat relay did.

use std::collections::HashMap;
use std::net::Ipv6Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a resolved tunnel address is trusted before asking again.
const FOUND_TTL: Duration = Duration::from_secs(30);
/// How long "no tunnel" is remembered, so a missing tunnel does not cost a
/// devicectl run on every connection.
const MISSING_TTL: Duration = Duration::from_secs(5);
/// A Wi-Fi phone whose tunnel is down is asked to bring it up at most this
/// often (`devicectl device info details` opens it).
const WAKE_EVERY: Duration = Duration::from_secs(60);
const LIST_TIMEOUT: Duration = Duration::from_secs(8);
const WAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// CoreDevice's view of one phone's tunnel, from `devicectl list devices -j`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelView {
    /// `tunnelState: connected` with an address: usable now.
    Connected(Ipv6Addr),
    /// A Wi-Fi (`localNetwork`) phone whose tunnel is not up yet.
    WifiDown,
    /// Listed, but not a Wi-Fi phone with a tunnel (wired, unavailable).
    None,
    /// devicectl does not list the phone, or its output is unreadable.
    Unknown,
}

/// The tunnel for `udid` (dash and case do not matter) in `devicectl list
/// devices -j` output.
pub fn tunnel_view(json: &str, udid: &str) -> TunnelView {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return TunnelView::Unknown;
    };
    let Some(devices) = value
        .pointer("/result/devices")
        .and_then(serde_json::Value::as_array)
    else {
        return TunnelView::Unknown;
    };
    let want = crate::usbmux::normalize_udid(udid);
    let Some(device) = devices.iter().find(|device| {
        device
            .pointer("/hardwareProperties/udid")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| crate::usbmux::normalize_udid(id) == want)
    }) else {
        return TunnelView::Unknown;
    };
    let field = |name: &str| {
        device
            .pointer(&format!("/connectionProperties/{name}"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    };
    let wifi = field("transportType") == "localNetwork";
    if field("tunnelState") == "connected" {
        if let Ok(address) = field("tunnelIPAddress").parse::<Ipv6Addr>() {
            // A wired phone has a tunnel too, but usbmuxd already reaches it
            // and CoreDevice keeps reporting it `connected` for minutes after
            // the cable is pulled; only a Wi-Fi tunnel is worth dialing.
            return if wifi {
                TunnelView::Connected(address)
            } else {
                TunnelView::None
            };
        }
    }
    if wifi {
        TunnelView::WifiDown
    } else {
        TunnelView::None
    }
}

struct Cached {
    at: Instant,
    address: Option<Ipv6Addr>,
}

fn cache() -> &'static Mutex<HashMap<String, Cached>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, Cached>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn woken() -> &'static Mutex<HashMap<String, Instant>> {
    static WOKEN: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    WOKEN.get_or_init(Default::default)
}

/// The tunnel address of a Wi-Fi phone, if CoreDevice has (or can open) one.
/// `refresh` skips the cache, for a connection that just failed.
pub async fn address(udid: &str, refresh: bool) -> Option<Ipv6Addr> {
    let key = crate::usbmux::normalize_udid(udid);
    if !refresh {
        let cache = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(&key) {
            let ttl = if entry.address.is_some() {
                FOUND_TTL
            } else {
                MISSING_TTL
            };
            if entry.at.elapsed() < ttl {
                return entry.address;
            }
        }
    }
    let mut view = list(udid).await;
    if view == TunnelView::WifiDown && should_wake(&key) {
        wake(udid).await;
        view = list(udid).await;
    }
    let address = match view {
        TunnelView::Connected(address) => Some(address),
        _ => None,
    };
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(
        key,
        Cached {
            at: Instant::now(),
            address,
        },
    );
    address
}

fn should_wake(key: &str) -> bool {
    let mut woken = woken().lock().unwrap_or_else(|e| e.into_inner());
    match woken.get(key) {
        Some(at) if at.elapsed() < WAKE_EVERY => false,
        _ => {
            woken.insert(key.to_string(), Instant::now());
            true
        }
    }
}

async fn list(udid: &str) -> TunnelView {
    match devicectl_json(LIST_TIMEOUT, &["list", "devices"]).await {
        Some(json) => tunnel_view(&json, udid),
        None => TunnelView::Unknown,
    }
}

/// Opening a phone's details makes CoreDevice bring its tunnel up.
async fn wake(udid: &str) {
    let _ = devicectl_json(
        WAKE_TIMEOUT,
        &["device", "info", "details", "--device", udid],
    )
    .await;
}

/// `xcrun devicectl <args> -j <file>`, its JSON, or `None` on any failure.
async fn devicectl_json(timeout: Duration, args: &[&str]) -> Option<String> {
    let file = tempfile::Builder::new()
        .prefix("iphone-use-tunnel.")
        .tempfile()
        .ok()?;
    let path = file.path().to_string_lossy().into_owned();
    let mut command = tokio::process::Command::new("/usr/bin/xcrun");
    command
        .arg("devicectl")
        .args(args)
        .args(["-j", &path])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(timeout, command.status())
        .await
        .ok()?
        .ok()?;
    if !status.success() {
        return None;
    }
    std::fs::read_to_string(file.path())
        .ok()
        .filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shapes from `devicectl list devices -j` on 2026-10-08: a wired 17 Pro
    // Max, an unplugged iPhone 13 and a Wi-Fi iPhone 14 with a live tunnel.
    const LIST: &str = r#"{"result":{"devices":[
        {"hardwareProperties":{"udid":"00008150-000A60EC1A02401C"},
         "connectionProperties":{"transportType":"wired","tunnelState":"connected","tunnelIPAddress":"fd5a:17ea:6b83::1"}},
        {"hardwareProperties":{"udid":"00008110-0002346211A0401E"},
         "connectionProperties":{"transportType":"localNetwork","tunnelState":"disconnected"}},
        {"hardwareProperties":{"udid":"00008110-001C18203AD2401E"},
         "connectionProperties":{"transportType":"localNetwork","tunnelState":"connected","tunnelIPAddress":"fd89:9bfc:f458::1"}}
    ]}}"#;

    #[test]
    fn a_wifi_phone_with_a_live_tunnel_is_dialable() {
        assert_eq!(
            tunnel_view(LIST, "00008110001c18203ad2401e"),
            TunnelView::Connected("fd89:9bfc:f458::1".parse().unwrap())
        );
    }

    #[test]
    fn a_wired_tunnel_is_left_to_usbmuxd() {
        assert_eq!(
            tunnel_view(LIST, "00008150-000A60EC1A02401C"),
            TunnelView::None
        );
    }

    #[test]
    fn a_wifi_phone_without_a_tunnel_is_down_and_others_unknown() {
        assert_eq!(
            tunnel_view(LIST, "00008110-0002346211A0401E"),
            TunnelView::WifiDown
        );
        assert_eq!(
            tunnel_view(LIST, "00008110-FFFFFFFFFFFFFFFF"),
            TunnelView::Unknown
        );
        assert_eq!(tunnel_view("not json", "x"), TunnelView::Unknown);
    }

    #[test]
    fn a_connected_tunnel_without_a_valid_address_is_not_dialed() {
        let json = r#"{"result":{"devices":[{"hardwareProperties":{"udid":"AA"},
            "connectionProperties":{"transportType":"localNetwork","tunnelState":"connected","tunnelIPAddress":"bogus"}}]}}"#;
        assert_eq!(tunnel_view(json, "AA"), TunnelView::WifiDown);
    }
}
