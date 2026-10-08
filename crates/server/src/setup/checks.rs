//! The read-only checks setup and doctor share: Xcode and its SDK, the
//! signing identity, the runner sources, WARP, macOS proxies, the USB phone,
//! and the relay binary.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use super::ctx::{Ctx, XCODE_APP_STORE_URL};
use super::sys;
use super::term::{info, ok, warn};
use crate::usbmux::Value;

// ── versions ────────────────────────────────────────────────────────────────

/// `27`, `27.2`, `27.2.1`.
pub fn valid_os_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    !text.is_empty()
        && parts.len() <= 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Dotted `a` strictly lower than `b` (missing parts count as 0).
pub fn version_lt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (pa, pb) = (parse(a), parse(b));
    for i in 0..3 {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x != y {
            return x < y;
        }
    }
    false
}

/// `27.2.1` → `27.2`, `27` → `27.0`.
pub fn os_major_minor(version: &str) -> String {
    let mut parts = version.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    format!(
        "{}.{}",
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0)
    )
}

// ── Xcode ───────────────────────────────────────────────────────────────────

/// First line of `xcodebuild -version` ("Xcode 27.0"), or empty.
pub fn xcode_version() -> String {
    sys::stdout_of("xcodebuild", &["-version"])
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

/// Identifies the selected Xcode: its developer directory plus the mtime of
/// its version.plist, so switching or updating Xcode changes it. Empty when
/// either cannot be read (callers then skip their cache).
fn xcode_stamp() -> String {
    use std::os::unix::fs::MetadataExt as _;
    let developer = sys::stdout_of("xcode-select", &["-p"]);
    (!developer.is_empty())
        .then(|| Path::new(&developer).join("../version.plist"))
        .and_then(|plist| {
            std::fs::metadata(&plist)
                .ok()
                .map(|meta| format!("{developer}|{}", meta.mtime()))
        })
        .unwrap_or_default()
}

/// [`xcode_version`] cached per selected developer directory: the key is the
/// directory plus the mtime of its version.plist, so switching or updating
/// Xcode re-reads it (`xcodebuild -version` costs ~0.4 s per reconnect).
pub fn xcode_version_cached(state_dir: &Path, xcodebuild: &str) -> String {
    let stamp = xcode_stamp();
    let cache = state_dir.join(".xcode-version.cache");
    let cache_is_link = std::fs::symlink_metadata(&cache).is_ok_and(|m| m.file_type().is_symlink());
    if !stamp.is_empty() && !cache_is_link {
        if let Ok(text) = std::fs::read_to_string(&cache) {
            if let Some((key, version)) = text.lines().next().and_then(|line| line.split_once('\t'))
            {
                if key == stamp
                    && version.starts_with("Xcode ")
                    && version[6..].starts_with(|c: char| c.is_ascii_digit())
                {
                    return version.to_string();
                }
            }
        }
    }
    let version = sys::stdout_of(xcodebuild, &["-version"])
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    if !stamp.is_empty() && !version.is_empty() && !cache_is_link {
        let _ = std::fs::write(&cache, format!("{stamp}\t{version}\n"));
    }
    version
}

pub fn xcode_major() -> String {
    xcode_version()
        .strip_prefix("Xcode ")
        .map(|rest| rest.chars().take_while(char::is_ascii_digit).collect())
        .unwrap_or_default()
}

/// The selected iPhoneOS SDK version, validated. Cached for the life of the
/// process per selected Xcode (developer directory + its version.plist mtime):
/// `xcrun --show-sdk-version` costs ~0.3 s and a round asks more than once.
pub fn ios_sdk_version() -> Option<String> {
    use std::sync::Mutex;
    static CACHE: Mutex<Option<(String, String)>> = Mutex::new(None);
    let stamp = xcode_stamp();
    if !stamp.is_empty() {
        if let Some((key, version)) = CACHE.lock().ok().and_then(|cache| cache.clone()) {
            if key == stamp {
                return Some(version);
            }
        }
    }
    let version = sys::stdout_of("xcrun", &["--sdk", "iphoneos", "--show-sdk-version"]);
    if !valid_os_version(&version) {
        return None;
    }
    if !stamp.is_empty() {
        if let Ok(mut cache) = CACHE.lock() {
            *cache = Some((stamp, version.clone()));
        }
    }
    Some(version)
}

/// Lowest deployment target the selected SDK accepts (its SDKSettings);
/// Xcode 27 is known to need 15.0 when the settings cannot be read there.
pub fn ios_sdk_min_deployment_target() -> Option<String> {
    let sdk = sys::stdout_of("xcrun", &["--sdk", "iphoneos", "--show-sdk-path"]);
    let settings = Path::new(&sdk).join("SDKSettings.plist");
    let mut minimum = String::new();
    if !sdk.is_empty() && settings.is_file() {
        minimum = sys::stdout_of(
            "plutil",
            &[
                "-extract",
                "SupportedTargets.iphoneos.MinimumDeploymentTarget",
                "raw",
                "-o",
                "-",
                &settings.to_string_lossy(),
            ],
        );
    }
    if !valid_os_version(&minimum) {
        minimum.clear();
        if xcode_major().parse::<u32>().is_ok_and(|major| major >= 27) {
            minimum = "15.0".into();
        }
    }
    (!minimum.is_empty()).then_some(minimum)
}

/// (lowest, highest) `IPHONEOS_DEPLOYMENT_TARGET` in the runner project.
pub fn project_deployment_targets(project: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(project.join("project.pbxproj")).ok()?;
    let mut low: Option<String> = None;
    let mut high: Option<String> = None;
    for line in text.lines() {
        let Some(index) = line.find("IPHONEOS_DEPLOYMENT_TARGET = ") else {
            continue;
        };
        let rest = &line[index + "IPHONEOS_DEPLOYMENT_TARGET = ".len()..];
        let Some(rest) = rest.find(';').map(|end| &rest[..end]) else {
            continue;
        };
        let value = rest.trim_matches('"');
        if !valid_os_version(value) {
            continue;
        }
        if low.as_deref().is_none_or(|l| version_lt(value, l)) {
            low = Some(value.to_string());
        }
        if high.as_deref().is_none_or(|h| version_lt(h, value)) {
            high = Some(value.to_string());
        }
    }
    Some((low?, high?))
}

/// The deployment target the build needs, or `None` when every target in the
/// project is already supported: max(highest project value, SDK minimum).
pub fn required_deployment_target(project: &Path) -> Option<String> {
    let minimum = ios_sdk_min_deployment_target()?;
    let (low, high) = project_deployment_targets(project)?;
    if !version_lt(&low, &minimum) {
        return None;
    }
    Some(if version_lt(&high, &minimum) {
        minimum
    } else {
        high
    })
}

// ── signing identity ────────────────────────────────────────────────────────

/// Team IDs of the accounts signed in to Xcode (deduplicated, sorted).
pub fn xcode_account_teams() -> Vec<String> {
    let text = sys::stdout_of("defaults", &["export", "com.apple.dt.Xcode", "-"]);
    let Ok(prefs) = crate::usbmux::parse_plist(&text) else {
        return Vec::new();
    };
    let mut teams = Vec::new();
    if let Some(Value::Dict(entries)) = prefs.get("IDEProvisioningTeamByIdentifier") {
        for (_, list) in entries {
            for team in list.as_array().unwrap_or_default() {
                if let Some(id) = team
                    .get("teamID")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    teams.push(id.to_string());
                }
            }
        }
    }
    teams.sort();
    teams.dedup();
    teams
}

