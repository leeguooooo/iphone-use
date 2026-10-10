//! Reading a scrolling list page by page (`POST /agent/collect`).
//!
//! One page is the rows of a kind (default `Cell`) whose centre lies on the
//! screen, or inside an optional region of it. Between pages the daemon swipes
//! once; it stops when the list stops moving, when a page adds nothing new,
//! when an end label shows up, or at a page budget. Rows are deduplicated on
//! `(kind, label, value)`, so two genuinely identical rows collapse into one.
//!
//! What this can NOT tell: whether the list is complete. Only an end label the
//! caller named proves the bottom was reached; a list that stopped moving may
//! be at its end, or the swipe may have landed on something that does not
//! scroll. The result says `complete: false` in every other case.
//!
//! The loop runs over a [`Pager`] so it can be tested without a phone; the
//! daemon's pager reads the runner's element tree and swipes with the same
//! helper `/agent/input` scrolls use.

use std::collections::HashSet;
use std::time::Duration;

use crate::wda::ElementRow;

pub const DEFAULT_ROW_KIND: &str = "Cell";
pub const DEFAULT_MAX_PAGES: u32 = 6;
pub const MAX_PAGES_CAP: u32 = 10;
pub const DEFAULT_FIND_SWIPES: u32 = 1;
pub const MAX_FIND_SWIPES: u32 = 5;
/// The whole collection, swipes and reads included. Below the MCP client's
/// 90 s budget for this call so the daemon, not a timed-out client, answers.
pub const COLLECT_DEADLINE: Duration = Duration::from_secs(80);
/// Time kept for the read after a swipe: no swipe starts with less left, so
/// the screen is never left moved without being read (a heavy tree reads in
/// 6–7 s).
pub const READ_RESERVE: Duration = Duration::from_secs(15);

/// Too little time left to swipe and still read the page it shows.
pub fn too_late_to_swipe(deadline: tokio::time::Instant) -> bool {
    deadline.saturating_duration_since(tokio::time::Instant::now()) < READ_RESERVE
}

/// After a swipe, before the next read: the list decelerates for a moment.
pub const SWIPE_SETTLE: Duration = Duration::from_millis(500);
/// Descendant texts kept per row when the row itself has no label.
const MAX_ROW_TEXTS: usize = 6;

/// A normalized `[0,1]` rectangle of the screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Region {
    /// The region in points on a `width`×`height` screen.
    pub fn to_points(self, width: f64, height: f64) -> [f64; 4] {
        [
            self.x * width,
            self.y * height,
            self.w * width,
            self.h * height,
        ]
    }

    /// The region's centre, normalized (the swipe anchor).
    pub fn center(self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub const FULL: Region = Region {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };
}

/// Which way the content should move: `Down` reveals what is below
/// (a positive `dy` scroll), `Up` what is above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Down,
    Up,
}

impl Direction {
    pub fn sign(self) -> f64 {
        match self {
            Direction::Down => 1.0,
            Direction::Up => -1.0,
        }
    }
}

/// The `dy` of one page-sized scroll inside a region `height_pts` tall:
/// `/agent/input` scrolls travel 1.5×|dy| points, so this moves about 60% of
/// the region and leaves the rest on screen as overlap — no row is skipped.
pub fn page_dy(height_pts: f64, direction: Direction) -> f64 {
    direction.sign() * (height_pts * 0.4).clamp(60.0, 400.0)
}

fn parse_region(value: &serde_json::Value) -> Result<Region, String> {
    let bad = || {
        "region must be [x, y, w, h] normalized to 0..1, inside the screen, with w and h > 0"
            .to_string()
    };
    let parts = value.as_array().ok_or_else(bad)?;
    let numbers: Vec<f64> = parts.iter().filter_map(serde_json::Value::as_f64).collect();
    let [x, y, w, h] = numbers.as_slice() else {
        return Err(bad());
    };
    let (x, y, w, h) = (*x, *y, *w, *h);
    let finite = [x, y, w, h].iter().all(|v| v.is_finite());
    if !finite
        || x < 0.0
        || y < 0.0
        || w <= 0.0
        || h <= 0.0
        || x + w > 1.0 + 1e-9
        || y + h > 1.0 + 1e-9
    {
        return Err(bad());
    }
    Ok(Region { x, y, w, h })
}

