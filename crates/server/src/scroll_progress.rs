//! Whether a scroll left the visible content exactly where it was.
//!
//! A wrong "no progress" misleads an agent more than no verdict at all, so the
//! rule only answers when the tree proves that nothing moved: every row (any
//! kind, labelled or not, duplicates included) is still at its origin, or was
//! replaced in place by a row whose label differs only in its digits (a clock
//! tick, a counter). Anything else (a row that moved in any direction, a row
//! that appeared or disappeared, a label swapped for different text, as when a
//! feed or carousel pages) counts as possible progress.

use crate::runner::ElementRow;
use std::collections::HashMap;

/// Origins within this many points on both axes are "the same place".
const SAME_ORIGIN: f64 = 2.0;

/// Kinds whose `value` follows the scroll offset or the page: a changed value
/// on one of these is progress even when nothing else moved.
const POSITION_VALUE_KINDS: &[&str] = &["ScrollBar", "PageIndicator"];

type Key<'a> = (&'a str, &'a str, Option<&'a str>);

fn key(row: &ElementRow) -> Key<'_> {
    (
        row.kind.as_str(),
        row.label.as_str(),
        row.identifier.as_deref(),
    )
}

fn same_origin(a: &ElementRow, b: &ElementRow) -> bool {
    (a.rect[0] - b.rect[0]).abs() <= SAME_ORIGIN && (a.rect[1] - b.rect[1]).abs() <= SAME_ORIGIN
}

/// A labelled row that stayed put is evidence; the application or window
/// frame always stays put, so it proves nothing.
fn proves_still(row: &ElementRow) -> bool {
    !row.label.is_empty() && !matches!(row.kind.as_str(), "Application" | "Window")
}

/// The label with every run of digits collapsed to `#`: `"9:59"` and
/// `"10:00"` both read `"#:#"`; `"@alice"` and `"@bob"` stay different.
fn digit_shape(label: &str) -> String {
    let mut shape = String::with_capacity(label.len());
    let mut in_digits = false;
    for c in label.chars() {
        if c.is_numeric() {
            if !in_digits {
                shape.push('#');
            }
            in_digits = true;
        } else {
            shape.push(c);
            in_digits = false;
        }
    }
    shape
}

/// The same element with a new number drawn at the same place.
fn redrawn_in_place(before: &ElementRow, after: &ElementRow) -> bool {
    before.kind == after.kind
        && before.identifier == after.identifier
        && same_origin(before, after)
        && !before.label.is_empty()
        && before.label != after.label
        && digit_shape(&before.label) == digit_shape(&after.label)
}

