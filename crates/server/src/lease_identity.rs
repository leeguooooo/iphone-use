//! Public process correlation ID for read-only control-lease checkpoints.
//! This ID is never accepted as an authentication credential.
pub fn daemon_instance() -> Option<&'static str> {
    static ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ID.get_or_init(|| core::runner_auth::new_token().ok())
        .as_deref()
}

#[cfg(test)]
mod tests {
    #[test]
    fn public_instance_id_has_stable_hex_encoding() {
        let first = super::daemon_instance().expect("system CSPRNG");
        assert_eq!(first.len(), 64);
        assert!(first
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(super::daemon_instance(), Some(first));
    }
}
