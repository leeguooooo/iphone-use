//! Release checks and `iphone-use upgrade`, following the `*-use` family's
//! upgrade convention (leeguooooo/plugins `docs/upgrade.md`).
//!
//! One release lookup serves three callers:
//!
//! * the daemon's daily background check (`main::spawn_update_check`), which
//!   feeds `/agent/status {version, latest, update_available}`;
//! * the once-a-day stderr notice printed by one-shot CLI commands;
//! * `iphone-use upgrade [--check] [--json]`.
//!
//! Everything that touches the environment, the clock, the network or a child
//! process is passed in, so the decisions are unit-testable without any of them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const NAME: &str = "iphone-use";
/// The newest non-prerelease release, as JSON (`tag_name`).
pub const API_LATEST_URL: &str =
    "https://api.github.com/repos/leeguooooo/iphone-use/releases/latest";
/// The same release through the web tier: a redirect whose `Location` ends in
/// `/tag/vX.Y.Z`. No anonymous rate limit (the API allows 60 requests an hour
/// per IP), so it is the fallback when the API refuses.
pub const WEB_LATEST_URL: &str = "https://github.com/leeguooooo/iphone-use/releases/latest";
/// The installer `upgrade` downloads and runs (the same script as
/// [`INSTALL_COMMAND`], fetched first so a failed download is an error).
pub const INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh";
/// The documented install/upgrade route. It installs the daemon app and the
/// release-matched skill together.
pub const INSTALL_COMMAND: &str =
    "curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh";
pub const PLUGIN_ID: &str = "iphone-use@leeguooooo-plugins";
pub const SKILLS_UPDATE_COMMAND: &str = "npx skills update iphone-use";
/// Marker the installer writes into the skill folder it owns.
pub const INSTALLER_MARKER: &str = ".iphone-use-release";
/// How long a cached answer suppresses another network check.
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// Network budget for the notice: it must never slow a command down.
pub const NOTICE_TIMEOUT: Duration = Duration::from_secs(2);
/// Network budget for an explicit `upgrade` / `upgrade --check`.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Any of these set (non-empty, not `0`/`false`) disables both the daemon's
/// check and the CLI notice. `PHONE_REMOTE_NO_UPDATE_CHECK` predates the
/// family convention and keeps working.
pub const OPT_OUT_VARS: [&str; 4] = [
    "CI",
    "IPHONE_USE_NO_UPDATE_CHECK",
    "USE_NO_UPDATE_CHECK",
    "PHONE_REMOTE_NO_UPDATE_CHECK",
];

// ---------------------------------------------------------------------------
// versions
// ---------------------------------------------------------------------------

/// `v1.2.3` / `1.2.3` / `1.2.3-rc.1` → `(1, 2, 3)`. Missing minor/patch count
/// as zero; anything else non-numeric is not a version.
pub fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.trim();
    let value = value.strip_prefix('v').unwrap_or(value);
    let core = value.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = match parts.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };
    let patch = match parts.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Strip the leading `v` a release tag carries.
pub fn display_version(tag: &str) -> &str {
    let tag = tag.trim();
    tag.strip_prefix('v').unwrap_or(tag)
}

/// Is `latest` a newer release than `current`?
///
/// Numeric comparison when both parse, so a build ahead of the last release
/// is not told to "upgrade" backwards. When either side is not a version, any
/// difference counts as an update — the daemon's behaviour before this module.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        // Same core: a release outranks a prerelease of it (semver), so a
        // `0.7.0-rc.1` build is offered `0.7.0`.
        (Some(latest_core), Some(current_core)) => {
            latest_core > current_core
                || (latest_core == current_core && !is_prerelease(latest) && is_prerelease(current))
        }
        _ => display_version(latest) != display_version(current),
    }
}

/// Does the version carry a `-pre` part (`1.2.3-rc.1`)? Build metadata
/// (`+sha`) alone does not make a prerelease.
fn is_prerelease(value: &str) -> bool {
    display_version(value)
        .split('+')
        .next()
        .is_some_and(|core| core.contains('-'))
}

// ---------------------------------------------------------------------------
// opt-out
// ---------------------------------------------------------------------------

fn env_flag_set(value: Option<String>) -> bool {
    value.is_some_and(|value| {
        let value = value.trim();
        !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
    })
}

/// Should update checks be skipped entirely? `env` is `std::env::var` in
/// production and a map in tests.
pub fn update_check_disabled(env: &dyn Fn(&str) -> Option<String>) -> bool {
    OPT_OUT_VARS.iter().any(|name| env_flag_set(env(name)))
}

pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

// ---------------------------------------------------------------------------
// fetching the latest release
// ---------------------------------------------------------------------------

/// Where to ask. Production uses [`Endpoints::github`]; tests point both at a
/// local mock server.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub api: String,
    pub web: String,
}

impl Endpoints {
    pub fn github() -> Self {
        Self {
            api: API_LATEST_URL.to_string(),
            web: WEB_LATEST_URL.to_string(),
        }
    }
}

