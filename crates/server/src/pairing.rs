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
use std::path::PathBuf;
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
    /// The port the daemon listens on, for the LAN addresses handed to a
    /// paired app (a request through a tunnel carries no usable port).
    pub port: Option<u16>,
    codes: Mutex<HashMap<String, Instant>>,
    /// A code a Mac dropped into the on-phone daemon's container over USB
    /// (`iphone-use setup-app --connect-to`), so another phone can connect
    /// without anyone tapping this one. Embedded daemon only.
    drop_file: std::sync::OnceLock<PathBuf>,
}

impl Pairing {
    pub fn new(lan_reachable: bool) -> Self {
        Self {
            lan_reachable,
            port: None,
            codes: Mutex::new(HashMap::new()),
            drop_file: std::sync::OnceLock::new(),
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// `http://<ip>:<port>` for every LAN address, best first, so a paired
    /// app that reached us through a public tunnel can go direct when it sits
    /// on the same network. Empty on a loopback-only daemon or with no port.
    pub fn lan_urls(&self, fallback_port: Option<u16>) -> Vec<String> {
        let Some(port) = self.port.or(fallback_port) else {
            return Vec::new();
        };
        if !self.lan_reachable {
            return Vec::new();
        }
        let mut urls: Vec<String> = lan_addresses()
            .into_iter()
            .map(|(_, ip)| format!("http://{ip}:{port}"))
            .collect();
        // DHCP addresses change when the Mac moves between networks. Keep
        // its Bonjour name as another candidate; clients still require the
        // pinned nonce proof before sending credentials to any candidate.
        #[cfg(target_os = "macos")]
        if let Some(host) = local_hostname() {
            urls.push(format!("http://{host}:{port}"));
        }
        urls
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
            // Keep the code lock through file-backed consumption too.
            None => self.dropped(code, true),
        }
    }

    /// Whether `code` is outstanding, without consuming it (the landing page).
    pub fn is_live(&self, code: &str) -> bool {
        let codes = self
            .codes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let live = codes
            .get(code)
            .is_some_and(|issued| issued.elapsed() < CODE_TTL);
        live || self.dropped(code, false)
    }

    /// Accept codes from `path`, a `{"code", "expires_at"}` file a Mac wrote.
    pub fn set_drop_file(&self, path: PathBuf) {
        let _ = self.drop_file.set(path);
    }

    /// Whether `code` matches the dropped code; `consume` spends it. An
    /// expired, unreadable or over-long-lived file is removed.
    fn dropped(&self, code: &str, consume: bool) -> bool {
        let Some(path) = self.drop_file.get() else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let parsed: Option<(String, u64)> = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| Some((v["code"].as_str()?.to_string(), v["expires_at"].as_u64()?)));
        let Some((dropped, expires_at)) = parsed else {
            let _ = std::fs::remove_file(path);
            return false;
        };
        if text.len() > 1024 || expires_at <= now || expires_at > now + CODE_TTL.as_secs() * 2 || dropped.len() < 16 {
            let _ = std::fs::remove_file(path);
            return false;
        }
        let matches = dropped.len() == code.len()
            && dropped.bytes().zip(code.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
        if matches && consume {
            // A successful comparison alone is not a committed redemption.
            return std::fs::remove_file(path).is_ok();
        }
        matches
    }
}

#[cfg(target_os = "macos")]
fn local_hostname() -> Option<String> {
    let mut name = [0u8; 256];
    // SAFETY: gethostname writes at most the supplied buffer length.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 { return None; }
    let end = name.iter().position(|byte| *byte == 0)?;
    stable_lan_hostname(std::str::from_utf8(&name[..end]).ok()?)
}

#[cfg(any(target_os = "macos", test))]
fn stable_lan_hostname(name: &str) -> Option<String> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() || name == "localhost" || name == "localhost.local"
        || name.parse::<std::net::IpAddr>().is_ok() { return None; }
    let host = if name.ends_with(".local") { name }
        else if !name.contains('.') { format!("{name}.local") } else { return None; };
    if host.len() > 253 || !host.split('.').all(|label| {
        !label.is_empty() && label.len() <= 63 && !label.starts_with('-') && !label.ends_with('-')
            && label.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
    }) { return None; }
    Some(host)
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

/// The key a paired app checks LAN addresses with (see [`lan_proof`]). It is
/// handed out only with a session, over the route the app paired on, and is
/// derived from the device key, so changing the password rotates it.
pub fn lan_key(device_key: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(b"iphone-use lan probe v1\0");
    h.update(device_key);
    h.finalize().to_vec()
}

/// The answer to a LAN probe's `nonce`: proof that this is the daemon the
/// app paired with, before the app sends it any credential in cleartext.
pub fn lan_proof(device_key: &[u8], nonce: &str) -> String {
    core::auth::sign(&lan_key(device_key), nonce)
}

/// A probe nonce: base64url or hex, 16–128 characters.
pub fn probe_nonce_ok(nonce: &str) -> bool {
    (16..=128).contains(&nonce.len())
        && nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
    #[test]
    fn stable_lan_names_survive_address_changes_without_using_public_dns_names() {
        assert_eq!(super::stable_lan_hostname("My-Mac.local."), Some("my-mac.local".into()));
        assert_eq!(super::stable_lan_hostname("My-Mac"), Some("my-mac.local".into()));
        for invalid in ["", "localhost", "localhost.local", "127.0.0.1", "::1",
            "mac.example.com", "bad/name", "bad..local", "-bad.local"] {
            assert_eq!(super::stable_lan_hostname(invalid), None, "{invalid}");
        }
    }
    use super::*;

    struct DropTestDir(std::path::PathBuf);
    impl Drop for DropTestDir {
        fn drop(&mut self) {
            #[cfg(unix)] {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn dropped_test_dir(label: &str) -> DropTestDir {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("ipu-drop-{label}-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        DropTestDir(dir)
    }
    #[test]
    fn dropped_code_has_one_winner_under_concurrent_redeems() {
        let dir = dropped_test_dir("concurrent");
        let file = dir.0.join("code.json");
        let pairing = std::sync::Arc::new(Pairing::new(true));
        pairing.set_drop_file(file.clone());
        for round in 0..16 {
            let code = format!("concurrent-import-code-{round:016x}");
            std::fs::write(&file, serde_json::json!({"code":code,"expires_at":unix_now()+120}).to_string()).unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let threads: Vec<_> = (0..8).map(|_| {
                let pairing=pairing.clone(); let barrier=barrier.clone(); let code=code.clone();
                std::thread::spawn(move || { barrier.wait(); pairing.redeem(&code) })
            }).collect();
            let winners = threads.into_iter().map(|thread| usize::from(thread.join().unwrap())).sum::<usize>();
            assert_eq!(winners,1,"one dropped code must issue at most one grant");
            assert!(!file.exists());
        }
    }
    #[cfg(unix)]
    #[test]
    fn dropped_code_refuses_if_the_file_cannot_be_consumed() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("directory permission refusal requires a non-root test user");
            return;
        }
        let dir = dropped_test_dir("unlink"); let file=dir.0.join("code.json");
        let pairing=Pairing::new(true); pairing.set_drop_file(file.clone());
        let code="non-consumable-import-code";
        std::fs::write(&file,serde_json::json!({"code":code,"expires_at":unix_now()+120}).to_string()).unwrap();
        std::fs::set_permissions(&dir.0,std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(!pairing.redeem(code));
        assert!(file.exists());
        assert!(!pairing.redeem(code));
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    }

    #[test]
    fn a_dropped_code_redeems_once_and_expired_ones_are_removed() {
        let dir = std::env::temp_dir().join(format!("ipu-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("connect-code-import.json");
        let p = Pairing::new(true);
        p.set_drop_file(file.clone());
        let code = "AbCdEfGhIjKlMnOpQrStUv";
        std::fs::write(&file, format!(r#"{{"code":"{code}","expires_at":{}}}"#, unix_now() + 120)).unwrap();
        assert!(!p.is_live("AbCdEfGhIjKlMnOpQrStUx"));
        assert!(p.is_live(code));
        assert!(p.redeem(code));
        assert!(!file.exists());
        assert!(!p.redeem(code));
        std::fs::write(&file, format!(r#"{{"code":"{code}","expires_at":{}}}"#, unix_now() - 1)).unwrap();
        assert!(!p.is_live(code));
        assert!(!file.exists());
        std::fs::write(&file, format!(r#"{{"code":"{code}","expires_at":{}}}"#, unix_now() + 86400)).unwrap();
        assert!(!p.redeem(code));
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn lan_proof_depends_on_the_key_and_the_nonce() {
        let a = device_key(b"secret", Some("a"));
        let b = device_key(b"secret", Some("b"));
        let n = "0123456789abcdef0123";
        assert_eq!(lan_proof(&a, n), lan_proof(&a, n));
        assert_ne!(lan_proof(&a, n), lan_proof(&b, n));
        assert_ne!(lan_proof(&a, n), lan_proof(&a, "0123456789abcdef0124"));
        assert!(probe_nonce_ok(n));
        assert!(!probe_nonce_ok("short"));
        assert!(!probe_nonce_ok("0123456789abcdef/123"));
    }

    #[test]
    fn lan_urls_carry_the_daemon_port() {
        let p = Pairing::new(true).with_port(44321);
        let urls = p.lan_urls(Some(9999));
        #[cfg(target_os = "macos")]
        let aliases = if let Some(host) = super::local_hostname() {
            assert!(urls.contains(&format!("http://{host}:44321")));
            1
        } else { 0 };
        #[cfg(not(target_os = "macos"))]
        let aliases = 0;
        assert_eq!(urls.len(), lan_addresses().len() + aliases);
        assert!(urls
            .iter()
            .all(|u| u.starts_with("http://") && u.ends_with(":44321")));
        let p = Pairing::new(true);
        assert!(p
            .lan_urls(Some(45432))
            .iter()
            .all(|u| u.ends_with(":45432")));
        assert!(p.lan_urls(None).is_empty(), "no port, no address");
        assert!(Pairing::new(false)
            .with_port(44321)
            .lan_urls(None)
            .is_empty());
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
