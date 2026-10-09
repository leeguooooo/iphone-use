//! Request signing for the on-phone device runner.
//!
//! The runner (`runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerHTTP.swift`,
//! `RunnerAuth`) listens on every interface of the phone, so anything on the
//! same Wi-Fi can reach its ports. Each runner launch gets a fresh random
//! token from the Mac (`IPU_RUNNER_TOKEN` in the test environment; the Mac
//! keeps it in `<state dir>/runner-token`, mode 0600). Every request to the
//! control port (8100) and the video port (9100) then carries
//!
//! ```text
//! Authorization: IPU-HMAC-SHA256 ts=<unix secs>, nonce=<32 hex>, sig=<64 hex>
//! sig = hex(HMAC-SHA256(token, "ipu-runner-v1\n" METHOD "\n" TARGET "\n" ts "\n" nonce "\n" hex(SHA-256(body))))
//! ```
//!
//! TARGET is the request target exactly as sent on the request line (path
//! plus `?query`). The token itself never crosses the network: a LAN relay
//! sends plain HTTP, and a bearer token there would be readable by anyone who
//! can see the Wi-Fi traffic. A signature binds one request (method, target,
//! body); the runner accepts each nonce once and only within
//! [`MAX_SKEW_SECS`] of its own clock, so a captured request cannot be
//! replayed or altered.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// The runner's environment variable that carries the token.
pub const TOKEN_ENV: &str = "IPU_RUNNER_TOKEN";
/// xcodebuild passes `TEST_RUNNER_<NAME>` to the test runner as `<NAME>`.
pub const XCODEBUILD_TOKEN_ENV: &str = "TEST_RUNNER_IPU_RUNNER_TOKEN";
/// The token's file in the instance state dir.
pub const TOKEN_FILE: &str = "runner-token";
/// The Authorization scheme.
pub const SCHEME: &str = "IPU-HMAC-SHA256";
/// How far a request's timestamp may be from the runner's clock.
pub const MAX_SKEW_SECS: i64 = 900;
const CONTEXT: &str = "ipu-runner-v1";

/// A token is 32–128 lowercase hex characters (setup writes 64).
pub fn valid_token(token: &str) -> bool {
    (32..=128).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_nonce(nonce: &str) -> bool {
    (16..=64).contains(&nonce.len()) && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0xf)] as char);
    }
    out
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A fresh token: 32 random bytes as hex.
pub fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("no system randomness: {e}"))?;
    Ok(hex(&bytes))
}

fn new_nonce() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // Never reached on macOS; a nonce only has to be unique, and a
        // clock + counter mix still is within this process.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default() as u64;
        bytes[..8].copy_from_slice(&t.to_le_bytes());
        bytes[8..].copy_from_slice(&(n ^ u64::from(std::process::id())).to_le_bytes());
    }
    hex(&bytes)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// What is signed.
pub fn string_to_sign(method: &str, target: &str, ts: i64, nonce: &str, body: &[u8]) -> String {
    format!(
        "{CONTEXT}\n{}\n{target}\n{ts}\n{nonce}\n{}",
        method.to_ascii_uppercase(),
        hex(&Sha256::digest(body))
    )
}

fn mac(token: &str, method: &str, target: &str, ts: i64, nonce: &str, body: &[u8]) -> Vec<u8> {
    let mut mac =
        HmacSha256::new_from_slice(token.as_bytes()).expect("HMAC accepts any key length");
    mac.update(string_to_sign(method, target, ts, nonce, body).as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// The Authorization value for one request, with a given time and nonce.
pub fn authorization_at(
    token: &str,
    method: &str,
    target: &str,
    body: &[u8],
    ts: i64,
    nonce: &str,
) -> String {
    let sig = hex(&mac(token, method, target, ts, nonce, body));
    format!("{SCHEME} ts={ts}, nonce={nonce}, sig={sig}")
}

/// The Authorization value for one request sent now.
pub fn authorization(token: &str, method: &str, target: &str, body: &[u8]) -> String {
    authorization_at(token, method, target, body, unix_now(), &new_nonce())
}

/// The parts of an Authorization value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub ts: i64,
    pub nonce: String,
    pub sig: Vec<u8>,
}

/// Parse `IPU-HMAC-SHA256 ts=…, nonce=…, sig=…` (any order, each once).
pub fn parse_authorization(value: &str) -> Option<Credentials> {
    let rest = value.trim().strip_prefix(SCHEME)?;
    if !rest.starts_with(' ') {
        return None;
    }
    let (mut ts, mut nonce, mut sig) = (None, None, None);
    for part in rest.split(',') {
        let (key, val) = part.trim().split_once('=')?;
        let slot = match key {
            "ts" => &mut ts,
            "nonce" => &mut nonce,
            "sig" => &mut sig,
            _ => return None,
        };
        if slot.replace(val.to_string()).is_some() {
            return None;
        }
    }
    let ts: i64 = ts?.parse().ok()?;
    let nonce = nonce?;
    let sig = sig?;
    if !valid_nonce(&nonce) || sig.len() != 64 {
        return None;
    }
    Some(Credentials {
        ts,
        nonce: nonce.to_ascii_lowercase(),
        sig: unhex(&sig)?,
    })
}

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Missing,
    Malformed,
    Skew,
    BadSignature,
}

