//! Vision OCR fallback: recognized text as element-like rows.
//!
//! Some screens have no usable accessibility tree (games, canvases,
//! Flutter/Unity apps). For those the daemon runs Apple's Vision text
//! recognizer on the screenshot through a small Swift helper
//! (`scripts/ocr/ocr.swift`, embedded here) that is compiled into the
//! instance state dir on first use and recompiled when the embedded source
//! changes. The rows it yields look like `/agent/elements` rows with kind
//! `OCRText` and `source: "ocr"`, so a tap by coordinate works from `rect`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use serde::Deserialize;
use serde_json::Value;

/// The helper's Swift source, built into the binary so the daemon does not
/// depend on the repository at run time.
pub const SOURCE: &str = include_str!("../../../scripts/ocr/ocr.swift");

/// Rows Vision is less sure of than this are dropped.
pub const MIN_CONFIDENCE: f64 = 0.3;

/// One recognition run on one screenshot.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(10);

/// A cold `swiftc -O` of the helper takes several seconds; allow headroom.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(180);

const HELPER_NAME: &str = "iphone-use-ocr";

/// One recognized line of text, in runner points.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OcrRow {
    pub text: String,
    /// Vision's confidence, 0..=1.
    pub confidence: f64,
    /// `[x, y, w, h]` in points, top-left origin.
    pub rect: [f64; 4],
}

/// The helper's wire row: pixels, top-left origin.
#[derive(Debug, Deserialize)]
struct HelperRow {
    text: String,
    confidence: f64,
    rect: [f64; 4],
}

/// A recognizer standing in for the Vision helper.
pub type StandIn = std::sync::Arc<dyn Fn(&[u8], f64) -> Result<Vec<OcrRow>> + Send + Sync>;

static STAND_IN: std::sync::Mutex<Option<StandIn>> = std::sync::Mutex::new(None);

/// Replace [`recognize`] with `stand_in` (`None` restores Vision). For tests
/// of the HTTP layer, which cannot run the helper off macOS.
#[doc(hidden)]
pub fn set_stand_in(stand_in: Option<StandIn>) {
    *STAND_IN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = stand_in;
}

/// Recognize the text in a PNG screenshot. `scale` is pixels per point (the
/// screenshot's width over the screen's width in points, e.g. 3.0).
pub async fn recognize(png: &[u8], scale: f64) -> Result<Vec<OcrRow>> {
    let stand_in = STAND_IN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(stand_in) = stand_in {
        return stand_in(png, scale);
    }
    if !cfg!(target_os = "macos") {
        bail!(
            "OCR needs macOS (Apple's Vision framework); this daemon runs on {}",
            std::env::consts::OS
        );
    }
    check_scale(scale)?;
    let helper = ensure_helper(&crate::instance::current().state_dir).await?;
    let mut image = tempfile::Builder::new()
        .prefix("iphone-use-ocr-")
        .suffix(".png")
        .tempfile()
        .context("OCR: create a temporary screenshot file")?;
    std::io::Write::write_all(&mut image, png).context("OCR: write the temporary screenshot")?;
    let run = tokio::process::Command::new(&helper)
        .arg(image.path())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(RUN_TIMEOUT, run)
        .await
        .map_err(|_| anyhow::anyhow!("OCR timed out after {}s", RUN_TIMEOUT.as_secs()))?
        .with_context(|| format!("OCR: run {}", helper.display()))?;
    if !output.status.success() {
        bail!(
            "OCR failed ({}): {}",
            output.status,
            first_lines(&String::from_utf8_lossy(&output.stderr))
        );
    }
    parse(&String::from_utf8_lossy(&output.stdout), scale)
}