/// Team IDs of the valid Apple Development signing identities in the
/// keychain (deduplicated, sorted). Xcode 26 no longer writes
/// `IDEProvisioningTeamByIdentifier` when an account is added, so a fresh
/// install with one signed-in Apple ID finds no team there; the team is the
/// OU of the development certificate Xcode created for that account.
pub fn dev_cert_teams() -> Vec<String> {
    let identities = sys::stdout_of("security", &["find-identity", "-v", "-p", "codesigning"]);
    let certs = sys::stdout_of(
        "security",
        &["find-certificate", "-a", "-c", "Apple Development"],
    );
    teams_from_keychain_dump(&certs, &identities)
}

/// The OU of every `Apple Development` certificate in a `security
/// find-certificate -a` dump whose label is also a valid identity in
/// `security find-identity -v` (a private key is present and the
/// certificate is neither expired nor revoked).
fn teams_from_keychain_dump(certs: &str, identities: &str) -> Vec<String> {
    let mut teams = Vec::new();
    let mut label = String::new();
    for line in certs.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("\"labl\"<blob>=\"") {
            label = rest.trim_end_matches('"').to_string();
        } else if let Some(rest) = line.strip_prefix("\"subj\"<blob>=0x") {
            let hex = rest.split_whitespace().next().unwrap_or_default();
            let valid = !label.is_empty() && identities.contains(&format!("\"{label}\""));
            if let Some(team) = valid.then(|| subject_ou(hex)).flatten() {
                teams.push(team);
            }
        }
    }
    teams.sort();
    teams.dedup();
    teams
}

/// The organizationalUnitName (2.5.4.11) of a hex-encoded DER subject, when
/// it is a valid team ID.
fn subject_ou(hex: &str) -> Option<String> {
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    const OU_OID: [u8; 5] = [0x06, 0x03, 0x55, 0x04, 0x0B];
    let at = bytes.windows(OU_OID.len()).position(|w| w == OU_OID)? + OU_OID.len();
    let (&tag, rest) = bytes.get(at..)?.split_first()?;
    let (&len, rest) = rest.split_first()?;
    // UTF8String or PrintableString, short-form length.
    if !matches!(tag, 0x0C | 0x13) || len >= 0x80 {
        return None;
    }
    let team = std::str::from_utf8(rest.get(..usize::from(len))?).ok()?;
    valid_team_id(team).then(|| team.to_string())
}

pub fn valid_team_id(team: &str) -> bool {
    team.len() == 10
        && team
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Dot-separated ASCII labels of letters, digits and inner hyphens; at least
/// two labels.
pub fn valid_bundle_id(bundle: &str) -> bool {
    let labels: Vec<&str> = bundle.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signing {
    pub team: String,
    pub bundle: String,
    /// The bundle id was derived for this team, not configured.
    pub derived: bool,
}

/// One signing identity for doctor, setup and the persisted supervisor. A
/// fresh install derives a team-specific bundle id only after the team ID
/// validates: a shared default cannot work across Apple Developer teams.
pub fn resolve_signing(ctx: &Ctx) -> Result<Signing, String> {
    let mut team = ctx.team_id.clone();
    if team.is_empty() {
        team = sys::defaults_read(
            "com.apple.dt.Xcode",
            "IDEProvisioningTeamManagerLastSelectedTeamID",
        );
    }
    if team.is_empty() {
        // Signing in lists the account teams; only selecting one in a project
        // records the "last selected" key. One team is unambiguous. Xcode 26
        // no longer lists the teams there; its development certificate does.
        let mut teams = xcode_account_teams();
        if teams.is_empty() {
            teams = dev_cert_teams();
        }
        match teams.len() {
            1 => team = teams[0].clone(),
            0 => {
                return Err("Xcode is not signed in to an Apple account, or the account has no Apple Development certificate yet. Open Xcode → Settings → Accounts, click + and add your Apple ID (a free Apple ID works); then select its team → Manage Certificates → + → Apple Development, and rerun.".into())
            }
            _ => {
                return Err(format!(
                    "Xcode is signed in to several teams ({}); pick one: export WDA_TEAM_ID=<one of them>, then rerun.",
                    teams.join(" ")
                ))
            }
        }
    }
    if !valid_team_id(&team) {
        return Err(format!(
            "Invalid WDA_TEAM_ID '{team}'. Expected exactly 10 uppercase ASCII letters/digits, e.g. ABCD123456."
        ));
    }
    let (bundle, derived) = if ctx.bundle_id.is_empty() {
        (
            format!("com.leeguoo.iphone-use.wda.{}", team.to_ascii_lowercase()),
            true,
        )
    } else {
        (ctx.bundle_id.clone(), false)
    };
    if !valid_bundle_id(&bundle) {
        return Err(format!(
            "Invalid WDA_BUNDLE_ID '{bundle}'. Use dot-separated ASCII letters, digits, dots, and hyphens only."
        ));
    }
    Ok(Signing {
        team,
        bundle,
        derived,
    })
}

/// Interactive runs only: put Xcode in front so the person can add an
/// account. Never from launchd or over SSH, where nobody sees the window.
pub fn open_xcode_for_account(ctx: &Ctx) {
    if ctx.keepalive || !sys::stdout_is_tty() {
        return;
    }
    if std::env::var_os("SSH_CONNECTION").is_some_and(|v| !v.is_empty())
        || std::env::var_os("SSH_TTY").is_some_and(|v| !v.is_empty())
    {
        return;
    }
    if sys::run("open", &["-a", "Xcode"]).is_some_and(|out| out.status.success()) {
        info("Opened Xcode: Settings (⌘,) → Accounts → + → Apple ID");
    }
}

// ── runner sources ──────────────────────────────────────────────────────────

/// The sources are built (and their build scripts run) as this user, so they
/// must be this user's own files that nobody else can change.
pub fn runner_source_valid(ctx: &Ctx) -> Result<(), String> {
    let src = &ctx.runner_src;
    let text = src.to_string_lossy();
    if !src.is_absolute() {
        return Err(format!(
            "IPU_RUNNER_SRC must be an absolute path (got '{text}')"
        ));
    }
    if text.chars().any(char::is_whitespace) {
        return Err(format!(
            "the runner source path must not contain whitespace: {text}"
        ));
    }
    if !ctx.runner_project.join("project.pbxproj").is_file() {
        return Err(format!(
            "device runner sources are missing: {}\n   Rerun the installer (it lays them down at {}), or set\n   IPU_RUNNER_SRC=<repo>/runner when working from a checkout.",
            ctx.runner_project.display(),
            ctx.runner_default_src.display()
        ));
    }
    for dir in [src.clone(), src.join("IPhoneUseRunner")] {
        if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!(
                "refusing a symlinked runner source directory: {}",
                dir.display()
            ));
        }
        let Some((owner, mode)) = sys::owner_and_mode(&dir) else {
            return Err(format!("cannot inspect {}", dir.display()));
        };
        if owner != ctx.uid {
            return Err(format!(
                "runner sources are not owned by this user: {}",
                dir.display()
            ));
        }
        if mode & 0o022 != 0 {
            return Err(format!(
                "runner sources are writable by other users: {}",
                dir.display()
            ));
        }
    }
    Ok(())
}

