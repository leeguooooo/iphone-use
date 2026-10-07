//! Device facts straight from the iPhone's lockdownd (TCP 62078 through
//! macOS's usbmuxd), so setup does not spawn `xcrun devicectl` for them.
//!
//! `devicectl` answers in 0.2–0.3 s over USB but goes through CoreDevice,
//! which over Wi-Fi took over a minute on one phone. Lockdown over usbmuxd is
//! a local socket: the plain (session-less) values come back in ~20 ms, and
//! the developer-disk-image state in ~0.15 s including a TLS session.
//!
//! - [`device_info`]: name, iOS version, model, build. Session-less GetValue,
//!   which lockdownd answers for these keys without pairing.
//! - [`ddi_status`]: whether the Developer Disk Image is mounted, from the
//!   `com.apple.mobile.mobile_image_mounter` service (needs a paired session).
//!
//! Lockdown cannot tell whether the phone is locked right now: its
//! `PasswordProtected` value says whether a passcode is *set*, not whether one
//! is required. Setup keeps `devicectl device info lockState` for that.
//!
//! Framing: a 4-byte big-endian length, then an XML property list. The TLS
//! session pins the device certificate from the pairing record and presents
//! the host certificate, the same identity Xcode and Finder use.

use crate::usbmux::{self, Value};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const LOCKDOWN_PORT: u16 = 62078;
const IMAGE_MOUNTER: &str = "com.apple.mobile.mobile_image_mounter";
const LABEL: &str = "iphone-use";
/// One request/reply exchange; lockdownd answers in milliseconds.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MESSAGE: usize = 4 << 20;
/// Where iOS 17+ mounts the personalized developer image.
const DEVELOPER_MOUNT_PATH: &str = "/System/Developer";

/// The phone is not attached to usbmuxd at all (unplugged, or a Wi-Fi-only
/// CoreDevice connection). Callers fall back to `devicectl` on this error.
#[derive(Debug)]
pub struct NotAttached(pub String);

impl std::fmt::Display for NotAttached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "iPhone {} is not attached to usbmuxd", self.0)
    }
}

impl std::error::Error for NotAttached {}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: Option<String>,
    pub product_version: Option<String>,
    pub product_type: Option<String>,
    pub build_version: Option<String>,
    /// `usb` or `network`, as usbmuxd lists the attachment.
    pub connection: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DdiStatus {
    pub udid: String,
    pub mounted: bool,
    /// `Personalized` (iOS 17+) or `Developer` (older iOS), when mounted.
    pub image_type: Option<String>,
    pub image_version: Option<String>,
    pub connection: &'static str,
}

/// Name, iOS version, model and build, without a lockdown session.
pub async fn device_info(udid: &str) -> Result<DeviceInfo> {
    let attached = attached(udid).await?;
    let mut stream = usbmux::connect(attached.device_id, LOCKDOWN_PORT)
        .await
        .context("connect to lockdownd")?;
    let mut values = Vec::new();
    for key in [
        "DeviceName",
        "ProductVersion",
        "ProductType",
        "BuildVersion",
    ] {
        let body = request(&[("Label", LABEL), ("Request", "GetValue"), ("Key", key)]);
        let reply = exchange(&mut stream, &body).await?;
        values.push((
            key,
            reply
                .get("Value")
                .and_then(Value::as_str)
                .map(str::to_string),
        ));
    }
    let take = |key: &str| {
        values
            .iter()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.clone())
    };
    Ok(DeviceInfo {
        udid: attached.serial.clone(),
        name: take("DeviceName"),
        product_version: take("ProductVersion"),
        product_type: take("ProductType"),
        build_version: take("BuildVersion"),
        connection: connection(&attached),
    })
}