fn parse_direction(value: Option<&serde_json::Value>) -> Result<Direction, String> {
    match value.and_then(serde_json::Value::as_str) {
        None => match value {
            None | Some(serde_json::Value::Null) => Ok(Direction::Down),
            Some(_) => Err("direction must be \"down\" or \"up\"".to_string()),
        },
        Some("down") => Ok(Direction::Down),
        Some("up") => Ok(Direction::Up),
        Some(_) => Err(
            "direction must be \"down\" or \"up\" (the way the content moves into view)"
                .to_string(),
        ),
    }
}

fn parse_kind(
    value: Option<&serde_json::Value>,
    default: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(default.map(str::to_string));
    };
    let kind = value
        .as_str()
        .ok_or("kind must be a string such as \"Cell\"")?;
    let kind = kind.strip_prefix("XCUIElementType").unwrap_or(kind);
    if kind.is_empty() || kind.len() > 40 || !kind.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(
            "kind must be an element type name such as \"Cell\" or \"StaticText\"".to_string(),
        );
    }
    Ok(Some(kind.to_string()))
}

fn parse_count(
    value: Option<&serde_json::Value>,
    name: &str,
    default: u32,
    min: u32,
    max: u32,
) -> Result<u32, String> {
    match value.filter(|v| !v.is_null()) {
        None => Ok(default),
        Some(value) => match value.as_u64() {
            Some(n) if (min as u64..=max as u64).contains(&n) => Ok(n as u32),
            _ => Err(format!("{name} must be an integer from {min} to {max}")),
        },
    }
}

fn parse_label(value: Option<&serde_json::Value>, name: &str) -> Result<Option<String>, String> {
    match value.filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(value) => match value.as_str() {
            Some(label) if !label.is_empty() && label.chars().count() <= 500 => {
                Ok(Some(label.to_string()))
            }
            _ => Err(format!("{name} must be a non-empty string")),
        },
    }
}

fn reject_unknown(
    body: &serde_json::Map<String, serde_json::Value>,
    known: &[&str],
) -> Result<(), String> {
    match body.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(format!(
            "unknown field \"{key}\"; accepted: {}",
            known.join(", ")
        )),
        None => Ok(()),
    }
}

/// `POST /agent/collect` body.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectRequest {
    pub row_kind: String,
    pub region: Option<Region>,
    pub max_pages: u32,
    pub end_label: Option<String>,
    pub direction: Direction,
}

impl CollectRequest {
    pub fn from_json(body: &serde_json::Value) -> Result<Self, String> {
        let empty = serde_json::Map::new();
        let body = match body {
            serde_json::Value::Object(map) => map,
            serde_json::Value::Null => &empty,
            _ => return Err("the body must be a JSON object".to_string()),
        };
        reject_unknown(
            body,
            &["row_kind", "region", "max_pages", "end_label", "direction"],
        )?;
        Ok(Self {
            row_kind: parse_kind(body.get("row_kind"), Some(DEFAULT_ROW_KIND))?
                .unwrap_or_else(|| DEFAULT_ROW_KIND.to_string()),
            region: body
                .get("region")
                .filter(|v| !v.is_null())
                .map(parse_region)
                .transpose()?,
            max_pages: parse_count(
                body.get("max_pages"),
                "max_pages",
                DEFAULT_MAX_PAGES,
                1,
                MAX_PAGES_CAP,
            )?,
            end_label: parse_label(body.get("end_label"), "end_label")?,
            direction: parse_direction(body.get("direction"))?,
        })
    }
}

/// `POST /agent/scroll_find` body.
#[derive(Debug, Clone, PartialEq)]
pub struct FindRequest {
    pub label: String,
    pub kind: Option<String>,
    pub max_swipes: u32,
    pub region: Option<Region>,
    pub direction: Direction,
}

impl FindRequest {
    pub fn from_json(body: &serde_json::Value) -> Result<Self, String> {
        let body = body
            .as_object()
            .ok_or("the body must be a JSON object with a \"label\"")?;
        reject_unknown(
            body,
            &["label", "kind", "max_swipes", "region", "direction"],
        )?;
        Ok(Self {
            label: parse_label(body.get("label"), "label")?.ok_or("label is required")?,
            kind: parse_kind(body.get("kind"), None)?,
            max_swipes: parse_count(
                body.get("max_swipes"),
                "max_swipes",
                DEFAULT_FIND_SWIPES,
                0,
                MAX_FIND_SWIPES,
            )?,
            region: body
                .get("region")
                .filter(|v| !v.is_null())
                .map(parse_region)
                .transpose()?,
            direction: parse_direction(body.get("direction"))?,
        })
    }
}