/// A release tag is `v` + a version; anything else is a malformed answer.
fn valid_tag(tag: &str) -> Option<String> {
    let tag = tag.trim();
    (tag.starts_with('v') && parse_version(tag).is_some()).then(|| tag.to_string())
}

/// `tag_name` of a `releases/latest` API body, unless it is a draft or a
/// prerelease (the endpoint already excludes both; this is belt and braces).
pub fn tag_from_api_body(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    if value["draft"].as_bool() == Some(true) || value["prerelease"].as_bool() == Some(true) {
        return None;
    }
    valid_tag(value["tag_name"].as_str()?)
}

/// The tag from a `releases/latest` redirect target
/// (`https://github.com/<repo>/releases/tag/v0.2.0`).
pub fn tag_from_location(location: &str) -> Option<String> {
    let (_, tag) = location.rsplit_once("/tag/")?;
    valid_tag(tag)
}

/// Resolve the latest release tag (`vX.Y.Z`): the API first (with
/// `GITHUB_TOKEN` when given), then the redirect when the API refuses.
pub async fn fetch_latest_tag(
    endpoints: &Endpoints,
    timeout: Duration,
    github_token: Option<&str>,
) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .user_agent(concat!("iphone-use/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let mut request = client
        .get(&endpoints.api)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json");
    if let Some(token) = github_token.map(str::trim).filter(|t| !t.is_empty()) {
        request = request.bearer_auth(token);
    }
    let api_error = match request.send().await {
        Ok(response) if response.status().is_success() => match response.bytes().await {
            Ok(body) => match tag_from_api_body(&body) {
                Some(tag) => return Ok(tag),
                None => "GitHub API answered without a release tag".to_string(),
            },
            Err(error) => format!("GitHub API read failed: {error}"),
        },
        Ok(response) => format!("GitHub API answered {}", response.status()),
        Err(error) => format!("GitHub API request failed: {error}"),
    };

    let web_error = match client.get(&endpoints.web).send().await {
        Ok(response) => match response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .and_then(tag_from_location)
        {
            Some(tag) => return Ok(tag),
            None => format!(
                "release redirect answered {} without a tag",
                response.status()
            ),
        },
        Err(error) => format!("release redirect failed: {error}"),
    };
    anyhow::bail!("{api_error}; {web_error}")
}

/// Blocking wrapper for the CLI: a private current-thread runtime and a hard
/// ceiling on the whole lookup (both requests together).
pub fn fetch_latest_tag_blocking(
    endpoints: &Endpoints,
    timeout: Duration,
    github_token: Option<&str>,
) -> anyhow::Result<String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        match tokio::time::timeout(timeout, fetch_latest_tag(endpoints, timeout, github_token))
            .await
        {
            Ok(result) => result,
            Err(_) => anyhow::bail!("release check timed out after {}s", timeout.as_secs_f32()),
        }
    });
    // Dropping a runtime waits for its blocking tasks, and reqwest resolves
    // DNS on one: on a broken network `getaddrinfo` can hang for tens of
    // seconds after the timeout fired. Abandon it instead of waiting.
    runtime.shutdown_background();
    result
}

// ---------------------------------------------------------------------------
// the daily cache and the stderr notice
// ---------------------------------------------------------------------------

/// `${XDG_CACHE_HOME:-~/.cache}/iphone-use/update-check.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckCache {
    pub checked_at: u64,
    /// `X.Y.Z` without the `v`; `null` until a check has succeeded.
    pub latest: Option<String>,
}

pub fn cache_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let base = match env("XDG_CACHE_HOME").filter(|value| Path::new(value).is_absolute()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env("HOME").filter(|home| !home.is_empty())?).join(".cache"),
    };
    Some(base.join(NAME).join("update-check.json"))
}

pub fn read_cache(path: &Path) -> Option<CheckCache> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Write via a sibling temp file + rename so a concurrent reader never sees a
/// half-written file.
pub fn write_cache(path: &Path, cache: &CheckCache) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("cache path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".update-check.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(cache)?)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Record a check. A failed check (`latest == None`) still moves
/// `checked_at` so an offline machine is not retried on every call, but keeps
/// the last known release.
pub fn record_check(path: &Path, now: u64, latest_tag: Option<&str>) {
    let previous = read_cache(path).and_then(|cache| cache.latest);
    let cache = CheckCache {
        checked_at: now,
        latest: latest_tag
            .map(|tag| display_version(tag).to_string())
            .or(previous),
    };
    let _ = write_cache(path, &cache);
}

pub fn notice_line(latest: &str, current: &str) -> String {
    format!(
        "{NAME} {} is available (you have {}). Upgrade: {NAME} upgrade",
        display_version(latest),
        display_version(current)
    )
}

