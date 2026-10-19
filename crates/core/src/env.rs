//! Environment variable names.
//!
//! Every setting is `IPHONE_USE_*`, and the device runner's own settings are
//! `IPU_RUNNER_*`. Installs made before two renames still carry the old
//! spellings — in LaunchAgent plists, MCP client registrations and shell
//! profiles:
//!
//! * `PHONE_REMOTE_X` for `IPHONE_USE_X`;
//! * `IPHONE_USE_WDA_X` for `IPHONE_USE_RUNNER_X`, and `WDA_X` for
//!   `IPU_RUNNER_X`, from when the runner was WebDriverAgent.
//!
//! So each entry point calls [`adopt_legacy_names`] first and the rest of the
//! code reads one name.

/// The prefix older installs used.
pub const LEGACY_PREFIX: &str = "PHONE_REMOTE_";
/// The prefix every setting uses now.
pub const PREFIX: &str = "IPHONE_USE_";
/// The runner's settings: `IPHONE_USE_RUNNER_X` and `IPU_RUNNER_X`.
const RUNNER_PREFIX: &str = "IPHONE_USE_RUNNER_";
const RUNNER_OWN_PREFIX: &str = "IPU_RUNNER_";
/// Their spellings from before the WebDriverAgent rename.
const LEGACY_RUNNER_PREFIX: &str = "IPHONE_USE_WDA_";
const LEGACY_RUNNER_OWN_PREFIX: &str = "WDA_";

/// The current name of a setting spelled `name`, or `None` when `name` is
/// already current (or not one of ours).
fn current_name(name: &str) -> Option<String> {
    let name = match name.strip_prefix(LEGACY_PREFIX) {
        Some(rest) if !rest.is_empty() => format!("{PREFIX}{rest}"),
        Some(_) => return None,
        None => name.to_string(),
    };
    let renamed = if let Some(rest) = name.strip_prefix(LEGACY_RUNNER_PREFIX) {
        format!("{RUNNER_PREFIX}{rest}")
    } else if let Some(rest) = name.strip_prefix(LEGACY_RUNNER_OWN_PREFIX) {
        format!("{RUNNER_OWN_PREFIX}{rest}")
    } else {
        name
    };
    (!renamed.ends_with('_')).then_some(renamed)
}

/// The `(new name, value)` pairs to set for `vars`: each variable spelled the
/// old way whose current name is not set. The current name wins when both
/// are set.
pub fn legacy_renames<I>(vars: I) -> Vec<(String, std::ffi::OsString)>
where
    I: IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
{
    let vars: Vec<_> = vars.into_iter().collect();
    let present = |name: &str| vars.iter().any(|(key, _)| key.as_os_str() == name);
    let mut renamed: Vec<(String, std::ffi::OsString)> = Vec::new();
    for (key, value) in &vars {
        let Some(old) = key.to_str() else { continue };
        let Some(name) = current_name(old) else { continue };
        if name != old && !present(&name) && !renamed.iter().any(|(n, _)| *n == name) {
            renamed.push((name, value.clone()));
        }
    }
    renamed
}

/// The older spellings of a setting, most recent first, for reading an
/// installed LaunchAgent plist that still uses one
/// (`IPHONE_USE_RUNNER_URL` → `IPHONE_USE_WDA_URL`, `PHONE_REMOTE_WDA_URL`).
pub fn legacy_names(name: &str) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(rest) = name.strip_prefix(RUNNER_OWN_PREFIX).filter(|r| !r.is_empty()) {
        names.push(format!("{LEGACY_RUNNER_OWN_PREFIX}{rest}"));
    }
    if let Some(rest) = name.strip_prefix(PREFIX).filter(|r| !r.is_empty()) {
        if let Some(runner) = name.strip_prefix(RUNNER_PREFIX).filter(|r| !r.is_empty()) {
            names.push(format!("{LEGACY_RUNNER_PREFIX}{runner}"));
        }
        names.push(format!("{LEGACY_PREFIX}{rest}"));
        if let Some(runner) = name.strip_prefix(RUNNER_PREFIX).filter(|r| !r.is_empty()) {
            names.push(format!("{LEGACY_PREFIX}{}", &LEGACY_RUNNER_PREFIX[PREFIX.len()..]) + runner);
        }
    }
    names
}

/// Copy every old spelling into its current name unless that is already
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
            ("PHONE_REMOTE_RUNNER_URL", "http://old"),
            ("IPHONE_USE_RUNNER_URL", "http://new"),
            ("PHONE_REMOTE_", "nothing after the prefix"),
            ("HOME", "/Users/x"),
        ]));
        assert_eq!(
            renamed,
            vec![("IPHONE_USE_PORT".to_string(), "45432".into())],
            "the new name wins; a bare prefix and other variables are left alone"
        );
    }

    #[test]
    fn webdriveragent_era_names_fill_the_runner_names() {
        let renamed = legacy_renames(pairs(&[
            ("PHONE_REMOTE_WDA_URL", "http://127.0.0.1:8100"),
            ("IPHONE_USE_WDA_MJPEG_URL", "http://127.0.0.1:9100"),
            ("WDA_TEAM_ID", "6ZPXG4KVVS"),
            ("WDA_PORT", "8100"),
            ("IPU_RUNNER_PORT", "8261"),
            ("WDA_", "nothing after the prefix"),
        ]));
        assert_eq!(
            renamed,
            vec![
                ("IPHONE_USE_RUNNER_URL".to_string(), "http://127.0.0.1:8100".into()),
                ("IPHONE_USE_RUNNER_MJPEG_URL".to_string(), "http://127.0.0.1:9100".into()),
                ("IPU_RUNNER_TEAM_ID".to_string(), "6ZPXG4KVVS".into()),
            ]
        );
    }

    #[test]
    fn every_older_spelling_is_listed_most_recent_first() {
        assert_eq!(legacy_names("IPHONE_USE_AGENT_TOKEN"), vec!["PHONE_REMOTE_AGENT_TOKEN"]);
        assert_eq!(
            legacy_names("IPHONE_USE_RUNNER_URL"),
            vec!["IPHONE_USE_WDA_URL", "PHONE_REMOTE_RUNNER_URL", "PHONE_REMOTE_WDA_URL"]
        );
        assert_eq!(legacy_names("IPU_RUNNER_TEAM_ID"), vec!["WDA_TEAM_ID"]);
        assert!(legacy_names("HOME").is_empty());
    }
}