/// One read of the screen.
pub struct Page {
    pub rows: Vec<ElementRow>,
    /// Screen size in points, when known.
    pub screen: Option<(f64, f64)>,
}

/// What the collection loop needs from a phone.
pub(crate) trait Pager {
    async fn read(&mut self) -> anyhow::Result<Page>;
    /// One page-sized swipe inside `region` (whole screen when `None`).
    async fn swipe(&mut self, region: Option<Region>, direction: Direction) -> anyhow::Result<()>;
}

/// Index one past the last descendant of `rows[index]` (pre-order rows).
fn subtree_end(rows: &[ElementRow], index: usize) -> usize {
    let depth = rows[index].depth;
    rows.iter()
        .enumerate()
        .skip(index + 1)
        .find(|(_, row)| row.depth <= depth)
        .map_or(rows.len(), |(i, _)| i)
}

/// The visible area in points: the region on the screen, else the screen
/// (from the size, else the Application root row's frame).
fn viewport(
    rows: &[ElementRow],
    screen: Option<(f64, f64)>,
    region: Option<Region>,
) -> Option<[f64; 4]> {
    let (width, height) = screen.or_else(|| {
        rows.first()
            .filter(|row| row.kind == "Application" && row.rect[2] > 0.0 && row.rect[3] > 0.0)
            .map(|row| (row.rect[2], row.rect[3]))
    })?;
    Some(region.unwrap_or(Region::FULL).to_points(width, height))
}

/// The row is drawn and its centre lies inside `area` (any centre when the
/// area is unknown).
pub fn row_in_area(row: &ElementRow, area: Option<[f64; 4]>) -> bool {
    let [x, y, w, h] = row.rect;
    if row.visible == Some(false) || w <= 0.0 || h <= 0.0 {
        return false;
    }
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    area.is_none_or(|[ax, ay, aw, ah]| cx >= ax && cx <= ax + aw && cy >= ay && cy <= ay + ah)
}

/// A collected row: what identifies it, and the page it was first seen on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CollectedRow {
    pub kind: String,
    /// The row's own label, or — for a row without one (most Cells) — its
    /// descendant texts joined with " · ".
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// The descendant texts, when the label was built from them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub texts: Vec<String>,
    pub page: u32,
}

impl CollectedRow {
    fn key(&self) -> (String, String, Option<String>) {
        (self.kind.clone(), self.label.clone(), self.value.clone())
    }
}

/// The rows of `kind` on this page, in tree order. A row with neither a
/// label, descendant text nor value carries nothing to collect and is left out.
pub fn page_rows(
    rows: &[ElementRow],
    screen: Option<(f64, f64)>,
    region: Option<Region>,
    kind: &str,
    page: u32,
) -> Vec<(CollectedRow, [f64; 4])> {
    let area = viewport(rows, screen, region);
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.kind == kind && row_in_area(row, area))
        .filter_map(|(index, row)| {
            let mut texts = Vec::new();
            let label = if row.label.trim().is_empty() {
                for child in &rows[index + 1..subtree_end(rows, index)] {
                    let text = child.label.trim();
                    if child.kind == "StaticText"
                        && !text.is_empty()
                        && !texts.iter().any(|t| t == text)
                    {
                        texts.push(text.to_string());
                        if texts.len() == MAX_ROW_TEXTS {
                            break;
                        }
                    }
                }
                texts.join(" · ")
            } else {
                row.label.clone()
            };
            if label.is_empty() && row.value.as_deref().is_none_or(str::is_empty) {
                return None;
            }
            Some((
                CollectedRow {
                    kind: row.kind.clone(),
                    label,
                    value: row.value.clone().filter(|v| !v.is_empty()),
                    identifier: row.identifier.clone(),
                    texts,
                    page,
                },
                row.rect,
            ))
        })
        .collect()
}