/// Content hash of the runner sources: every regular file under
/// `IPhoneUseRunner` (Xcode's per-user state and dotfiles excluded), path and
/// bytes, in sorted order. It keys the product cache.
pub fn runner_source_hash(src: &Path) -> Option<String> {
    let root = src.join("IPhoneUseRunner");
    let mut entries = Vec::new();
    collect_files(&root, &root, &mut entries);
    if entries.is_empty() {
        return None;
    }
    entries.sort();
    let mut digest = Sha256::new();
    for relative in entries {
        let bytes = std::fs::read(root.join(&relative)).ok()?;
        digest.update(relative.as_bytes());
        digest.update([0u8]);
        digest.update(Sha256::digest(&bytes));
    }
    Some(
        digest
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if name != "xcuserdata" {
                collect_files(root, &path, out);
            }
        } else if meta.is_file() {
            if let Ok(relative) = path.strip_prefix(root) {
                out.push(relative.to_string_lossy().into_owned());
            }
        }
    }
}

// ── WARP ────────────────────────────────────────────────────────────────────

pub fn warp_cli() -> Option<PathBuf> {
    match std::env::var("IPHONE_USE_INTERNAL_TEST_WARP_CLI") {
        Ok(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => sys::which("warp-cli"),
    }
}

fn warp_output(args: &[&str]) -> Option<String> {
    let cli = warp_cli()?;
    let out = sys::run(&cli.to_string_lossy(), args)?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `Connected` as a whole word, any case: `Disconnected` does not count.
pub fn status_says_connected(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| word.eq_ignore_ascii_case("connected"))
}

pub fn warp_on() -> bool {
    warp_output(&["status"]).is_some_and(|text| status_says_connected(&text))
}

/// The value after the first `Mode:` in `warp-cli settings`.
pub fn warp_mode_in(settings: &str) -> Option<String> {
    settings.lines().find_map(|line| {
        let index = line.find("Mode:")?;
        Some(line[index + 5..].trim().to_string())
    })
}

/// Local proxy mode tunnels only HTTP(S) sent to its loopback proxy; it
/// installs no catch-all routes, so CoreDevice traffic is never captured.
pub fn warp_mode_is_local_proxy(mode: &str) -> bool {
    let mode = mode.to_ascii_lowercase();
    matches!(mode.as_str(), "proxy" | "localproxy" | "local proxy") || mode.starts_with("warpproxy")
}

fn warp_local_proxy_mode() -> bool {
    warp_output(&["settings"])
        .and_then(|text| warp_mode_in(&text))
        .is_some_and(|mode| warp_mode_is_local_proxy(&mode))
}

/// The routes under `Excluded:` in `warp-cli tunnel dump`.
pub fn tunnel_dump_excluded(dump: &str) -> Vec<String> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in dump.lines() {
        let trimmed = line.trim();
        if !inside {
            if line.trim_end() == "Excluded:"
                || (line.starts_with("Excluded:") && line["Excluded:".len()..].trim().is_empty())
            {
                inside = true;
            }
            continue;
        }
        let is_header = line.ends_with(':') || line.trim_end().ends_with(':');
        if is_header
            && line.starts_with(|c: char| c.is_ascii_alphabetic())
            && line
                .trim_end()
                .trim_end_matches(':')
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == ' ')
        {
            break;
        }
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// CoreDevice's RSD tunnel uses IPv6 link-local plus a dynamic ULA /64 even
/// over USB; WARP must exclude both or devicectl hangs and the runner dies.
pub fn coredevice_bypass_ready(excluded: &[String]) -> bool {
    excluded.iter().any(|route| route == "fe80::/10")
        && excluded
            .iter()
            .any(|route| route == "fd00::/8" || route == "fc00::/7")
}

pub const WARP_PREFLIGHT_ERROR: &str = "WARP is connected, but its effective Split Tunnel exclusions do not cover the CoreDevice device tunnel.
   If WARP is only needed for specific destinations, prefer a Traffic only
   device profile with Split Tunnels in Include mode and only those destination
   IPs/CIDRs. (Local proxy mode is also route-safe, but its request timeout can
   be unsuitable for long Git uploads.)
   Otherwise add BOTH routes to the device profile's Exclude list:
     - fe80::/10  (IPv6 link-local)
     - fd00::/8   (CoreDevice RSD ULA)
   Then wait for policy propagation/reconnect and verify with:
     warp-cli tunnel dump
   Temporary alternative: warp-cli disconnect
   The script did not change WARP or organization policy.";

/// `Ok` when WARP is off, in local proxy mode, or excludes the CoreDevice routes.
pub fn warp_preflight() -> Result<(), &'static str> {
    if !warp_on() || warp_local_proxy_mode() {
        return Ok(());
    }
    let excluded = warp_output(&["tunnel", "dump"])
        .map(|dump| tunnel_dump_excluded(&dump))
        .unwrap_or_default();
    if coredevice_bypass_ready(&excluded) {
        Ok(())
    } else {
        Err(WARP_PREFLIGHT_ERROR)
    }
}

pub fn warp_ready_summary() -> &'static str {
    if warp_local_proxy_mode() {
        "WARP: connected in Local proxy mode; only explicitly proxied traffic is tunneled"
    } else {
        "WARP: connected with CoreDevice Split Tunnel exclusions (fe80::/10 + fd00::/8)"
    }
}

// ── macOS system proxies ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyEntry {
    pub protocol: String,
    pub host: String,
    pub port: String,
}

/// The enabled top-level HTTP/HTTPS/SOCKS entries of `scutil --proxy`.
/// Nested scoped dictionaries are separate configurations and never override
/// the global values. `None` for an unreadable or malformed snapshot. A
/// well-formed dictionary without the Enable keys means none enabled (#91).
pub fn parse_scutil_proxy(snapshot: &str) -> Option<Vec<ProxyEntry>> {
    let mut depth: i64 = 0;
    let mut root_seen = false;
    let mut invalid = false;
    let mut values: Vec<(String, String)> = Vec::new();
    for line in snapshot.lines() {
        let before = depth;
        let opens = line.matches('{').count() as i64;
        let closes = line.matches('}').count() as i64;
        let fields: Vec<&str> = line.split_whitespace().collect();
        if before == 0 && opens > 0 && fields.first() == Some(&"<dictionary>") {
            root_seen = true;
        }
        if before == 1 && fields.get(1) == Some(&":") {
            let key = fields[0];
            let known = ["HTTP", "HTTPS", "SOCKS"].iter().any(|protocol| {
                key.strip_prefix(protocol)
                    .is_some_and(|rest| matches!(rest, "Enable" | "Proxy" | "Port"))
            });
            if known {
                let value = fields.get(2).copied().unwrap_or("").to_string();
                values.retain(|(k, _)| k != key);
                values.push((key.to_string(), value));
            }
        }
        depth += opens - closes;
        if depth < 0 {
            invalid = true;
        }
    }
    if !root_seen || depth != 0 || invalid {
        return None;
    }
    let get = |key: &str| {
        values
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    Some(
        ["HTTP", "HTTPS", "SOCKS"]
            .iter()
            .filter(|protocol| get(&format!("{protocol}Enable")) == "1")
            .map(|protocol| ProxyEntry {
                protocol: protocol.to_string(),
                host: get(&format!("{protocol}Proxy")),
                port: get(&format!("{protocol}Port")),
            })
            .collect(),
    )
}

pub fn valid_proxy_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:%-".contains(&b))
}

