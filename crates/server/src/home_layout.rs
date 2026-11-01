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

/// The shape SpringBoard accepts: the Dock plus at least one page.
pub fn validate(state: &plist::Value) -> Result<()> {
    if state.as_array().is_none_or(|a| a.len() < 2) {
        bail!("icon state must be an array holding the Dock and at least one page");
    }
    Ok(())
}

/// Replaces the whole icon state. Callers preview it and back up the current
/// layout first ([`save_backup`]).
pub async fn set_state(udid: &str, state: plist::Value) -> Result<()> {
    check_udid(udid)?;
    validate(&state)?;
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut stream =
            crate::lockdown::open_service(udid, "com.apple.springboardservices").await?;
        write_state(&mut stream, state).await
    })
    .await
    .context("home layout write timed out")?
}

async fn write_state<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, state: plist::Value) -> Result<()> {
    let mut request = plist::Dictionary::new();
    request.insert("command".into(), "setIconState".into());
    request.insert("iconState".into(), state);
    // Success is an empty acknowledgement or a closed stream; only an
    // error dictionary is a refusal.
    if let Ok(reply) = send_plist(stream, &plist::Value::Dictionary(request)).await {
        if let Some(d) = reply.as_dictionary() {
            if d.contains_key("error") || d.contains_key("Error") {
                bail!("SpringBoard refused setIconState: {d:?}");
            }
        }
    }
    Ok(())
}

/// Where layouts are kept before every write: `~/.iphone-use-backups`.
pub fn backup_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".iphone-use-backups")
}

/// Saves `state` as `<prefix>-<unix seconds>.plist` in [`backup_dir`] and
/// returns the path. Dates are cut to whole seconds: SpringBoard's carry
/// nanoseconds, which `plutil` and Python's plistlib cannot read.
pub fn save_backup(state: &plist::Value, prefix: &str) -> Result<std::path::PathBuf> {
    let dir = backup_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let mut path = dir.join(format!("{prefix}-{stamp}.plist"));
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{prefix}-{stamp}-{n}.plist"));
        n += 1;
    }
    plist::to_file_xml(&path, &whole_second_dates(state.clone()))?;
    Ok(path)
}

/// The newest backup made before a write, for `restore`.
pub fn latest_backup() -> Option<std::path::PathBuf> {
    std::fs::read_dir(backup_dir())
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("home-before-") && n.ends_with(".plist")))
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

fn whole_second_dates(value: plist::Value) -> plist::Value {
    match value {
        plist::Value::Date(date) => {
            let at = std::time::SystemTime::from(date);
            let secs = at.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            plist::Value::Date((std::time::UNIX_EPOCH + Duration::from_secs(secs)).into())
        }
        plist::Value::Array(items) => plist::Value::Array(items.into_iter().map(whole_second_dates).collect()),
        plist::Value::Dictionary(dict) => plist::Value::Dictionary(
            dict.into_iter().map(|(k, v)| (k, whole_second_dates(v))).collect(),
        ),
        other => other,
    }
}

