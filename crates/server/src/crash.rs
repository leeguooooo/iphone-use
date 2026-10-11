//! Crash reports from the phone: `com.apple.crashreportcopymobile` serves the
//! crash log folder (`/var/mobile/Library/Logs/CrashReporter`) over AFC.
//!
//! Reports are moved into that folder by `com.apple.crashreportmover`;
//! connecting to it starts the move, and it writes `ping` when done. Listing
//! pokes the mover first so a crash from seconds ago is there.
//!
//! Report names carry the process and the device-local time:
//! `MyApp-2026-10-11-070102.ips` (iOS 14+), `MyApp_2020-01-02-030405_iPhone.crash`
//! (older). Older reports sit one level down in `Retired/` and similar folders;
//! [`list`] walks those too, and [`read`] finds a bare name in any of them.

use crate::afc::{self, AfcClient, AfcError};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

pub const MOVER: &str = "com.apple.crashreportmover";
pub const COPY_MOBILE: &str = "com.apple.crashreportcopymobile";
/// Largest report [`read`] returns; longer ones are cut and flagged.
pub const READ_CAP: usize = 2 << 20;
/// How long the mover may take to say it has moved everything.
const MOVER_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

#[derive(Debug, Clone, Default)]
pub struct CrashFilter {
    /// Case-insensitive substring of the process name.
    pub process: Option<String>,
    /// Only reports modified at or after this Unix time (seconds).
    pub since: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CrashEntry {
    /// The file name, as [`read`] takes it.
    pub name: String,
    /// The subfolder it is in (`Retired`), absent at the top level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub process: Option<String>,
    /// The file's modification time, Unix seconds.
    pub time: Option<u64>,
    /// The device-local time in the name, `2026-10-11 07:01:02`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stamp: Option<String>,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CrashReport {
    pub name: String,
    pub text: String,
    /// Bytes returned (at most [`READ_CAP`]).
    pub size: usize,
    /// The report was longer than [`READ_CAP`] and is cut.
    pub truncated: bool,
}

/// The process and the device-local time in a report name, if it has them.
pub fn parse_name(name: &str) -> (Option<String>, Option<String>) {
    let stem = name.strip_suffix(".synced").unwrap_or(name);
    let stem = stem
        .strip_suffix(".ips")
        .or_else(|| stem.strip_suffix(".crash"))
        .unwrap_or(stem);
    let bytes = stem.as_bytes();
    // `[-_]YYYY-MM-DD-HHMMSS`: 18 bytes after the separator.
    for start in (1..bytes.len()).filter(|&i| bytes[i] == b'-' || bytes[i] == b'_') {
        let Some(stamp) = stem.get(start + 1..start + 1 + 17) else {
            break;
        };
        if let Some(formatted) = parse_stamp(stamp) {
            return (Some(stem[..start].to_string()), Some(formatted));
        }
    }
    (None, None)
}

/// `2026-10-11-070102` as `2026-10-11 07:01:02`.
fn parse_stamp(stamp: &str) -> Option<String> {
    let b = stamp.as_bytes();
    let digits = |range: std::ops::Range<usize>| b[range].iter().all(u8::is_ascii_digit);
    if b.len() != 17
        || !digits(0..4)
        || b[4] != b'-'
        || !digits(5..7)
        || b[7] != b'-'
        || !digits(8..10)
        || b[10] != b'-'
        || !digits(11..17)
    {
        return None;
    }
    Some(format!(
        "{} {}:{}:{}",
        &stamp[..10],
        &stamp[11..13],
        &stamp[13..15],
        &stamp[15..17]
    ))
}

/// Whether `name` is a bare report file name: no path, no `..`, a report
/// extension.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.contains(['/', '\\', '\0'])
        && !name.contains("..")
        && !name.starts_with('.')
        && (name.ends_with(".ips") || name.ends_with(".crash") || name.ends_with(".synced"))
}

/// Reject a name [`read`] must not touch.
pub fn validate_name(name: &str) -> Result<()> {
    if !valid_name(name) {
        bail!("{name:?} is not a crash report name (a bare file name ending in .ips, .crash or .synced)");
    }
    Ok(())
}