/// Map OCR rows to `/agent/elements` rows: kind `OCRText`, the text as
/// `label`, `rect` in points, depth 0, plus `source: "ocr"` and the rounded
/// `confidence` so an agent can tell recognized text from accessibility
/// elements. There is no element behind such a row: tap its rect's center.
pub fn rows_for_elements(ocr: &[OcrRow]) -> Vec<Value> {
    ocr.iter()
        .map(|row| {
            let element = crate::runner::ElementRow {
                kind: "OCRText".to_string(),
                label: row.text.clone(),
                rect: row.rect,
                depth: 0,
                ..Default::default()
            };
            let mut value = serde_json::to_value(element).unwrap_or(Value::Null);
            if let Some(object) = value.as_object_mut() {
                object.insert("source".into(), "ocr".into());
                object.insert("confidence".into(), round_to(row.confidence, 100.0).into());
            }
            value
        })
        .collect()
}

/// Parse the helper's stdout: convert pixel rects to points, drop empty and
/// low-confidence rows, sort in reading order.
pub fn parse(stdout: &str, scale: f64) -> Result<Vec<OcrRow>> {
    check_scale(scale)?;
    let raw: Vec<HelperRow> =
        serde_json::from_str(stdout.trim()).context("OCR: unreadable helper output")?;
    let rows = raw
        .into_iter()
        .filter_map(|row| {
            let text = row.text.trim();
            let usable = !text.is_empty()
                && row.confidence.is_finite()
                && row.confidence >= MIN_CONFIDENCE
                && row.rect.iter().all(|v| v.is_finite());
            usable.then(|| OcrRow {
                text: text.to_string(),
                confidence: row.confidence.min(1.0),
                rect: to_points(row.rect, scale),
            })
        })
        .collect();
    Ok(reading_order(rows))
}

fn check_scale(scale: f64) -> Result<()> {
    if !(scale.is_finite() && scale > 0.0) {
        bail!("OCR: invalid screen scale {scale}");
    }
    Ok(())
}

/// Pixels to points, rounded to a tenth of a point (thirds of a point would
/// print as long decimals).
fn to_points(rect: [f64; 4], scale: f64) -> [f64; 4] {
    rect.map(|v| round_to(v / scale, 10.0))
}

fn round_to(value: f64, per_unit: f64) -> f64 {
    (value * per_unit).round() / per_unit
}

/// Top-to-bottom, then left-to-right within a line. Rows whose vertical
/// centers lie within half the shorter row's height of the line's first row
/// share that line, so a slightly taller word does not jump ahead.
fn reading_order(mut rows: Vec<OcrRow>) -> Vec<OcrRow> {
    let center = |r: &OcrRow| r.rect[1] + r.rect[3] / 2.0;
    rows.sort_by(|a, b| {
        center(a)
            .total_cmp(&center(b))
            .then(a.rect[0].total_cmp(&b.rect[0]))
    });
    let mut ordered = Vec::with_capacity(rows.len());
    let mut line: Vec<OcrRow> = Vec::new();
    for row in rows {
        let joins = line.first().is_some_and(|first| {
            (center(&row) - center(first)).abs() <= first.rect[3].min(row.rect[3]) / 2.0
        });
        if !joins {
            flush_line(&mut line, &mut ordered);
        }
        line.push(row);
    }
    flush_line(&mut line, &mut ordered);
    ordered
}

fn flush_line(line: &mut Vec<OcrRow>, ordered: &mut Vec<OcrRow>) {
    line.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
    ordered.append(line);
}

// ── the helper binary ───────────────────────────────────────────────────────

/// Identifies one helper source; a changed source (an upgrade) recompiles.
pub fn cache_key(source: &str) -> String {
    crate::setup::legacy_ios::sha256_hex(source.as_bytes())[..16].to_string()
}

struct HelperPaths {
    bin: PathBuf,
    source: PathBuf,
    stamp: PathBuf,
}

fn helper_paths(state_dir: &Path) -> HelperPaths {
    let dir = state_dir.join("bin");
    HelperPaths {
        bin: dir.join(HELPER_NAME),
        source: dir.join(format!("{HELPER_NAME}.swift")),
        stamp: dir.join(format!("{HELPER_NAME}.key")),
    }
}

