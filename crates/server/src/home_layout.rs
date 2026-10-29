//! A fresh read of SpringBoard's persisted layout, bound to one paired device.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_PLIST_BYTES: usize = 4 * 1024 * 1024;

pub async fn read(udid: &str) -> Result<Value> {
    if udid.is_empty() || !udid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        bail!("an explicit device UDID is required");
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut stream =
            crate::lockdown::open_service(udid, "com.apple.springboardservices").await?;
        let mut body = read_stream(&mut stream).await?;
        body["udid"] = json!(udid);
        body["transport"] = json!("paired_usbmuxd");
        body["ok"] = json!(true);
        Ok(body)
    })
    .await
    .context("home layout read timed out")?
}

async fn read_stream<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> Result<Value> {
    let state = exchange(stream, "getIconState", true).await?;
    let metrics = exchange(stream, "getHomeScreenIconMetrics", false).await?;
    core::home_layout::decode(&state, &metrics).map_err(anyhow::Error::msg)
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    command: &str,
    version: bool,
) -> Result<Value> {
    let mut request = plist::Dictionary::new();
    request.insert("command".into(), command.into());
    if version {
        request.insert("formatVersion".into(), "2".into());
    }
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, &request)?;
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    let size = stream.read_u32().await? as usize;
    if !(1..=MAX_PLIST_BYTES).contains(&size) {
        bail!("{command} response size is invalid");
    }
    let mut response = vec![0; size];
    stream.read_exact(&mut response).await?;
    let value = plist::Value::from_reader(std::io::Cursor::new(response))
        .context("invalid SpringBoard plist")?;
    let json = serde_json::to_value(value)?;
    if json.get("error").is_some() || json.get("Error").is_some() {
        bail!("SpringBoard refused {command}");
    }
    Ok(json)
}

/// The untouched icon state, as SpringBoard stores it, for backup and restore.
pub async fn raw_state(udid: &str) -> Result<plist::Value> {
    check_udid(udid)?;
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut stream =
            crate::lockdown::open_service(udid, "com.apple.springboardservices").await?;
        let mut request = plist::Dictionary::new();
        request.insert("command".into(), "getIconState".into());
        request.insert("formatVersion".into(), "2".into());
        let reply = send_plist(&mut stream, &plist::Value::Dictionary(request)).await?;
        Ok(reply)
    })
    .await
    .context("home layout read timed out")?
}

/// Replaces the whole icon state. Only the CLI calls this, with a plist the
/// person has seen; the caller backs up first.
pub async fn set_state(udid: &str, state: plist::Value) -> Result<()> {
    check_udid(udid)?;
    if !state.as_array().is_some_and(|a| a.len() >= 2) {
        bail!("icon state must be an array holding the Dock and at least one page");
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut stream =
            crate::lockdown::open_service(udid, "com.apple.springboardservices").await?;
        let mut request = plist::Dictionary::new();
        request.insert("command".into(), "setIconState".into());
        request.insert("iconState".into(), state);
        // Success is an empty acknowledgement or a closed stream; only an
        // error dictionary is a refusal.
        if let Ok(reply) = send_plist(&mut stream, &plist::Value::Dictionary(request)).await {
            if let Some(d) = reply.as_dictionary() {
                if d.contains_key("error") || d.contains_key("Error") {
                    bail!("SpringBoard refused setIconState: {d:?}");
                }
            }
        }
        Ok(())
    })
    .await
    .context("home layout write timed out")?
}

/// Page and icon counts of an icon state, folders counted by what they hold.
pub fn summarize(state: &plist::Value) -> String {
    fn icons(items: &[plist::Value]) -> usize {
        items
            .iter()
            .map(|it| match it.as_dictionary().and_then(|d| d.get("iconLists")) {
                Some(plist::Value::Array(pages)) if !pages.is_empty() => pages
                    .iter()
                    .filter_map(|p| p.as_array())
                    .map(|p| icons(p))
                    .sum(),
                _ => 1,
            })
            .sum()
    }
    let Some(pages) = state.as_array() else {
        return "not an icon state".into();
    };
    let counts: Vec<String> = pages
        .iter()
        .map(|p| p.as_array().map_or(0, |p| p.len()).to_string())
        .collect();
    let total: usize = pages.iter().filter_map(|p| p.as_array()).map(|p| icons(p)).sum();
    format!(
        "{} home pages, {} icons; slots per Dock/page: [{}]",
        pages.len().saturating_sub(1),
        total,
        counts.join(", ")
    )
}

fn check_udid(udid: &str) -> Result<()> {
    if udid.is_empty() || !udid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        bail!("an explicit device UDID is required");
    }
    Ok(())
}

