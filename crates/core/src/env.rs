//! Environment variable names.
//!
//! Every setting is `IPHONE_USE_*`. Installs made before the rename carry the
//! same settings as `PHONE_REMOTE_*` — in LaunchAgent plists, MCP client
//! registrations and shell profiles — so each entry point calls
//! [`adopt_legacy_names`] first and the rest of the code reads one name.

/// The prefix older installs used.
pub const LEGACY_PREFIX: &str = "PHONE_REMOTE_";
/// The prefix every setting uses now.
pub const PREFIX: &str = "IPHONE_USE_";

/// The `(new name, value)` pairs to set for `vars`: each `PHONE_REMOTE_X`
/// whose `IPHONE_USE_X` is not set. The new name wins when both are set.
pub fn legacy_renames<I>(vars: I) -> Vec<(String, std::ffi::OsString)>
where
    I: IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
{
    let vars: Vec<_> = vars.into_iter().collect();
    let present = |name: &str| vars.iter().any(|(key, _)| key.as_os_str() == name);
    vars.iter()
        .filter_map(|(key, value)| {
            let rest = key.to_str()?.strip_prefix(LEGACY_PREFIX)?;
            let name = format!("{PREFIX}{rest}");
            (!rest.is_empty() && !present(&name)).then(|| (name, value.clone()))
        })
        .collect()
}

/// The pre-rename spelling of a setting (`IPHONE_USE_X` → `PHONE_REMOTE_X`),
/// for reading an installed LaunchAgent plist that still uses it.
pub fn legacy_name(name: &str) -> Option<String> {
    name.strip_prefix(PREFIX)
        .filter(|rest| !rest.is_empty())
        .map(|rest| format!("{LEGACY_PREFIX}{rest}"))
}

/// Copy every `PHONE_REMOTE_X` into `IPHONE_USE_X` unless that is already
/// set. Call it first thing in `main`, before any thread starts: changing the
/// environment is only sound while the process is single-threaded. Child
/// processes inherit both names.
pub fn adopt_legacy_names() {
    for (name, value) in legacy_renames(std::env::vars_os()) {
        std::env::set_var(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        list.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
    }

    #[test]
    fn an_old_name_fills_the_new_one_unless_the_new_one_is_set() {
        let renamed = legacy_renames(pairs(&[
            ("PHONE_REMOTE_PORT", "45432"),
            ("PHONE_REMOTE_WDA_URL", "http://old"),
            ("IPHONE_USE_WDA_URL", "http://new"),
            ("PHONE_REMOTE_", "nothing after the prefix"),
            ("HOME", "/Users/x"),
        ]));
        assert_eq!(
            renamed,
            vec![("IPHONE_USE_PORT".to_string(), "45432".into())],
            "the new name wins; a bare prefix and other variables are left alone"
        );
        assert_eq!(legacy_name("IPHONE_USE_AGENT_TOKEN").as_deref(), Some("PHONE_REMOTE_AGENT_TOKEN"));
        assert_eq!(legacy_name("WDA_PORT"), None);
    }
}