/// The once-a-day notice. Uses the cache when it is fresh; otherwise calls
/// `fetch` (which must honour the 2 s budget) and records the result. Writes
/// at most one line, and only to `stderr`. Returns whether it wrote.
pub fn maybe_notice(
    env: &dyn Fn(&str) -> Option<String>,
    now: u64,
    current: &str,
    fetch: &mut dyn FnMut() -> Option<String>,
    stderr: &mut dyn Write,
) -> bool {
    if update_check_disabled(env) {
        return false;
    }
    let Some(path) = cache_path(env) else {
        return false;
    };
    let cached = read_cache(&path);
    let fresh = cached.as_ref().is_some_and(|cache| {
        cache.checked_at <= now && now - cache.checked_at < CHECK_INTERVAL_SECS
    });
    let latest = if fresh {
        cached.and_then(|cache| cache.latest)
    } else {
        let fetched = fetch();
        record_check(&path, now, fetched.as_deref());
        read_cache(&path).and_then(|cache| cache.latest)
    };
    match latest {
        Some(latest) if is_newer(&latest, current) => {
            writeln!(stderr, "{}", notice_line(&latest, current)).is_ok()
        }
        _ => false,
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// where the skill lives
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillChannel {
    /// Claude Code plugin from the leeguooooo-plugins marketplace.
    ClaudePlugin,
    /// A git work tree (someone cloned the repo and linked the skill).
    Git,
    /// A plain copy, e.g. from `npx skills add`.
    Copied,
    /// The folder `install.sh` owns (carries `.iphone-use-release`); the
    /// installer refreshes it together with the daemon.
    Installer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SkillInstall {
    pub channel: SkillChannel,
    pub path: String,
    /// The command that refreshes this copy.
    pub update: String,
}

/// Skill folders a user or tool may have linked, relative to `$HOME`.
pub const SKILL_DIRS: [&str; 3] = [
    ".agents/skills/iphone-use",
    ".claude/skills/iphone-use",
    ".codex/skills/iphone-use",
];

fn plugin_install(home: &Path) -> Option<SkillInstall> {
    let file = home.join(".claude/plugins/installed_plugins.json");
    let text = std::fs::read_to_string(&file).ok()?;
    let prefix = format!("{NAME}@");
    if !text.contains(&format!("\"{prefix}")) {
        return None;
    }
    // Point at the installed copy when the file has the usual shape
    // (`{"plugins": {"name@market": [{"installPath": …}]}}`); the plugin
    // registry itself otherwise.
    let install_path = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| {
            let plugins = value.get("plugins").unwrap_or(&value).as_object()?.clone();
            plugins.iter().find_map(|(key, entry)| {
                if !key.starts_with(&prefix) {
                    return None;
                }
                let entry = entry
                    .as_array()
                    .and_then(|list| list.first())
                    .unwrap_or(entry);
                entry["installPath"].as_str().map(str::to_string)
            })
        });
    Some(SkillInstall {
        channel: SkillChannel::ClaudePlugin,
        path: install_path.unwrap_or_else(|| file.display().to_string()),
        update: format!("claude plugin update {PLUGIN_ID}"),
    })
}

/// The top of the git work tree containing `dir`, if any.
pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let top = String::from_utf8(output.stdout).ok()?;
    let top = top.trim();
    (!top.is_empty()).then(|| PathBuf::from(top))
}

/// Does `remote` (an `origin` URL) name leeguooooo/iphone-use? Accepts the
/// HTTPS, `git@host:` and `ssh://` forms, with or without `.git`.
pub fn remote_is_iphone_use(remote: &str) -> bool {
    let remote = remote.trim().trim_end_matches('/');
    let remote = remote
        .strip_suffix(".git")
        .unwrap_or(remote)
        .to_ascii_lowercase();
    [
        "github.com/leeguooooo/iphone-use",
        "github.com:leeguooooo/iphone-use",
    ]
    .iter()
    .any(|suffix| {
        remote
            .strip_suffix(suffix)
            .is_some_and(|head| head.is_empty() || head.ends_with('/') || head.ends_with('@'))
    })
}

/// Only a checkout of this repository is pulled; a work tree that merely
/// vendors a copy of the skill is treated as a copied folder.
fn is_iphone_use_checkout(root: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|remote| remote_is_iphone_use(&remote))
}