/// The compiled helper is current when it exists and its stamp names the
/// embedded source's key.
fn is_current(paths: &HelperPaths, key: &str) -> bool {
    paths.bin.is_file()
        && std::fs::read_to_string(&paths.stamp).is_ok_and(|stamp| stamp.trim() == key)
}

/// Serializes compiles: two concurrent first uses must not race `swiftc`.
static COMPILE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The compiled helper, building it first when missing or stale.
async fn ensure_helper(state_dir: &Path) -> Result<PathBuf> {
    let paths = helper_paths(state_dir);
    let key = cache_key(SOURCE);
    if is_current(&paths, &key) {
        return Ok(paths.bin);
    }
    let _guard = COMPILE.lock().await;
    if is_current(&paths, &key) {
        return Ok(paths.bin);
    }
    let dir = paths.bin.parent().unwrap_or(state_dir);
    std::fs::create_dir_all(dir).with_context(|| format!("OCR: create {}", dir.display()))?;
    std::fs::write(&paths.source, SOURCE)
        .with_context(|| format!("OCR: write {}", paths.source.display()))?;
    let staging = paths.bin.with_extension("new");
    let compile = tokio::process::Command::new("xcrun")
        .args(["swiftc", "-O"])
        .arg(&paths.source)
        .arg("-o")
        .arg(&staging)
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(COMPILE_TIMEOUT, compile)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "OCR: compiling the helper timed out after {}s",
                COMPILE_TIMEOUT.as_secs()
            )
        })?
        .context("OCR needs the Xcode command-line tools (xcrun swiftc)")?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&staging);
        bail!(
            "OCR: could not compile the helper with xcrun swiftc: {}",
            first_lines(&String::from_utf8_lossy(&output.stderr))
        );
    }
    std::fs::rename(&staging, &paths.bin)
        .with_context(|| format!("OCR: install {}", paths.bin.display()))?;
    std::fs::write(&paths.stamp, &key)
        .with_context(|| format!("OCR: write {}", paths.stamp.display()))?;
    tracing::info!(helper = %paths.bin.display(), key = %key, "compiled the OCR helper");
    Ok(paths.bin)
}