/// Whether the Developer Disk Image is mounted, through a paired session.
pub async fn ddi_status(udid: &str) -> Result<DdiStatus> {
    let attached = attached(udid).await?;
    let pair = PairRecord::parse(&usbmux::read_pair_record(&attached.serial).await?)?;
    let tls = Arc::new(pair.tls_config()?);

    let mut plain = usbmux::connect(attached.device_id, LOCKDOWN_PORT)
        .await
        .context("connect to lockdownd")?;
    let reply = exchange(
        &mut plain,
        &request(&[
            ("Label", LABEL),
            ("Request", "StartSession"),
            ("HostID", &pair.host_id),
            ("SystemBUID", &pair.system_buid),
        ]),
    )
    .await?;
    lockdown_error(&reply, "StartSession")?;
    let session_ssl = reply.get("EnableSessionSSL").and_then(Value::as_bool) == Some(true);
    let service = if session_ssl {
        let mut session = handshake(&tls, plain)
            .await
            .context("lockdown TLS session")?;
        start_service(&mut session).await?
    } else {
        start_service(&mut plain).await?
    };

    let raw = usbmux::connect(attached.device_id, service.port)
        .await
        .context("connect to the image mounter")?;
    let (mounted, image_type, image_version) = if service.ssl {
        let mut stream = handshake(&tls, raw).await.context("image mounter TLS")?;
        query_mounted(&mut stream).await?
    } else {
        let mut stream = raw;
        query_mounted(&mut stream).await?
    };
    Ok(DdiStatus {
        udid: attached.serial.clone(),
        mounted,
        image_type,
        image_version,
        connection: connection(&attached),
    })
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RunnerStatus {
    pub udid: String,
    pub port: u16,
    /// The runner's per-launch session id: a new runner process answers with
    /// a new one, which is how setup tells it from one still exiting.
    pub session_id: Option<String>,
    pub state: Option<String>,
    pub connection: &'static str,
}

/// `GET /status` on the runner's device port, straight through usbmuxd.
///
/// Setup used to learn that the runner serves from the `ServerURLHere` line
/// xcodebuild copies into its log, but xcodebuild writes that file through a
/// block buffer: on an iPhone 13 the line landed ~4 s after the runner
/// printed it. Asking the port answers within milliseconds of the server
/// starting.
pub async fn runner_status(udid: &str, port: u16) -> Result<RunnerStatus> {
    const STATUS_TIMEOUT: Duration = Duration::from_millis(800);
    let attached = attached(udid).await?;
    let mut stream = usbmux::connect(attached.device_id, port).await?;
    let body = tokio::time::timeout(STATUS_TIMEOUT, async {
        stream
            .write_all(b"GET /status HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut response = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let read = stream.read(&mut chunk).await?;
            if read == 0 || response.len() > MAX_MESSAGE {
                break;
            }
            response.extend_from_slice(&chunk[..read]);
        }
        anyhow::Ok(response)
    })
    .await
    .context("the runner did not answer /status in time")??;
    let (session_id, state) = parse_runner_status(&body)?;
    Ok(RunnerStatus {
        udid: attached.serial.clone(),
        port,
        session_id,
        state,
        connection: connection(&attached),
    })
}

/// The session id and `value.state` from a raw HTTP `/status` response.
fn parse_runner_status(response: &[u8]) -> Result<(Option<String>, Option<String>)> {
    let text = String::from_utf8_lossy(response);
    let status_line = text.lines().next().unwrap_or_default();
    if !status_line.starts_with("HTTP/") || !status_line.contains(" 200") {
        bail!("the runner answered {status_line:?}");
    }
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    let json: serde_json::Value =
        serde_json::from_str(body.trim()).context("the runner's /status is not JSON")?;
    let session = json
        .get("sessionId")
        .or_else(|| json.pointer("/value/sessionId"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let state = json
        .pointer("/value/state")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Ok((session, state))
}

async fn attached(udid: &str) -> Result<usbmux::Attached> {
    let want = usbmux::normalize_udid(udid);
    if want.is_empty() {
        bail!("empty UDID");
    }
    usbmux::find_attached(&want)
        .await?
        .ok_or_else(|| NotAttached(udid.to_string()).into())
}

fn connection(attached: &usbmux::Attached) -> &'static str {
    if attached.usb {
        "usb"
    } else {
        "network"
    }
}

struct Service {
    port: u16,
    ssl: bool,
}

async fn start_service<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> Result<Service> {
    let reply = exchange(
        stream,
        &request(&[
            ("Label", LABEL),
            ("Request", "StartService"),
            ("Service", IMAGE_MOUNTER),
        ]),
    )
    .await?;
    lockdown_error(&reply, "StartService")?;
    let port = reply
        .get("Port")
        .and_then(Value::as_int)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)
        .context("StartService reply had no port")?;
    Ok(Service {
        port,
        ssl: reply.get("EnableServiceSSL").and_then(Value::as_bool) == Some(true),
    })
}

/// iOS 17+ lists the mounted personalized image in `CopyDevices`; older iOS
/// answers `LookupImage` for the `Developer` type with its signature.
async fn query_mounted<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
) -> Result<(bool, Option<String>, Option<String>)> {
    let reply = exchange(stream, &request(&[("Command", "CopyDevices")])).await?;
    if let Some(found) = developer_image(&reply) {
        return Ok(found);
    }
    let reply = exchange(
        stream,
        &request(&[("Command", "LookupImage"), ("ImageType", "Developer")]),
    )
    .await?;
    let signed = reply
        .get("ImageSignature")
        .and_then(Value::as_array)
        .is_some_and(|signatures| !signatures.is_empty());
    Ok(if signed {
        (true, Some("Developer".to_string()), None)
    } else {
        (false, None, None)
    })
}

