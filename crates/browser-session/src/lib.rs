//! Interoperable cookie-use v1/v2 .cusession codec. No filesystem or browser I/O.
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{engine::general_purpose::STANDARD, Engine};
use rand::RngCore;
use serde_json::{json, Value};
use std::ffi::{c_char, CStr, CString};
use zeroize::Zeroizing;

const MAX_BYTES: usize = 8 * 1024 * 1024;
#[cfg(feature = "desktop")]
pub mod desktop;

pub fn seal_message(value: &Value, key: &[u8; 32], context: &[u8]) -> Result<String, &'static str> {
    use aes_gcm::aead::Payload;
    let bytes = serde_json::to_vec(value).map_err(|_| "invalid_message")?;
    if bytes.len() > MAX_BYTES {
        return Err("message_too_large");
    }
    let mut nonce = [0; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "invalid_key")?;
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &bytes,
                aad: context,
            },
        )
        .map_err(|_| "encrypt_failed")?;
    let mut combined = nonce.to_vec();
    combined.extend(encrypted);
    Ok(STANDARD.encode(combined))
}

pub fn open_message(value: &str, key: &[u8; 32], context: &[u8]) -> Result<Value, &'static str> {
    use aes_gcm::aead::Payload;
    if value.len() > MAX_BYTES * 2 {
        return Err("message_too_large");
    }
    let bytes = STANDARD.decode(value).map_err(|_| "invalid_message")?;
    if bytes.len() < 28 {
        return Err("invalid_message");
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "invalid_key")?;
    let plain = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&bytes[..12]),
                Payload {
                    msg: &bytes[12..],
                    aad: context,
                },
            )
            .map_err(|_| "authentication_failed")?,
    );
    serde_json::from_slice(&plain).map_err(|_| "invalid_message")
}

fn derive(password: &[u8], salt: &[u8]) -> Result<Zeroizing<[u8; 32]>, &'static str> {
    if password.is_empty() || password.len() > 4096 || salt.len() != 16 {
        return Err("invalid_password_or_salt");
    }
    let mut key = Zeroizing::new([0; 32]);
    let params = Params::new(65536, 3, 1, Some(32)).map_err(|_| "invalid_kdf")?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password, salt, &mut *key)
        .map_err(|_| "invalid_kdf")?;
    Ok(key)
}

pub fn open(bytes: &[u8], password: &[u8]) -> Result<Value, &'static str> {
    if bytes.len() > MAX_BYTES {
        return Err("bundle_too_large");
    }
    let bundle: Value = serde_json::from_slice(bytes).map_err(|_| "invalid_bundle")?;
    let version = bundle["cookie_use_bundle"]
        .as_u64()
        .ok_or("unsupported_bundle")?;
    if !matches!(version, 1 | 2) || bundle["kdf"] != "argon2id" {
        return Err("unsupported_bundle");
    }
    let decode = |key: &str| {
        STANDARD
            .decode(bundle[key].as_str().ok_or("invalid_bundle")?)
            .map_err(|_| "invalid_bundle")
    };
    let salt = decode("salt")?;
    let encrypted = decode("ciphertext")?;
    if encrypted.len() < 28 || encrypted.len() > MAX_BYTES {
        return Err("invalid_bundle");
    }
    let key = derive(password, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&*key).map_err(|_| "invalid_key")?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(Nonce::from_slice(&encrypted[..12]), &encrypted[12..])
            .map_err(|_| "wrong_password_or_corrupt_bundle")?,
    );
    let payload: Value = serde_json::from_slice(&plaintext).map_err(|_| "invalid_payload")?;
    let payload = if version == 1 {
        json!({"accounts": [payload]})
    } else {
        payload
    };
    let accounts = payload["accounts"].as_array().ok_or("invalid_accounts")?;
    if accounts.len() > 100 {
        return Err("too_many_accounts");
    }
    for account in accounts {
        if account["id"].as_str().is_none()
            || account["site"].as_str().is_none()
            || account["cookies"]
                .as_array()
                .is_none_or(|cookies| cookies.len() > 10_000)
        {
            return Err("invalid_account");
        }
    }
    Ok(payload)
}

