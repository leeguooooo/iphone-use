//! Loopback TCP relay to a port on a USB-connected iPhone, through macOS's
//! own usbmuxd (`/var/run/usbmuxd`).
//!
//! This replaces libimobiledevice's `iproxy` for the runner's control and
//! video relays, so a user no longer needs Homebrew or libimobiledevice. Each
//! accepted connection looks the phone up again (its usbmux device id changes
//! when the cable is replugged) and asks usbmuxd to connect to the device
//! port; after usbmuxd answers `Result 0` the socket is a raw tunnel.
//!
//! The usbmux plist protocol: a 16-byte little-endian header
//! (`length` including the header, `version` 1, `message` 8 = plist, `tag`)
//! followed by an XML property list.

use anyhow::{anyhow, bail, Context, Result};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};

const USBMUXD_SOCKET: &str = "/var/run/usbmuxd";
const HEADER_LEN: usize = 16;
const PLIST_MESSAGE: u32 = 8;
const PROTOCOL_VERSION: u32 = 1;
/// A usbmuxd reply larger than this is not something this relay asked for.
const MAX_REPLY: usize = 4 << 20;
const MUX_TIMEOUT: Duration = Duration::from_secs(5);

/// `iphone-use relay`: listen on `listen` (loopback only) and forward every
/// connection to `device_port` on the iPhone whose UDID is `udid`.
pub async fn run_relay(udid: &str, listen: SocketAddr, device_port: u16) -> Result<()> {
    if !listen.ip().is_loopback() {
        bail!("the relay listens on loopback only; {listen} is not a loopback address");
    }
    if device_port == 0 {
        bail!("device port must be 1-65535");
    }
    let want = normalize_udid(udid);
    if want.is_empty() {
        bail!("empty UDID");
    }
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    match find_device(&want).await {
        Ok(Some(_)) => tracing::info!("relay {listen} -> {udid}:{device_port} over usbmuxd"),
        Ok(None) => tracing::warn!(
            "relay {listen} -> {udid}:{device_port}: the iPhone is not attached yet; \
             connections fail until it is"
        ),
        Err(error) => tracing::warn!("relay {listen}: usbmuxd is not answering yet: {error:#}"),
    }
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!("accept on {listen}: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let want = want.clone();
        tokio::spawn(async move {
            if let Err(error) = forward(client, &want, device_port).await {
                tracing::warn!("relay {peer} -> device:{device_port}: {error:#}");
            }
        });
    }
}

async fn forward(mut client: TcpStream, want: &str, device_port: u16) -> Result<()> {
    let _ = client.set_nodelay(true);
    let device_id = find_device(want)
        .await?
        .ok_or_else(|| anyhow!("iPhone {want} is not attached to usbmuxd"))?;
    let mut device = connect(device_id, device_port).await?;
    tokio::io::copy_bidirectional(&mut client, &mut device).await?;
    Ok(())
}

/// The usbmux device id for `want` (a normalized UDID), preferring a USB
/// attachment over a network one.
async fn find_device(want: &str) -> Result<Option<u64>> {
    Ok(find_attached(want)
        .await?
        .map(|attached| attached.device_id))
}

/// One phone as usbmuxd lists it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Attached {
    pub device_id: u64,
    /// The UDID exactly as usbmuxd spells it; pair records are keyed by it.
    pub serial: String,
    pub usb: bool,
}

/// `want` (a normalized UDID) as usbmuxd currently lists it, preferring a USB
/// attachment over a network one.
pub(crate) async fn find_attached(want: &str) -> Result<Option<Attached>> {
    let mut mux = UnixStream::connect(USBMUXD_SOCKET)
        .await
        .context("connect /var/run/usbmuxd")?;
    let reply = request(&mut mux, &list_devices_message()).await?;
    Ok(pick_attached(&reply, want))
}

/// The pairing record usbmuxd keeps for `serial` (the plist bytes of
/// `/var/db/lockdown/<udid>.plist`, which only root can read directly).
pub(crate) async fn read_pair_record(serial: &str) -> Result<Vec<u8>> {
    let mut mux = UnixStream::connect(USBMUXD_SOCKET)
        .await
        .context("connect /var/run/usbmuxd")?;
    let reply = request(&mut mux, &read_pair_record_message(serial)).await?;
    match reply.get("PairRecordData") {
        Some(Value::Data(bytes)) => Ok(bytes.clone()),
        _ => match reply.get("Number").and_then(Value::as_int) {
            Some(code) => bail!("usbmuxd has no pairing record for {serial} (result {code}); trust this Mac on the iPhone"),
            None => bail!("usbmuxd ReadPairRecord reply had no record"),
        },
    }
}