/// `Some(still)` when the scroll provably moved nothing: every baseline row
/// is matched by a current row with the same kind, label and identifier at
/// the same origin (sizes and plain `value` changes are ignored), every
/// unmatched row is a digits-only label redrawn at the same origin, there are
/// no more such redraws than still rows, and no scroll bar or page indicator
/// changed its value. `still` counts the labelled rows (other than the
/// application/window) that kept their place, and is at least 1.
///
/// `None` means possible progress or no evidence: a row moved (any direction,
/// any kind), a row appeared or disappeared, a label changed to different
/// text, no labelled row stayed, or `dx`/`dy` is zero or not finite. `dx`/`dy`
/// follow the `scroll` convention (positive reveals right/below); for a direct
/// `swipe`/`drag` pass start minus end. Only their validity matters here,
/// since a move in any direction already counts. `Some` does not mean the
/// list has been read completely.
pub fn scroll_without_progress(
    baseline: &[ElementRow],
    current: &[ElementRow],
    dx: f64,
    dy: f64,
) -> Option<usize> {
    if !dx.is_finite() || !dy.is_finite() || (dx == 0.0 && dy == 0.0) {
        return None;
    }
    let mut unmatched: HashMap<Key<'_>, Vec<&ElementRow>> = HashMap::new();
    for row in current {
        unmatched.entry(key(row)).or_default().push(row);
    }
    let mut removed = Vec::new();
    let mut still = 0usize;
    for row in baseline {
        let next = unmatched.get_mut(&key(row)).and_then(|rows| {
            let index = rows.iter().position(|next| same_origin(row, next))?;
            Some(rows.swap_remove(index))
        });
        match next {
            Some(next) => {
                if POSITION_VALUE_KINDS.contains(&row.kind.as_str()) && row.value != next.value {
                    return None;
                }
                if proves_still(row) {
                    still += 1;
                }
            }
            None => removed.push(row),
        }
    }
    let mut added: Vec<&ElementRow> = unmatched.into_values().flatten().collect();
    let mut redrawn = 0usize;
    for row in removed {
        let index = added.iter().position(|next| redrawn_in_place(row, next))?;
        added.swap_remove(index);
        redrawn += 1;
    }
    (added.is_empty() && still > 0 && redrawn <= still).then_some(still)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> ElementRow {
        ElementRow {
            kind: "Cell".into(),
            label: "Stable item".into(),
            rect: [20.0, 100.0, 80.0, 40.0],
            ..Default::default()
        }
    }

    fn labelled(kind: &str, label: &str, x: f64, y: f64) -> ElementRow {
        ElementRow {
            kind: kind.into(),
            label: label.into(),
            rect: [x, y, 200.0, 30.0],
            ..Default::default()
        }
    }

    fn anchors(count: usize) -> Vec<ElementRow> {
        (0..count)
            .map(|i| labelled("StaticText", &format!("Anchor {i}"), 20.0, 40.0 * i as f64))
            .collect()
    }

    #[test]
    fn scroll_progress_still_ignores_size_but_requires_both_origin_axes() {
        let mut next = row();
        next.rect[2] += 20.0;
        next.rect[3] += 41.0;
        assert_eq!(
            scroll_without_progress(&[row()], &[next.clone()], 0.0, 100.0),
            Some(1)
        );
        next.rect[0] += 2.1;
        assert_eq!(scroll_without_progress(&[row()], &[next], 0.0, 100.0), None);
    }

    #[test]
    fn scroll_progress_any_motion_counts_in_any_direction() {
        // Within 2 pt is still; 2.1 pt along, against, or across the scroll is motion.
        let mut next = row();
        next.rect[1] -= 2.0;
        assert_eq!(
            scroll_without_progress(&[row()], &[next], 0.0, 100.0),
            Some(1)
        );
        for (dx, dy) in [(0.0, -2.1), (0.0, 2.1), (2.1, 0.0), (-10.0, 0.0)] {
            let mut next = row();
            next.rect[0] += dx;
            next.rect[1] += dy;
            for (sx, sy) in [(0.0, 100.0), (0.0, -100.0), (100.0, 0.0)] {
                assert_eq!(
                    scroll_without_progress(&[row()], &[next.clone()], sx, sy),
                    None,
                    "move ({dx},{dy}) scroll ({sx},{sy})"
                );
            }
        }
    }

    #[test]
    fn scroll_progress_button_rows_under_fixed_title_are_progress() {
        // Review probe 1: a SwiftUI-style list of Buttons (and one of
        // unlabelled Cells) scrolls under a fixed inline title. The title is
        // the only anchor-like row and stays put; the rows moved.
        let title = labelled("StaticText", "Settings", 20.0, 50.0);
        let button_rows = |offset: f64| -> Vec<ElementRow> {
            (0..6)
                .map(|i| labelled("Button", &format!("Row {i}"), 0.0, 100.0 + 44.0 * i as f64 - offset))
                .filter(|row| row.rect[1] >= 100.0)
                .collect()
        };
        let mut baseline = vec![title.clone()];
        baseline.extend(button_rows(0.0));
        let mut current = vec![title.clone()];
        current.extend(button_rows(44.0));
        assert_eq!(scroll_without_progress(&baseline, &current, 0.0, 100.0), None);

        let cells = |offset: f64| -> Vec<ElementRow> {
            (0..6)
                .map(|i| ElementRow {
                    kind: "Cell".into(),
                    rect: [0.0, 100.0 + 44.0 * i as f64 - offset, 393.0, 44.0],
                    ..Default::default()
                })
                .collect()
        };
        let mut baseline = vec![title.clone()];
        baseline.extend(cells(0.0));
        let mut current = vec![title];
        current.extend(cells(44.0));
        assert_eq!(scroll_without_progress(&baseline, &current, 0.0, 100.0), None);
    }

    #[test]
    fn scroll_progress_paged_carousel_swap_is_progress() {
        // Review probe 2: a vertical feed or carousel snaps to the next page.
        // Chrome stays put; the author and caption are replaced at the same
        // origin rather than moved.
        let chrome = vec![
            labelled("StaticText", "For You", 150.0, 60.0),
            labelled("Button", "Like", 340.0, 500.0),
            labelled("Button", "Share", 340.0, 580.0),
            labelled("Image", "Logo", 20.0, 60.0),
        ];
        let page = |author: &str, caption: &str| {
            let mut rows = chrome.clone();
            rows.push(labelled("StaticText", author, 20.0, 700.0));
            rows.push(labelled("StaticText", caption, 20.0, 740.0));
            rows
        };
        let baseline = page("@alice", "Sunset at the beach");
        let current = page("@bob", "Three-minute noodles");
        for (dx, dy) in [(0.0, 100.0), (100.0, 0.0)] {
            assert_eq!(scroll_without_progress(&baseline, &current, dx, dy), None);
        }
        // Only the author changed: still a different label, still progress.
        let current = page("@bob", "Sunset at the beach");
        assert_eq!(scroll_without_progress(&baseline, &current, 0.0, 100.0), None);
    }

    #[test]
    fn scroll_progress_full_page_replacement_is_unknown() {
        let mut baseline = anchors(1);
        let mut current = baseline.clone();
        for index in 0..6 {
            let mut old = row();
            old.label = format!("Old item {index}");
            old.rect[1] += index as f64 * 40.0;
            let mut new = old.clone();
            new.label = format!("New item {index}");
            baseline.push(old);
            current.push(new);
        }
        assert_eq!(scroll_without_progress(&baseline, &current, 0.0, 100.0), None);
    }

    #[test]
    fn scroll_progress_clock_ticks_in_place_are_not_progress() {
        for count in 1..=2 {
            let mut baseline = anchors(3);
            let mut current = baseline.clone();
            for index in 0..count {
                let y = 200.0 + 40.0 * index as f64;
                baseline.push(labelled("StaticText", &format!("Timestamp {index}: 9:59"), 20.0, y));
                current.push(labelled("StaticText", &format!("Timestamp {index}: 10:00"), 20.0, y));
            }
            assert_eq!(
                scroll_without_progress(&baseline, &current, 0.0, 100.0),
                Some(3),
                "count={count}"
            );
        }
    }

    #[test]
    fn scroll_progress_redraws_must_stay_in_place_and_be_outnumbered() {
        // A redrawn number that also moved is motion.
        let mut baseline = anchors(3);
        let mut current = baseline.clone();
        baseline.push(labelled("StaticText", "10:00", 20.0, 200.0));
        current.push(labelled("StaticText", "10:01", 20.0, 150.0));
        assert_eq!(scroll_without_progress(&baseline, &current, 0.0, 100.0), None);
        // A page of numbers swapped under one fixed row (prices in a carousel).
        let mut baseline = anchors(1);
        let mut current = baseline.clone();
        for (index, (old, new)) in [("¥299", "¥159"), ("4.5", "3.9")].into_iter().enumerate() {
            let y = 200.0 + 40.0 * index as f64;
            baseline.push(labelled("StaticText", old, 20.0, y));
            current.push(labelled("StaticText", new, 20.0, y));
        }
        assert_eq!(scroll_without_progress(&baseline, &current, 0.0, 100.0), None);
    }

    #[test]
    fn scroll_progress_two_cell_replacement_is_unknown() {
        let mut baseline = anchors(1);
        let mut current = baseline.clone();
        for (index, (old_label, new_label)) in [("A", "C"), ("B", "D")].into_iter().enumerate() {
            let mut old = row();
            old.label = old_label.into();
            old.rect = [0.0, 100.0 + index as f64 * 300.0, 393.0, 300.0];
            let mut new = old.clone();
            new.label = new_label.into();
            baseline.push(old);
            current.push(new);
        }
        assert_eq!(scroll_without_progress(&baseline, &current, 0.0, 100.0), None);
    }

    #[test]
    fn scroll_progress_any_appeared_or_removed_row_is_progress() {
        let baseline = anchors(16);
        let mut current = baseline.clone();
        current.push(labelled("Cell", "Appeared", 20.0, 900.0));
        assert_eq!(scroll_without_progress(&baseline, &current, 0.0, 100.0), None);
        let mut current = baseline.clone();
        current.pop();
        assert_eq!(scroll_without_progress(&baseline, &current, 0.0, 100.0), None);
        // An unlabelled row appearing counts too.
        let mut current = baseline.clone();
        current.push(ElementRow {
            kind: "Other".into(),
            rect: [0.0, 900.0, 393.0, 44.0],
            ..Default::default()
        });
        assert_eq!(scroll_without_progress(&baseline, &current, 0.0, 100.0), None);
    }

    #[test]
    fn scroll_progress_value_only_is_not_motion() {
        let mut next = row();
        next.value = Some("new price".into());
        assert_eq!(
            scroll_without_progress(&[row()], &[next], 0.0, 100.0),
            Some(1)
        );
    }

    #[test]
    fn scroll_progress_scroll_position_value_change_is_progress() {
        for kind in POSITION_VALUE_KINDS {
            let mut indicator = labelled(kind, "", 0.0, 800.0);
            indicator.value = Some("page 1 of 3".into());
            let mut baseline = anchors(3);
            baseline.push(indicator.clone());
            let mut current = anchors(3);
            indicator.value = Some("page 2 of 3".into());
            current.push(indicator);
            assert_eq!(scroll_without_progress(&baseline, &current, 0.0, 100.0), None);
        }
    }

    #[test]
    fn scroll_progress_duplicates_participate() {
        let single = vec![row()];
        let duplicate = vec![row(), row()];
        assert_eq!(scroll_without_progress(&single, &duplicate, 0.0, 1.0), None);
        assert_eq!(scroll_without_progress(&duplicate, &single, 0.0, 1.0), None);
        assert_eq!(
            scroll_without_progress(&duplicate, &duplicate, 0.0, 1.0),
            Some(2)
        );
        let mut moved = duplicate.clone();
        moved[0].rect[1] -= 20.0;
        assert_eq!(scroll_without_progress(&duplicate, &moved, 0.0, 1.0), None);
    }

    #[test]
    fn scroll_progress_needs_a_labelled_row_that_stayed() {
        let mut next = row();
        next.label = "different".into();
        assert_eq!(scroll_without_progress(&[row()], &[next], 0.0, 1.0), None);
        for kind in ["Application", "Window"] {
            let mut frame = row();
            frame.kind = kind.into();
            assert_eq!(
                scroll_without_progress(&[frame.clone()], &[frame], 0.0, 1.0),
                None
            );
        }
        let mut empty = row();
        empty.label.clear();
        assert_eq!(
            scroll_without_progress(&[empty.clone()], &[empty], 0.0, 1.0),
            None
        );
        // Buttons are evidence like any other labelled row.
        let mut button = row();
        button.kind = "Button".into();
        assert_eq!(
            scroll_without_progress(&[button.clone()], &[button], 0.0, 1.0),
            Some(1)
        );
    }

    #[test]
    fn scroll_progress_invalid_direction() {
        for (dx, dy) in [(0.0, 0.0), (f64::NAN, 1.0), (0.0, f64::INFINITY)] {
            assert_eq!(scroll_without_progress(&[row()], &[row()], dx, dy), None);
        }
    }

    #[test]
    fn scroll_progress_digit_shape_collapses_runs() {
        assert_eq!(digit_shape("9:59"), digit_shape("10:00"));
        assert_eq!(digit_shape("Timestamp 0: 9:59"), "Timestamp #: #:#");
        assert_ne!(digit_shape("@alice"), digit_shape("@bob"));
    }
}