/// Every identified icon by page (0 is the Dock), and how many items have no
/// identifier (widgets and stacks), which a rearranged layout can drop.
fn placements(state: &plist::Value) -> (std::collections::BTreeMap<String, Vec<usize>>, usize) {
    fn walk(items: &[plist::Value], page: usize, out: &mut std::collections::BTreeMap<String, Vec<usize>>, unnamed: &mut usize) {
        for item in items {
            let Some(d) = item.as_dictionary() else { continue };
            if let Some(plist::Value::Array(lists)) = d.get("iconLists") {
                for list in lists.iter().filter_map(|l| l.as_array()) {
                    walk(list, page, out, unnamed);
                }
                continue;
            }
            // Identifiers, never display names: two apps can share a name.
            let id = ["bundleIdentifier", "displayIdentifier", "webClipIdentifier"]
                .iter()
                .find_map(|k| d.get(k).and_then(|v| v.as_string()));
            match id {
                Some(id) => out.entry(id.to_string()).or_default().push(page),
                None => *unnamed += 1,
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    let mut unnamed = 0;
    for (page, items) in state.as_array().into_iter().flatten().enumerate() {
        if let Some(items) = items.as_array() {
            walk(items, page, &mut out, &mut unnamed);
        }
    }
    (out, unnamed)
}

/// What writing `next` over `current` changes, by app identifier.
pub fn diff(current: &plist::Value, next: &plist::Value) -> Value {
    let (before, unnamed_before) = placements(current);
    let (after, unnamed_after) = placements(next);
    let (mut added, mut removed, mut moved) = (vec![], vec![], vec![]);
    for (id, pages) in &after {
        match before.get(id) {
            None => added.push(json!({"id": id, "page": pages[0]})),
            Some(old) if old != pages => moved.push(json!({"id": id, "from": old, "to": pages})),
            _ => {}
        }
    }
    for (id, pages) in &before {
        if !after.contains_key(id) {
            removed.push(json!({"id": id, "page": pages[0]}));
        }
    }
    let mut warnings = vec![];
    if unnamed_after < unnamed_before {
        warnings.push(format!(
            "{} widget(s) or stack(s) without an identifier would be dropped",
            unnamed_before - unnamed_after
        ));
    }
    if !removed.is_empty() {
        warnings.push("apps missing from the new layout go to the App Library; SpringBoard may also bring hidden apps back to the Home Screen over several passes, so read the layout again after writing".into());
    }
    json!({"added": added, "removed": removed, "moved": moved,
           "widgets_before": unnamed_before, "widgets_after": unnamed_after, "warnings": warnings})
}

/// Counts before and after, plus [`diff`].
pub fn preview(current: &plist::Value, next: &plist::Value) -> Value {
    json!({"now": summarize(current), "after": summarize(next), "changes": diff(current, next)})
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
    fn layout(value: serde_json::Value) -> plist::Value {
        plist::to_value(&value).unwrap()
    }

    #[test]
    fn a_write_sends_set_icon_state_and_reports_a_refusal() {
        for (reply, refused) in [(json!({}), false), (json!({"error": "denied"}), true)] {
            block(async move {
                let (mut peer, mut client) = tokio::io::duplex(8192);
                let server = tokio::spawn(async move {
                    let size = peer.read_u32().await.unwrap() as usize;
                    let mut data = vec![0; size];
                    peer.read_exact(&mut data).await.unwrap();
                    let request: plist::Value = plist::from_bytes(&data).unwrap();
                    let d = request.as_dictionary().unwrap();
                    assert_eq!(d["command"].as_string(), Some("setIconState"));
                    assert_eq!(d["iconState"].as_array().unwrap().len(), 2);
                    let mut bytes = Vec::new();
                    plist::to_writer_binary(&mut bytes, &plist::to_value(&reply).unwrap()).unwrap();
                    peer.write_u32(bytes.len() as u32).await.unwrap();
                    peer.write_all(&bytes).await.unwrap();
                });
                let state = layout(json!([[], [{"bundleIdentifier": "a"}]]));
                assert_eq!(write_state(&mut client, state).await.is_err(), refused);
                server.await.unwrap();
            });
        }
    }

    #[test]
    fn only_a_dock_plus_pages_is_writable() {
        assert!(validate(&layout(json!([[]]))).is_err(), "no page");
        assert!(validate(&layout(json!({"not": "a list"}))).is_err());
        assert!(validate(&layout(json!([[], []]))).is_ok());
    }

    #[test]
    fn the_preview_matches_apps_by_identifier_not_name() {
        // Two apps share a name; a widget has no identifier.
        let current = layout(json!([
            [{"bundleIdentifier": "com.apple.mobilephone"}],
            [{"displayName": "Authenticator", "bundleIdentifier": "com.google.Authenticator"},
             {"displayName": "Authenticator", "bundleIdentifier": "com.microsoft.azureauthenticator"},
             {"iconType": "custom", "gridSize": "medium"}],
            [{"displayName": "Tools", "iconLists": [[{"bundleIdentifier": "com.apple.calculator"}]]}]
        ]));
        let next = layout(json!([
            [{"bundleIdentifier": "com.apple.mobilephone"}],
            [{"displayName": "Authenticator", "bundleIdentifier": "com.google.Authenticator"},
             {"bundleIdentifier": "com.apple.calculator"}],
            [{"bundleIdentifier": "com.apple.mobilesafari"}]
        ]));
        let changes = diff(&current, &next);
        assert_eq!(changes["removed"], json!([{"id": "com.microsoft.azureauthenticator", "page": 1}]));
        assert_eq!(changes["added"], json!([{"id": "com.apple.mobilesafari", "page": 2}]));
        assert_eq!(changes["moved"], json!([{"id": "com.apple.calculator", "from": [2], "to": [1]}]));
        assert_eq!((changes["widgets_before"].clone(), changes["widgets_after"].clone()), (json!(1), json!(0)));
        let warnings = changes["warnings"].as_array().unwrap();
        assert!(warnings.iter().any(|w| w.as_str().unwrap().contains("widget")));
        assert!(warnings.iter().any(|w| w.as_str().unwrap().contains("App Library")));
        assert!(diff(&current, &current)["warnings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn backups_keep_whole_second_dates() {
        let at = std::time::UNIX_EPOCH + Duration::new(1_800_000_000, 123_456_789);
        let cut = whole_second_dates(plist::Value::Array(vec![plist::Value::Date(at.into())]));
        let date = cut.as_array().unwrap()[0].as_date().unwrap();
        assert_eq!(std::time::SystemTime::from(date), std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000));
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