fn first_lines(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(5)
        .collect();
    if lines.is_empty() {
        "no error output".to_string()
    } else {
        lines.join(" | ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(text: &str, rect: [f64; 4]) -> OcrRow {
        OcrRow {
            text: text.into(),
            confidence: 0.9,
            rect,
        }
    }

    #[test]
    fn parses_helper_output_and_converts_pixels_to_points() {
        let out = r#"[{"text":"设置","confidence":0.98,"rect":[60,300,120,51]}]"#;
        let rows = parse(out, 3.0).unwrap();
        assert_eq!(
            rows,
            vec![OcrRow {
                text: "设置".into(),
                confidence: 0.98,
                rect: [20.0, 100.0, 40.0, 17.0]
            }]
        );
    }

    #[test]
    fn thirds_of_a_point_round_to_a_tenth() {
        assert_eq!(to_points([10.0, 20.0, 1.0, 2.0], 3.0), [3.3, 6.7, 0.3, 0.7]);
        assert_eq!(
            to_points([10.0, 20.0, 30.0, 40.0], 2.0),
            [5.0, 10.0, 15.0, 20.0]
        );
    }

    #[test]
    fn empty_low_confidence_and_non_finite_rows_are_dropped() {
        let out = json!([
            {"text": "  ", "confidence": 0.99, "rect": [0, 0, 10, 10]},
            {"text": "blurry", "confidence": 0.29, "rect": [0, 20, 10, 10]},
            {"text": "edge", "confidence": 0.3, "rect": [0, 40, 10, 10]},
            {"text": " OK ", "confidence": 0.8, "rect": [0, 60, 10, 10]},
        ])
        .to_string();
        let rows = parse(&out, 1.0).unwrap();
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["edge", "OK"]);
    }

    #[test]
    fn malformed_output_and_bad_scales_are_errors() {
        assert!(parse("not json", 3.0).is_err());
        assert!(parse(r#"[{"text":"a","confidence":1}]"#, 3.0).is_err());
        assert!(parse("[]", 0.0).is_err());
        assert!(parse("[]", f64::NAN).is_err());
        assert_eq!(parse(" []\n", 3.0).unwrap(), vec![]);
    }

    #[test]
    fn rows_sort_top_to_bottom_then_left_to_right() {
        let rows = vec![
            row("bottom", [0.0, 200.0, 50.0, 20.0]),
            row("right", [100.0, 10.0, 50.0, 20.0]),
            // A taller word on the same line starts a little higher.
            row("left", [0.0, 6.0, 50.0, 26.0]),
            row("middle", [10.0, 100.0, 50.0, 20.0]),
        ];
        let texts: Vec<String> = reading_order(rows).into_iter().map(|r| r.text).collect();
        assert_eq!(texts, ["left", "right", "middle", "bottom"]);
    }

    #[test]
    fn separate_lines_stay_separate_even_when_close() {
        let rows = vec![
            row("second", [0.0, 22.0, 50.0, 20.0]),
            row("first", [80.0, 0.0, 50.0, 20.0]),
        ];
        let texts: Vec<String> = reading_order(rows).into_iter().map(|r| r.text).collect();
        assert_eq!(texts, ["first", "second"]);
    }

    #[test]
    fn element_rows_use_the_elements_shape_and_say_they_are_ocr() {
        let rows = rows_for_elements(&[
            OcrRow {
                text: "登录".into(),
                confidence: 0.98765,
                rect: [20.0, 100.0, 40.0, 17.0],
            },
            OcrRow {
                text: "Skip".into(),
                confidence: 0.5,
                rect: [3.3, 6.7, 10.0, 5.5],
            },
        ]);
        assert_eq!(
            rows[0],
            json!({"kind": "OCRText", "label": "登录", "rect": [20, 100, 40, 17], "depth": 0,
                   "source": "ocr", "confidence": 0.99})
        );
        assert_eq!(rows[1]["rect"], json!([3.3, 6.7, 10, 5.5]));
        assert!(rows_for_elements(&[]).is_empty());
    }

    #[test]
    fn the_cache_key_follows_the_source() {
        let key = cache_key(SOURCE);
        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(key, cache_key(SOURCE));
        assert_ne!(key, cache_key(&format!("{SOURCE}\n// changed")));
    }

    #[test]
    fn a_helper_is_current_only_with_a_matching_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let paths = helper_paths(dir.path());
        assert_eq!(paths.bin, dir.path().join("bin/iphone-use-ocr"));
        let key = cache_key(SOURCE);
        assert!(!is_current(&paths, &key), "nothing compiled yet");
        std::fs::create_dir_all(paths.bin.parent().unwrap()).unwrap();
        std::fs::write(&paths.bin, b"binary").unwrap();
        assert!(
            !is_current(&paths, &key),
            "a binary without a stamp is rebuilt"
        );
        std::fs::write(&paths.stamp, "0123456789abcdef").unwrap();
        assert!(
            !is_current(&paths, &key),
            "a stale stamp (an upgrade) is rebuilt"
        );
        std::fs::write(&paths.stamp, format!("{key}\n")).unwrap();
        assert!(is_current(&paths, &key));
        std::fs::remove_file(&paths.bin).unwrap();
        assert!(
            !is_current(&paths, &key),
            "a stamp without a binary is rebuilt"
        );
    }

    #[test]
    fn the_embedded_source_is_the_vision_helper() {
        assert!(SOURCE.contains("VNRecognizeTextRequest"));
        assert!(SOURCE.contains("zh-Hans"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn recognition_off_macos_is_a_clear_error() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let error = runtime
            .block_on(recognize(b"png", 3.0))
            .unwrap_err()
            .to_string();
        assert!(error.contains("needs macOS"), "{error}");
    }
}
