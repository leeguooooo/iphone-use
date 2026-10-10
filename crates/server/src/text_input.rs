//! Long text input: typed in chunks so every runner request stays short.
//!
//! The device runner types at about 60 characters a second, so one request
//! for a long text outlives any fixed timeout and, when it fails, says nothing
//! about how much landed. Chunks keep each request a few seconds long and let
//! a failure report exactly how many characters were acknowledged.
//!
//! A chunk boundary never splits what a reader sees as one character (a
//! grapheme cluster): emoji ZWJ sequences, skin tones, flags, keycaps,
//! combining marks, CRLF, Hangul jamo, Indic conjuncts. Without a Unicode
//! segmentation table in the dependency tree, the rule is conservative: a cut
//! is made only before a character that always starts a new cluster (ASCII,
//! Latin, CJK, kana, Hangul syllables, emoji bases, …) and never after a
//! joiner or prepend mark. Where none is found nearby the chunk grows instead
//! — a longer chunk only takes longer; a split cluster would type garbage.

use std::time::Duration;

/// Text up to this many characters goes out in one request, exactly as it
/// always did; longer text is chunked to about this size.
pub const CHUNK_CHARS: usize = 200;

/// Longest `text` an action may carry, in Unicode scalar values.
pub const MAX_TEXT_CHARS: usize = 20_000;

/// The runner's default typing speed (`/wda/keys` `frequency`).
pub const TYPING_CHARS_PER_SEC: f64 = 60.0;

/// Number of characters as every limit and count here measures them:
/// Unicode scalar values (Rust `char`s), not UTF-16 units or bytes.
pub fn char_count(text: &str) -> usize {
    text.chars().count()
}

/// Whether `c` always begins a new grapheme cluster when it follows an
/// ordinary character. Deliberately incomplete: anything not listed is never
/// a cut point.
fn starts_cluster(c: char) -> bool {
    matches!(c as u32,
        0x0A | 0x0D | 0x09 | 0x20..=0x7E
        | 0x00A0..=0x024F          // Latin-1, Latin Extended A/B
        | 0x0370..=0x03FF          // Greek (combining marks are 0300–036F)
        | 0x0400..=0x0482 | 0x048A..=0x052F // Cyrillic letters
        | 0x1E00..=0x1EFF          // Latin Extended Additional
        | 0x2010..=0x2027 | 0x2030..=0x205E // punctuation (no ZW*/bidi controls)
        | 0x20A0..=0x20CF          // currency
        | 0x2100..=0x23FF | 0x2460..=0x27BF | 0x2900..=0x2BFF // symbols, dingbats
        | 0x2E80..=0x2FDF | 0x3000..=0x3029 | 0x3030..=0x303F // CJK radicals, punctuation
        | 0x3041..=0x3096 | 0x309B..=0x30FF // kana (3099/309A combine)
        | 0x3100..=0x312F | 0x3131..=0x318F | 0x31A0..=0x31FF | 0x3200..=0x33FF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF // CJK ideographs
        | 0xAC00..=0xD7A3          // Hangul syllables
        | 0xF900..=0xFAFF | 0xFE30..=0xFE4F
        | 0xFF01..=0xFF9D          // fullwidth / halfwidth (FF9E/FF9F combine)
        | 0x1F000..=0x1F1FF        // tiles, cards, enclosed, regional indicators
        | 0x1F200..=0x1F3FA | 0x1F400..=0x1FAFF // emoji (1F3FB–1F3FF are skin tones)
        | 0x20000..=0x3FFFF        // CJK extensions
    )
}

fn regional_indicator(c: char) -> bool {
    matches!(c as u32, 0x1F1E6..=0x1F1FF)
}

/// Characters that attach to what FOLLOWS them (grapheme "Prepend"), plus the
/// joiners: nothing may be cut right after one.
fn binds_forward(c: char) -> bool {
    matches!(c as u32,
        0x200C | 0x200D            // ZWNJ, ZWJ
        | 0x0600..=0x0605 | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2 | 0x0D4E
        | 0x110BD | 0x110CD | 0x111C2..=0x111C3
        | 0x1100..=0x115F | 0xA960..=0xA97F // Hangul leading jamo
    )
}