/// Check one request's Authorization value (no replay memory: that is the
/// runner's job; tests and the fake runner use this).
pub fn verify(
    token: &str,
    method: &str,
    target: &str,
    body: &[u8],
    authorization: Option<&str>,
    now: i64,
) -> Result<Credentials, Refusal> {
    let value = authorization.ok_or(Refusal::Missing)?;
    let credentials = parse_authorization(value).ok_or(Refusal::Malformed)?;
    if (now - credentials.ts).abs() > MAX_SKEW_SECS {
        return Err(Refusal::Skew);
    }
    let expected = mac(
        token,
        method,
        target,
        credentials.ts,
        &credentials.nonce,
        body,
    );
    if bool::from(expected.ct_eq(&credentials.sig)) {
        Ok(credentials)
    } else {
        Err(Refusal::BadSignature)
    }
}

/// The token in `path`, when the file holds a valid one.
pub fn read_token_file(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let token = text.trim();
    valid_token(token).then(|| token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn known_answer_matches_the_runner_and_the_scripts() {
        // The same vector is checked in runner/unit-check/main.swift and
        // scripts/runner_auth.py: all three must sign identically.
        let value = authorization_at(
            TOKEN,
            "post",
            "/session/S/actions?x=1",
            br#"{"a":1}"#,
            1_700_000_000,
            "0123456789abcdef0123456789abcdef",
        );
        assert_eq!(
            value,
            "IPU-HMAC-SHA256 ts=1700000000, nonce=0123456789abcdef0123456789abcdef, sig=d45cade8fef49833329384ea5669106fce22a98fcc182db1ff3dc0eb946e6e11"
        );
    }

    #[test]
    fn a_signed_request_verifies_and_any_change_does_not() {
        let now = 1_700_000_000;
        let nonce = "0123456789abcdef0123456789abcdef";
        let value = authorization_at(TOKEN, "POST", "/wda/tap", b"{}", now, nonce);
        assert!(verify(TOKEN, "POST", "/wda/tap", b"{}", Some(&value), now).is_ok());
        assert!(verify(TOKEN, "POST", "/wda/tap", b"{}", Some(&value), now + 60).is_ok());
        for (method, target, body) in [
            ("GET", "/wda/tap", &b"{}"[..]),
            ("POST", "/wda/tap?x", &b"{}"[..]),
            ("POST", "/wda/tap", &b"{ }"[..]),
        ] {
            assert_eq!(
                verify(TOKEN, method, target, body, Some(&value), now),
                Err(Refusal::BadSignature)
            );
        }
        let other = "ff".repeat(32);
        assert_eq!(
            verify(&other, "POST", "/wda/tap", b"{}", Some(&value), now),
            Err(Refusal::BadSignature)
        );
        assert_eq!(
            verify(
                TOKEN,
                "POST",
                "/wda/tap",
                b"{}",
                Some(&value),
                now + MAX_SKEW_SECS + 1
            ),
            Err(Refusal::Skew)
        );
        assert_eq!(
            verify(TOKEN, "POST", "/wda/tap", b"{}", None, now),
            Err(Refusal::Missing)
        );
        // A bearer token is not a signature: the token never travels.
        assert_eq!(
            verify(
                TOKEN,
                "POST",
                "/wda/tap",
                b"{}",
                Some(&format!("Bearer {TOKEN}")),
                now
            ),
            Err(Refusal::Malformed)
        );
    }

    #[test]
    fn parsing_is_strict() {
        let sig = "ab".repeat(32);
        let ok = format!("{SCHEME} ts=1, nonce=0123456789abcdef, sig={sig}");
        assert!(parse_authorization(&ok).is_some());
        let reordered = format!("{SCHEME} sig={sig},nonce=0123456789abcdef,ts=1");
        assert!(parse_authorization(&reordered).is_some());
        for bad in [
            format!("{SCHEME}ts=1, nonce=0123456789abcdef, sig={sig}"),
            format!("{SCHEME} ts=1, ts=2, nonce=0123456789abcdef, sig={sig}"),
            format!("{SCHEME} ts=x, nonce=0123456789abcdef, sig={sig}"),
            format!("{SCHEME} ts=1, nonce=short, sig={sig}"),
            format!("{SCHEME} ts=1, nonce=0123456789abcdef, sig=abcd"),
            format!("{SCHEME} ts=1, nonce=0123456789abcdef, sig={sig}, extra=1"),
            format!("{SCHEME} ts=1, nonce=0123456789abcdef"),
            String::new(),
        ] {
            assert!(parse_authorization(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn tokens_are_fresh_hex_and_files_must_hold_one() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert!(valid_token(&a) && a.len() == 64);
        assert_ne!(a, b);
        assert!(!valid_token("short"));
        assert!(!valid_token(&"G".repeat(64)));
        assert!(
            !valid_token(&"A".repeat(64)),
            "upper case is not what setup writes"
        );
        let dir = std::env::temp_dir().join(format!("ipu-runner-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(TOKEN_FILE);
        std::fs::write(&file, format!("{a}\n")).unwrap();
        assert_eq!(read_token_file(&file).as_deref(), Some(a.as_str()));
        std::fs::write(&file, "not a token").unwrap();
        assert_eq!(read_token_file(&file), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
