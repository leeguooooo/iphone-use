//! Wall-clock seconds, once for every crate.
//!
//! Kept out of a module named `time`: this crate is the dependency literally
//! named `core`, so `core::time::…` inside a dependent would shadow the
//! standard library's `core::time` (see the `extern crate std as core` note
//! in `lib.rs`).

/// Seconds since the Unix epoch; `0` if the clock is before it.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn unix_now_is_after_2025() {
        assert!(
            super::unix_now() > 1_735_689_600,
            "the clock is in the past"
        );
    }
}