/// Wait for the mover's `ping`.
async fn await_ping<S: AsyncRead + Unpin>(stream: &mut S) -> Result<()> {
    let mut reply = [0u8; 4];
    tokio::time::timeout(MOVER_TIMEOUT, stream.read_exact(&mut reply))
        .await
        .context("the crash report mover did not answer in time")?
        .context("read the crash report mover's reply")?;
    if &reply != b"ping" {
        bail!("the crash report mover answered {reply:02x?}, not ping");
    }
    Ok(())
}

/// Ask the phone to move pending crash reports into the copy folder.
pub async fn flush(udid: &str) -> Result<()> {
    let mut stream = crate::lockdown::open_service(udid, MOVER).await?;
    await_ping(&mut stream).await
}

/// Every report at the top of the folder and one level down.
async fn collect<S: AsyncRead + AsyncWrite + Unpin>(
    client: &mut AfcClient<S>,
) -> Result<Vec<CrashEntry>> {
    let mut entries = Vec::new();
    let mut folders = Vec::new();
    for name in client.read_dir("/").await? {
        if valid_name(&name) {
            if let Some(entry) = entry(client, &name, None).await {
                entries.push(entry);
            }
        } else if !name.starts_with('.') && !name.contains('.') {
            folders.push(name);
        }
    }
    for folder in folders {
        // A plain file without an extension, or a folder we may not read.
        let Ok(info) = client.file_info(&format!("/{folder}")).await else {
            continue;
        };
        if !info.is_dir() {
            continue;
        }
        let Ok(names) = client.read_dir(&format!("/{folder}")).await else {
            continue;
        };
        for name in names.into_iter().filter(|name| valid_name(name)) {
            if let Some(entry) = entry(client, &name, Some(&folder)).await {
                entries.push(entry);
            }
        }
    }
    Ok(entries)
}

async fn entry<S: AsyncRead + AsyncWrite + Unpin>(
    client: &mut AfcClient<S>,
    name: &str,
    folder: Option<&str>,
) -> Option<CrashEntry> {
    let path = match folder {
        Some(folder) => format!("/{folder}/{name}"),
        None => format!("/{name}"),
    };
    // Reports can vanish between the listing and the stat (the phone prunes).
    let info = client.file_info(&path).await.ok()?;
    if !info.is_file() {
        return None;
    }
    let (process, stamp) = parse_name(name);
    Some(CrashEntry {
        name: name.to_string(),
        folder: folder.map(str::to_string),
        process,
        time: info.modified_secs(),
        stamp,
        size: info.size,
    })
}

/// Filter, newest first, and cap.
pub fn select(mut entries: Vec<CrashEntry>, filter: &CrashFilter) -> Vec<CrashEntry> {
    if let Some(process) = filter.process.as_deref().filter(|p| !p.is_empty()) {
        let process = process.to_lowercase();
        entries.retain(|entry| {
            entry
                .process
                .as_deref()
                .unwrap_or(&entry.name)
                .to_lowercase()
                .contains(&process)
        });
    }
    if let Some(since) = filter.since {
        entries.retain(|entry| entry.time.is_none_or(|time| time >= since));
    }
    entries.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| b.name.cmp(&a.name)));
    entries.truncate(filter.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT));
    entries
}

/// List over an open copy-mobile AFC stream.
pub async fn list_with<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    filter: &CrashFilter,
) -> Result<Vec<CrashEntry>> {
    let mut client = AfcClient::new(stream);
    Ok(select(collect(&mut client).await?, filter))
}

/// Read `name` over an open copy-mobile AFC stream: the top level first,
/// then each subfolder.
pub async fn read_with<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    name: &str,
) -> Result<CrashReport> {
    validate_name(name)?;
    let mut client = AfcClient::new(stream);
    let (bytes, truncated) = match client.read_file(&format!("/{name}"), READ_CAP).await {
        Ok(read) => read,
        Err(error) if afc::is_status(&error, AfcError::NOT_FOUND) => {
            let mut found = None;
            for folder in client.read_dir("/").await? {
                if folder.starts_with('.') || folder.contains('.') {
                    continue;
                }
                match client
                    .read_file(&format!("/{folder}/{name}"), READ_CAP)
                    .await
                {
                    Ok(read) => {
                        found = Some(read);
                        break;
                    }
                    Err(_) => continue,
                }
            }
            found.with_context(|| format!("no crash report named {name}"))?
        }
        Err(error) => return Err(error),
    };
    Ok(CrashReport {
        name: name.to_string(),
        size: bytes.len(),
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
    })
}