/// May `chars` be cut before `chars[index]`?
pub fn safe_boundary(chars: &[char], index: usize) -> bool {
    if index == 0 || index >= chars.len() {
        return true;
    }
    let (previous, next) = (chars[index - 1], chars[index]);
    if !starts_cluster(next) || binds_forward(previous) {
        return false;
    }
    if previous == '\r' && next == '\n' {
        return false;
    }
    if regional_indicator(next) && regional_indicator(previous) {
        // Flags are pairs: a cut is safe only after an even run.
        let run = chars[..index]
            .iter()
            .rev()
            .take_while(|c| regional_indicator(**c))
            .count();
        return run % 2 == 0;
    }
    true
}

/// Split `text` into consecutive pieces of about `size` characters that
/// rejoin to exactly `text`, cutting only at [`safe_boundary`]. A piece is
/// shortened down to half of `size` to find a boundary, then lengthened.
pub fn split(text: &str, size: usize) -> Vec<&str> {
    let size = size.max(1);
    let offsets: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let byte_at = |index: usize| offsets.get(index).copied().unwrap_or(text.len());
    let mut pieces = Vec::new();
    let mut start = 0;
    while n - start > size {
        let target = start + size;
        let floor = start + (size / 2).max(1);
        let cut = (floor..=target)
            .rev()
            .find(|&i| safe_boundary(&chars, i))
            .or_else(|| (target + 1..n).find(|&i| safe_boundary(&chars, i)))
            .unwrap_or(n);
        pieces.push(&text[byte_at(start)..byte_at(cut)]);
        start = cut;
    }
    if start < n {
        pieces.push(&text[byte_at(start)..]);
    }
    pieces
}

/// Expected typing time for `chars` characters at the runner's default speed.
pub fn typing_time(chars: usize) -> Duration {
    Duration::from_secs_f64(chars as f64 / TYPING_CHARS_PER_SEC)
}

/// The timeout of one chunk's request: twice its typing time plus overhead,
/// and never below the client's ordinary 20 s.
pub fn chunk_timeout(chars: usize) -> Duration {
    (typing_time(chars) * 2 + Duration::from_secs(10)).max(Duration::from_secs(20))
}

/// How much longer than a short action a request typing `chars` characters
/// may take, on top of the ordinary action deadline. Zero for text that goes
/// out in one request, so short text keeps its old deadline exactly.
pub fn typing_allowance(chars: usize) -> Duration {
    if chars <= CHUNK_CHARS {
        return Duration::ZERO;
    }
    let chunks = chars.div_ceil(CHUNK_CHARS) as u32;
    typing_time(chars).mul_f64(1.5) + Duration::from_secs(1) * chunks
}

/// The extra time a `text` action (or any JSON action) needs; zero otherwise.
pub fn action_allowance(action: &serde_json::Value) -> Duration {
    if action.get("type").and_then(serde_json::Value::as_str) != Some("text") {
        return Duration::ZERO;
    }
    action
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map_or(Duration::ZERO, |text| typing_allowance(char_count(text)))
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
    fn a_run_with_no_boundary_grows_instead_of_splitting() {
        // Thai has no cut point this table knows of until the space.
        let thai = "สวัสดีครับ".repeat(30);
        let text = format!("{thai} tail");
        let pieces = split(&text, 20);
        assert_eq!(rejoin(&pieces), text);
        assert_eq!(pieces[0], thai.as_str());
    }

    #[test]
    fn timeouts_scale_with_length_and_short_text_gets_no_allowance() {
        assert_eq!(chunk_timeout(1), Duration::from_secs(20));
        assert_eq!(chunk_timeout(200), Duration::from_secs(20));
        assert!(chunk_timeout(1200) > Duration::from_secs(45));
        assert_eq!(typing_allowance(0), Duration::ZERO);
        assert_eq!(typing_allowance(CHUNK_CHARS), Duration::ZERO);
        assert!(typing_allowance(CHUNK_CHARS + 1) > Duration::ZERO);
        // 20 000 characters: 333 s of typing, 500 s with margin, + 100 chunks.
        let most = typing_allowance(MAX_TEXT_CHARS).as_secs_f64();
        assert!((599.0..=601.0).contains(&most), "{most}");
        assert_eq!(
            action_allowance(&serde_json::json!({"type":"text","text":"x".repeat(400)})),
            typing_allowance(400)
        );
        assert_eq!(
            action_allowance(&serde_json::json!({"type":"tap","x":0.5})),
            Duration::ZERO
        );
    }
}
