//! App Store Connect API, for the legacy (iOS 15/16) runner's signing.
//!
//! From Xcode 26 on, Xcode cannot see an iOS 15/16 phone, so it never
//! registers it or puts it in a provisioning profile. Setup does both itself
//! with the same API key `WDA_ASC_*` signing already uses: register the
//! phone's UDID (`POST /v1/devices`) and keep one development profile of its
//! own for the runner (`POST /v1/profiles`), recreated whenever it lacks this
//! phone or the signing certificate, or is close to expiring. A profile cannot
//! be edited, only replaced.
//!
//! The key is read from `WDA_ASC_KEY_PATH` and never logged; requests carry a
//! 20-minute ES256 token.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};

const API: &str = "https://api.appstoreconnect.apple.com/v1";
const TIMEOUT: Duration = Duration::from_secs(60);

/// An authenticated API client.
pub struct Asc {
    key_id: String,
    issuer: String,
    key: EcdsaKeyPair,
    rng: SystemRandom,
    http: reqwest::Client,
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The DER inside a PEM file (`-----BEGIN PRIVATE KEY-----` …).
pub fn pem_body(pem: &str) -> Result<Vec<u8>> {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .map(str::trim)
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .context("the API key is not PEM")
}

impl Asc {
    pub fn new(key_path: &Path, key_id: &str, issuer: &str) -> Result<Asc> {
        let pem = std::fs::read_to_string(key_path)
            .with_context(|| format!("read the App Store Connect key {}", key_path.display()))?;
        let der = pem_body(&pem)?;
        let rng = SystemRandom::new();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &rng)
            .map_err(|_| anyhow!("the App Store Connect key is not a P-256 PKCS#8 key"))?;
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .context("HTTP client")?;
        Ok(Asc {
            key_id: key_id.to_string(),
            issuer: issuer.to_string(),
            key,
            rng,
            http,
        })
    }