/// The phone's crash reports, newest first. Pokes the mover first; a mover
/// that does not answer still leaves the reports already moved.
pub async fn list(udid: &str, filter: &CrashFilter) -> Result<Vec<CrashEntry>> {
    if let Err(error) = flush(udid).await {
        tracing::debug!("crash report mover: {error:#}");
    }
    let stream = crate::lockdown::open_service(udid, COPY_MOBILE).await?;
    list_with(stream, filter).await
}

/// One report's text (first [`READ_CAP`] bytes).
pub async fn read(udid: &str, name: &str) -> Result<CrashReport> {
    validate_name(name)?;
    let stream = crate::lockdown::open_service(udid, COPY_MOBILE).await?;
    read_with(stream, name).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::afc::{encode, read_packet, OP_DATA, OP_FILE_OPEN_RESULT, OP_STATUS};
    use tokio::io::{AsyncWriteExt, DuplexStream};

    /// `#[tokio::test]` expands to `::core::…`, which this crate's `core`
    /// dependency shadows; tests build their own runtime.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn names_yield_the_process_and_the_local_time() {
        assert_eq!(
            parse_name("MyApp-2026-10-11-070102.ips"),
            (Some("MyApp".into()), Some("2026-10-11 07:01:02".into()))
        );
        assert_eq!(
            parse_name("My-App-2026-10-11-070102.ips.synced"),
            (Some("My-App".into()), Some("2026-10-11 07:01:02".into()))
        );
        assert_eq!(
            parse_name("SpringBoard_2020-01-02-030405_iPhone.crash"),
            (
                Some("SpringBoard".into()),
                Some("2020-01-02 03:04:05".into())
            )
        );
        assert_eq!(
            parse_name("JetsamEvent-2026-10-11-070102.ips").0.as_deref(),
            Some("JetsamEvent")
        );
        assert_eq!(parse_name("notes.ips"), (None, None));
        assert_eq!(parse_name("MyApp-2026-10-11.ips"), (None, None));
        assert_eq!(parse_name("-2026-10-11-070102.ips"), (None, None));
    }

    #[test]
    fn only_bare_report_names_pass_validation() {
        assert!(valid_name("MyApp-2026-10-11-070102.ips"));
        assert!(valid_name("a.crash"));
        assert!(valid_name("a.ips.synced"));
        for bad in [
            "",
            "Retired/a.ips",
            "../a.ips",
            "a..ips",
            "..",
            "a\\b.ips",
            ".hidden.ips",
            "a.txt",
            "a.ips\0",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
            assert!(validate_name(bad).is_err());
        }
    }

    fn entry(name: &str, time: Option<u64>) -> CrashEntry {
        let (process, stamp) = parse_name(name);
        CrashEntry {
            name: name.into(),
            folder: None,
            process,
            time,
            stamp,
            size: 1,
        }
    }

    #[test]
    fn select_filters_by_process_and_time_and_sorts_newest_first() {
        let entries = vec![
            entry("MyApp-2026-10-11-070102.ips", Some(300)),
            entry("Other-2026-10-11-070102.ips", Some(200)),
            entry("myapp-2026-10-10-070102.ips", Some(100)),
            entry("odd.ips", None),
        ];
        let all = select(entries.clone(), &CrashFilter::default());
        assert_eq!(all[0].time, Some(300));
        assert_eq!(all.last().unwrap().time, None);
        let mine = select(
            entries.clone(),
            &CrashFilter {
                process: Some("MYAPP".into()),
                ..Default::default()
            },
        );
        assert_eq!(mine.len(), 2);
        let recent = select(
            entries.clone(),
            &CrashFilter {
                since: Some(150),
                limit: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].time, Some(300));
    }

    #[test]
    fn the_mover_must_say_ping() {
        block_on(the_mover_must_say_ping_body());
    }

    async fn the_mover_must_say_ping_body() {
        assert!(await_ping(&mut &b"ping"[..]).await.is_ok());
        assert!(await_ping(&mut &b"pong"[..]).await.is_err());
        assert!(await_ping(&mut &b"pi"[..]).await.is_err());
    }

    /// A fake copy-mobile service with a fixed folder tree.
    async fn fake_service(mut device: DuplexStream) {
        let tree: &[(&str, Option<&[u8]>)] = &[
            ("/", None),
            ("/Retired", None),
            ("/MyApp-2026-10-11-070102.ips", Some(b"{\"app\":1}")),
            ("/Retired/Old-2026-01-01-000000.ips", Some(b"old")),
        ];
        let mut open: Option<(Vec<u8>, usize)> = None;
        loop {
            let Ok(request) = read_packet(&mut device).await else {
                return;
            };
            let path = String::from_utf8_lossy(match request.operation {
                afc::OP_FILE_OPEN => &request.arguments[8..],
                _ => &request.arguments,
            })
            .into_owned();
            let node = tree.iter().find(|(p, _)| *p == path);
            let not_found = (OP_STATUS, 8u64.to_le_bytes().to_vec(), vec![]);
            let (op, args, data) = match request.operation {
                afc::OP_READ_DIR => match path.as_str() {
                    "/" => (
                        OP_DATA,
                        vec![],
                        b".\0..\0MyApp-2026-10-11-070102.ips\0Retired\0README.txt\0".to_vec(),
                    ),
                    "/Retired" => (OP_DATA, vec![], b"Old-2026-01-01-000000.ips\0".to_vec()),
                    _ => not_found,
                },
                afc::OP_GET_FILE_INFO => match node {
                    Some((_, None)) => (
                        OP_DATA,
                        vec![],
                        b"st_size\x000\x00st_ifmt\x00S_IFDIR\x00".to_vec(),
                    ),
                    Some((_, Some(body))) => (
                        OP_DATA,
                        vec![],
                        format!(
                            "st_size\0{}\0st_ifmt\0S_IFREG\0st_mtime\0{}\0",
                            body.len(),
                            5_000_000_000u64 * body.len() as u64
                        )
                        .into_bytes(),
                    ),
                    None => not_found,
                },
                afc::OP_FILE_OPEN => match node {
                    Some((_, Some(body))) => {
                        open = Some((body.to_vec(), 0));
                        (OP_FILE_OPEN_RESULT, 1u64.to_le_bytes().to_vec(), vec![])
                    }
                    _ => not_found,
                },
                afc::OP_FILE_READ => {
                    let (body, at) = open.as_mut().unwrap();
                    let chunk = body[*at..].to_vec();
                    *at = body.len();
                    (OP_DATA, vec![], chunk)
                }
                afc::OP_FILE_CLOSE => (OP_STATUS, 0u64.to_le_bytes().to_vec(), vec![]),
                _ => (OP_STATUS, 15u64.to_le_bytes().to_vec(), vec![]),
            };
            device
                .write_all(&encode(request.number, op, &args, &data))
                .await
                .unwrap();
        }
    }

    #[test]
    fn listing_walks_one_level_of_subfolders() {
        block_on(listing_walks_one_level_of_subfolders_body());
    }

    async fn listing_walks_one_level_of_subfolders_body() {
        let (host, device) = tokio::io::duplex(1 << 16);
        tokio::spawn(fake_service(device));
        let entries = list_with(host, &CrashFilter::default()).await.unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].name, "MyApp-2026-10-11-070102.ips");
        assert_eq!(entries[0].process.as_deref(), Some("MyApp"));
        assert_eq!(entries[0].size, 9);
        assert_eq!(entries[0].time, Some(45));
        assert_eq!(entries[1].folder.as_deref(), Some("Retired"));
    }

    #[test]
    fn reading_finds_a_report_at_the_top_or_in_a_subfolder() {
        block_on(reading_finds_a_report_at_the_top_or_in_a_subfolder_body());
    }

    async fn reading_finds_a_report_at_the_top_or_in_a_subfolder_body() {
        let (host, device) = tokio::io::duplex(1 << 16);
        tokio::spawn(fake_service(device));
        let report = read_with(host, "MyApp-2026-10-11-070102.ips")
            .await
            .unwrap();
        assert_eq!(report.text, "{\"app\":1}");
        assert!(!report.truncated);

        let (host, device) = tokio::io::duplex(1 << 16);
        tokio::spawn(fake_service(device));
        let report = read_with(host, "Old-2026-01-01-000000.ips").await.unwrap();
        assert_eq!(report.text, "old");

        let (host, device) = tokio::io::duplex(1 << 16);
        tokio::spawn(fake_service(device));
        assert!(read_with(host, "Gone-2026-01-01-000000.ips").await.is_err());

        let (host, _device) = tokio::io::duplex(1 << 16);
        assert!(read_with(host, "../etc/passwd.ips").await.is_err());
    }
}