async fn send_plist<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    request: &plist::Value,
) -> Result<plist::Value> {
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, request)?;
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    let size = stream.read_u32().await? as usize;
    if !(1..=MAX_PLIST_BYTES).contains(&size) {
        bail!("SpringBoard response size is invalid");
    }
    let mut response = vec![0; size];
    stream.read_exact(&mut response).await?;
    plist::Value::from_reader(std::io::Cursor::new(response)).context("invalid SpringBoard plist")
}

/// Filtering changes only the response, never the phone or the original page indexes.
pub fn select_page(mut body: Value, page: Option<usize>) -> Result<Value> {
    if let Some(page) = page {
        let pages = body["pages"].as_array().context("layout has no pages")?;
        let selected = pages
            .get(page)
            .cloned()
            .context("page is outside the returned home layout")?;
        let total = pages.len();
        body["pages"] = json!([selected]);
        body["selection"] = json!({"page":page,"total_pages":total});
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn block<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }
    #[test]
    fn protocol_sends_only_two_read_commands_and_decodes_binary_replies() {
        block(async {
            let (mut peer, mut client) = tokio::io::duplex(8192);
            let server = tokio::spawn(async move {
                let replies = [
                    json!([[],[{"displayName":"App","bundleIdentifier":"example.app"}]]),
                    json!({"homeScreenIconRows":6,"homeScreenIconColumns":4,
                "homeScreenIconFolderRows":3,"homeScreenIconFolderColumns":3,"homeScreenIconDockMaxCount":4}),
                ];
                for (expected, reply) in ["getIconState", "getHomeScreenIconMetrics"]
                    .into_iter()
                    .zip(replies)
                {
                    let size = peer.read_u32().await.unwrap() as usize;
                    let mut data = vec![0; size];
                    peer.read_exact(&mut data).await.unwrap();
                    let request: plist::Value = plist::from_bytes(&data).unwrap();
                    assert_eq!(
                        request.as_dictionary().unwrap()["command"].as_string(),
                        Some(expected)
                    );
                    if expected == "getIconState" {
                        assert_eq!(
                            request.as_dictionary().unwrap()["formatVersion"].as_string(),
                            Some("2")
                        );
                    }
                    let mut bytes = Vec::new();
                    plist::to_writer_binary(&mut bytes, &reply).unwrap();
                    peer.write_u32(bytes.len() as u32).await.unwrap();
                    peer.write_all(&bytes).await.unwrap();
                }
            });
            let out = read_stream(&mut client).await.unwrap();
            assert_eq!(out["pages"][0]["entries"][0]["bundle_id"], "example.app");
            server.await.unwrap();
        });
    }
    #[test]
    fn oversized_or_error_replies_never_become_empty_layouts() {
        block(async {
            for error in [true, false] {
                let (mut peer, mut client) = tokio::io::duplex(2048);
                let server = tokio::spawn(async move {
                    let size = peer.read_u32().await.unwrap() as usize;
                    let mut data = vec![0; size];
                    peer.read_exact(&mut data).await.unwrap();
                    if error {
                        let mut bytes = Vec::new();
                        plist::to_writer_xml(
                            &mut bytes,
                            &plist::Value::Dictionary(
                                [("Error".to_string(), plist::Value::String("Denied".into()))]
                                    .into_iter()
                                    .collect(),
                            ),
                        )
                        .unwrap();
                        peer.write_u32(bytes.len() as u32).await.unwrap();
                        peer.write_all(&bytes).await.unwrap();
                    } else {
                        peer.write_u32((MAX_PLIST_BYTES + 1) as u32).await.unwrap();
                    }
                });
                assert!(read_stream(&mut client).await.is_err());
                server.await.unwrap();
            }
        });
    }
    #[test]
    fn summary_counts_folder_contents_not_folders() {
        let state: plist::Value = plist::from_bytes(
            br#"<plist version="1.0"><array><array/><array>
            <dict><key>displayIdentifier</key><string>a</string></dict>
            <dict><key>iconLists</key><array><array>
              <dict><key>displayIdentifier</key><string>b</string></dict>
              <dict><key>displayIdentifier</key><string>c</string></dict>
            </array></array></dict></array></array></plist>"#,
        )
        .unwrap();
        let text = summarize(&state);
        assert!(text.contains("1 home pages, 3 icons"), "{text}");
    }
    #[test]
    fn page_selection_retains_its_original_index_and_total() {
        let out = select_page(
            json!({"pages":[{"page":0},{"page":1}],"dock":{"entries":[]}}),
            Some(1),
        )
        .unwrap();
        assert_eq!(out["pages"], json!([{"page":1}]));
        assert_eq!(out["selection"]["total_pages"], 2);
        assert!(select_page(out, Some(10)).is_err());
    }
}