/// Ask usbmuxd to tunnel to `port` on `device_id`; the returned stream is the
/// tunnel once usbmuxd has answered `Result 0`.
pub(crate) async fn connect(device_id: u64, port: u16) -> Result<UnixStream> {
    let mut mux = UnixStream::connect(USBMUXD_SOCKET)
        .await
        .context("connect /var/run/usbmuxd")?;
    let reply = request(&mut mux, &connect_message(device_id, port)).await?;
    match reply.get("Number").and_then(Value::as_int) {
        Some(0) => Ok(mux),
        Some(3) => bail!("the iPhone refused the connection to port {port} (nothing listening)"),
        Some(code) => bail!("usbmuxd Connect to port {port} failed with result {code}"),
        None => bail!("usbmuxd Connect reply had no result"),
    }
}

async fn request(mux: &mut UnixStream, body: &str) -> Result<Value> {
    tokio::time::timeout(MUX_TIMEOUT, async {
        mux.write_all(&frame(body, 1)).await?;
        let mut header = [0u8; HEADER_LEN];
        mux.read_exact(&mut header).await?;
        let length = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
        if !(HEADER_LEN..=MAX_REPLY).contains(&length) {
            bail!("usbmuxd reply length {length} is out of range");
        }
        let mut payload = vec![0u8; length - HEADER_LEN];
        mux.read_exact(&mut payload).await?;
        let text = String::from_utf8(payload).context("usbmuxd reply is not UTF-8")?;
        parse_plist(&text)
    })
    .await
    .context("usbmuxd did not answer in time")?
}

fn frame(body: &str, tag: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(&((HEADER_LEN + body.len()) as u32).to_le_bytes());
    out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    out.extend_from_slice(&PLIST_MESSAGE.to_le_bytes());
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(body.as_bytes());
    out
}

fn plist_dict(entries: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>{entries}\
         <key>ClientVersionString</key><string>iphone-use</string>\
         <key>ProgName</key><string>iphone-use</string>\
         </dict></plist>\n"
    )
}

fn list_devices_message() -> String {
    plist_dict("<key>MessageType</key><string>ListDevices</string>")
}

fn read_pair_record_message(serial: &str) -> String {
    plist_dict(&format!(
        "<key>MessageType</key><string>ReadPairRecord</string>\
         <key>PairRecordID</key><string>{}</string>",
        escape(serial)
    ))
}

/// XML text escaping for the few strings this module writes.
pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn connect_message(device_id: u64, port: u16) -> String {
    // usbmuxd takes the port in network byte order, read as a host integer.
    let port_number = port.swap_bytes();
    plist_dict(&format!(
        "<key>MessageType</key><string>Connect</string>\
         <key>DeviceID</key><integer>{device_id}</integer>\
         <key>PortNumber</key><integer>{port_number}</integer>"
    ))
}

/// Upper-case hex digits only: `00008110-0002346211A0401E` and
/// `000081100002346211a0401e` name the same phone.
pub fn normalize_udid(udid: &str) -> String {
    udid.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
fn pick_device(reply: &Value, want: &str) -> Option<u64> {
    pick_attached(reply, want).map(|attached| attached.device_id)
}

fn pick_attached(reply: &Value, want: &str) -> Option<Attached> {
    let mut network = None;
    for entry in reply.get("DeviceList")?.as_array()? {
        let Some(properties) = entry.get("Properties") else {
            continue;
        };
        let Some(serial) = properties.get("SerialNumber").and_then(Value::as_str) else {
            continue;
        };
        if normalize_udid(serial) != want {
            continue;
        }
        let Some(device_id) = entry
            .get("DeviceID")
            .or_else(|| properties.get("DeviceID"))
            .and_then(Value::as_int)
        else {
            continue;
        };
        let usb = properties.get("ConnectionType").and_then(Value::as_str) == Some("USB");
        let attached = Attached {
            device_id,
            serial: serial.to_string(),
            usb,
        };
        if usb {
            return Some(attached);
        }
        network = network.or(Some(attached));
    }
    network
}

// ── A minimal XML property-list reader (what usbmuxd and lockdownd send) ───

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Dict(Vec<(String, Value)>),
    Array(Vec<Value>),
    String(String),
    Int(u64),
    Bool(bool),
    Data(Vec<u8>),
    Other,
}

