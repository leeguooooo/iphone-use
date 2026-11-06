//! XCTest attachments left on the iPhone (#415).
//!
//! Before #410 every runner session screen-recorded the whole session, and
//! testmanagerd kept the recordings in its own container under
//! `tmp/Attachments` because the runner's test never finishes. They are not
//! purgeable, Settings files them under "System Data", and nothing on the
//! phone removes them: one iPhone held 60 GB in 3617 files. Version 0.27.40
//! stopped making them; this clears what is already there.
//!
//! `devicectl` lists the container but has no delete, so each stale file is
//! overwritten with an empty one. A directory copy replaces same-named files
//! and leaves the others alone, so one call clears a whole batch.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// testmanagerd's container, as `devicectl --domain-type appDataContainer`
/// names it.
pub const CONTAINER: &str = "com.apple.testmanagerd";
/// The directory inside it that holds the attachments.
pub const DIR: &str = "tmp/Attachments";
/// A file changed this recently may still be written by a live session.
pub const MIN_AGE: Duration = Duration::from_secs(600);
/// Files emptied per `devicectl device copy to` call.
const BATCH: usize = 500;

/// One non-empty attachment old enough to clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale {
    pub name: String,
    pub bytes: u64,
}

/// The non-empty files directly in [`DIR`] that last changed at least
/// `min_age` before `now`, from `devicectl device info files -j` output.
/// A file whose date cannot be read is kept.
pub fn stale_attachments(listing: &str, now: SystemTime, min_age: Duration) -> Vec<Stale> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(listing) else {
        return Vec::new();
    };
    let Some(files) = value.pointer("/result/files").and_then(|f| f.as_array()) else {
        return Vec::new();
    };
    let prefix = format!("{DIR}/");
    files
        .iter()
        .filter(|f| {
            f.pointer("/resources/isDirectory")
                .and_then(|d| d.as_bool())
                == Some(false)
        })
        .filter_map(|f| {
            let path = f.get("relativePath")?.as_str()?;
            let name = path.strip_prefix(&prefix)?;
            if name.is_empty() || name.contains('/') || name.starts_with('.') {
                return None;
            }
            let bytes = f.pointer("/metadata/size")?.as_u64()?;
            let modified = parse_utc(f.pointer("/metadata/lastModDate")?.as_str()?)?;
            let old = now.duration_since(modified).is_ok_and(|age| age >= min_age);
            (bytes > 0 && old).then(|| Stale {
                name: name.to_string(),
                bytes,
            })
        })
        .collect()
}

/// `2026-10-24T09:08:12.000Z` (fraction optional) as a time.
fn parse_utc(text: &str) -> Option<SystemTime> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let time = time.split('.').next()?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

/// Empties the stale attachments on the phone `udid`. Returns (files emptied,
/// bytes freed); (0, 0) when the phone cannot be reached or nothing is stale.
pub fn sweep(udid: &str) -> (usize, u64) {
    sweep_where(udid, MIN_AGE, |_| true)
}

/// [`sweep`] limited to the stale files `wanted` accepts.
pub fn sweep_where(udid: &str, min_age: Duration, wanted: impl Fn(&Stale) -> bool) -> (usize, u64) {
    let Some(listing) = super::sys::devicectl_json(
        120,
        &[
            "device",
            "info",
            "files",
            "--device",
            udid,
            "--domain-type",
            "appDataContainer",
            "--domain-identifier",
            CONTAINER,
        ],
    ) else {
        tracing::debug!("could not list the iPhone's XCTest attachments; nothing cleared");
        return (0, 0);
    };
    let mut stale = stale_attachments(&listing, SystemTime::now(), min_age);
    stale.retain(|s| wanted(s));
    let mut emptied = 0;
    let mut freed = 0;
    for batch in stale.chunks(BATCH) {
        if empty_on_phone(udid, batch) {
            emptied += batch.len();
            freed += batch.iter().map(|s| s.bytes).sum::<u64>();
        } else {
            tracing::debug!(
                "could not empty {} XCTest attachments on the iPhone",
                batch.len()
            );
        }
    }
    (emptied, freed)
}

