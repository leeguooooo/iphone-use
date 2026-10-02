//! Scan-to-connect pairing.
//!
//! A signed-in browser asks for a one-time code (`POST /pair/new`) and shows
//! it as a QR code of `http://<lan-ip>:<port>/pair?c=<code>`. Whoever scans it
//! within [`CODE_TTL`] trades the code — once — for a session: the system
//! camera lands on the `/pair` page in Safari, and the iOS app posts the code
//! itself and also receives a long-lived device token, so it can renew its
//! session later without the control password.
//!
//! Device tokens are signed with a key derived from the daemon secret *and*
//! the control password, so changing the password revokes every paired phone.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// How long a scanned code stays valid.
pub const CODE_TTL: Duration = Duration::from_secs(300);
/// Outstanding codes kept at once; the oldest is dropped past this.
const MAX_CODES: usize = 8;
/// Lifetime of a paired device's token.
pub const DEVICE_TOKEN_TTL_SECS: u64 = 180 * 24 * 3600;

pub struct Pairing {
    /// False when the daemon only listens on loopback: a QR code would point
    /// a phone at an address it cannot reach.
    pub lan_reachable: bool,
    codes: Mutex<HashMap<String, Instant>>,
}

impl Pairing {
    pub fn new(lan_reachable: bool) -> Self {
        Self {
            lan_reachable,
            codes: Mutex::new(HashMap::new()),
        }
    }

    /// Mint a fresh single-use code.
    pub fn issue(&self) -> String {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("os randomness");
        let code = URL_SAFE_NO_PAD.encode(bytes);
        let now = Instant::now();
        let mut codes = self
            .codes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        codes.retain(|_, issued| now.duration_since(*issued) < CODE_TTL);
        while codes.len() >= MAX_CODES {
            let oldest = codes
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(c, _)| c.clone());
            match oldest {
                Some(c) => codes.remove(&c),
                None => break,
            };
        }
        codes.insert(code.clone(), now);
        code
    }

    /// Consume `code`: true exactly once, and only within [`CODE_TTL`].
    pub fn redeem(&self, code: &str) -> bool {
        let mut codes = self
            .codes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match codes.remove(code) {
            Some(issued) => issued.elapsed() < CODE_TTL,
            None => false,
        }
    }

    /// Whether `code` is outstanding, without consuming it (the landing page).
    pub fn is_live(&self, code: &str) -> bool {
        let codes = self
            .codes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        codes
            .get(code)
            .is_some_and(|issued| issued.elapsed() < CODE_TTL)
    }
}

/// Signing key for device tokens: bound to the daemon secret and the control
/// password, distinct from the session-cookie key so neither token passes as
/// the other.
pub fn device_key(secret: &[u8], password: Option<&str>) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(b"iphone-use device token v1\0");
    h.update(secret);
    h.update(b"\0");
    h.update(password.unwrap_or("").as_bytes());
    h.finalize().to_vec()
}

/// This Mac's IPv4 addresses a phone could reach, best first: Wi-Fi/Ethernet
/// (`en*`) before anything else, VPN tunnels (`utun*`, e.g. WARP or
/// Tailscale) last. Loopback and link-local addresses are left out.
pub fn lan_addresses() -> Vec<(String, Ipv4Addr)> {
    let mut found = interface_ipv4s();
    found.retain(|(_, ip)| !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified());
    found.sort_by_key(|(name, ip)| {
        let rank = if name.starts_with("en") {
            if ip.is_private() {
                0
            } else {
                1
            }
        } else if name.starts_with("utun") {
            3
        } else {
            2
        };
        (rank, name.clone())
    });
    found.dedup_by(|a, b| a.1 == b.1);
    found
}

fn interface_ipv4s() -> Vec<(String, Ipv4Addr)> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs fills a linked list we walk read-only and free once.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            if ifa.ifa_flags & (libc::IFF_UP as u32) == 0 {
                continue;
            }
            let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                .to_string_lossy()
                .into_owned();
            out.push((name, ip));
        }
        libc::freeifaddrs(head);
    }
    out
}

/// True for `localhost`, `127.x`, `::1` — hosts a phone cannot use.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Split a `Host` header into host and its explicit port, if any.
pub fn split_host(header: &str) -> (String, Option<u16>) {
    if let Some(rest) = header.strip_prefix('[') {
        // [v6]:port
        if let Some((h, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return (format!("[{h}]"), port);
        }
    }
    match header.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h.to_string(), p.parse().ok()),
        _ => (header.to_string(), None),
    }
}

/// The QR code for `text` as a standalone SVG.
pub fn qr_svg(text: &str) -> Option<String> {
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::M).ok()?;
    Some(
        code.render::<qrcode::render::svg::Color>()
            .min_dimensions(240, 240)
            .quiet_zone(true)
            .dark_color(qrcode::render::svg::Color("#000000"))
            .light_color(qrcode::render::svg::Color("#ffffff"))
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_redeems_once() {
        let p = Pairing::new(true);
        let code = p.issue();
        assert!(p.is_live(&code));
        assert!(p.redeem(&code));
        assert!(!p.redeem(&code));
        assert!(!p.is_live(&code));
        assert!(!p.redeem("not-a-code"));
    }

    #[test]
    fn outstanding_codes_are_capped() {
        let p = Pairing::new(true);
        let first = p.issue();
        for _ in 0..MAX_CODES {
            p.issue();
        }
        assert!(!p.redeem(&first), "the oldest code is dropped past the cap");
    }

    #[test]
    fn device_key_changes_with_the_password() {
        let s = b"secret";
        assert_ne!(device_key(s, Some("a")), device_key(s, Some("b")));
        assert_ne!(device_key(s, Some("a")), s.to_vec());
        assert_eq!(device_key(s, Some("a")), device_key(s, Some("a")));
    }

    #[test]
    fn host_header_parsing() {
        assert_eq!(
            split_host("192.168.1.11:45432"),
            ("192.168.1.11".into(), Some(45432))
        );
        assert_eq!(split_host("mac.local"), ("mac.local".into(), None));
        assert_eq!(split_host("[::1]:8080"), ("[::1]".into(), Some(8080)));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(!is_loopback_host("192.168.1.11"));
    }

    #[test]
    fn lan_addresses_skip_loopback() {
        assert!(lan_addresses().iter().all(|(_, ip)| !ip.is_loopback()));
    }

    #[test]
    fn qr_renders_svg() {
        let svg = qr_svg("http://192.168.1.11:45432/pair?c=abc").unwrap();
        assert!(svg.contains("<svg"));
    }
}