fn developer_image(reply: &Value) -> Option<(bool, Option<String>, Option<String>)> {
    reply
        .get("EntryList")?
        .as_array()?
        .iter()
        .find_map(|entry| {
            let mount_path = entry.get("MountPath").and_then(Value::as_str);
            let personalized = entry.get("PersonalizedImageType").and_then(Value::as_str);
            let disk_type = entry.get("DiskImageType").and_then(Value::as_str);
            let developer = mount_path == Some(DEVELOPER_MOUNT_PATH)
                || personalized == Some("DeveloperDiskImage")
                || disk_type == Some("Developer");
            if !developer || entry.get("IsMounted").and_then(Value::as_bool) == Some(false) {
                return None;
            }
            Some((
                true,
                disk_type.map(str::to_string),
                entry
                    .get("PersonalizedImageVersion")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ))
        })
}

fn lockdown_error(reply: &Value, what: &str) -> Result<()> {
    if let Some(error) = reply.get("Error").and_then(Value::as_str) {
        match error {
            "InvalidHostID" | "InvalidPairRecord" => {
                bail!("{what}: this Mac's pairing is no longer valid on the iPhone ({error}); trust this Mac again")
            }
            _ => bail!("{what} failed: {error}"),
        }
    }
    Ok(())
}

/// An XML property list with string values, which is all these requests need.
fn request(entries: &[(&str, &str)]) -> String {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>",
    );
    for (key, value) in entries {
        body.push_str(&format!(
            "<key>{}</key><string>{}</string>",
            usbmux::escape(key),
            usbmux::escape(value)
        ));
    }
    body.push_str("</dict></plist>\n");
    body
}

fn frame(body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body.as_bytes());
    out
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, body: &str) -> Result<Value> {
    tokio::time::timeout(EXCHANGE_TIMEOUT, async {
        stream.write_all(&frame(body)).await?;
        stream.flush().await?;
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).await?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > MAX_MESSAGE {
            bail!("lockdown reply length {length} is out of range");
        }
        let mut payload = vec![0u8; length];
        stream.read_exact(&mut payload).await?;
        if payload.starts_with(b"bplist") {
            bail!("lockdown answered with a binary property list");
        }
        let text = String::from_utf8(payload).context("lockdown reply is not UTF-8")?;
        usbmux::parse_plist(&text)
    })
    .await
    .context("lockdownd did not answer in time")?
}

// ── Pairing record and TLS ─────────────────────────────────────────────────

struct PairRecord {
    host_id: String,
    system_buid: String,
    host_certificate: Vec<u8>,
    host_private_key: Vec<u8>,
    device_certificate: Vec<u8>,
}

impl std::fmt::Debug for PairRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material.
        f.debug_struct("PairRecord").finish_non_exhaustive()
    }
}

impl PairRecord {
    fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.starts_with(b"bplist") {
            bail!(
                "the pairing record is a binary property list, which this reader does not handle"
            );
        }
        let text = std::str::from_utf8(bytes).context("pairing record is not UTF-8")?;
        let record = usbmux::parse_plist(text).context("parse the pairing record")?;
        let string = |key: &str| {
            record
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .with_context(|| format!("pairing record has no {key}"))
        };
        let data = |key: &str| {
            record
                .get(key)
                .and_then(Value::as_data)
                .map(<[u8]>::to_vec)
                .with_context(|| format!("pairing record has no {key}"))
        };
        Ok(Self {
            host_id: string("HostID")?,
            system_buid: string("SystemBUID")?,
            host_certificate: data("HostCertificate")?,
            host_private_key: data("HostPrivateKey")?,
            device_certificate: data("DeviceCertificate")?,
        })
    }

    fn tls_config(&self) -> Result<rustls::ClientConfig> {
        use rustls::pki_types::{
            CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer,
        };
        let (_, host_cert) = pem_to_der(&self.host_certificate).context("host certificate")?;
        let (_, device_cert) =
            pem_to_der(&self.device_certificate).context("device certificate")?;
        let (label, key) = pem_to_der(&self.host_private_key).context("host private key")?;
        let key = match label.as_str() {
            "PRIVATE KEY" => PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
            "RSA PRIVATE KEY" => PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(key)),
            other => bail!("unsupported host private key type {other:?}"),
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PinnedDevice {
            certificate: device_cert,
            provider: provider.clone(),
        });
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
            .context("TLS versions")?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(vec![CertificateDer::from(host_cert)], key)
            .context("host certificate and key")?;
        config.enable_sni = false;
        Ok(config)
    }
}