pub fn proxy_is_loopback(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if matches!(
        host.as_str(),
        "localhost" | "localhost." | "::1" | "0:0:0:0:0:0:0:1"
    ) {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts[0] == "127"
        && parts[1..].iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u32>().is_ok_and(|n| n <= 255)
        })
}

/// 0 = a listener answered, 1 = nothing listens, 2 = could not probe.
fn proxy_tcp_reachable(host: &str, port: &str) -> i32 {
    if let Ok(probe) = std::env::var("IPHONE_USE_INTERNAL_TEST_PROXY_PROBE") {
        if !probe.is_empty() {
            if !sys::is_executable(Path::new(&probe)) {
                return 2;
            }
            return sys::run(&probe, &[host, port])
                .and_then(|out| out.status.code())
                .unwrap_or(2);
        }
    }
    if sys::is_executable(Path::new("/usr/bin/nc")) {
        return sys::run("/usr/bin/nc", &["-z", "-w", "1", host, port])
            .and_then(|out| out.status.code())
            .unwrap_or(2);
    }
    2
}

/// Detect active HTTP/HTTPS/SOCKS settings without changing them, printing
/// what it finds. Fails only for a malformed enabled entry or a loopback
/// endpoint with no listener: concrete local faults (the latter reproduced
/// the CoreDevice/DDI failure that motivated this check).
pub fn system_proxy_check() -> Result<(), String> {
    let scutil = std::env::var("IPHONE_USE_INTERNAL_TEST_SCUTIL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/sbin/scutil".into());
    let entries = sys::is_executable(Path::new(&scutil))
        .then(|| sys::run(&scutil, &["--proxy"]))
        .flatten()
        .filter(|out| out.status.success())
        .and_then(|out| parse_scutil_proxy(&String::from_utf8_lossy(&out.stdout)));
    let Some(entries) = entries else {
        return Err(
            "Could not inspect macOS HTTP/HTTPS/SOCKS proxy state with /usr/sbin/scutil.
   The script did not change any proxy settings. Run '/usr/sbin/scutil --proxy'
   to repair System Configuration access, then rerun setup."
                .into(),
        );
    };
    if entries.is_empty() {
        ok("System proxies (HTTP/HTTPS/SOCKS): none enabled");
        return Ok(());
    }
    let mut invalid = Vec::new();
    let mut dead = Vec::new();
    for entry in &entries {
        if !valid_proxy_host(&entry.host) || super::ctx::valid_port(&entry.port).is_none() {
            invalid.push(entry.protocol.clone());
            continue;
        }
        if proxy_is_loopback(&entry.host) {
            match proxy_tcp_reachable(&entry.host, &entry.port) {
                0 => warn(&format!(
                    "~ {} system proxy enabled at {}:{} (TCP listener responds)",
                    entry.protocol, entry.host, entry.port
                )),
                1 => dead.push(format!("{} {}:{}", entry.protocol, entry.host, entry.port)),
                _ => warn(&format!(
                    "~ {} system proxy enabled at {}:{} (local endpoint could not be probed)",
                    entry.protocol, entry.host, entry.port
                )),
            }
        } else {
            warn(&format!(
                "~ {} system proxy enabled at {}:{} (endpoint not probed)",
                entry.protocol, entry.host, entry.port
            ));
        }
    }
    if !invalid.is_empty() || !dead.is_empty() {
        let mut error = "macOS has an enabled but unusable system proxy configuration.".to_string();
        if !invalid.is_empty() {
            error.push_str(&format!(
                "\n   Invalid or incomplete entries: {}",
                invalid.join(", ")
            ));
        }
        if !dead.is_empty() {
            error.push_str(&format!(
                "\n   No reachable TCP listener at configured loopback endpoints: {}",
                dead.join(", ")
            ));
        }
        error.push_str(
            "\n   A stale system proxy can prevent Xcode/CoreDevice from reaching developer services;
   this check does not claim that the iPhone or DDI is defective.
   Fix ONE of:
     - restart the proxy app so the listed local endpoint is listening
     - disable only the stale protocols in System Settings -> Network -> active service
       -> Details -> Proxies
   The script did not change proxy settings. Verify with '/usr/sbin/scutil --proxy',
   then rerun setup.",
        );
        return Err(error);
    }
    warn("~ Active system proxies are not automatically treated as a blocker. If CoreDevice/DDI stalls, retry after bypassing or disabling them.");
    Ok(())
}

// ── the phone ───────────────────────────────────────────────────────────────

/// iPhones physically on USB, as usbmuxd spells their UDIDs (no
/// libimobiledevice, and it cannot hang like devicectl).
pub fn usb_udids() -> Vec<String> {
    sys::block_on(async {
        tokio::time::timeout(Duration::from_secs(3), crate::usbmux::usb_serials())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    })
}

/// `udid` is attached over USB (dash and case do not matter).
pub fn on_usb(udid: &str, usb: &[String]) -> bool {
    let want = crate::usbmux::normalize_udid(udid);
    !want.is_empty()
        && usb
            .iter()
            .any(|serial| crate::usbmux::normalize_udid(serial) == want)
}

/// Whether this Mac can reach the phone at all, by any transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Present,
    /// Not in usbmuxd, and CoreDevice says it is unavailable or does not
    /// list it: nothing to build for or launch on.
    Absent,
    /// devicectl gave no readable answer. Never treated as absent.
    Unknown,
}

/// How usbmuxd reaches the phone right now. USB wins when it is attached both
/// ways; `Unknown` when usbmuxd does not list it or cannot be asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Usb,
    Network,
    Unknown,
}

pub fn transport(udid: &str) -> Transport {
    sys::block_on(async {
        tokio::time::timeout(
            Duration::from_secs(3),
            crate::usbmux::find_attached(&crate::usbmux::normalize_udid(udid)),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
        .map_or(Transport::Unknown, |attached| {
            if attached.usb {
                Transport::Usb
            } else {
                Transport::Network
            }
        })
    })
}

/// What usbmuxd says about one phone. `Unreadable` (no socket, timeout, bad
/// reply) says nothing about the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbmuxView {
    Listed,
    NotListed,
    Unreadable,
}

fn usbmux_view(udid: &str) -> UsbmuxView {
    sys::block_on(async {
        match tokio::time::timeout(
            Duration::from_secs(3),
            crate::usbmux::find_attached(&crate::usbmux::normalize_udid(udid)),
        )
        .await
        {
            Ok(Ok(Some(_))) => UsbmuxView::Listed,
            Ok(Ok(None)) => UsbmuxView::NotListed,
            _ => UsbmuxView::Unreadable,
        }
    })
}