impl Value {
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub(crate) fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    pub(crate) fn as_int(&self) -> Option<u64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }
    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub(crate) fn as_data(&self) -> Option<&[u8]> {
        match self {
            Value::Data(bytes) => Some(bytes),
            _ => None,
        }
    }
}

pub fn parse_plist(text: &str) -> Result<Value> {
    let start = text.find("<plist").context("no <plist> element")?;
    let mut parser = Parser {
        rest: &text[start..],
    };
    parser.open_tag()?; // <plist version="1.0">
    parser.value()
}

struct Parser<'a> {
    rest: &'a str,
}

impl<'a> Parser<'a> {
    fn skip_space(&mut self) {
        self.rest = self.rest.trim_start();
    }

    /// Consume the next tag; returns its name and whether it self-closes.
    fn open_tag(&mut self) -> Result<(&'a str, bool)> {
        self.skip_space();
        let rest = self.rest.strip_prefix('<').context("expected a tag")?;
        let end = rest.find('>').context("unterminated tag")?;
        let inner = &rest[..end];
        self.rest = &rest[end + 1..];
        let self_closing = inner.ends_with('/');
        let name = inner
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        Ok((name, self_closing))
    }

    fn text_until_close(&mut self, name: &str) -> Result<String> {
        let close = format!("</{name}>");
        let end = self
            .rest
            .find(&close)
            .with_context(|| format!("missing {close}"))?;
        let raw = &self.rest[..end];
        self.rest = &self.rest[end + close.len()..];
        Ok(unescape(raw))
    }