async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    config: &Arc<rustls::ClientConfig>,
    stream: S,
) -> Result<tokio_rustls::client::TlsStream<S>> {
    // lockdownd has no host name; SNI is off and the verifier pins the
    // certificate, so this name is never checked.
    let name = rustls::pki_types::ServerName::try_from("lockdown")
        .context("server name")?
        .to_owned();
    tokio::time::timeout(
        EXCHANGE_TIMEOUT,
        tokio_rustls::TlsConnector::from(config.clone()).connect(name, stream),
    )
    .await
    .context("TLS handshake timed out")?
    .context("TLS handshake")
}

/// The first PEM block in `pem`: its label and DER bytes.
fn pem_to_der(pem: &[u8]) -> Result<(String, Vec<u8>)> {
    use base64::Engine as _;
    let text = std::str::from_utf8(pem).context("PEM is not UTF-8")?;
    let begin = text.find("-----BEGIN ").context("no PEM header")?;
    let after = &text[begin + "-----BEGIN ".len()..];
    let label_end = after.find("-----").context("malformed PEM header")?;
    let label = after[..label_end].to_string();
    let body_start = &after[label_end + "-----".len()..];
    let end_marker = format!("-----END {label}-----");
    let body_end = body_start.find(&end_marker).context("no PEM footer")?;
    let base64: String = body_start[..body_end]
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(base64.as_bytes())
        .context("PEM body is not base64")?;
    Ok((label, der))
}

/// Accepts exactly the device certificate recorded at pairing time.
#[derive(Debug)]
struct PinnedDevice {
    certificate: Vec<u8>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedDevice {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.certificate.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier as _;

    #[test]
    fn requests_are_length_prefixed_big_endian_xml_with_escaped_strings() {
        let body = request(&[("Label", LABEL), ("Request", "GetValue"), ("Key", "A&B")]);
        assert!(body.contains("<key>Request</key><string>GetValue</string>"));
        assert!(body.contains("<string>A&amp;B</string>"));
        let framed = frame(&body);
        assert_eq!(&framed[..4], &(body.len() as u32).to_be_bytes());
        assert_eq!(&framed[4..], body.as_bytes());
        // The request parses back with the same reader that reads replies.
        let parsed = usbmux::parse_plist(&body).unwrap();
        assert_eq!(parsed.get("Key").and_then(Value::as_str), Some("A&B"));
    }

    #[test]
    fn data_values_decode_across_line_breaks() {
        let reply = "<plist version=\"1.0\"><dict><key>PairRecordData</key>\
                     <data>\n\taGVs\n\tbG8=\n\t</data></dict></plist>";
        let value = usbmux::parse_plist(reply).unwrap();
        assert_eq!(
            value.get("PairRecordData").and_then(Value::as_data),
            Some(&b"hello"[..])
        );
    }

    const PEM: &str = "-----BEGIN CERTIFICATE-----\naGVs\nbG8=\n-----END CERTIFICATE-----\n";

    #[test]
    fn pem_blocks_yield_their_label_and_der() {
        assert_eq!(
            pem_to_der(PEM.as_bytes()).unwrap(),
            ("CERTIFICATE".to_string(), b"hello".to_vec())
        );
        let key = "-----BEGIN RSA PRIVATE KEY-----\nAAE=\n-----END RSA PRIVATE KEY-----";
        assert_eq!(
            pem_to_der(key.as_bytes()).unwrap(),
            ("RSA PRIVATE KEY".to_string(), vec![0, 1])
        );
        assert!(pem_to_der(b"no pem here").is_err());
        assert!(
            pem_to_der(b"-----BEGIN X-----\nAAE=\n").is_err(),
            "missing footer"
        );
    }

    fn pair_record_xml(include_buid: bool) -> String {
        use base64::Engine as _;
        let b64 = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
        format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
             <key>DeviceCertificate</key><data>{}</data>\
             <key>HostCertificate</key><data>{}</data>\
             <key>HostID</key><string>HOST-1</string>\
             <key>HostPrivateKey</key><data>{}</data>\
             {}\
             </dict></plist>",
            b64(PEM),
            b64(PEM),
            b64("-----BEGIN PRIVATE KEY-----\nAAE=\n-----END PRIVATE KEY-----\n"),
            if include_buid {
                "<key>SystemBUID</key><string>BUID-1</string>"
            } else {
                ""
            },
        )
    }