/// usbmuxd lists `udid`, over USB or the network. Cheap and cannot hang.
pub fn usbmux_lists(udid: &str) -> bool {
    usbmux_view(udid) == UsbmuxView::Listed
}

/// Phones usbmuxd has listed during this process. Once a phone has been
/// reached through usbmuxd, its disappearing from usbmuxd means it left:
/// CoreDevice keeps reporting a just-unplugged phone as `connected` for
/// minutes, so it cannot overrule that.
fn seen_in_usbmux() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
}

/// usbmuxd first; only when it does not list the phone, ask CoreDevice.
pub fn presence(udid: &str) -> Presence {
    if udid.is_empty() {
        return Presence::Present;
    }
    let key = crate::usbmux::normalize_udid(udid);
    let view = usbmux_view(udid);
    let seen = {
        let mut seen = seen_in_usbmux().lock().unwrap_or_else(|e| e.into_inner());
        if view == UsbmuxView::Listed {
            seen.insert(key.clone());
        }
        seen.contains(&key)
    };
    if view == UsbmuxView::Listed {
        return presence_from(view, seen, None, udid);
    }
    let json = sys::devicectl_json(10, &["list", "devices"]);
    presence_from(view, seen, json.as_deref(), udid)
}

/// CoreDevice has a live Wi-Fi tunnel to `udid` right now (the relay can
/// reach the runner through it; see `crate::tunnel`).
pub fn wifi_tunnel(udid: &str) -> bool {
    !udid.is_empty()
        && sys::devicectl_json(10, &["list", "devices"]).is_some_and(|json| {
            matches!(
                crate::tunnel::tunnel_view(&json, udid),
                crate::tunnel::TunnelView::Connected(_)
            )
        })
}

/// The presence verdict from usbmuxd's view, whether usbmuxd listed the phone
/// earlier in this process, and CoreDevice's `devicectl list devices -j`
/// output (`None` when devicectl gave no answer).
///
/// - Listed by usbmuxd: present.
/// - A live CoreDevice Wi-Fi tunnel: present, even when usbmuxd lost the
///   phone (the relay reaches the runner through the tunnel).
/// - Not listed, but listed earlier: absent. CoreDevice's cached state can
///   still say `connected` long after the cable is pulled.
/// - Not listed, and CoreDevice calls it `wired`: absent. A wired phone is
///   always in usbmuxd, so CoreDevice's record is stale.
/// - Otherwise CoreDevice decides (a Wi-Fi phone usbmuxd never lists), and no
///   readable answer is `Unknown`, which callers never treat as absent.
pub fn presence_from(
    view: UsbmuxView,
    seen_in_usbmux: bool,
    coredevice_json: Option<&str>,
    udid: &str,
) -> Presence {
    if view == UsbmuxView::Listed {
        return Presence::Present;
    }
    if coredevice_json.is_some_and(|json| {
        matches!(
            crate::tunnel::tunnel_view(json, udid),
            crate::tunnel::TunnelView::Connected(_)
        )
    }) {
        return Presence::Present;
    }
    if view == UsbmuxView::NotListed && seen_in_usbmux {
        return Presence::Absent;
    }
    let Some(json) = coredevice_json else {
        return Presence::Unknown;
    };
    let verdict = coredevice_presence(json, udid);
    if view == UsbmuxView::NotListed
        && verdict == Presence::Present
        && coredevice_transport(json, udid).as_deref() == Some("wired")
    {
        return Presence::Absent;
    }
    verdict
}

fn coredevice_device<'a>(
    value: &'a serde_json::Value,
    udid: &str,
) -> Option<Option<&'a serde_json::Value>> {
    let devices = value
        .pointer("/result/devices")
        .and_then(serde_json::Value::as_array)?;
    let want = crate::usbmux::normalize_udid(udid);
    Some(devices.iter().find(|device| {
        device
            .pointer("/hardwareProperties/udid")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| crate::usbmux::normalize_udid(id) == want)
    }))
}

/// CoreDevice's `transportType` for `udid` (`wired`, `localNetwork`), if any.
fn coredevice_transport(json: &str, udid: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
    coredevice_device(&value, udid)??
        .pointer("/connectionProperties/transportType")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// CoreDevice's view of `udid` in `devicectl list devices -j` output. An
/// unplugged phone stays listed with `tunnelState: unavailable` and no
/// transport; a Wi-Fi one has a `localNetwork` transport.
pub fn coredevice_presence(json: &str, udid: &str) -> Presence {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Presence::Unknown;
    };
    let Some(found) = coredevice_device(&value, udid) else {
        return Presence::Unknown;
    };
    let Some(device) = found else {
        return Presence::Absent;
    };
    let tunnel = device
        .pointer("/connectionProperties/tunnelState")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let transport = device
        .pointer("/connectionProperties/transportType")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if tunnel == "unavailable" && transport.is_empty() {
        Presence::Absent
    } else {
        Presence::Present
    }
}

/// The phone's iOS version: lockdownd over usbmuxd, else devicectl's JSON.
/// Never gates setup on its own.
pub fn device_ios_version(udid: &str) -> Option<String> {
    if udid.is_empty() {
        return None;
    }
    let from_lockdown = sys::block_on(async {
        tokio::time::timeout(Duration::from_secs(5), crate::lockdown::device_info(udid))
            .await
            .ok()
            .and_then(Result::ok)
    })
    .and_then(|info| info.product_version)
    .filter(|version| valid_os_version(version));
    if from_lockdown.is_some() {
        return from_lockdown;
    }
    let json = sys::devicectl_json(10, &["device", "info", "details", "--device", udid])?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    find_key(&value, "osVersionNumber")
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|version| valid_os_version(version))
}

fn find_key<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|nested| find_key(nested, key))),
        serde_json::Value::Array(items) => items.iter().find_map(|item| find_key(item, key)),
        _ => None,
    }
}