/// Copies a directory of empty files named like `batch` over [`DIR`].
fn empty_on_phone(udid: &str, batch: &[Stale]) -> bool {
    let Ok(staging) = tempfile::Builder::new()
        .prefix("iphone-use-attachments.")
        .tempdir()
    else {
        return false;
    };
    let dir = staging.path().join("Attachments");
    if std::fs::create_dir(&dir).is_err()
        || !batch
            .iter()
            .all(|s| std::fs::File::create(dir.join(&s.name)).is_ok())
    {
        return false;
    }
    copied_over(udid, &dir)
}

fn copied_over(udid: &str, source: &Path) -> bool {
    let source = source.to_string_lossy();
    let (_, finished) = super::sys::run_bounded(
        "xcrun",
        &[
            "devicectl",
            "device",
            "copy",
            "to",
            "--device",
            udid,
            "--domain-type",
            "appDataContainer",
            "--domain-identifier",
            CONTAINER,
            "--source",
            &source,
            "--destination",
            DIR,
        ],
        Duration::from_secs(300),
    );
    finished
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> SystemTime {
        parse_utc(text).unwrap()
    }

    fn entry(path: &str, dir: bool, size: u64, modified: &str) -> serde_json::Value {
        serde_json::json!({
            "relativePath": path,
            "resources": {"isDirectory": dir},
            "metadata": {"size": size, "lastModDate": modified},
        })
    }

    #[test]
    fn parses_devicectl_dates() {
        assert_eq!(at("1970-01-01T00:00:00Z"), UNIX_EPOCH);
        assert_eq!(
            at("2026-10-24T09:08:12.000Z")
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1_792_832_892
        );
        assert_eq!(
            at("2000-03-01T00:00:00Z")
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            951_868_800
        );
        for bad in [
            "2026-10-24 09:08:12Z",
            "2026-13-01T00:00:00Z",
            "2026-10-24T09:08:12",
            "x",
        ] {
            assert_eq!(parse_utc(bad), None, "{bad}");
        }
    }

    #[test]
    fn picks_old_non_empty_files_directly_in_attachments() {
        let listing = serde_json::json!({"result": {"files": [
            entry("tmp/Attachments", true, 0, "2026-11-04T09:00:00.000Z"),
            entry("tmp/Attachments/OLD-BIG", false, 10_470_000_000, "2026-10-24T09:08:12.000Z"),
            entry("tmp/Attachments/OLD-EMPTY", false, 0, "2026-10-24T09:08:12.000Z"),
            entry("tmp/Attachments/FRESH", false, 5_000, "2026-11-06T11:55:00.000Z"),
            entry("tmp/Attachments/sub/NESTED", false, 5_000, "2026-10-24T09:08:12.000Z"),
            entry("tmp/Attachments/.hidden", false, 5_000, "2026-10-24T09:08:12.000Z"),
            entry("tmp/Other/FILE", false, 5_000, "2026-10-24T09:08:12.000Z"),
            entry("tmp/Attachments/BAD-DATE", false, 5_000, "yesterday"),
        ]}})
        .to_string();
        let stale = stale_attachments(&listing, at("2026-11-06T12:00:00Z"), MIN_AGE);
        assert_eq!(
            stale,
            vec![Stale {
                name: "OLD-BIG".into(),
                bytes: 10_470_000_000
            }]
        );
    }

    #[test]
    fn unreadable_listing_clears_nothing() {
        let now = at("2026-11-06T12:00:00Z");
        assert!(stale_attachments("", now, MIN_AGE).is_empty());
        assert!(stale_attachments("{\"error\":{}}", now, MIN_AGE).is_empty());
    }
}
