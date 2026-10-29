//! Long text input: typed in chunks so every runner request stays short.
//!
//! Long input is divided so each request can acknowledge a bounded prefix.
//! Timeout calculations use a nominal rate, not a measured device throughput.
//!
//! A chunk boundary preserves a full Unicode extended grapheme cluster.
//! Unicode UAX #29 segmentation keeps ordinary letters bounded without
//! splitting combining marks, Indic conjuncts, flags or emoji sequences.
//! Only a single cluster larger than the requested size may exceed it.

use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation;

/// Text up to this many characters goes out in one request, exactly as it
/// always did; longer text is chunked to about this size.
pub const CHUNK_CHARS: usize = 200;

/// Longest `text` an action may carry, in Unicode scalar values.
pub const MAX_TEXT_CHARS: usize = 20_000;

/// Nominal rate for timeout budgeting, not a device throughput measurement.
pub const TYPING_CHARS_PER_SEC: f64 = 60.0;

/// Number of characters as every limit and count here measures them:
/// Unicode scalar values (Rust `char`s), not UTF-16 units or bytes.
pub fn char_count(text: &str) -> usize {
    text.chars().count()
}

/// Split into consecutive slices bounded by `size` Unicode scalar values.
/// A single extended grapheme cluster is indivisible even when larger than
/// the limit. Slices preserve the original UTF-8 bytes without normalization.
pub fn split(text: &str, size: usize) -> Vec<&str> {
    let size = size.max(1);
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut scalars = 0;
    for (at, cluster) in text.grapheme_indices(true) {
        let count = cluster.chars().count();
        if scalars > 0 && scalars + count > size {
            pieces.push(&text[start..at]);
            start = at;
            scalars = 0;
        }
        scalars += count;
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

/// Nominal typing-time estimate used by the request budget.
pub fn typing_time(chars: usize) -> Duration {
    Duration::from_secs_f64(chars as f64 / TYPING_CHARS_PER_SEC)
}

/// The timeout of one chunk's request: twice its typing time plus overhead,
/// and never below the client's ordinary 20 s.
pub fn chunk_timeout(chars: usize) -> Duration {
    (typing_time(chars) * 2 + Duration::from_secs(10)).max(Duration::from_secs(20))
}

/// Extra typing time above a short action's ordinary deadline. Short text
/// keeps its existing deadline; long text includes the actual request count,
/// since whole clusters can leave a short chunk beside a full-size cluster.
pub fn typing_allowance(text: &str) -> Duration {
    let chars = char_count(text);
    if chars <= CHUNK_CHARS {
        return Duration::ZERO;
    }
    typing_time(chars).mul_f64(1.5)
        + Duration::from_secs(split(text, CHUNK_CHARS).len() as u64)
}

/// The extra time a `text` action (or any JSON action) needs; zero otherwise.
pub fn action_allowance(action: &serde_json::Value) -> Duration {
    if action.get("type").and_then(serde_json::Value::as_str) != Some("text") {
        return Duration::ZERO;
    }
    action
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map_or(Duration::ZERO, typing_allowance)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejoin(pieces: &[&str]) -> String {
        pieces.concat()
    }

    #[test]
    fn short_text_is_one_piece() {
        assert_eq!(split("hello", 200), vec!["hello"]);
        assert_eq!(split("", 200), Vec::<&str>::new());
        let exact: String = "a".repeat(200);
        assert_eq!(split(&exact, 200).len(), 1);
    }

    #[test]
    fn separate_devanagari_letters_keep_the_requested_chunk_bound() {
        let text = "क".repeat(500);
        let pieces = split(&text, 200);
        assert_eq!(rejoin(&pieces), text);
        assert_eq!(pieces.iter().map(|piece| char_count(piece)).collect::<Vec<_>>(),
            vec![200, 200, 100]);
    }

    #[test]
    fn long_ascii_splits_at_the_size() {
        let text = "a".repeat(450);
        let pieces = split(&text, 200);
        assert_eq!(
            pieces.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![200, 200, 50]
        );
        assert_eq!(rejoin(&pieces), text);
    }

    /// Every cut of every split, for every size, keeps clusters whole.
    fn assert_never_cuts_inside(text: &str, unit: &str) {
        for size in 1..=12 {
            let pieces = split(text, size);
            assert_eq!(rejoin(&pieces), text, "size {size}");
            for piece in &pieces {
                // Each piece is whole units.
                assert!(
                    piece.len() % unit.len() == 0
                        && piece.matches(unit).count() * unit.len() == piece.len(),
                    "size {size}: piece {piece:?} cuts {unit:?}"
                );
            }
        }
    }

    #[test]
    fn modern_prepend_signs_stay_with_the_following_letter_at_chunk_limit() {
        // Unicode 17 Prepend signs missing from the previous boundary table.
        for code in [0x113D1, 0x1193F, 0x11941, 0x11A84, 0x11A85, 0x11A86,
            0x11A87, 0x11A88, 0x11A89, 0x11D46, 0x11F02] {
            let sign = char::from_u32(code).unwrap();
            let text = format!("{}{sign}b", "a".repeat(199));
            let pieces = split(&text, 200);
            assert_eq!(rejoin(&pieces), text);
            assert!(pieces.iter().all(|piece| !piece.ends_with(sign)),
                "U+{code:04X} was separated from its following letter");
        }
    }

    #[test]
    fn grapheme_clusters_are_never_split() {
        for unit in [
            "👨\u{200D}👩\u{200D}👧\u{200D}👦", // family ZWJ sequence
            "👍🏽",                               // skin tone
            "🇨🇳",                               // flag
            "1\u{FE0F}\u{20E3}",                // keycap
            "❤\u{FE0F}",                        // variation selector
            "e\u{0301}",                        // combining acute
            "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}", // tag sequence
            "\r\n",
            "\u{1100}\u{1161}\u{11A8}", // conjoining jamo
            "क\u{094D}ष",               // Devanagari conjunct
            "ガ",                       // precomposed kana
            "か\u{3099}",               // kana + combining voiced mark
            "ﾊﾟ",                        // halfwidth + semi-voiced mark
        ] {
            let text = unit.repeat(40);
            assert_never_cuts_inside(&text, unit);
        }
    }

    #[test]
    fn flags_pair_up_even_in_a_long_run() {
        let text = "🇨🇳🇯🇵🇺🇸".repeat(30);
        for piece in split(&text, 7) {
            assert_eq!(piece.chars().count() % 2, 0, "{piece:?}");
        }
    }

    #[test]
    fn mixed_text_rejoins_exactly_and_chunks_stay_near_the_size() {
        let text = "你好，世界！Hello, world. 👋🏻 café naïve 🇯🇵 한국어 テスト\r\n".repeat(60);
        let pieces = split(&text, 200);
        assert_eq!(rejoin(&pieces), text);
        assert!(pieces.len() > 1);
        for piece in &pieces[..pieces.len() - 1] {
            let n = piece.chars().count();
            assert!((100..=200).contains(&n), "{n}");
        }
    }

    #[test]
    fn a_single_oversized_cluster_is_kept_whole() {
        let cluster = format!("e{}", "\u{0301}".repeat(300));
        let text = format!("{cluster} tail");
        let pieces = split(&text, 20);
        assert_eq!(rejoin(&pieces), text);
        assert_eq!(pieces, vec![cluster.as_str(), " tail"]);
    }

    #[test]
    fn allowance_accounts_for_whole_cluster_packing() {
        let text = format!("ae{}b", "\u{0301}".repeat(199));
        assert_eq!(split(&text, 200).iter().map(|p| char_count(p)).collect::<Vec<_>>(), vec![1, 200, 1]);
        assert_eq!(action_allowance(&serde_json::json!({"type":"text","text":text})),
            typing_time(202).mul_f64(1.5) + Duration::from_secs(3));
    }

    #[test]
    fn timeouts_scale_with_length_and_short_text_gets_no_allowance() {
        assert_eq!(chunk_timeout(1), Duration::from_secs(20));
        assert_eq!(chunk_timeout(200), Duration::from_secs(20));
        assert!(chunk_timeout(1200) > Duration::from_secs(45));
        assert_eq!(typing_allowance(""), Duration::ZERO);
        assert_eq!(typing_allowance(&"x".repeat(CHUNK_CHARS)), Duration::ZERO);
        assert!(typing_allowance(&"x".repeat(CHUNK_CHARS + 1)) > Duration::ZERO);
        // 20 000 characters: 333 s of typing, 500 s with margin, + 100 chunks.
        let most = typing_allowance(&"x".repeat(MAX_TEXT_CHARS)).as_secs_f64();
        assert!((599.0..=601.0).contains(&most), "{most}");
        assert_eq!(
            action_allowance(&serde_json::json!({"type":"text","text":"x".repeat(400)})),
            typing_allowance(&"x".repeat(400))
        );
        assert_eq!(
            action_allowance(&serde_json::json!({"type":"tap","x":0.5})),
            Duration::ZERO
        );
    }
}