/// `enabled`, `disabled`, or empty when devicectl cannot tell.
pub fn developer_mode_status(udid: &str) -> String {
    sys::devicectl_json(5, &["device", "info", "details", "--device", udid])
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .and_then(|value| {
            value
                .get("result")?
                .get("deviceProperties")?
                .get("developerModeStatus")?
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_default()
}

// ── the relay binary ────────────────────────────────────────────────────────

/// The iphone-use binary that serves the USB relays: the daemon this setup
/// configures (an instance runs its own runtime copy), then the standard app
/// locations. It must understand `relay` and sit on a path without spaces:
/// the PID record matches its exact argv.
pub fn relay_binary(ctx: &Ctx) -> Option<PathBuf> {
    let program = sys::plist_program_argument(&ctx.daemon_plist, 0);
    let candidates = [
        std::env::var("IPHONE_USE_RELAY_BIN").unwrap_or_default(),
        program,
        ctx.home
            .join("Applications/iPhoneUse.app/Contents/MacOS/iphone-use")
            .to_string_lossy()
            .into_owned(),
        "/Applications/iPhoneUse.app/Contents/MacOS/iphone-use".to_string(),
    ];
    candidates
        .into_iter()
        .filter(|c| !c.is_empty())
        .find_map(|candidate| {
            let path = PathBuf::from(&candidate);
            let shaped = candidate.starts_with('/')
                && candidate.ends_with("/iphone-use")
                && !candidate.chars().any(char::is_whitespace);
            (shaped
                && sys::is_executable(&path)
                && sys::run(&candidate, &["relay", "--help"])
                    .is_some_and(|out| out.status.success()))
            .then_some(path)
        })
}

/// The Xcode checks doctor adds for the two ways an Xcode upgrade wedged
/// setup: a deployment target the new Xcode rejects, and stale .xctestrun
/// files an earlier Xcode left. `false` when one blocks setup.
pub fn doctor_xcode_compat(ctx: &Ctx) -> bool {
    let passed = true;
    let major = xcode_major();
    let major = if major.is_empty() {
        "?".to_string()
    } else {
        major
    };
    if let Some(required) = required_deployment_target(&ctx.runner_project) {
        let minimum = ios_sdk_min_deployment_target().unwrap_or_default();
        let lowest = project_deployment_targets(&ctx.runner_project)
            .map(|t| t.0)
            .unwrap_or_default();
        let current = std::fs::symlink_metadata(&ctx.xcconfig_file)
            .ok()
            .filter(|m| m.is_file())
            .and_then(|_| std::fs::read_to_string(&ctx.xcconfig_file).ok())
            .and_then(|text| {
                text.lines()
                    .filter_map(|line| line.strip_prefix("IPHONEOS_DEPLOYMENT_TARGET = "))
                    .rfind(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
                    .map(str::to_string)
            })
            .unwrap_or_default();
        if current == required {
            ok(&format!(
                "Xcode {major} deployment target override: IPHONEOS_DEPLOYMENT_TARGET = {required} ({})",
                ctx.xcconfig_file.display()
            ));
        } else {
            warn(&format!(
                "~ Xcode {major} supports iOS deployment targets from {minimum}, but the runner project sets {lowest}; no override is in place yet. Setup writes {} (IPHONEOS_DEPLOYMENT_TARGET = {required}) on its next run",
                ctx.xcconfig_file.display()
            ));
        }
    }
    let sdk = ios_sdk_version();
    let products = ctx.runner_derived_data.join("Build/Products");
    if products.is_dir() {
        let mut runs: Vec<String> = std::fs::read_dir(&products)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.ends_with(".xctestrun"))
                    .collect()
            })
            .unwrap_or_default();
        runs.sort();
        if runs.len() > 1 {
            let matches = sdk
                .as_ref()
                .map(|sdk| {
                    runs.iter()
                        .filter(|name| name.contains(&format!("_iphoneos{sdk}-")))
                        .count()
                })
                .unwrap_or(0);
            warn(&format!(
                "~ multiple .xctestrun files in {} (left by an earlier Xcode):",
                products.display()
            ));
            for name in &runs {
                println!("     {name}");
            }
            if matches == 1 {
                println!(
                    "     setup uses the one for the current SDK (iphoneos{}); the others are stale and can be deleted",
                    sdk.unwrap_or_default()
                );
            } else {
                println!(
                    "     none is uniquely for the current SDK ({}); setup cannot pick a product to launch until the stale files are deleted",
                    sdk.map(|s| format!("iphoneos{s}")).unwrap_or_default()
                );
            }
        }
    }
    passed
}

pub fn xcode_missing_message() -> String {
    format!("X Xcode is not installed: get it from the App Store ({XCODE_APP_STORE_URL}), open it once, then rerun")
}

#[cfg(test)]
mod tests {
    use super::*;

    // `security find-certificate -a -c "Apple Development"`, trimmed to the
    // attributes read; the subject is the DER of a real development
    // certificate with the personal fields replaced.
    const CERT_DUMP: &str = r#"keychain: "/Users/me/Library/Keychains/login.keychain-db"
version: 512
class: 0x80001000
attributes:
    "alis"<blob>="Apple Development: me@example.com (ab12cd34ef)"
    "labl"<blob>="Apple Development: me@example.com (AB12CD34EF)"
    "subj"<blob>=0x305B3137303506035504030C2E4170706C6520446576656C6F706D656E743A206D65406578616D706C652E636F6D2028414231324344333445462931133011060355040B0C0A58353437514B34384244310B3009060355040613025553  "0\201\2051..."
keychain: "/Users/me/Library/Keychains/login.keychain-db"
attributes:
    "labl"<blob>="Apple Development: old@example.com (ZZ99ZZ99ZZ)"
    "subj"<blob>=0x301531133011060355040B0C0A5A5A39395A5A39395A5A
"#;

    #[test]
    fn dev_cert_team_is_the_subject_ou_of_a_valid_identity() {
        let identities = r#"  1) 9A84910CBD8B11B6A4B6410E364A3849262C9985 "Apple Development: me@example.com (AB12CD34EF)"
     1 valid identities found"#;
        assert_eq!(
            teams_from_keychain_dump(CERT_DUMP, identities),
            vec!["X547QK48BD"]
        );
        assert!(
            teams_from_keychain_dump(CERT_DUMP, "     0 valid identities found").is_empty(),
            "an expired, revoked or keyless certificate names no team"
        );
    }

    #[test]
    fn subject_ou_reads_only_a_team_shaped_value() {
        assert_eq!(
            subject_ou("31133011060355040B0C0A41424344453132333435"),
            Some("ABCDE12345".into())
        );
        assert_eq!(
            subject_ou("31113011060355040B0C086162636465666768"),
            None,
            "not a team ID"
        );
        assert_eq!(subject_ou("3009060355040613025553"), None, "no OU");
        assert_eq!(subject_ou("zz"), None);
        assert_eq!(subject_ou("060355040B0C0A4142"), None, "truncated");
    }