/// Every copy of the skill under `home`, one entry per real folder (the
/// installer's `~/.claude/skills/iphone-use` is a link to the `~/.agents`
/// copy, so it is reported once).
pub fn find_skills(home: &Path) -> Vec<SkillInstall> {
    let mut found = Vec::new();
    if let Some(plugin) = plugin_install(home) {
        found.push(plugin);
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    for relative in SKILL_DIRS {
        let Ok(real) = std::fs::canonicalize(home.join(relative)) else {
            continue;
        };
        if !real.is_dir() || seen.contains(&real) {
            continue;
        }
        seen.push(real.clone());
        let path = real.display().to_string();
        if real.join(INSTALLER_MARKER).is_file() {
            found.push(SkillInstall {
                channel: SkillChannel::Installer,
                path,
                update: format!("{NAME} upgrade"),
            });
        } else if let Some(root) = git_toplevel(&real)
            .filter(|root| real == root.join("skills").join(NAME) && is_iphone_use_checkout(root))
        {
            found.push(SkillInstall {
                channel: SkillChannel::Git,
                path,
                update: format!("git -C {} pull --ff-only", root.display()),
            });
        } else if real.join("SKILL.md").is_file() {
            found.push(SkillInstall {
                channel: SkillChannel::Copied,
                path,
                update: SKILLS_UPDATE_COMMAND.to_string(),
            });
        }
    }
    found
}

/// Runs a refresh command; returns `Ok(true)` on success, `Ok(false)` on a
/// non-zero exit. Production spawns the process; tests record the call.
pub type Runner<'a> = dyn FnMut(&str, &[String]) -> std::io::Result<bool> + 'a;

/// Refresh each skill copy the way its channel allows and say what happened.
/// `installer_ran` is whether step 1 ran the installer (which refreshed the
/// installer-owned folder); `claude_on_path` gates `claude plugin update`.
pub fn refresh_skills(
    skills: &[SkillInstall],
    installer_ran: bool,
    claude_on_path: bool,
    run: &mut Runner<'_>,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    if skills.is_empty() {
        writeln!(out, "skill: no installed copy found")?;
    }
    for skill in skills {
        match skill.channel {
            SkillChannel::Installer => {
                if installer_ran {
                    writeln!(
                        out,
                        "skill (installer): {} — refreshed by install.sh",
                        skill.path
                    )?;
                } else {
                    writeln!(
                        out,
                        "skill (installer): {} — matches the installed release",
                        skill.path
                    )?;
                }
            }
            SkillChannel::ClaudePlugin => {
                if claude_on_path {
                    let args = vec!["plugin".into(), "update".into(), PLUGIN_ID.into()];
                    match run("claude", &args) {
                        Ok(true) => writeln!(
                            out,
                            "skill (claude-plugin): updated with `{}`",
                            skill.update
                        )?,
                        Ok(false) => writeln!(
                            out,
                            "skill (claude-plugin): `{}` failed; run it yourself",
                            skill.update
                        )?,
                        Err(error) => writeln!(
                            out,
                            "skill (claude-plugin): could not run `{}`: {error}",
                            skill.update
                        )?,
                    }
                } else {
                    writeln!(out, "skill (claude-plugin): run `{}`", skill.update)?;
                }
            }
            SkillChannel::Git => {
                let root = skill
                    .update
                    .strip_prefix("git -C ")
                    .and_then(|rest| rest.strip_suffix(" pull --ff-only"))
                    .unwrap_or(&skill.path)
                    .to_string();
                let args = vec!["-C".into(), root, "pull".into(), "--ff-only".into()];
                match run("git", &args) {
                    Ok(true) => writeln!(out, "skill (git): {} — pulled", skill.path)?,
                    Ok(false) => writeln!(
                        out,
                        "skill (git): {} — `{}` failed (local changes or diverged history?); left as is",
                        skill.path, skill.update
                    )?,
                    Err(error) => writeln!(out, "skill (git): could not run git: {error}")?,
                }
            }
            SkillChannel::Copied => {
                writeln!(
                    out,
                    "skill (copied): {} — run `{}`",
                    skill.path, skill.update
                )?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `iphone-use upgrade --check / --json`
// ---------------------------------------------------------------------------

/// The `--json` document. `error` appears only when the check failed.
#[derive(Clone, Debug, Serialize)]
pub struct UpgradeReport {
    pub name: &'static str,
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    pub skills: Vec<SkillInstall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UpgradeReport {
    pub fn new(current: &str, fetched: &anyhow::Result<String>, skills: Vec<SkillInstall>) -> Self {
        let current = display_version(current).to_string();
        match fetched {
            Ok(tag) => Self {
                name: NAME,
                update_available: is_newer(tag, &current),
                latest: Some(display_version(tag).to_string()),
                current,
                skills,
                error: None,
            },
            Err(error) => Self {
                name: NAME,
                current,
                latest: None,
                update_available: false,
                skills,
                error: Some(format!("{error:#}")),
            },
        }
    }

    /// `0`, or `2` when the check itself failed.
    pub fn exit_code(&self) -> i32 {
        if self.error.is_some() {
            2
        } else {
            0
        }
    }

    /// The one-line `--check` verdict.
    pub fn summary(&self) -> String {
        match (&self.latest, self.update_available) {
            (Some(latest), true) => format!("{NAME} {} -> {latest}", self.current),
            _ => format!("{NAME} {} is up to date", self.current),
        }
    }

    /// Human output for `--check`: the verdict, then one line per skill copy.
    pub fn write_text(&self, out: &mut dyn Write) -> std::io::Result<()> {
        writeln!(out, "{}", self.summary())?;
        for skill in &self.skills {
            let channel = serde_json::to_value(skill.channel)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            writeln!(
                out,
                "  skill ({channel}): {} — update: {}",
                skill.path, skill.update
            )?;
        }
        Ok(())
    }
}

/// Was this binary installed by `install.sh`? The installer puts it inside
/// `~/Applications/iPhoneUse.app`; a `cargo build` or other copy is upgraded
/// the way it was built, never by overwriting the app behind its back.
pub fn installed_by_installer(exe: &Path) -> bool {
    let text = exe.to_string_lossy();
    text.contains("/iPhoneUse.app/Contents/MacOS/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    // --- version comparison ------------------------------------------------

    #[test]
    fn parses_tags_and_plain_versions() {
        assert_eq!(parse_version("v0.6.6"), Some((0, 6, 6)));
        assert_eq!(parse_version("0.6.6"), Some((0, 6, 6)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("0.7.0-rc.1"), Some((0, 7, 0)));
        assert_eq!(parse_version("v1\"</script>"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn newer_is_numeric_not_lexical() {
        assert!(is_newer("v0.6.10", "0.6.9"));
        assert!(is_newer("v0.7.0", "0.6.6"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("v0.6.6", "0.6.6"));
        assert!(is_newer("v0.10.0", "0.9.0"));
        assert!(is_newer("v0.7.0", "0.7.0-rc.1"));
        assert!(!is_newer("v0.7.0-rc.1", "0.7.0"));
        assert!(!is_newer("v0.7.0+build", "0.7.0"));
        assert!(!is_newer("0.6.6", "v0.6.6"));
        // A build ahead of the last release is not told to go back.
        assert!(!is_newer("v0.6.5", "0.6.6"));
    }

    #[test]
    fn unparsable_tags_fall_back_to_any_difference() {
        assert!(is_newer("v1\"</script>", "0.6.6"));
        assert!(!is_newer("nightly", "nightly"));
    }

    // --- opt-out ------------------------------------------------------------

    #[test]
    fn each_opt_out_variable_disables_the_check() {
        assert!(!update_check_disabled(&env_of(&[])));
        for name in OPT_OUT_VARS {
            assert!(update_check_disabled(&env_of(&[(name, "1")])), "{name}");
            assert!(update_check_disabled(&env_of(&[(name, "true")])), "{name}");
            assert!(!update_check_disabled(&env_of(&[(name, "")])), "{name}");
            assert!(!update_check_disabled(&env_of(&[(name, "0")])), "{name}");
            assert!(
                !update_check_disabled(&env_of(&[(name, "false")])),
                "{name}"
            );
        }
    }

    // --- cache + notice -------------------------------------------------------

    struct Home {
        dir: tempfile::TempDir,
    }

    impl Home {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }
        fn home(&self) -> String {
            self.dir.path().display().to_string()
        }
        fn cache(&self) -> PathBuf {
            self.dir.path().join(".cache/iphone-use/update-check.json")
        }
    }

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn cache_path_prefers_absolute_xdg_cache_home() {
        let path = cache_path(&env_of(&[("HOME", "/h"), ("XDG_CACHE_HOME", "/x")])).unwrap();
        assert_eq!(path, PathBuf::from("/x/iphone-use/update-check.json"));
        let path = cache_path(&env_of(&[("HOME", "/h"), ("XDG_CACHE_HOME", "rel")])).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/h/.cache/iphone-use/update-check.json")
        );
        assert_eq!(cache_path(&env_of(&[])), None);
    }

    #[test]
    fn stale_cache_fetches_once_then_notice_goes_to_stderr() {
        let home = Home::new();
        let env = env_of(&[("HOME", &home.home())]);
        let mut calls = 0;
        let mut fetch = || {
            calls += 1;
            Some("v0.7.0".to_string())
        };
        let mut stderr = Vec::new();
        assert!(maybe_notice(&env, NOW, "0.6.6", &mut fetch, &mut stderr));
        assert_eq!(calls, 1);
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "iphone-use 0.7.0 is available (you have 0.6.6). Upgrade: iphone-use upgrade\n"
        );
        let cache = read_cache(&home.cache()).unwrap();
        assert_eq!(
            cache,
            CheckCache {
                checked_at: NOW,
                latest: Some("0.7.0".into())
            }
        );
        // On disk the file has exactly the documented shape.
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.cache()).unwrap()).unwrap();
        assert_eq!(
            raw,
            serde_json::json!({"checked_at": NOW, "latest": "0.7.0"})
        );
    }

    #[test]
    fn fresh_cache_is_used_without_network() {
        let home = Home::new();
        write_cache(
            &home.cache(),
            &CheckCache {
                checked_at: NOW - 60,
                latest: Some("0.7.0".into()),
            },
        )
        .unwrap();
        let env = env_of(&[("HOME", &home.home())]);
        let mut fetch = || -> Option<String> { panic!("must not hit the network") };
        let mut stderr = Vec::new();
        assert!(maybe_notice(&env, NOW, "0.6.6", &mut fetch, &mut stderr));
        assert!(!stderr.is_empty());
    }

    #[test]
    fn a_day_old_cache_is_rechecked() {
        let home = Home::new();
        write_cache(
            &home.cache(),
            &CheckCache {
                checked_at: NOW - CHECK_INTERVAL_SECS,
                latest: Some("0.6.6".into()),
            },
        )
        .unwrap();
        let env = env_of(&[("HOME", &home.home())]);
        let mut calls = 0;
        let mut fetch = || {
            calls += 1;
            Some("v0.6.7".to_string())
        };
        let mut stderr = Vec::new();
        assert!(maybe_notice(&env, NOW, "0.6.6", &mut fetch, &mut stderr));
        assert_eq!(calls, 1);
    }

    #[test]
    fn failed_check_is_silent_and_still_throttles() {
        let home = Home::new();
        write_cache(
            &home.cache(),
            &CheckCache {
                checked_at: NOW - 2 * CHECK_INTERVAL_SECS,
                latest: Some("0.6.6".into()),
            },
        )
        .unwrap();
        let env = env_of(&[("HOME", &home.home())]);
        let mut calls = 0;
        let mut fetch = || {
            calls += 1;
            None
        };
        let mut stderr = Vec::new();
        assert!(!maybe_notice(&env, NOW, "0.6.6", &mut fetch, &mut stderr));
        assert!(stderr.is_empty());
        let cache = read_cache(&home.cache()).unwrap();
        assert_eq!(cache.checked_at, NOW, "offline must not retry every call");
        assert_eq!(
            cache.latest.as_deref(),
            Some("0.6.6"),
            "keeps the last known release"
        );
        // Next call within the day: no second network attempt.
        assert!(!maybe_notice(
            &env,
            NOW + 5,
            "0.6.6",
            &mut fetch,
            &mut stderr
        ));
        assert_eq!(calls, 1);
    }

    #[test]
    fn up_to_date_prints_nothing() {
        let home = Home::new();
        let env = env_of(&[("HOME", &home.home())]);
        let mut fetch = || Some("v0.6.6".to_string());
        let mut stderr = Vec::new();
        assert!(!maybe_notice(&env, NOW, "0.6.6", &mut fetch, &mut stderr));
        assert!(stderr.is_empty());
    }

    #[test]
    fn opt_out_skips_check_and_cache() {
        for name in ["CI", "IPHONE_USE_NO_UPDATE_CHECK", "USE_NO_UPDATE_CHECK"] {
            let home = Home::new();
            let env = env_of(&[("HOME", &home.home()), (name, "1")]);
            let mut fetch = || -> Option<String> { panic!("{name} must skip the check") };
            let mut stderr = Vec::new();
            assert!(!maybe_notice(&env, NOW, "0.6.6", &mut fetch, &mut stderr));
            assert!(stderr.is_empty());
            assert!(!home.cache().exists(), "{name} must not write the cache");
        }
    }

    #[test]
    fn corrupt_cache_counts_as_stale() {
        let home = Home::new();
        std::fs::create_dir_all(home.cache().parent().unwrap()).unwrap();
        std::fs::write(home.cache(), b"not json").unwrap();
        let env = env_of(&[("HOME", &home.home())]);
        let mut calls = 0;
        let mut fetch = || {
            calls += 1;
            Some("v0.7.0".to_string())
        };
        let mut stderr = Vec::new();
        assert!(maybe_notice(&env, NOW, "0.6.6", &mut fetch, &mut stderr));
        assert_eq!(calls, 1);
    }

    // --- network (local mock server) ------------------------------------------

    #[derive(Clone)]
    struct Mock {
        api_status: u16,
        api_body: &'static str,
        web_location: Option<&'static str>,
        delay: Duration,
    }

    async fn serve_mock(mock: Mock) -> (Endpoints, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::{extract::State, http::HeaderMap, response::IntoResponse, routing::get, Router};
        let seen_auth = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        #[derive(Clone)]
        struct S {
            mock: Mock,
            seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        }
        async fn api(State(s): State<S>, headers: HeaderMap) -> impl IntoResponse {
            tokio::time::sleep(s.mock.delay).await;
            if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
                s.seen.lock().unwrap().push(auth.to_string());
            }
            (
                axum::http::StatusCode::from_u16(s.mock.api_status).unwrap(),
                s.mock.api_body,
            )
        }
        async fn web(State(s): State<S>) -> axum::response::Response {
            tokio::time::sleep(s.mock.delay).await;
            match s.mock.web_location {
                Some(location) => (
                    axum::http::StatusCode::FOUND,
                    [(axum::http::header::LOCATION, location)],
                )
                    .into_response(),
                None => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        }
        let state = S {
            mock,
            seen: seen_auth.clone(),
        };
        let app = Router::new()
            .route("/api", get(api))
            .route("/web", get(web))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (
            Endpoints {
                api: format!("http://{addr}/api"),
                web: format!("http://{addr}/web"),
            },
            seen_auth,
        )
    }

    fn block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn api_answer_is_used_and_token_is_sent() {
        block(async {
            let (endpoints, seen) = serve_mock(Mock {
                api_status: 200,
                api_body: r#"{"tag_name":"v0.7.1","draft":false,"prerelease":false}"#,
                web_location: None,
                delay: Duration::ZERO,
            })
            .await;
            let tag = fetch_latest_tag(
                &endpoints,
                Duration::from_secs(5),
                Some("placeholder-token"),
            )
            .await
            .unwrap();
            assert_eq!(tag, "v0.7.1");
            assert_eq!(
                seen.lock().unwrap().as_slice(),
                ["Bearer placeholder-token"]
            );
        });
    }

    #[test]
    fn rate_limited_api_falls_back_to_the_redirect() {
        block(async {
            let (endpoints, seen) = serve_mock(Mock {
                api_status: 403,
                api_body: r#"{"message":"API rate limit exceeded"}"#,
                web_location: Some("https://github.com/leeguooooo/iphone-use/releases/tag/v0.7.2"),
                delay: Duration::ZERO,
            })
            .await;
            let tag = fetch_latest_tag(&endpoints, Duration::from_secs(5), None)
                .await
                .unwrap();
            assert_eq!(tag, "v0.7.2");
            assert!(seen.lock().unwrap().is_empty(), "no token, no header");
        });
    }

    #[test]
    fn both_routes_failing_is_an_error() {
        block(async {
            let (endpoints, _) = serve_mock(Mock {
                api_status: 500,
                api_body: "",
                web_location: None,
                delay: Duration::ZERO,
            })
            .await;
            let error = fetch_latest_tag(&endpoints, Duration::from_secs(5), None)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("500"), "{error}");
        });
    }

    #[test]
    fn slow_network_hits_the_budget() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (endpoints, _) = runtime.block_on(serve_mock(Mock {
            api_status: 200,
            api_body: r#"{"tag_name":"v9.9.9"}"#,
            web_location: None,
            delay: Duration::from_secs(10),
        }));
        let started = std::time::Instant::now();
        let result = fetch_latest_tag_blocking(&endpoints, Duration::from_millis(300), None);
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        drop(runtime);
    }

    #[test]
    fn malformed_answers_are_rejected() {
        assert_eq!(
            tag_from_api_body(br#"{"tag_name":"v1.2.3"}"#),
            Some("v1.2.3".into())
        );
        assert_eq!(
            tag_from_api_body(br#"{"tag_name":"v1.2.3","prerelease":true}"#),
            None
        );
        assert_eq!(tag_from_api_body(br#"{"tag_name":"latest"}"#), None);
        assert_eq!(tag_from_api_body(b"<html>"), None);
        assert_eq!(
            tag_from_location("https://github.com/o/r/releases/tag/v0.6.6"),
            Some("v0.6.6".into())
        );
        assert_eq!(tag_from_location("https://github.com/o/r/releases"), None);
    }

    // --- --json shape -----------------------------------------------------------

    #[test]
    fn json_report_has_the_documented_shape() {
        let skills = vec![SkillInstall {
            channel: SkillChannel::ClaudePlugin,
            path: "/placeholder/path".into(),
            update: format!("claude plugin update {PLUGIN_ID}"),
        }];
        let report = UpgradeReport::new("0.6.6", &Ok("v0.6.7".into()), skills);
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "name": "iphone-use",
                "current": "0.6.6",
                "latest": "0.6.7",
                "update_available": true,
                "skills": [{
                    "channel": "claude-plugin",
                    "path": "/placeholder/path",
                    "update": "claude plugin update iphone-use@leeguooooo-plugins"
                }]
            })
        );
        assert_eq!(report.exit_code(), 0);
        assert_eq!(report.summary(), "iphone-use 0.6.6 -> 0.6.7");
    }

    #[test]
    fn up_to_date_and_failed_reports() {
        let report = UpgradeReport::new("0.6.6", &Ok("v0.6.6".into()), Vec::new());
        assert!(!report.update_available);
        assert_eq!(report.summary(), "iphone-use 0.6.6 is up to date");
        assert_eq!(report.exit_code(), 0);

        let report = UpgradeReport::new("0.6.6", &Err(anyhow::anyhow!("offline")), Vec::new());
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["latest"], serde_json::Value::Null);
        assert_eq!(value["update_available"], false);
        assert_eq!(value["error"], "offline");
        assert_eq!(report.exit_code(), 2);
    }

    // --- skill discovery / refresh ------------------------------------------------

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn finds_each_channel_once() {
        let home = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(home.path()).unwrap();
        // Installer-owned copy + the installer's Claude discovery link to it.
        write(&home.join(".agents/skills/iphone-use/SKILL.md"), "skill");
        write(
            &home.join(".agents/skills/iphone-use/.iphone-use-release"),
            "release_ref=v0.6.6\n",
        );
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
        std::os::unix::fs::symlink(
            home.join(".agents/skills/iphone-use"),
            home.join(".claude/skills/iphone-use"),
        )
        .unwrap();
        // A plain copy for Codex.
        write(&home.join(".codex/skills/iphone-use/SKILL.md"), "skill");
        // The plugin registry.
        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            r#"{"version":2,"plugins":{"iphone-use@leeguooooo-plugins":[{"installPath":"/placeholder/plugin"}]}}"#,
        );

        let skills = find_skills(&home);
        let channels: Vec<_> = skills.iter().map(|s| s.channel).collect();
        assert_eq!(
            channels,
            [
                SkillChannel::ClaudePlugin,
                SkillChannel::Installer,
                SkillChannel::Copied
            ]
        );
        assert_eq!(skills[0].path, "/placeholder/plugin");
        assert_eq!(skills[2].update, SKILLS_UPDATE_COMMAND);
    }

    fn git(args: &[&str], dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn remote_identity_accepts_only_this_repository() {
        for remote in [
            "https://github.com/leeguooooo/iphone-use",
            "https://github.com/leeguooooo/iphone-use.git",
            "https://github.com/leeguooooo/iphone-use/",
            "git@github.com:leeguooooo/iphone-use.git",
            "ssh://git@github.com/leeguooooo/iphone-use.git",
        ] {
            assert!(remote_is_iphone_use(remote), "{remote}");
        }
        for remote in [
            "https://github.com/someone/iphone-use.git",
            "https://github.com/leeguooooo/iphone-use-fork.git",
            "https://github.com/xleeguooooo/iphone-use",
            "https://example.com/leeguooooo/iphone-use",
            "",
        ] {
            assert!(!remote_is_iphone_use(remote), "{remote}");
        }
    }

    #[test]
    fn a_repository_that_vendors_the_skill_is_a_copy_not_a_checkout() {
        let home = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(home.path()).unwrap();
        let other = home.join("src/other-project");
        write(&other.join("skills/iphone-use/SKILL.md"), "skill");
        git(&["init", "-q"], &other);
        git(
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/someone/other-project.git",
            ],
            &other,
        );
        std::fs::create_dir_all(home.join(".agents/skills")).unwrap();
        std::os::unix::fs::symlink(
            other.join("skills/iphone-use"),
            home.join(".agents/skills/iphone-use"),
        )
        .unwrap();
        let skills = find_skills(&home);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].channel, SkillChannel::Copied);
    }

    #[test]
    fn git_checkout_is_detected_and_pulled() {
        let home = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(home.path()).unwrap();
        let checkout = home.join("src/iphone-use");
        write(&checkout.join("skills/iphone-use/SKILL.md"), "skill");
        git(&["init", "-q"], &checkout);
        git(
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:leeguooooo/iphone-use.git",
            ],
            &checkout,
        );
        std::fs::create_dir_all(home.join(".agents/skills")).unwrap();
        std::os::unix::fs::symlink(
            checkout.join("skills/iphone-use"),
            home.join(".agents/skills/iphone-use"),
        )
        .unwrap();

        let skills = find_skills(&home);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].channel, SkillChannel::Git);
        assert_eq!(
            skills[0].update,
            format!("git -C {} pull --ff-only", checkout.display())
        );

        let mut calls = Vec::new();
        let mut runner = |program: &str, args: &[String]| {
            calls.push(format!("{program} {}", args.join(" ")));
            Ok(false)
        };
        let mut out = Vec::new();
        refresh_skills(&skills, true, false, &mut runner, &mut out).unwrap();
        assert_eq!(
            calls,
            [format!("git -C {} pull --ff-only", checkout.display())]
        );
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("failed"), "{out}");
    }

    #[test]
    fn refresh_runs_claude_only_when_on_path_and_never_runs_npx() {
        let skills = vec![
            SkillInstall {
                channel: SkillChannel::ClaudePlugin,
                path: "/p".into(),
                update: format!("claude plugin update {PLUGIN_ID}"),
            },
            SkillInstall {
                channel: SkillChannel::Copied,
                path: "/c".into(),
                update: SKILLS_UPDATE_COMMAND.into(),
            },
            SkillInstall {
                channel: SkillChannel::Installer,
                path: "/i".into(),
                update: "iphone-use upgrade".into(),
            },
        ];
        let mut calls = Vec::new();
        let mut runner = |program: &str, args: &[String]| {
            calls.push(format!("{program} {}", args.join(" ")));
            Ok(true)
        };
        let mut out = Vec::new();
        refresh_skills(&skills, true, true, &mut runner, &mut out).unwrap();
        assert_eq!(
            calls,
            ["claude plugin update iphone-use@leeguooooo-plugins"]
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("npx skills update iphone-use"), "{text}");
        assert!(text.contains("refreshed by install.sh"), "{text}");

        let mut calls = Vec::new();
        let mut runner = |program: &str, args: &[String]| {
            calls.push(format!("{program} {}", args.join(" ")));
            Ok(true)
        };
        let mut out = Vec::new();
        refresh_skills(&skills[..1], true, false, &mut runner, &mut out).unwrap();
        assert!(calls.is_empty());
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("run `claude plugin update iphone-use@leeguooooo-plugins`"));
    }

    #[test]
    fn only_the_installed_app_binary_runs_the_installer() {
        assert!(installed_by_installer(Path::new(
            "/Users/placeholder/Applications/iPhoneUse.app/Contents/MacOS/iphone-use"
        )));
        assert!(!installed_by_installer(Path::new(
            "/src/iphone-use/target/debug/iphone-use"
        )));
    }
}