/// What a page looks like for "did the list move": the rows' kinds, labels
/// and frames. Values are left out on purpose — a counter or a timestamp that
/// refreshes in place is not a new page.
fn page_key(rows: &[(CollectedRow, [f64; 4])]) -> Vec<(String, String, [i64; 4])> {
    rows.iter()
        .map(|(row, rect)| {
            (
                row.kind.clone(),
                row.label.clone(),
                rect.map(|v| v.round() as i64),
            )
        })
        .collect()
}

/// An on-screen row labelled exactly `label`, of any kind.
fn end_label_on_screen(
    rows: &[ElementRow],
    screen: Option<(f64, f64)>,
    region: Option<Region>,
    label: &str,
) -> bool {
    let area = viewport(rows, screen, region);
    rows.iter()
        .any(|row| row.label == label && row_in_area(row, area))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The end label was on screen: the only proof of having reached the end.
    EndLabel,
    /// The page after a swipe was identical to one already read: the list
    /// did not move (its end, or a swipe that hit something else).
    DuplicatePage,
    /// The list moved but showed no row not already collected.
    NoProgress,
    MaxPages,
    Deadline,
    /// A read after a swipe failed; what was collected before is returned.
    ReadFailed,
    /// The swipe itself failed; nothing after it was read.
    SwipeFailed,
    /// The first page had no rows of the requested kind.
    NoRows,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::EndLabel => "end_label",
            StopReason::DuplicatePage => "duplicate_page",
            StopReason::NoProgress => "no_progress",
            StopReason::MaxPages => "max_pages",
            StopReason::Deadline => "deadline",
            StopReason::ReadFailed => "read_failed",
            StopReason::SwipeFailed => "swipe_failed",
            StopReason::NoRows => "no_rows",
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PageReport {
    pub page: u32,
    pub rows_seen: usize,
    pub new_rows: usize,
}

pub struct Collection {
    pub rows: Vec<CollectedRow>,
    pub pages: Vec<PageReport>,
    pub swipes: u32,
    pub stop: StopReason,
    pub end_label_seen: bool,
    /// The last page read (for a snapshot the caller can act on).
    pub last: Page,
    /// Why the run stopped early, when a read or swipe failed.
    pub error: Option<String>,
}

/// Read, swipe, read again — until something says stop. Fails only when the
/// first read fails: then nothing was collected and nothing was swiped.
pub(crate) async fn collect<P: Pager>(
    pager: &mut P,
    request: &CollectRequest,
    deadline: tokio::time::Instant,
) -> anyhow::Result<Collection> {
    let mut page = pager.read().await?;
    let mut rows: Vec<CollectedRow> = Vec::new();
    let mut seen: HashSet<(String, String, Option<String>)> = HashSet::new();
    let mut seen_pages: Vec<Vec<(String, String, [i64; 4])>> = Vec::new();
    let mut pages = Vec::new();
    let mut swipes = 0;
    let mut end_label_seen = false;
    let mut error = None;
    let mut number = 1;
    let stop = loop {
        let current = page_rows(
            &page.rows,
            page.screen,
            request.region,
            &request.row_kind,
            number,
        );
        let key = page_key(&current);
        let duplicate = number > 1 && seen_pages.contains(&key);
        let mut new_rows = 0;
        if !duplicate {
            for (row, _) in &current {
                if seen.insert(row.key()) {
                    rows.push(row.clone());
                    new_rows += 1;
                }
            }
        }
        pages.push(PageReport {
            page: number,
            rows_seen: current.len(),
            new_rows,
        });
        if let Some(label) = &request.end_label {
            if end_label_on_screen(&page.rows, page.screen, request.region, label) {
                end_label_seen = true;
                break StopReason::EndLabel;
            }
        }
        if number == 1 && current.is_empty() {
            break StopReason::NoRows;
        }
        if duplicate {
            break StopReason::DuplicatePage;
        }
        if number > 1 && new_rows == 0 {
            break StopReason::NoProgress;
        }
        if number >= request.max_pages {
            break StopReason::MaxPages;
        }
        if too_late_to_swipe(deadline) {
            break StopReason::Deadline;
        }
        seen_pages.push(key);
        if let Err(e) = pager.swipe(request.region, request.direction).await {
            error = Some(format!("{e:#}"));
            break StopReason::SwipeFailed;
        }
        swipes += 1;
        match pager.read().await {
            Ok(next) => page = next,
            Err(e) => {
                error = Some(format!("{e:#}"));
                break StopReason::ReadFailed;
            }
        }
        number += 1;
    };
    Ok(Collection {
        rows,
        pages,
        swipes,
        stop,
        end_label_seen,
        last: page,
        error,
    })
}

/// What the caller should and should not conclude from a collection.
pub fn coverage_note(collection: &Collection) -> String {
    let dedupe = "rows are deduplicated on (kind, label, value), so identical-looking rows collapse into one; only what the accessibility tree exposes is collected";
    match collection.stop {
        StopReason::EndLabel => format!(
            "the end label was on screen on page {}: the list was read to that point. {dedupe}.",
            collection.pages.len()
        ),
        StopReason::DuplicatePage | StopReason::NoProgress => format!(
            "NOT proven complete: the list stopped yielding new rows ({}), which may be its end or a swipe that did not scroll it. Check the screen or pass end_label. {dedupe}.",
            collection.stop.as_str()
        ),
        StopReason::MaxPages => format!(
            "NOT complete: the page budget ran out after {} pages; more rows may follow. {dedupe}.",
            collection.pages.len()
        ),
        StopReason::NoRows => "no rows of that kind were on screen: check row_kind (phone_elements shows the kinds) or region.".to_string(),
        _ => format!(
            "NOT complete: the collection stopped early ({}). {dedupe}.",
            collection.stop.as_str()
        ),
    }
}

/// The JSON answer for a finished collection (`snapshot` is the last page's).
pub fn collection_json(collection: &Collection, snapshot: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({
        "ok": true,
        "rows": collection.rows,
        "row_count": collection.rows.len(),
        "pages": collection.pages,
        "swipes": collection.swipes,
        "stop_reason": collection.stop.as_str(),
        "end_label_seen": collection.end_label_seen,
        "complete": collection.end_label_seen,
        "coverage": coverage_note(collection),
    });
    if let Some(snapshot) = snapshot {
        body["snapshot"] = snapshot.into();
        body["snapshot_note"] = "the snapshot is the last page's tree; rows from earlier pages are no longer on screen — find them with phone_scroll_find or a label tap".into();
    }
    if let Some(error) = &collection.error {
        body["stop_error"] = error.clone().into();
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, label: &str, y: f64, depth: u32) -> ElementRow {
        ElementRow {
            kind: kind.to_string(),
            label: label.to_string(),
            rect: [0.0, y, 390.0, 44.0],
            depth,
            ..ElementRow::default()
        }
    }

    fn screen_of(labels: &[(&str, f64)]) -> Vec<ElementRow> {
        let mut rows = vec![{
            let mut root = row("Application", "App", 0.0, 0);
            root.rect = [0.0, 0.0, 390.0, 844.0];
            root
        }];
        for (label, y) in labels {
            rows.push(row("Cell", "", *y, 1));
            rows.push(row("StaticText", label, *y, 2));
        }
        rows
    }

    struct Scripted {
        pages: Vec<Vec<ElementRow>>,
        at: usize,
        swipes: Vec<(Option<Region>, Direction)>,
        fail_read_at: Option<usize>,
    }

    impl Scripted {
        fn new(pages: Vec<Vec<ElementRow>>) -> Self {
            Self {
                pages,
                at: 0,
                swipes: Vec::new(),
                fail_read_at: None,
            }
        }
    }

    impl Pager for Scripted {
        async fn read(&mut self) -> anyhow::Result<Page> {
            if self.fail_read_at == Some(self.at) {
                anyhow::bail!("source failed");
            }
            let rows = self.pages[self.at.min(self.pages.len() - 1)].clone();
            Ok(Page {
                rows,
                screen: Some((390.0, 844.0)),
            })
        }
        async fn swipe(
            &mut self,
            region: Option<Region>,
            direction: Direction,
        ) -> anyhow::Result<()> {
            self.swipes.push((region, direction));
            self.at += 1;
            Ok(())
        }
    }

    fn run(pager: &mut Scripted, request: &CollectRequest) -> Collection {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(collect(
                pager,
                request,
                tokio::time::Instant::now() + Duration::from_secs(60),
            ))
            .unwrap()
    }

    fn request(body: serde_json::Value) -> CollectRequest {
        CollectRequest::from_json(&body).unwrap()
    }

    fn labels(collection: &Collection) -> Vec<&str> {
        collection
            .rows
            .iter()
            .map(|row| row.label.as_str())
            .collect()
    }

    #[test]
    fn rows_accumulate_across_pages_and_a_repeated_page_stops() {
        let mut pager = Scripted::new(vec![
            screen_of(&[("A", 100.0), ("B", 200.0), ("C", 300.0)]),
            screen_of(&[("B", 100.0), ("C", 200.0), ("D", 300.0)]),
            screen_of(&[("B", 100.0), ("C", 200.0), ("D", 300.0)]),
        ]);
        let collection = run(&mut pager, &request(serde_json::json!({})));
        assert_eq!(labels(&collection), ["A", "B", "C", "D"]);
        assert_eq!(collection.stop, StopReason::DuplicatePage);
        assert_eq!(collection.swipes, 2);
        assert!(!collection.end_label_seen);
        let json = collection_json(&collection, Some("snap"));
        assert_eq!(json["complete"], false);
        assert_eq!(json["stop_reason"], "duplicate_page");
        assert!(json["coverage"]
            .as_str()
            .unwrap()
            .starts_with("NOT proven complete"));
        assert_eq!(json["rows"][3]["page"], 2);
    }

    #[test]
    fn only_the_end_label_makes_a_collection_complete() {
        let mut pager = Scripted::new(vec![
            screen_of(&[("A", 100.0), ("B", 200.0)]),
            screen_of(&[("C", 100.0), ("没有更多了", 700.0)]),
        ]);
        let collection = run(
            &mut pager,
            &request(serde_json::json!({"end_label": "没有更多了"})),
        );
        assert_eq!(collection.stop, StopReason::EndLabel);
        assert!(collection.end_label_seen);
        assert_eq!(collection_json(&collection, None)["complete"], true);
        assert_eq!(collection.swipes, 1);
    }

    #[test]
    fn the_page_budget_stops_and_is_not_complete() {
        let pages = (0..10)
            .map(|n| screen_of(&[(["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"][n], 100.0)]))
            .collect();
        let mut pager = Scripted::new(pages);
        let collection = run(&mut pager, &request(serde_json::json!({"max_pages": 3})));
        assert_eq!(collection.stop, StopReason::MaxPages);
        assert_eq!(collection.pages.len(), 3);
        assert_eq!(collection.swipes, 2, "no swipe after the last page");
        assert_eq!(collection_json(&collection, None)["complete"], false);
    }

    #[test]
    fn a_moved_page_with_nothing_new_is_no_progress() {
        let mut pager = Scripted::new(vec![
            screen_of(&[("A", 100.0), ("B", 200.0)]),
            screen_of(&[("A", 150.0), ("B", 250.0)]),
        ]);
        let collection = run(&mut pager, &request(serde_json::json!({})));
        assert_eq!(collection.stop, StopReason::NoProgress);
        assert_eq!(labels(&collection), ["A", "B"]);
    }

    #[test]
    fn a_value_refreshing_in_place_is_still_the_same_page() {
        let mut first = screen_of(&[("A", 100.0)]);
        first[1].value = Some("1".into());
        let mut second = screen_of(&[("A", 100.0)]);
        second[1].value = Some("2".into());
        let mut pager = Scripted::new(vec![first, second]);
        let collection = run(&mut pager, &request(serde_json::json!({})));
        assert_eq!(collection.stop, StopReason::DuplicatePage);
        assert_eq!(collection.rows.len(), 1);
    }

    #[test]
    fn identical_rows_collapse_on_kind_label_value() {
        let mut pager = Scripted::new(vec![screen_of(&[
            ("Same", 100.0),
            ("Same", 200.0),
            ("Other", 300.0),
        ])]);
        let collection = run(&mut pager, &request(serde_json::json!({"max_pages": 1})));
        assert_eq!(labels(&collection), ["Same", "Other"]);
        assert!(collection_json(&collection, None)["coverage"]
            .as_str()
            .unwrap()
            .contains("identical-looking rows collapse"));
    }

    #[test]
    fn region_and_visibility_bound_a_page() {
        let mut rows = screen_of(&[("Top", 50.0), ("Mid", 400.0), ("Gone", 1200.0)]);
        rows.push({
            let mut hidden = row("Cell", "Hidden", 500.0, 1);
            hidden.visible = Some(false);
            hidden
        });
        let all = page_rows(&rows, Some((390.0, 844.0)), None, "Cell", 1);
        let names: Vec<_> = all.iter().map(|(row, _)| row.label.as_str()).collect();
        assert_eq!(
            names,
            ["Top", "Mid"],
            "off-screen and undrawn rows are not on the page"
        );
        let lower = Region {
            x: 0.0,
            y: 0.25,
            w: 1.0,
            h: 0.75,
        };
        let inside = page_rows(&rows, Some((390.0, 844.0)), Some(lower), "Cell", 1);
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].0.label, "Mid");
    }

    #[test]
    fn a_failed_read_after_a_swipe_keeps_what_was_collected() {
        let mut pager = Scripted::new(vec![screen_of(&[("A", 100.0)]), screen_of(&[("B", 100.0)])]);
        pager.fail_read_at = Some(1);
        let collection = run(&mut pager, &request(serde_json::json!({})));
        assert_eq!(collection.stop, StopReason::ReadFailed);
        assert_eq!(labels(&collection), ["A"]);
        assert!(collection_json(&collection, None)["stop_error"].is_string());
    }

    #[test]
    fn no_rows_of_the_kind_stops_at_once() {
        let mut pager = Scripted::new(vec![screen_of(&[("A", 100.0)])]);
        let collection = run(
            &mut pager,
            &request(serde_json::json!({"row_kind": "Button"})),
        );
        assert_eq!(collection.stop, StopReason::NoRows);
        assert!(pager.swipes.is_empty());
    }

    #[test]
    fn swipes_go_inside_the_region_in_the_asked_direction() {
        let mut pager = Scripted::new(vec![screen_of(&[("A", 400.0)]), screen_of(&[("B", 400.0)])]);
        let body =
            serde_json::json!({"region": [0.0, 0.2, 1.0, 0.6], "direction": "up", "max_pages": 2});
        run(&mut pager, &request(body));
        assert_eq!(
            pager.swipes,
            vec![(
                Some(Region {
                    x: 0.0,
                    y: 0.2,
                    w: 1.0,
                    h: 0.6
                }),
                Direction::Up
            )]
        );
        assert!(page_dy(844.0 * 0.6, Direction::Up) < 0.0);
        assert_eq!(page_dy(2000.0, Direction::Down), 400.0, "capped");
    }

    #[test]
    fn no_swipe_starts_without_time_to_read_its_page() {
        let mut pager = Scripted::new(vec![screen_of(&[("A", 100.0)]), screen_of(&[("B", 100.0)])]);
        let request = request(serde_json::json!({}));
        let collection = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(collect(
                &mut pager,
                &request,
                tokio::time::Instant::now() + READ_RESERVE / 2,
            ))
            .unwrap();
        assert_eq!(collection.stop, StopReason::Deadline);
        assert!(pager.swipes.is_empty(), "the screen is not moved past the budget");
    }

    #[test]
    fn requests_are_validated() {
        let defaults = request(serde_json::json!({}));
        assert_eq!(defaults.row_kind, "Cell");
        assert_eq!(defaults.max_pages, 6);
        assert_eq!(defaults.direction, Direction::Down);
        assert_eq!(
            request(serde_json::json!({"row_kind": "XCUIElementTypeStaticText"})).row_kind,
            "StaticText"
        );
        for bad in [
            serde_json::json!({"max_pages": 11}),
            serde_json::json!({"max_pages": 0}),
            serde_json::json!({"region": [0.5, 0.5, 0.6, 0.1]}),
            serde_json::json!({"region": [0, 0, 1]}),
            serde_json::json!({"direction": "left"}),
            serde_json::json!({"row_kind": "Cell; rm"}),
            serde_json::json!({"rows": 3}),
            serde_json::json!([]),
        ] {
            assert!(CollectRequest::from_json(&bad).is_err(), "{bad}");
        }
        let find = FindRequest::from_json(&serde_json::json!({"label": "设置"})).unwrap();
        assert_eq!(find.max_swipes, 1);
        assert!(FindRequest::from_json(&serde_json::json!({})).is_err());
        assert!(
            FindRequest::from_json(&serde_json::json!({"label": "x", "max_swipes": 6})).is_err()
        );
    }
}