    #[test]
    fn coredevice_says_absent_only_for_an_unavailable_phone_without_transport() {
        // Shapes taken from `devicectl list devices -j` with one phone wired,
        // one unplugged, and one reachable over Wi-Fi.
        let json = r#"{"result":{"devices":[
            {"hardwareProperties":{"udid":"00008110-0002346211A0401E"},
             "connectionProperties":{"tunnelState":"connected","transportType":"wired"}},
            {"hardwareProperties":{"udid":"00008150-000A60EC1A02401C"},
             "connectionProperties":{"tunnelState":"unavailable","pairingState":"paired"}},
            {"hardwareProperties":{"udid":"00008110-001C18203AD2401E"},
             "connectionProperties":{"tunnelState":"disconnected","transportType":"localNetwork"}}
        ]}}"#;
        assert_eq!(
            coredevice_presence(json, "00008110-0002346211A0401E"),
            Presence::Present
        );
        assert_eq!(
            coredevice_presence(json, "00008150000a60ec1a02401c"),
            Presence::Absent,
            "dash and case do not matter"
        );
        assert_eq!(
            coredevice_presence(json, "00008110-001C18203AD2401E"),
            Presence::Present,
            "a Wi-Fi phone is reachable"
        );
        assert_eq!(
            coredevice_presence(json, "00008101-0000000000000001"),
            Presence::Absent,
            "a phone CoreDevice does not list is not connected"
        );
        assert_eq!(coredevice_presence("", "00008150"), Presence::Unknown);
        assert_eq!(
            coredevice_presence(r#"{"error":{}}"#, "00008150"),
            Presence::Unknown
        );
    }

    #[test]
    fn a_phone_usbmux_no_longer_lists_is_absent_even_when_coredevice_says_connected() {
        // CoreDevice's cached record minutes after the USB cable was pulled
        // (seen on hardware: devicectl kept "connected" for ~4 minutes).
        let stale_wired = r#"{"result":{"devices":[
            {"hardwareProperties":{"udid":"00008110-0002346211A0401E"},
             "connectionProperties":{"tunnelState":"connected","transportType":"wired"}}
        ]}}"#;
        let wifi = r#"{"result":{"devices":[
            {"hardwareProperties":{"udid":"00008150-000A60EC1A02401C"},
             "connectionProperties":{"tunnelState":"connected","transportType":"localNetwork"}}
        ]}}"#;
        let usb_phone = "00008110-0002346211A0401E";
        let wifi_phone = "00008150-000A60EC1A02401C";
        use UsbmuxView::{Listed, NotListed, Unreadable};

        // USB phone unplugged: usbmuxd lost it, CoreDevice still says wired.
        assert_eq!(
            presence_from(NotListed, true, Some(stale_wired), usb_phone),
            Presence::Absent
        );
        assert_eq!(
            presence_from(NotListed, false, Some(stale_wired), usb_phone),
            Presence::Absent,
            "a wired phone missing from usbmuxd is gone even in a fresh process"
        );
        // A Wi-Fi phone usbmuxd used to list is gone too, whatever CoreDevice says.
        assert_eq!(
            presence_from(NotListed, true, Some(wifi), wifi_phone),
            Presence::Absent
        );
        // A CoreDevice-only Wi-Fi phone usbmuxd never listed: CoreDevice decides.
        assert_eq!(
            presence_from(NotListed, false, Some(wifi), wifi_phone),
            Presence::Present
        );
        // Listed by usbmuxd: present without asking CoreDevice.
        assert_eq!(
            presence_from(Listed, true, None, usb_phone),
            Presence::Present
        );
        // usbmuxd unreadable: it says nothing, so CoreDevice decides, stale or not.
        assert_eq!(
            presence_from(Unreadable, true, Some(stale_wired), usb_phone),
            Presence::Present
        );
        // No readable answer anywhere: unknown, never absent.
        assert_eq!(
            presence_from(NotListed, false, None, usb_phone),
            Presence::Unknown
        );
        assert_eq!(
            presence_from(Unreadable, false, None, usb_phone),
            Presence::Unknown
        );
    }

    #[test]
    fn a_live_wifi_tunnel_keeps_a_phone_present_after_the_cable_is_pulled() {
        // guouli on hardware: launched over USB, unplugged, CoreDevice keeps a
        // Wi-Fi tunnel the relay reaches the runner through.
        let tunnel = r#"{"result":{"devices":[
            {"hardwareProperties":{"udid":"00008110-001C18203AD2401E"},
             "connectionProperties":{"tunnelState":"connected","transportType":"localNetwork",
                                     "tunnelIPAddress":"fd89:9bfc:f458::1"}}
        ]}}"#;
        let phone = "00008110-001C18203AD2401E";
        use UsbmuxView::{NotListed, Unreadable};
        assert_eq!(
            presence_from(NotListed, true, Some(tunnel), phone),
            Presence::Present
        );
        assert_eq!(
            presence_from(Unreadable, true, Some(tunnel), phone),
            Presence::Present
        );
        // A stale wired record with an address is still not a Wi-Fi tunnel.
        let stale_wired = r#"{"result":{"devices":[
            {"hardwareProperties":{"udid":"00008110-001C18203AD2401E"},
             "connectionProperties":{"tunnelState":"connected","transportType":"wired",
                                     "tunnelIPAddress":"fd5a:17ea:6b83::1"}}
        ]}}"#;
        assert_eq!(
            presence_from(NotListed, true, Some(stale_wired), phone),
            Presence::Absent
        );
    }

    #[test]
    fn versions_compare_like_the_shell() {
        assert!(valid_os_version("27"));
        assert!(valid_os_version("27.2.1"));
        assert!(!valid_os_version("27.2.1.4"));
        assert!(!valid_os_version("27.x"));
        assert!(!valid_os_version(""));
        assert!(version_lt("27.0", "27.2"));
        assert!(!version_lt("27.2", "27.2.0"));
        assert!(version_lt("15", "15.0.1"));
        assert!(!version_lt("27.2", "27.1"));
        assert_eq!(os_major_minor("27.2.1"), "27.2");
        assert_eq!(os_major_minor("27"), "27.0");
    }

    #[test]
    fn team_and_bundle_rules() {
        assert!(valid_team_id("6ZPXG4KVVS"));
        assert!(!valid_team_id("6zpxg4kvvs"));
        assert!(!valid_team_id("6ZPXG4KVV"));
        assert!(valid_bundle_id("com.leeguoo.iphone-use.wda.6zpxg4kvvs"));
        assert!(!valid_bundle_id("com"));
        assert!(!valid_bundle_id("com..x"));
        assert!(!valid_bundle_id("com.-x"));
        assert!(!valid_bundle_id("com.x y"));
    }

    #[test]
    fn warp_status_needs_the_whole_word() {
        assert!(status_says_connected("Status update: Connected"));
        assert!(!status_says_connected("Status update: Disconnected"));
        assert!(
            status_says_connected("Dis-Connected"),
            "a hyphen is a word boundary, as in grep -w"
        );
        assert_eq!(
            warp_mode_in("Always On: true\nMode: WarpProxy on port 40000\n").as_deref(),
            Some("WarpProxy on port 40000")
        );
        assert!(warp_mode_is_local_proxy("warpproxy on port 40000"));
        assert!(warp_mode_is_local_proxy("Local Proxy"));
        assert!(!warp_mode_is_local_proxy("Warp"));
    }

    #[test]
    fn tunnel_dump_exclusions() {
        let dump = "Included:\n  0.0.0.0/0\nExcluded:\n  fe80::/10\n  fd00::/8\n  10.0.0.0/8\nFallback domains:\n  local\n";
        let excluded = tunnel_dump_excluded(dump);
        assert_eq!(excluded, vec!["fe80::/10", "fd00::/8", "10.0.0.0/8"]);
        assert!(coredevice_bypass_ready(&excluded));
        assert!(coredevice_bypass_ready(&[
            "fe80::/10".into(),
            "fc00::/7".into()
        ]));
        assert!(!coredevice_bypass_ready(&["fd00::/8".into()]));
    }

    #[test]
    fn scutil_proxy_snapshot() {
        let none = "<dictionary> {\n  ExceptionsList : <array> {\n    0 : *.local\n  }\n  FTPPassive : 1\n}\n";
        assert_eq!(parse_scutil_proxy(none), Some(vec![]));
        let some = "<dictionary> {\n  HTTPEnable : 1\n  HTTPPort : 7890\n  HTTPProxy : 127.0.0.1\n  HTTPSEnable : 0\n  SOCKSEnable : 1\n  SOCKSProxy : 10.0.0.1\n  SOCKSPort : 1080\n  __SCOPED__ : <dictionary> {\n    en0 : <dictionary> {\n      HTTPEnable : 1\n      HTTPProxy : 9.9.9.9\n    }\n  }\n}\n";
        let entries = parse_scutil_proxy(some).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            ProxyEntry {
                protocol: "HTTP".into(),
                host: "127.0.0.1".into(),
                port: "7890".into()
            }
        );
        assert_eq!(entries[1].protocol, "SOCKS");
        assert_eq!(parse_scutil_proxy("garbage"), None);
        assert_eq!(
            parse_scutil_proxy("<dictionary> {\n  HTTPEnable : 1\n"),
            None
        );
        assert!(proxy_is_loopback("127.0.0.1"));
        assert!(proxy_is_loopback("LOCALHOST"));
        assert!(!proxy_is_loopback("127.0.0.256"));
        assert!(!proxy_is_loopback("10.0.0.1"));
        assert!(valid_proxy_host("fe80::1%en0"));
        assert!(!valid_proxy_host("a b"));
    }

    #[test]
    fn deployment_targets_from_the_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("project.pbxproj"),
            "IPHONEOS_DEPLOYMENT_TARGET = 14.0;\n\t\tIPHONEOS_DEPLOYMENT_TARGET = \"16.4\";\nIPHONEOS_DEPLOYMENT_TARGET = bad;\n",
        )
        .unwrap();
        assert_eq!(
            project_deployment_targets(dir.path()),
            Some(("14.0".into(), "16.4".into()))
        );
    }

    #[test]
    fn source_hash_is_stable_and_content_keyed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("IPhoneUseRunner");
        std::fs::create_dir_all(root.join("a/xcuserdata")).unwrap();
        std::fs::write(root.join("a/one.swift"), "1").unwrap();
        std::fs::write(root.join("two.swift"), "2").unwrap();
        std::fs::write(root.join(".hidden"), "x").unwrap();
        std::fs::write(root.join("a/xcuserdata/state"), "x").unwrap();
        let first = runner_source_hash(dir.path()).unwrap();
        std::fs::write(root.join(".hidden"), "y").unwrap();
        std::fs::write(root.join("a/xcuserdata/state"), "y").unwrap();
        assert_eq!(
            runner_source_hash(dir.path()).unwrap(),
            first,
            "ignored files do not count"
        );
        std::fs::write(root.join("two.swift"), "3").unwrap();
        assert_ne!(runner_source_hash(dir.path()).unwrap(), first);
    }

    /// The hash must equal what `setup-wda.sh` computed, or an upgrade would
    /// throw away every recorded runner product.
    #[test]
    fn source_hash_matches_the_python_definition() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("IPhoneUseRunner");
        std::fs::create_dir_all(root.join("Sub")).unwrap();
        std::fs::write(root.join("Sub/b.swift"), "bee").unwrap();
        std::fs::write(root.join("a.plist"), "<x/>").unwrap();
        let script = r#"