    fn at_close(&mut self, name: &str) -> bool {
        self.skip_space();
        let close = format!("</{name}>");
        if let Some(rest) = self.rest.strip_prefix(close.as_str()) {
            self.rest = rest;
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Value> {
        let (name, self_closing) = self.open_tag()?;
        if self_closing {
            return Ok(match name {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                "dict" => Value::Dict(Vec::new()),
                "array" => Value::Array(Vec::new()),
                "string" => Value::String(String::new()),
                _ => Value::Other,
            });
        }
        match name {
            "dict" => {
                let mut entries = Vec::new();
                while !self.at_close("dict") {
                    let (key_tag, _) = self.open_tag()?;
                    if key_tag != "key" {
                        bail!("expected <key> in <dict>, found <{key_tag}>");
                    }
                    let key = self.text_until_close("key")?;
                    entries.push((key, self.value()?));
                }
                Ok(Value::Dict(entries))
            }
            "array" => {
                let mut items = Vec::new();
                while !self.at_close("array") {
                    items.push(self.value()?);
                }
                Ok(Value::Array(items))
            }
            "string" => Ok(Value::String(self.text_until_close("string")?)),
            "integer" => {
                let text = self.text_until_close("integer")?;
                Ok(text
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .and_then(|n| u64::try_from(n).ok())
                    .map_or(Value::Other, Value::Int))
            }
            "data" => {
                use base64::Engine as _;
                let text = self.text_until_close("data")?;
                let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
                Ok(base64::engine::general_purpose::STANDARD
                    .decode(compact.as_bytes())
                    .map_or(Value::Other, Value::Data))
            }
            "true" | "false" | "date" | "real" => {
                let bool_value = name == "true";
                self.text_until_close(name)?;
                Ok(match name {
                    "true" | "false" => Value::Bool(bool_value),
                    _ => Value::Other,
                })
            }
            other => bail!("unsupported plist element <{other}>"),
        }
    }
}

fn unescape(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST_REPLY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>DeviceList</key>
	<array>
		<dict>
			<key>DeviceID</key>
			<integer>7</integer>
			<key>MessageType</key>
			<string>Attached</string>
			<key>Properties</key>
			<dict>
				<key>ConnectionType</key>
				<string>Network</string>
				<key>DeviceID</key>
				<integer>7</integer>
				<key>EscapedFullServiceName</key>
				<string>a&amp;b</string>
				<key>NetworkAddress</key>
				<data>EAIAAMCoAD8AAAAAAAAAAA==</data>
				<key>SerialNumber</key>
				<string>00008110-0002346211A0401E</string>
			</dict>
		</dict>
		<dict>
			<key>DeviceID</key>
			<integer>3</integer>
			<key>MessageType</key>
			<string>Attached</string>
			<key>Properties</key>
			<dict>
				<key>ConnectionSpeed</key>
				<integer>480000000</integer>
				<key>ConnectionType</key>
				<string>USB</string>
				<key>DeviceID</key>
				<integer>3</integer>
				<key>SerialNumber</key>
				<string>000081100002346211A0401E</string>
				<key>USBSerialNumber</key>
				<string>000081100002346211A0401E</string>
				<key>Charging</key>
				<true/>
			</dict>
		</dict>
	</array>
</dict>
</plist>
"#;

    #[test]
    fn frame_has_a_little_endian_header_that_counts_itself() {
        let framed = frame("abc", 9);
        assert_eq!(&framed[0..4], &19u32.to_le_bytes());
        assert_eq!(&framed[4..8], &1u32.to_le_bytes());
        assert_eq!(&framed[8..12], &8u32.to_le_bytes());
        assert_eq!(&framed[12..16], &9u32.to_le_bytes());
        assert_eq!(&framed[16..], b"abc");
    }

    #[test]
    fn connect_sends_the_port_in_network_byte_order() {
        let message = connect_message(3, 8100);
        // 8100 = 0x1FA4 -> 0xA41F = 42015
        assert!(message.contains("<key>PortNumber</key><integer>42015</integer>"));
        assert!(message.contains("<key>DeviceID</key><integer>3</integer>"));
        let parsed = parse_plist(&message).unwrap();
        assert_eq!(
            parsed.get("MessageType").and_then(Value::as_str),
            Some("Connect")
        );
    }

    #[test]
    fn a_usb_attachment_wins_over_the_same_phone_on_the_network() {
        let reply = parse_plist(LIST_REPLY).unwrap();
        let want = normalize_udid("00008110-0002346211a0401e");
        assert_eq!(pick_device(&reply, &want), Some(3));
    }

    #[test]
    fn a_network_only_phone_is_still_found_and_a_stranger_is_not() {
        let reply = parse_plist(&LIST_REPLY.replace(">USB<", ">Network<")).unwrap();
        assert_eq!(
            pick_device(&reply, &normalize_udid("00008110-0002346211A0401E")),
            Some(7)
        );
        assert_eq!(
            pick_device(&reply, &normalize_udid("00008150-000A60EC1A02401C")),
            None
        );
    }

    #[test]
    fn result_replies_and_escapes_parse() {
        let reply = parse_plist(
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
             <key>MessageType</key><string>Result</string>\
             <key>Number</key><integer>0</integer></dict></plist>",
        )
        .unwrap();
        assert_eq!(reply.get("Number").and_then(Value::as_int), Some(0));
        let list = parse_plist(LIST_REPLY).unwrap();
        let first = &list.get("DeviceList").unwrap().as_array().unwrap()[0];
        assert_eq!(
            first
                .get("Properties")
                .and_then(|p| p.get("EscapedFullServiceName"))
                .and_then(Value::as_str),
            Some("a&b")
        );
    }

    #[test]
    fn malformed_plists_are_errors_not_panics() {
        for text in [
            "",
            "<plist>",
            "<plist><dict><key>a</key>",
            "<plist><foo/></plist>",
        ] {
            let _ = parse_plist(text);
        }
        assert!(parse_plist("<plist><dict><string>x</string></dict></plist>").is_err());
    }

    #[test]
    fn the_relay_refuses_a_non_loopback_listen_address() {
        // A plain runtime: `#[tokio::test]` expands to `core::…`, which this
        // crate's `core` dependency shadows.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(run_relay(
                "00008110-0002346211A0401E",
                "0.0.0.0:0".parse().unwrap(),
                8100,
            ))
            .unwrap_err();
        assert!(error.to_string().contains("loopback"), "{error}");
    }
}