    fn token(&self) -> Result<String> {
        let now = super::retry::now();
        let header = json!({"alg": "ES256", "kid": self.key_id, "typ": "JWT"});
        let claims =
            json!({"iss": self.issuer, "iat": now, "exp": now + 1200, "aud": "appstoreconnect-v1"});
        let signing_input = format!(
            "{}.{}",
            b64url(header.to_string().as_bytes()),
            b64url(claims.to_string().as_bytes())
        );
        let signature = self
            .key
            .sign(&self.rng, signing_input.as_bytes())
            .map_err(|_| anyhow!("could not sign the App Store Connect token"))?;
        Ok(format!("{signing_input}.{}", b64url(signature.as_ref())))
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let url = if path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{API}{path}")
        };
        let mut request = self
            .http
            .request(method.clone(), &url)
            .bearer_auth(self.token()?);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("{method} {path}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("errors")?.as_array().map(|errors| {
                        errors
                            .iter()
                            .filter_map(|e| e.get("detail").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                })
                .unwrap_or_default();
            bail!("App Store Connect {method} {path}: HTTP {status} {detail}");
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).context("App Store Connect answered non-JSON")
    }

    async fn get(&self, path: &str) -> Result<Vec<Value>> {
        let reply = self.call(reqwest::Method::GET, path, None).await?;
        Ok(reply
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// The registered device id for `udid`, registering it (as `name`) when
    /// the team does not have it yet.
    pub async fn ensure_device(&self, udid: &str, name: &str) -> Result<(String, bool)> {
        let found = self
            .get(&format!("/devices?filter[udid]={udid}&limit=20"))
            .await?;
        if let Some(device) = found
            .iter()
            .find(|d| attr(d, "udid").is_some_and(|u| u.eq_ignore_ascii_case(udid)))
        {
            if attr(device, "status") == Some("DISABLED") {
                bail!("this iPhone ({udid}) is registered to the team but disabled in the Apple Developer portal; enable it there");
            }
            return Ok((id(device)?, false));
        }
        let created = self
            .call(
                reqwest::Method::POST,
                "/devices",
                Some(json!({"data": {"type": "devices", "attributes": {
                    "name": name, "udid": udid, "platform": "IOS"}}})),
            )
            .await?;
        Ok((id(created.get("data").unwrap_or(&Value::Null))?, true))
    }

    /// Development certificates of the team (id, DER).
    pub async fn development_certificates(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let mut out = Vec::new();
        for cert in self.get("/certificates?limit=200").await? {
            let kind = attr(&cert, "certificateType").unwrap_or_default();
            if !matches!(kind, "DEVELOPMENT" | "IOS_DEVELOPMENT") {
                continue;
            }
            let Some(content) = attr(&cert, "certificateContent") else {
                continue;
            };
            let der = base64::engine::general_purpose::STANDARD
                .decode(content)
                .unwrap_or_default();
            if !der.is_empty() {
                out.push((id(&cert)?, der));
            }
        }
        Ok(out)
    }

    /// The bundle id resource a runner profile can use: one matching
    /// `identifier` exactly, else a wildcard that covers it (Xcode's own
    /// `*`), else a new explicit one.
    pub async fn bundle_id_for(&self, identifier: &str) -> Result<String> {
        let all = self
            .get("/bundleIds?filter[platform]=IOS&limit=200")
            .await?;
        let covers = |pattern: &str| {
            pattern == identifier
                || pattern == "*"
                || pattern
                    .strip_suffix('*')
                    .is_some_and(|prefix| identifier.starts_with(prefix))
        };
        if let Some(exact) = all
            .iter()
            .find(|b| attr(b, "identifier") == Some(identifier))
        {
            return id(exact);
        }
        if let Some(wild) = all
            .iter()
            .find(|b| attr(b, "identifier").is_some_and(covers))
        {
            return id(wild);
        }
        let created = self
            .call(
                reqwest::Method::POST,
                "/bundleIds",
                Some(json!({"data": {"type": "bundleIds", "attributes": {
                    "identifier": identifier,
                    "name": format!("iphone-use runner {}", identifier.replace('.', " ")),
                    "platform": "IOS"}}})),
            )
            .await?;
        id(created.get("data").unwrap_or(&Value::Null))
    }

    /// Enabled iPhone/iPad device ids of the team.
    pub async fn ios_devices(&self) -> Result<Vec<String>> {
        let devices = self
            .get("/devices?filter[platform]=IOS&filter[status]=ENABLED&limit=200")
            .await?;
        Ok(devices
            .iter()
            .filter(|d| {
                !matches!(
                    attr(d, "deviceClass"),
                    Some("MAC") | Some("APPLE_TV") | Some("APPLE_WATCH") | Some("APPLE_VISION_PRO")
                )
            })
            .filter_map(|d| id(d).ok())
            .collect())
    }

    /// Replace the profile named `name` with a new development profile for
    /// `bundle_id` covering `devices` and `certificates`. Returns its
    /// `.mobileprovision` bytes.
    pub async fn recreate_profile(
        &self,
        name: &str,
        bundle_id: &str,
        certificates: &[String],
        devices: &[String],
    ) -> Result<Vec<u8>> {
        let encoded: String = name
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        for old in self
            .get(&format!("/profiles?filter[name]={encoded}&limit=20"))
            .await?
        {
            if attr(&old, "name") == Some(name) {
                self.call(
                    reqwest::Method::DELETE,
                    &format!("/profiles/{}", id(&old)?),
                    None,
                )
                .await?;
            }
        }
        let rel = |kind: &str, ids: &[String]| json!({"data": ids.iter().map(|i| json!({"type": kind, "id": i})).collect::<Vec<_>>()});
        let created = self
            .call(
                reqwest::Method::POST,
                "/profiles",
                Some(json!({"data": {
                "type": "profiles",
                "attributes": {"name": name, "profileType": "IOS_APP_DEVELOPMENT"},
                "relationships": {
                    "bundleId": {"data": {"type": "bundleIds", "id": bundle_id}},
                    "certificates": rel("certificates", certificates),
                    "devices": rel("devices", devices),
                }}})),
            )
            .await?;
        let content = created
            .pointer("/data/attributes/profileContent")
            .and_then(Value::as_str)
            .context("the new profile has no content")?;
        base64::engine::general_purpose::STANDARD
            .decode(content)
            .context("the new profile's content is not base64")
    }
}

fn attr<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
    item.get("attributes")?.get(key)?.as_str()
}

fn id(item: &Value) -> Result<String> {
    item.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("App Store Connect item without an id")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_bodies_decode() {
        let der = pem_body("-----BEGIN PRIVATE KEY-----\nAAEC\nAwQ=\n-----END PRIVATE KEY-----\n")
            .unwrap();
        assert_eq!(der, vec![0, 1, 2, 3, 4]);
        assert!(pem_body("-----BEGIN X-----\n@@@\n").is_err());
    }

    #[test]
    fn tokens_are_three_part_es256_jws() {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AuthKey_TEST.p8");
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref())
        );
        std::fs::write(&path, pem).unwrap();
        let asc = Asc::new(&path, "KEYID12345", "issuer-uuid").unwrap();
        let token = asc.token().unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[0])
            .unwrap();
        let header: Value = serde_json::from_slice(&header).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], "KEYID12345");
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[2])
            .unwrap();
        assert_eq!(signature.len(), 64, "JWS ES256 is r||s");
    }
}