import hashlib, os, sys
root = sys.argv[1]
digest = hashlib.sha256()
entries = []
for directory, dirs, files in os.walk(root, followlinks=False):
    dirs[:] = sorted(d for d in dirs if d != "xcuserdata" and not d.startswith("."))
    for name in files:
        if name.startswith("."):
            continue
        full = os.path.join(directory, name)
        if os.path.islink(full) or not os.path.isfile(full):
            continue
        entries.append(os.path.relpath(full, root))
for relative in sorted(entries):
    digest.update(relative.encode() + b"\0")
    with open(os.path.join(root, relative), "rb") as handle:
        digest.update(hashlib.sha256(handle.read()).digest())
print(digest.hexdigest())
"#;
        let Ok(out) = std::process::Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(&root)
            .output()
        else {
            return;
        };
        if !out.status.success() {
            return;
        }
        let expected = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(runner_source_hash(dir.path()).unwrap(), expected);
    }

    /// A fake tool on disk, executable.
    fn fake_tool(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    static PREFLIGHT_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn warp_preflight_with_a_fake_client() {
        let _env = PREFLIGHT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            ("Status update: Disconnected", "Mode: Warp", "", true),
            (
                "Status update: Connected",
                "Mode: Warp",
                "Excluded:\n  fe80::/10\n  fd00::/8\n",
                true,
            ),
            (
                "Status update: Connected",
                "Mode: Warp",
                "Excluded:\n  fe80::/10\n",
                false,
            ),
            (
                "Status update: Connected",
                "Mode: WarpProxy on port 40000",
                "",
                true,
            ),
        ];
        for (status, settings, dump, passes) in cases {
            let cli = fake_tool(
                dir.path(),
                "warp-cli",
                &format!(
                    "case \"$1\" in status) echo '{status}';; settings) echo '{settings}';; tunnel) printf '{dump}';; esac"
                ),
            );
            std::env::set_var("IPHONE_USE_INTERNAL_TEST_WARP_CLI", &cli);
            assert_eq!(
                warp_preflight().is_ok(),
                passes,
                "{status} / {settings} / {dump:?}"
            );
        }
        std::env::remove_var("IPHONE_USE_INTERNAL_TEST_WARP_CLI");
    }

    #[test]
    fn proxy_check_with_a_fake_scutil_and_probe() {
        let _env = PREFLIGHT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let none = fake_tool(
            dir.path(),
            "scutil-none",
            "printf '<dictionary> {\\n  FTPPassive : 1\\n}\\n'",
        );
        let dead = fake_tool(
            dir.path(),
            "scutil-dead",
            "printf '<dictionary> {\\n  HTTPEnable : 1\\n  HTTPProxy : 127.0.0.1\\n  HTTPPort : 7890\\n}\\n'",
        );
        let broken = fake_tool(dir.path(), "scutil-broken", "printf 'garbage\\n'");
        let refused = fake_tool(dir.path(), "probe-refused", "exit 1");
        let answers = fake_tool(dir.path(), "probe-answers", "exit 0");
        std::env::set_var("IPHONE_USE_INTERNAL_TEST_SCUTIL", &none);
        assert!(system_proxy_check().is_ok(), "no enabled proxy");
        std::env::set_var("IPHONE_USE_INTERNAL_TEST_SCUTIL", &dead);
        std::env::set_var("IPHONE_USE_INTERNAL_TEST_PROXY_PROBE", &refused);
        let error = system_proxy_check().unwrap_err();
        assert!(error.contains("HTTP 127.0.0.1:7890"), "{error}");
        std::env::set_var("IPHONE_USE_INTERNAL_TEST_PROXY_PROBE", &answers);
        assert!(
            system_proxy_check().is_ok(),
            "a live loopback proxy only warns"
        );
        std::env::set_var("IPHONE_USE_INTERNAL_TEST_SCUTIL", &broken);
        assert!(system_proxy_check()
            .unwrap_err()
            .contains("Could not inspect"));
        std::env::remove_var("IPHONE_USE_INTERNAL_TEST_SCUTIL");
        std::env::remove_var("IPHONE_USE_INTERNAL_TEST_PROXY_PROBE");
    }

    #[test]
    fn usb_membership_ignores_dashes_and_case() {
        let usb = vec!["000081100002346211A0401E".to_string()];
        assert!(on_usb("00008110-0002346211a0401e", &usb));
        assert!(!on_usb("00008150-000A60EC1A02401C", &usb));
        assert!(!on_usb("", &usb));
    }
}