pub fn seal(payload: &Value, password: &[u8]) -> Result<Vec<u8>, &'static str> {
    let accounts = payload["accounts"].as_array().ok_or("invalid_accounts")?;
    if accounts.len() > 100 {
        return Err("too_many_accounts");
    }
    let plaintext = Zeroizing::new(serde_json::to_vec(payload).map_err(|_| "invalid_payload")?);
    if plaintext.len() > MAX_BYTES / 2 {
        return Err("bundle_too_large");
    }
    let mut salt = [0; 16];
    let mut nonce = [0; 12];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce);
    let key = derive(password, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&*key).map_err(|_| "invalid_key")?;
    let encrypted = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
        .map_err(|_| "encrypt_failed")?;
    let mut combined = nonce.to_vec();
    combined.extend(encrypted);
    serde_json::to_vec(&json!({"cookie_use_bundle": 2, "accounts": accounts.iter().map(|a| json!({"id":a["id"],"site":a["site"]})).collect::<Vec<_>>(),
        "kdf":"argon2id", "salt":STANDARD.encode(salt), "ciphertext":STANDARD.encode(combined)})).map_err(|_| "invalid_bundle")
}

/// Input/output are UTF-8 JSON. The caller owns passwords; no secrets are logged.
/// A returned C string is released exactly once with ipu_session_free.
///
/// # Safety
/// Non-null inputs must point to valid NUL-terminated UTF-8 strings for this call.
#[no_mangle]
pub unsafe extern "C" fn ipu_session_open(
    bundle: *const c_char,
    password: *const c_char,
) -> *mut c_char {
    ffi(bundle, password, false)
}

/// Encrypt UTF-8 JSON for a cookie-use handoff.
///
/// # Safety
/// Non-null inputs must point to valid NUL-terminated UTF-8 strings for this call.
#[no_mangle]
pub unsafe extern "C" fn ipu_session_seal(
    payload: *const c_char,
    password: *const c_char,
) -> *mut c_char {
    ffi(payload, password, true)
}

unsafe fn ffi(input: *const c_char, password: *const c_char, encrypt: bool) -> *mut c_char {
    let result = std::panic::catch_unwind(|| {
        if input.is_null() || password.is_null() {
            return json!({"ok":false,"error":"invalid_input"});
        }
        let bytes = CStr::from_ptr(input).to_bytes();
        let password = CStr::from_ptr(password).to_bytes();
        let result = if encrypt {
            serde_json::from_slice(bytes)
                .map_err(|_| "invalid_payload")
                .and_then(|v| seal(&v, password))
                .and_then(|b| serde_json::from_slice(&b).map_err(|_| "invalid_bundle"))
        } else {
            open(bytes, password)
        };
        match result {
            Ok(payload) => json!({"ok":true,"payload":payload}),
            Err(error) => json!({"ok":false,"error":error}),
        }
    })
    .unwrap_or_else(|_| json!({"ok":false,"error":"codec_failed"}));
    CString::new(result.to_string()).unwrap().into_raw()
}

/// Release and erase a returned codec buffer.
///
/// # Safety
/// A non-null pointer must be returned by this codec and not previously freed.
#[no_mangle]
pub unsafe extern "C" fn ipu_session_free(value: *mut c_char) {
    if !value.is_null() {
        let value = CString::from_raw(value);
        let mut bytes = value.into_bytes_with_nul();
        zeroize::Zeroize::zeroize(&mut bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_cookie_use_v1_fixture() {
        let payload = open(
            include_bytes!("../tests/fixtures/synthetic-v1.cusession"),
            b"synthetic-cookie-use-password",
        )
        .unwrap();
        assert_eq!(payload["accounts"][0]["id"], "fixture/example");
        assert_eq!(
            payload["accounts"][0]["cookies"][0]["value"],
            "synthetic-cookie-value"
        );
    }
    #[test]
    fn shared_format_roundtrip_and_tamper() {
        let payload = json!({"accounts":[{"id":"fixture/work","site":"example.com","cookies":[{"name":"sid","value":"synthetic-secret","domain":".example.com"}]}],"browser_data":{"bookmarks":[]}});
        let sealed = seal(&payload, b"synthetic-long-password").unwrap();
        assert!(!String::from_utf8_lossy(&sealed).contains("synthetic-secret"));
        assert_eq!(open(&sealed, b"synthetic-long-password").unwrap(), payload);
        assert!(open(&sealed, b"wrong-password").is_err());
        let mut changed: Value = serde_json::from_slice(&sealed).unwrap();
        let mut cipher = STANDARD
            .decode(changed["ciphertext"].as_str().unwrap())
            .unwrap();
        cipher[15] ^= 1;
        changed["ciphertext"] = json!(STANDARD.encode(cipher));
        assert!(open(
            &serde_json::to_vec(&changed).unwrap(),
            b"synthetic-long-password"
        )
        .is_err());
    }
    #[test]
    fn rejects_unbounded_or_unknown_inputs() {
        assert!(open(&vec![0; MAX_BYTES + 1], b"x").is_err());
        assert!(open(br#"{"cookie_use_bundle":9}"#, b"x").is_err());
    }
}