    #[test]
    fn a_pairing_record_yields_its_ids_and_pem_blobs_and_never_prints_keys() {
        let record = PairRecord::parse(pair_record_xml(true).as_bytes()).unwrap();
        assert_eq!(record.host_id, "HOST-1");
        assert_eq!(record.system_buid, "BUID-1");
        assert!(record
            .host_private_key
            .starts_with(b"-----BEGIN PRIVATE KEY-----"));
        assert!(!format!("{record:?}").contains("PRIVATE"));
        let missing = PairRecord::parse(pair_record_xml(false).as_bytes()).unwrap_err();
        assert!(format!("{missing:#}").contains("SystemBUID"), "{missing:#}");
        assert!(PairRecord::parse(b"bplist00....").is_err());
    }

    fn mounter_reply(entries: &str) -> Value {
        usbmux::parse_plist(&format!(
            "<plist version=\"1.0\"><dict><key>EntryList</key><array>{entries}</array>\
             <key>Status</key><string>Complete</string></dict></plist>"
        ))
        .unwrap()
    }

    #[test]
    fn the_personalized_developer_image_counts_as_mounted() {
        // Shape observed on an iPhone 13, iOS 27.0.
        let reply = mounter_reply(
            "<dict><key>DiskImageType</key><string>Personalized</string>\
             <key>IsMounted</key><true/>\
             <key>MountPath</key><string>/System/Developer</string>\
             <key>PersonalizedImageType</key><string>DeveloperDiskImage</string>\
             <key>PersonalizedImageVersion</key><string>642.16</string></dict>",
        );
        assert_eq!(
            developer_image(&reply),
            Some((true, Some("Personalized".into()), Some("642.16".into())))
        );
    }

    #[test]
    fn other_or_unmounted_images_do_not_count() {
        let other = mounter_reply(
            "<dict><key>DiskImageType</key><string>Cryptex</string>\
             <key>MountPath</key><string>/private/preboot/x</string></dict>",
        );
        assert_eq!(developer_image(&other), None);
        let unmounted = mounter_reply(
            "<dict><key>PersonalizedImageType</key><string>DeveloperDiskImage</string>\
             <key>IsMounted</key><false/></dict>",
        );
        assert_eq!(developer_image(&unmounted), None);
        assert_eq!(developer_image(&mounter_reply("")), None);
    }

    #[test]
    fn lockdown_errors_name_a_stale_pairing() {
        let reply = usbmux::parse_plist(
            "<plist version=\"1.0\"><dict><key>Error</key><string>InvalidHostID</string></dict></plist>",
        )
        .unwrap();
        let error = lockdown_error(&reply, "StartSession").unwrap_err();
        assert!(
            format!("{error}").contains("trust this Mac again"),
            "{error}"
        );
        let ok = usbmux::parse_plist("<plist version=\"1.0\"><dict></dict></plist>").unwrap();
        assert!(lockdown_error(&ok, "StartSession").is_ok());
    }

    #[test]
    fn runner_status_yields_the_session_and_state() {
        // The shape the runner serves (RunnerWDA.swift: `.value(statusValue(), sessionId:)`).
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
            {\"sessionId\":\"328381F5-0604\",\"value\":{\"state\":\"success\",\"ready\":true}}";
        assert_eq!(
            parse_runner_status(response).unwrap(),
            (Some("328381F5-0604".into()), Some("success".into()))
        );
        assert!(parse_runner_status(b"HTTP/1.1 503 Service Unavailable\r\n\r\n").is_err());
        assert!(
            parse_runner_status(b"").is_err(),
            "a closed connection is not a status"
        );
        assert!(parse_runner_status(b"HTTP/1.1 200 OK\r\n\r\nnot json").is_err());
    }

    #[test]
    fn the_verifier_accepts_only_the_pinned_device_certificate() {
        let verifier = PinnedDevice {
            certificate: b"device-cert".to_vec(),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        };
        let name = rustls::pki_types::ServerName::try_from("lockdown").unwrap();
        let now = rustls::pki_types::UnixTime::now();
        let pinned = rustls::pki_types::CertificateDer::from(b"device-cert".to_vec());
        let other = rustls::pki_types::CertificateDer::from(b"someone-else".to_vec());
        assert!(verifier
            .verify_server_cert(&pinned, &[], &name, &[], now)
            .is_ok());
        assert!(verifier
            .verify_server_cert(&other, &[], &name, &[], now)
            .is_err());
        assert!(!verifier.supported_verify_schemes().is_empty());
    }
}
