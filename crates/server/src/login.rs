//! `POST /agent/login` — sign the phone's foreground app in with the user's own
//! Bitwarden / Vaultwarden entry, the phone twin of `chrome-use auth login
//! --bwu`.
//!
//! The secret path never leaves the daemon. `bwu` (bitwarden-use) runs as a
//! child of the daemon with only an entry id in its argv; the values come back
//! on a pipe and go straight into the phone's fields through the runner.
//! Nothing that leaves the daemon carries them:
//! - the HTTP response names the entry and a masked account only;
//! - nothing is logged from the fill path, and WDA error text is never relayed
//!   (a failed type could echo its argument);
//! - timing records hold the route and byte counts only;
//! - the flow trail is cut at a login (see `http::agent_login`);
//! - secure-field values are masked where every element row is built
//!   (`wda::flatten_node`), so a read after the login cannot echo a password.
//!
//! Second factors follow the chrome-use playbook: a TOTP from the same entry
//! is filled automatically; SMS and email codes go through `message-use` /
//! `mail-use` in a separate, bounded call (`POST /agent/login/code`), at most
//! twice per login.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::wda::{ElementRow, WdaClient};

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// A credential value. Zeroed on drop; its `Debug` never prints it.
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Secret(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: zero bytes are valid UTF-8, so the String stays well-formed.
        unsafe { self.0.as_mut_vec().fill(0) };
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// What a vault entry yields for a login.
#[derive(Debug)]
pub struct Credentials {
    pub username: Option<Secret>,
    pub password: Option<Secret>,
    pub has_totp: bool,
}

/// A vault entry as `bwu list --raw` describes it: no secret in here.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct VaultEntry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub uris: Vec<Value>,
}

impl VaultEntry {
    fn uri_strings(&self) -> impl Iterator<Item = &str> {
        self.uris.iter().filter_map(|uri| match uri {
            Value::String(s) => Some(s.as_str()),
            Value::Object(o) => o.get("uri").and_then(Value::as_str),
            _ => None,
        })
    }

    /// The entry as a response may show it: its name and a masked account.
    pub fn public(&self) -> Value {
        json!({
            "name": self.name,
            "account": self.user.as_deref().map(mask_account),
        })
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A refusal or failure, ready to become the JSON body. The hint never names a
/// flag that bypasses a check.
#[derive(Debug)]
pub struct LoginError {
    pub status: u16,
    pub error: &'static str,
    pub hint: String,
    pub extra: Value,
}

impl LoginError {
    fn new(status: u16, error: &'static str, hint: impl Into<String>) -> Self {
        LoginError {
            status,
            error,
            hint: hint.into(),
            extra: json!({}),
        }
    }

    fn with(mut self, key: &str, value: Value) -> Self {
        if let Value::Object(map) = &mut self.extra {
            map.insert(key.to_string(), value);
        }
        self
    }

    pub fn body(&self) -> Value {
        let mut body = json!({ "ok": false, "error": self.error, "hint": self.hint });
        if let (Value::Object(body), Value::Object(extra)) = (&mut body, &self.extra) {
            for (key, value) in extra {
                body.insert(key.clone(), value.clone());
            }
        }
        body
    }
}

fn phone_error(step: &str) -> LoginError {
    // The WDA error text is deliberately dropped: a failed type can quote what
    // it was asked to type.
    LoginError::new(
        502,
        "phone_error",
        format!("the phone did not complete the {step}; read the screen before trying again"),
    )
}

// ---------------------------------------------------------------------------
// Tools: bwu, message-use, mail-use
// ---------------------------------------------------------------------------

/// A launchd daemon starts with a bare PATH, so look where the installers put
/// these tools as well. `override_var` points at a specific binary (tests).
fn find_tool(names: &[&str], override_var: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(override_var) {
        let path = PathBuf::from(path);
        return is_executable(&path).then_some(path);
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    names
        .iter()
        .flat_map(|name| dirs.iter().map(move |dir| dir.join(name)))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

async fn run_tool(
    program: &Path,
    args: &[&str],
    timeout: Duration,
) -> Option<std::process::Output> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .ok()?
        .ok()
}

/// An id that `bwu` will read as an id, never as an option.
fn safe_id(id: &str) -> bool {
    id.len() <= 64
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The password vault, through bitwarden-use.
pub struct Vault {
    bwu: PathBuf,
}

impl Vault {
    pub fn locate() -> Result<Vault, LoginError> {
        find_tool(&["bwu", "bitwarden-use"], "IPHONE_USE_BWU")
            .map(|bwu| Vault { bwu })
            .ok_or_else(|| {
                LoginError::new(
                    503,
                    "vault_unavailable",
                    "bitwarden-use is not installed on this Mac: curl -fsSL \
                     https://raw.githubusercontent.com/leeguooooo/bitwarden-use/main/install.sh | sh, \
                     then `bwu login`",
                )
            })
    }

    /// Unlock without a prompt when the keychain holds the master password;
    /// bitwarden-use falls back to its own dialog on the Mac otherwise.
    async fn ensure_unlocked(&self) -> Result<(), LoginError> {
        let unlocked = |output: Option<std::process::Output>| {
            output.is_some_and(|output| output.status.success())
        };
        if unlocked(run_tool(&self.bwu, &["unlocked"], Duration::from_secs(10)).await) {
            return Ok(());
        }
        let _ = run_tool(&self.bwu, &["unlock"], Duration::from_secs(90)).await;
        if unlocked(run_tool(&self.bwu, &["unlocked"], Duration::from_secs(10)).await) {
            return Ok(());
        }
        Err(LoginError::new(
            503,
            "vault_locked",
            "the password vault on the Mac is locked: ask the user to unlock it (`bwu unlock`), then try again",
        ))
    }

    pub async fn entries(&self) -> Result<Vec<VaultEntry>, LoginError> {
        self.ensure_unlocked().await?;
        let output = run_tool(&self.bwu, &["list", "--raw"], Duration::from_secs(30))
            .await
            .filter(|output| output.status.success())
            .ok_or_else(|| {
                LoginError::new(
                    503,
                    "vault_unavailable",
                    "bitwarden-use could not list the vault",
                )
            })?;
        serde_json::from_slice(&output.stdout).map_err(|_| {
            LoginError::new(
                503,
                "vault_unavailable",
                "bitwarden-use returned an unreadable list",
            )
        })
    }

    pub async fn credentials(&self, id: &str) -> Result<Credentials, LoginError> {
        if !safe_id(id) {
            return Err(LoginError::new(
                500,
                "vault_unavailable",
                "unexpected vault entry id",
            ));
        }
        let mut output = run_tool(
            &self.bwu,
            &["get", "--raw", "--reveal", id],
            Duration::from_secs(30),
        )
        .await
        .filter(|output| output.status.success())
        .ok_or_else(|| {
            LoginError::new(
                503,
                "vault_unavailable",
                "bitwarden-use could not read the entry",
            )
        })?;
        let parsed = parse_credentials(&output.stdout);
        output.stdout.fill(0);
        parsed.ok_or_else(|| {
            LoginError::new(
                503,
                "vault_unavailable",
                "bitwarden-use returned an unreadable entry",
            )
        })
    }

    pub async fn totp(&self, id: &str) -> Result<Secret, LoginError> {
        if !safe_id(id) {
            return Err(LoginError::new(
                500,
                "vault_unavailable",
                "unexpected vault entry id",
            ));
        }
        let mut output = run_tool(&self.bwu, &["code", id], Duration::from_secs(20))
            .await
            .filter(|output| output.status.success())
            .ok_or_else(|| {
                LoginError::new(
                    503,
                    "code_unavailable",
                    "bitwarden-use could not produce the authenticator code",
                )
            })?;
        let code: String = String::from_utf8_lossy(&output.stdout).trim().to_string();
        output.stdout.fill(0);
        if code.is_empty() {
            return Err(LoginError::new(
                503,
                "code_unavailable",
                "bitwarden-use could not produce the authenticator code",
            ));
        }
        Ok(Secret::new(code))
    }
}

/// `bwu get --raw --reveal` → the login's username, password and whether it
/// carries a TOTP seed (the seed itself is never kept).
pub fn parse_credentials(stdout: &[u8]) -> Option<Credentials> {
    let value: Value = serde_json::from_slice(stdout).ok()?;
    let data = value.get("data")?;
    let take = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| Secret::new(s.to_string()))
    };
    Some(Credentials {
        username: take("username"),
        password: take("password"),
        has_totp: data
            .get("totp")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
    })
}

// ---------------------------------------------------------------------------
// Matching the foreground app to a vault entry
// ---------------------------------------------------------------------------

const TLDS: &[&str] = &[
    "com", "net", "org", "io", "app", "co", "cn", "jp", "me", "tv", "info", "biz", "dev", "ai",
    "us", "uk", "de", "fr", "kr", "tw", "hk", "sg",
];
const SECOND_LEVEL: &[&str] = &["co", "ne", "or", "ac", "com", "net", "org", "go"];

/// Web hosts a bundle id implies: `com.taobao.taobao4iphone` → `taobao.com`,
/// `jp.co.rakuten.mobile` → `rakuten.co.jp`.
pub fn bundle_hosts(bundle: &str) -> Vec<String> {
    let parts: Vec<String> = bundle.split('.').map(str::to_ascii_lowercase).collect();
    let mut hosts = Vec::new();
    if parts.len() >= 3
        && SECOND_LEVEL.contains(&parts[1].as_str())
        && TLDS.contains(&parts[0].as_str())
        && parts[0] != "com"
    {
        // `co.jp` alone would match every Japanese company's site.
        hosts.push(format!("{}.{}.{}", parts[2], parts[1], parts[0]));
    } else if parts.len() >= 2 && TLDS.contains(&parts[0].as_str()) && !parts[1].is_empty() {
        hosts.push(format!("{}.{}", parts[1], parts[0]));
    }
    hosts
}

/// The host of a vault URI, without scheme, port, path or `www.`.
pub fn uri_host(uri: &str) -> Option<String> {
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let host = rest
        .split(['/', '?', '#'])
        .next()?
        .rsplit('@')
        .next()?
        .split(':')
        .next()?
        .trim()
        .to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    (!host.is_empty()).then_some(host)
}

fn host_matches(entry_host: &str, app_host: &str) -> bool {
    entry_host == app_host || entry_host.ends_with(&format!(".{app_host}"))
}

/// Vault entries for the app in front, best evidence first: an `iosapp://`
/// URI naming the bundle, then a web host the bundle implies, then the app's
/// display name in the entry name. Only the strongest non-empty tier counts.
pub fn candidates<'a>(
    entries: &'a [VaultEntry],
    bundle: Option<&str>,
    app_name: &str,
    extra_hosts: &[String],
) -> Vec<&'a VaultEntry> {
    let mut tiers: [Vec<&VaultEntry>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut hosts = bundle.map(bundle_hosts).unwrap_or_default();
    hosts.extend(extra_hosts.iter().cloned());
    let ios_uri = bundle.map(|b| format!("iosapp://{}", b.to_ascii_lowercase()));
    let app = app_name.trim().to_lowercase();
    for entry in entries {
        let uris: Vec<String> = entry.uri_strings().map(str::to_ascii_lowercase).collect();
        if ios_uri
            .as_ref()
            .is_some_and(|wanted| uris.iter().any(|u| u == wanted))
        {
            tiers[0].push(entry);
        } else if uris
            .iter()
            .filter_map(|u| uri_host(u))
            .any(|host| hosts.iter().any(|app_host| host_matches(&host, app_host)))
        {
            tiers[1].push(entry);
        } else if app.chars().count() >= 2 {
            let name = entry.name.trim().to_lowercase();
            if name.contains(&app) || (name.chars().count() >= 3 && app.contains(&name)) {
                tiers[2].push(entry);
            }
        }
    }
    tiers
        .into_iter()
        .find(|tier| !tier.is_empty())
        .unwrap_or_default()
}

/// The entry an explicit `item` (id or name) and optional `user` name.
pub fn by_item<'a>(
    entries: &'a [VaultEntry],
    item: &str,
    user: Option<&str>,
) -> Vec<&'a VaultEntry> {
    let item = item.trim();
    if let Some(entry) = entries.iter().find(|e| e.id == item) {
        return vec![entry];
    }
    let wanted = item.to_lowercase();
    entries
        .iter()
        .filter(|e| e.name.trim().to_lowercase() == wanted)
        .filter(|e| match user {
            Some(user) => e
                .user
                .as_deref()
                .is_some_and(|u| u.eq_ignore_ascii_case(user.trim())),
            None => true,
        })
        .collect()
}

/// `leeguoo@qq.com` → `le***@qq.com`; `18612130974` → `186***74`.
pub fn mask_account(user: &str) -> String {
    let user = user.trim();
    if let Some((local, domain)) = user.split_once('@') {
        let head: String = local.chars().take(2).collect();
        return format!("{head}***@{domain}");
    }
    let count = user.chars().count();
    if count > 5 {
        let head: String = user.chars().take(3).collect();
        let tail: String = user.chars().skip(count - 2).collect();
        format!("{head}***{tail}")
    } else {
        "***".to_string()
    }
}

// ---------------------------------------------------------------------------
// Reading the login form
// ---------------------------------------------------------------------------

const ACCOUNT_HINTS: &[&str] = &[
    "user",
    "email",
    "e-mail",
    "mail",
    "phone",
    "mobile",
    "account",
    "login",
    "账号",
    "帐号",
    "账户",
    "用户名",
    "手机",
    "邮箱",
    "电话",
    "ユーザー",
    "メール",
    "アカウント",
    "電話",
    "携帯",
];
const CODE_HINTS: &[&str] = &[
    "code",
    "otp",
    "verification",
    "one-time",
    "验证码",
    "校验码",
    "动态码",
    "認証",
    "コード",
];
/// Buttons that sign in or move a two-step form on. Exact matches win.
const LOGIN_VERBS: &[&str] = &[
    "登录",
    "登入",
    "立即登录",
    "登录/注册",
    "登录 / 注册",
    "log in",
    "login",
    "sign in",
    "ログイン",
    "サインイン",
    "continue",
    "继续",
    "下一步",
    "next",
    "次へ",
];
const VERIFY_VERBS: &[&str] = &[
    "验证",
    "确认",
    "确定",
    "提交",
    "完成",
    "verify",
    "confirm",
    "submit",
    "done",
    "continue",
    "继续",
    "下一步",
    "next",
    "次へ",
    "確認",
    "登录",
    "log in",
    "sign in",
    "ログイン",
];
/// Words that make a button something other than signing in.
const NOT_LOGIN: &[&str] = &[
    "注册", "sign up", "signup", "create", "register", "新規", "忘记", "forgot", "忘れ", "reset",
    "重置", "其他", "other", "apple", "google", "wechat", "微信", "qq",
];

fn usable(row: &ElementRow) -> bool {
    row.visible != Some(false)
        && row.enabled != Some(false)
        && row.rect[2] > 0.0
        && row.rect[3] > 0.0
}

fn describe(row: &ElementRow) -> String {
    format!(
        "{} {} {}",
        row.label,
        row.placeholder.as_deref().unwrap_or(""),
        row.identifier.as_deref().unwrap_or("")
    )
    .to_lowercase()
}

const PHONE_HINTS: &[&str] = &["phone", "mobile", "tel", "手机", "电话", "電話", "携帯"];
/// Words that make an account field also take something other than a number
/// (`手机号/邮箱`, "Email or phone").
const NOT_ONLY_PHONE_HINTS: &[&str] = &[
    "mail", "user", "account", "邮箱", "用户名", "账号", "帐号", "账户", "メール", "ユーザー", "アカウント",
];

/// An account field that takes a phone number and nothing else.
fn phone_only_field(row: &ElementRow) -> bool {
    let text = describe(row);
    PHONE_HINTS.iter().any(|hint| text.contains(hint))
        && !NOT_ONLY_PHONE_HINTS.iter().any(|hint| text.contains(hint))
}

/// Digits, with an optional leading `+` and the usual separators.
fn phone_shaped(value: &str) -> bool {
    let digits: String = value
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
        .collect();
    let digits = digits.strip_prefix('+').unwrap_or(&digits);
    digits.len() >= 5 && digits.chars().all(|c| c.is_ascii_digit())
}

fn mentions(text: &str, hints: &[&str]) -> bool {
    hints.iter().any(|hint| text.contains(hint))
        || text
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| word == "id")
            && hints == ACCOUNT_HINTS
}

/// Where the login form's fields are on this screen.
#[derive(Debug, Default, PartialEq)]
pub struct Form {
    pub account: Option<usize>,
    pub password: Option<usize>,
    pub code: Option<usize>,
    /// More than one password field means sign-up or a password change.
    pub secure_fields: usize,
}

const TOKEN_HINTS: &[&str] = &[
    "token", "bearer", "api key", "api-key", "apikey", "secret key", "access key", "密钥", "令牌",
];

/// A secure field for pasting an access token, not a password (hardware: an
/// admin login with "或直接粘贴 Bearer Token" under the password). An empty web
/// input reports its placeholder as its value, so that is read too; a secure
/// field's typed value reads as bullets and carries no words.
fn token_field(row: &ElementRow) -> bool {
    let mut text = describe(row);
    if let Some(value) = &row.value {
        text.push(' ');
        text.push_str(&value.to_lowercase());
    }
    TOKEN_HINTS.iter().any(|hint| text.contains(hint))
}

pub fn read_form(rows: &[ElementRow]) -> Form {
    let secure: Vec<usize> = (0..rows.len())
        .filter(|&i| {
            rows[i].kind == "SecureTextField" && usable(&rows[i]) && !token_field(&rows[i])
        })
        .collect();
    let text: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].kind == "TextField" && usable(&rows[i]))
        .collect();
    let code = text
        .iter()
        .copied()
        .find(|&i| mentions(&describe(&rows[i]), CODE_HINTS));
    let others: Vec<usize> = text.iter().copied().filter(|&i| Some(i) != code).collect();
    let account = others
        .iter()
        .copied()
        .find(|&i| mentions(&describe(&rows[i]), ACCOUNT_HINTS))
        .or_else(|| match secure.first() {
            // The text field just above the password field.
            Some(&password) => others
                .iter()
                .copied()
                .filter(|&i| rows[i].rect[1] < rows[password].rect[1])
                .max_by(|&a, &b| rows[a].rect[1].total_cmp(&rows[b].rect[1])),
            None if code.is_none() && others.len() == 1 => Some(others[0]),
            None => None,
        });
    Form {
        account,
        password: (secure.len() == 1).then(|| secure[0]),
        code,
        secure_fields: secure.len(),
    }
}

/// The button that submits this form: an exact verb first, then a short
/// label containing one, never anything that signs up, resets or hands the
/// login to another provider.
pub fn submit_button(rows: &[ElementRow], verbs: &[&str]) -> Option<usize> {
    let buttons: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].kind == "Button" && usable(&rows[i]))
        .collect();
    let label = |i: usize| rows[i].label.trim().to_lowercase();
    let exact = buttons
        .iter()
        .copied()
        .filter(|&i| verbs.contains(&label(i).as_str()))
        .max_by(|&a, &b| rows[a].rect[1].total_cmp(&rows[b].rect[1]));
    exact.or_else(|| {
        buttons.iter().copied().find(|&i| {
            let text = label(i);
            text.chars().count() <= 8
                && verbs.iter().any(|verb| text.contains(verb))
                && !NOT_LOGIN.iter().any(|word| text.contains(word))
        })
    })
}

fn code_channel(rows: &[ElementRow]) -> &'static str {
    let text: String = rows.iter().map(describe).collect::<Vec<_>>().join(" ");
    if ["短信", "sms", "text message", "ショートメッセージ"]
        .iter()
        .any(|w| text.contains(w))
    {
        "sms"
    } else if ["邮件", "邮箱", "email", "e-mail", "メール"]
        .iter()
        .any(|w| text.contains(w))
    {
        "mail"
    } else {
        "unknown"
    }
}

// ---------------------------------------------------------------------------
// Driving the phone
// ---------------------------------------------------------------------------

/// The live element for a row, matched by class and frame.
///
/// A web page can shift between the tree read and this lookup (Safari's
/// toolbar settling after a load moved a login form on hardware), so when no
/// frame matches exactly, the one element of the same class and size that
/// moved the least is taken — only when it is unambiguous.
async fn element_for(w: &mut WdaClient, row: &ElementRow) -> Result<String, LoginError> {
    let ids = w
        .find_elements("class chain", &format!("**/XCUIElementType{}", row.kind))
        .await
        .map_err(|_| phone_error("field lookup"))?;
    let mut live = Vec::new();
    for id in ids {
        if let Ok(rect) = w.element_rect(&id).await {
            live.push((id, rect));
        }
    }
    let rects: Vec<[f64; 4]> = live.iter().map(|(_, rect)| *rect).collect();
    match pick_frame(&rects, row.rect) {
        Ok(index) => Ok(live.swap_remove(index).0),
        Err(FrameMatch::Ambiguous) => Err(LoginError::new(
            422,
            "field_ambiguous",
            "two fields share that frame; finish this login by hand",
        )),
        Err(FrameMatch::Missing) => Err(LoginError::new(
            422,
            "field_not_found",
            "the login field moved; read the screen and try again",
        )),
    }
}

#[derive(Debug, PartialEq)]
pub enum FrameMatch {
    Missing,
    Ambiguous,
}

/// Which live frame is the row's: an exact frame (±2 pt), else the single
/// same-size frame within 80 pt, and never a guess between two.
pub fn pick_frame(live: &[[f64; 4]], wanted: [f64; 4]) -> Result<usize, FrameMatch> {
    let close = |a: &[f64; 4], indices: std::ops::Range<usize>| {
        indices.into_iter().all(|i| (a[i] - wanted[i]).abs() <= 2.0)
    };
    let exact: Vec<usize> = (0..live.len()).filter(|&i| close(&live[i], 0..4)).collect();
    match exact.as_slice() {
        [one] => return Ok(*one),
        [_, _, ..] => return Err(FrameMatch::Ambiguous),
        [] => {}
    }
    let moved: Vec<(usize, f64)> = (0..live.len())
        .filter(|&i| close(&live[i], 2..4))
        .map(|i| {
            let dx = live[i][0] - wanted[0];
            let dy = live[i][1] - wanted[1];
            (i, (dx * dx + dy * dy).sqrt())
        })
        .filter(|(_, distance)| *distance <= 80.0)
        .collect();
    match moved.as_slice() {
        [(one, _)] => Ok(*one),
        [] => Err(FrameMatch::Missing),
        _ => Err(FrameMatch::Ambiguous),
    }
}

fn same_text(a: &str, b: &str) -> bool {
    let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    strip(a) == strip(b)
}

/// Whether the field now holds the secret, judged inside the daemon. A text
/// field must read back equal; a password field cannot be read, so it must at
/// least have left its placeholder.
async fn holds(w: &mut WdaClient, id: &str, row: &ElementRow, secret: &Secret) -> bool {
    let Ok(now) = w.element_value(id).await else {
        return false;
    };
    let now = now.unwrap_or_default();
    if row.kind == "SecureTextField" {
        !now.is_empty() && Some(now.as_str()) != row.placeholder.as_deref()
    } else {
        same_text(&now, secret.expose())
    }
}

/// Titles of iOS's own password picker. Tapping a web password field on a phone
/// with saved passwords opens it full-screen (hardware: Safari, iOS 27): the
/// keys typed next went to the field behind it, and a tree read showed only the
/// sheet, so a secret that landed in a plain field could not be seen.
const AUTOFILL_SHEET_TITLES: &[&str] = &[
    "自动填充密码",
    "自動填充密碼",
    "AutoFill Password",
    "AutoFill Passwords",
    "パスワードを自動入力",
];
const SHEET_CLOSE_LABELS: &[&str] = &["取消", "Cancel", "Close", "关闭", "キャンセル"];

/// Buttons of iOS's password suggestion sheet, the bottom-sheet variant that
/// offers one saved login ("通过 Bitwarden 中已存 … 密码登录", 关闭 / 填充密码).
/// Hardware, 17 Pro Max: it opened over GitHub's web sign-in and hid the form.
const SHEET_FILL_LABELS: &[&str] = &[
    "填充密码",
    "填入密碼",
    "Fill Password",
    "Use Password",
    "パスワードを入力",
];

/// A system password sheet is on screen: the full-screen picker (by its
/// title) or the suggestion sheet (by its fill button).
fn autofill_sheet_up(rows: &[ElementRow]) -> bool {
    rows.iter().any(|r| {
        (matches!(r.kind.as_str(), "NavigationBar" | "StaticText")
            && AUTOFILL_SHEET_TITLES.contains(&r.label.trim()))
            || (r.kind == "Button" && SHEET_FILL_LABELS.contains(&r.label.trim()))
    })
}

/// The close button of the system password sheet when one is up.
fn autofill_sheet_close(rows: &[ElementRow]) -> Option<usize> {
    if !autofill_sheet_up(rows) {
        return None;
    }
    let is_close =
        |r: &ElementRow| r.kind == "Button" && SHEET_CLOSE_LABELS.contains(&r.label.trim());
    // The full-screen picker's close button sits in its title bar, at the top.
    if let Some(top) = rows.iter().position(|r| is_close(r) && r.rect[1] < 220.0) {
        return Some(top);
    }
    // The suggestion sheet's sits beside its fill button, lower down.
    let fill_y = rows
        .iter()
        .find(|r| r.kind == "Button" && SHEET_FILL_LABELS.contains(&r.label.trim()))?
        .rect[1];
    rows.iter()
        .position(|r| is_close(r) && (r.rect[1] - fill_y).abs() < 160.0)
}

/// Close the system password sheet if it is up. `Ok(true)` when the screen is
/// clear of it; `Ok(false)` when it is still there after trying.
async fn close_autofill_sheet(w: &mut WdaClient) -> Result<bool, LoginError> {
    for _ in 0..2 {
        let rows = w.elements().await.map_err(|_| phone_error("screen read"))?;
        if !autofill_sheet_up(&rows) {
            return Ok(true);
        }
        let Some(close) = autofill_sheet_close(&rows) else {
            return Ok(false);
        };
        // A click on the element acknowledged and left the sheet up on
        // hardware; a tap on its frame closes it.
        tap_row(w, &rows[close]).await?;
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    let rows = w.elements().await.map_err(|_| phone_error("screen read"))?;
    Ok(!autofill_sheet_up(&rows))
}

/// The current frame of `row` in a fresh read: the row of the same kind and
/// label closest to where it was (a zoom moves every field), when its frame is
/// usable.
fn live_frame(rows: &[ElementRow], row: &ElementRow) -> Option<[f64; 4]> {
    let centre = |r: &[f64; 4]| (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
    let (ox, oy) = centre(&row.rect);
    rows.iter()
        .filter(|r| {
            r.kind == row.kind && r.label == row.label && r.rect[2] > 0.0 && r.rect[3] > 0.0
        })
        .min_by(|a, b| {
            let d = |r: &ElementRow| {
                let (x, y) = centre(&r.rect);
                (x - ox).powi(2) + (y - oy).powi(2)
            };
            d(a).total_cmp(&d(b))
        })
        .map(|r| r.rect)
}

/// Replace a field's contents with a secret; nothing read is returned.
///
/// The direct element write comes first: native fields take it. Web inputs
/// (WKWebView) acknowledge it and keep their contents (hardware: Safari on
/// iOS 27, and issue #70's bank form), so the fallback focuses the field like
/// a person and types into the focus. Web inputs also report `focused` for
/// every field, so focus cannot be confirmed up front: instead every other
/// text field is checked afterwards, and a value that landed in one is
/// cleared at once.
async fn fill(
    w: &mut WdaClient,
    row: &ElementRow,
    secret: &Secret,
    what: &'static str,
) -> Result<(), LoginError> {
    let id = element_for(w, row).await?;
    let _ = w.clear_element(&id).await;
    if w.type_into(&id, secret.expose()).await.is_ok() && holds(w, &id, row, secret).await {
        return Ok(());
    }
    let _ = w.clear_element(&id).await;
    // Tap the element where it is now, not where the tree read saw it: a page
    // that shifted in between turned a frame tap into a tap on another field.
    // A pointer tap at its live frame, not an element click: in a web sign-in
    // sheet (SafariViewService) the element click acknowledged without moving
    // focus, so the password was typed into the account field (hardware, 17
    // Pro Max, GitHub). The element click is only the fallback.
    // Where the field is now comes from a fresh tree read (same kind and label,
    // nearest to where it was): in the sheet, element_rect disagreed with the
    // tree after Safari zoomed, and a tap there focused the other field.
    let live = match w.elements().await {
        Ok(rows) => live_frame(&rows, row),
        Err(_) => None,
    };
    let focused = match live {
        Some([x, y, width, height]) => w.tap_point(x + width / 2.0, y + height / 2.0).await.is_ok(),
        None => false,
    };
    if !focused {
        w.click_element(&id)
            .await
            .map_err(|_| phone_error(&format!("{what} focus")))?;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    // Never type while the system password sheet covers the page: the keys go
    // to whatever field was focused before, in clear text.
    if !close_autofill_sheet(w).await? {
        return Err(LoginError::new(
            409,
            "autofill_sheet",
            "iOS's password picker opened over the form and would not close; nothing was typed. Close it on the phone and try again",
        ));
    }
    w.keys(secret.expose())
        .await
        .map_err(|_| phone_error(&format!("{what} entry")))?;
    // A sheet that opened while typing would hide the page from the check below.
    let _ = close_autofill_sheet(w).await;
    let landed = holds(w, &id, row, secret).await;
    // Anywhere else it may have gone: a plain field showing it in clear text.
    // The target is told apart by where it is now (element ids are handed out
    // per lookup, so they cannot be compared).
    let target_now = w.element_rect(&id).await.ok();
    if let Ok(rows) = w.elements().await {
        for other in rows.iter().filter(|r| {
            matches!(r.kind.as_str(), "TextField" | "SearchField" | "TextView")
                && r.value
                    .as_deref()
                    .is_some_and(|value| value.contains(secret.expose()))
                && !target_now.is_some_and(|now| {
                    now.iter()
                        .zip(r.rect.iter())
                        .all(|(a, b)| (a - b).abs() <= 2.0)
                })
        }) {
            if let Ok(stray) = element_for(w, other).await {
                let _ = w.clear_element(&stray).await;
            }
            return Err(LoginError::new(
                422,
                "value_landed_elsewhere",
                format!(
                    "the {what} went into another field and was cleared; finish this login by hand"
                ),
            ));
        }
    }
    if landed {
        return Ok(());
    }
    let _ = w.clear_element(&id).await;
    Err(LoginError::new(
        422,
        "value_not_applied",
        format!(
            "the {what} field did not take the typed text (a keyboard language that composes \
             letters, or autocorrect); switch the phone's keyboard to English and try again"
        ),
    ))
}

/// Press the form's button the way a person would: the keyboard away first
/// (it can cover the button), then the button found again on a fresh read and
/// clicked where it is now — a tap on the frame an earlier read saw missed it
/// on hardware while Safari's page was still settling. Returns the screen just
/// before the click, to compare the next one against; `None` when there is no
/// such button.
async fn press(w: &mut WdaClient, verbs: &[&str]) -> Result<Option<Vec<ElementRow>>, LoginError> {
    let _ = w.dismiss_keyboard().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rows = w.elements().await.map_err(|_| phone_error("screen read"))?;
    let Some(button) = submit_button(&rows, verbs) else {
        return Ok(None);
    };
    match element_for(w, &rows[button]).await {
        Ok(id) => w.click_element(&id).await.map_err(|_| phone_error("tap"))?,
        Err(_) => tap_row(w, &rows[button]).await?,
    }
    Ok(Some(rows))
}

async fn tap_row(w: &mut WdaClient, row: &ElementRow) -> Result<(), LoginError> {
    let [x, y, width, height] = row.rect;
    w.tap_point(x + width / 2.0, y + height / 2.0)
        .await
        .map_err(|_| phone_error("tap"))
}

fn screen_key(rows: &[ElementRow]) -> Vec<(String, String, [i64; 4])> {
    rows.iter()
        .map(|r| {
            (
                r.kind.clone(),
                r.label.clone(),
                r.rect.map(|v| v.round() as i64),
            )
        })
        .collect()
}

/// Titles of iOS's "save / update this password?" prompt that follows a web
/// sign-in (hardware, 17 Pro Max, GitHub's sheet: 密码 / Bitwarden / 以后).
const SAVE_PROMPT_TITLES: &[&str] = &[
    "保存密码？",
    "更新密码？",
    "儲存密碼？",
    "更新密碼？",
    "Save Password?",
    "Update Password?",
    "パスワードを保存しますか?",
    "パスワードをアップデートしますか?",
];
const LATER_LABELS: &[&str] = &["以后", "稍后", "Not Now", "今はしない", "以後"];

/// The "later" button of a save / update password prompt on screen.
fn save_prompt_later(rows: &[ElementRow]) -> Option<usize> {
    let prompt = rows.iter().any(|r| {
        matches!(r.kind.as_str(), "Alert" | "StaticText")
            && SAVE_PROMPT_TITLES.contains(&r.label.trim())
    });
    if !prompt {
        return None;
    }
    rows.iter()
        .position(|r| r.kind == "Button" && LATER_LABELS.contains(&r.label.trim()))
}

/// Decline iOS's offers to save or update the password after a submit. The
/// vault already holds it, and choosing a password manager there opened its
/// unlock sheet over the page (hardware: Bitwarden asked for its master
/// password). Returns the screen after, when a prompt was declined.
async fn decline_save_password(
    w: &mut WdaClient,
    rows: &[ElementRow],
) -> Result<Option<Vec<ElementRow>>, LoginError> {
    let mut current = rows.to_vec();
    let mut declined = false;
    // Save and update can come one after the other.
    for _ in 0..2 {
        let Some(later) = save_prompt_later(&current) else {
            break;
        };
        tap_row(w, &current[later]).await?;
        declined = true;
        current = next_screen(w, &current, Duration::from_secs(5)).await;
    }
    Ok(declined.then_some(current))
}

/// Only the app's root row: something the runner cannot read covers the
/// screen (a password manager's extension, for one), so nothing on it can be
/// concluded.
fn unreadable(rows: &[ElementRow]) -> bool {
    rows.iter().filter(|r| r.kind != "Application").count() == 0
}

/// Wait for the screen to change after a tap, then for it to hold still.
async fn next_screen(
    w: &mut WdaClient,
    before: &[ElementRow],
    budget: Duration,
) -> Vec<ElementRow> {
    let deadline = Instant::now() + budget;
    let before = screen_key(before);
    let mut last: Option<Vec<ElementRow>> = None;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let Ok(rows) = w.elements().await else {
            continue;
        };
        let key = screen_key(&rows);
        if key != before {
            if last.as_ref().is_some_and(|prev| screen_key(prev) == key) {
                return rows;
            }
            last = Some(rows);
            continue;
        }
        last = Some(rows);
    }
    last.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The login, and its second factor
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    /// Vault entry id or exact name; needed when several entries match.
    #[serde(default)]
    pub item: Option<String>,
    /// Username, to pick between same-named entries.
    #[serde(default)]
    pub user: Option<String>,
    /// Tap the login button after filling (default true).
    #[serde(default)]
    pub submit: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeRequest {
    /// `sms`, `mail`, `totp` or `auto` (SMS, then mail).
    #[serde(default)]
    pub via: Option<String>,
    /// Sender, brand or subject fragment of the message carrying the code.
    #[serde(default)]
    pub from: Option<String>,
    /// How long to wait for a code to arrive (seconds, at most 25).
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

/// The login a code call belongs to.
struct Session {
    started: Instant,
    entry_id: String,
    code_requests: u8,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
const SESSION_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_CODE_REQUESTS: u8 = 2;

/// System view services that present web pages over an app (the in-app
/// Safari and web sign-in sheets). While one is in front, the vault entry
/// belongs to the app presenting it, not to the service.
const VIEW_SERVICES: &[&str] = &["com.apple.SafariViewService"];

/// The app to match vault entries for, from the active apps in order (the
/// one in front first), and whether a web sheet is presenting over it.
/// Hardware, 17 Pro Max: GitHub's web sign-in sheet put
/// `com.apple.SafariViewService` first and GitHub second.
fn presenting_app(active: &[String]) -> (Option<String>, bool) {
    match active.first() {
        Some(first) if VIEW_SERVICES.contains(&first.as_str()) => (
            active
                .iter()
                .skip(1)
                .find(|b| !VIEW_SERVICES.contains(&b.as_str()))
                .cloned(),
            true,
        ),
        first => (first.cloned(), false),
    }
}

/// The page host a web sheet shows in its address bar (top quarter of the
/// screen), e.g. `github.com`: the strongest hint for a web sign-in.
fn sheet_host(rows: &[ElementRow]) -> Option<String> {
    let height = rows
        .iter()
        .find(|r| r.kind == "Application")
        .map(|r| r.rect[3])
        .filter(|h| *h > 0.0)?;
    rows.iter()
        .filter(|r| r.rect[1] < height * 0.25)
        .flat_map(|r| [Some(r.label.as_str()), r.value.as_deref()])
        .flatten()
        .find_map(|text| {
            // Safari's address bar reads "\u{200e}github.com": a bidi mark
            // that trim() keeps (found on hardware), so strip format marks.
            let text: String = text.chars().filter(|c| !is_format_mark(*c)).collect();
            let lower = text.trim().to_ascii_lowercase();
            let looks_like_host = !lower.contains(' ')
                && lower.contains('.')
                && lower
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-/:".contains(c))
                && lower
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .split('/')
                    .next()
                    .and_then(|host| host.rsplit('.').next())
                    .is_some_and(|tld| {
                        tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
                    });
            looks_like_host.then(|| uri_host(&lower)).flatten()
        })
}

/// Invisible bidi and format marks (LRM/RLM, embeddings, isolates, BOM).
fn is_format_mark(c: char) -> bool {
    matches!(
        c,
        '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}'
    )
}

/// Among equally good matches, the entries named after the page host itself
/// (`github.com` beats an entry that merely lists github.com among its URIs,
/// e.g. a token entry for another tool). Unchanged when none or all are.
fn prefer_named_after_host<'a>(
    found: Vec<&'a VaultEntry>,
    hosts: &[String],
) -> Vec<&'a VaultEntry> {
    if found.len() < 2 || hosts.is_empty() {
        return found;
    }
    let named: Vec<&VaultEntry> = found
        .iter()
        .copied()
        .filter(|e| {
            let name = e.name.trim().to_ascii_lowercase();
            let name = name.trim_start_matches("www.");
            hosts.iter().any(|h| h.trim_start_matches("www.") == name)
        })
        .collect();
    if named.is_empty() {
        found
    } else {
        named
    }
}

/// Among equally good matches, entries that hold a username: a site's
/// second entry often keeps only its one-time code (GitHub: one entry with
/// the account and TOTP, one with the TOTP alone).
fn prefer_with_username(found: Vec<&VaultEntry>) -> Vec<&VaultEntry> {
    if found.len() < 2 {
        return found;
    }
    let with_user: Vec<&VaultEntry> = found
        .iter()
        .copied()
        .filter(|e| e.user.as_deref().is_some_and(|u| !u.trim().is_empty()))
        .collect();
    if with_user.is_empty() {
        found
    } else {
        with_user
    }
}

fn pick_entry<'a>(
    entries: &'a [VaultEntry],
    request: &LoginRequest,
    bundle: Option<&str>,
    app_name: &str,
    extra_hosts: &[String],
) -> Result<&'a VaultEntry, LoginError> {
    let matches = match &request.item {
        Some(item) => by_item(entries, item, request.user.as_deref()),
        None => {
            let found = prefer_with_username(prefer_named_after_host(
                candidates(entries, bundle, app_name, extra_hosts),
                extra_hosts,
            ));
            match &request.user {
                Some(user) => found
                    .into_iter()
                    .filter(|e| {
                        e.user
                            .as_deref()
                            .is_some_and(|u| u.eq_ignore_ascii_case(user.trim()))
                    })
                    .collect(),
                None => found,
            }
        }
    };
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(LoginError::new(
            404,
            "no_vault_entry",
            match &request.item {
                Some(_) => "no vault entry has that id or name; ask the user which entry to use".to_string(),
                None => format!(
                    "no vault entry matches {}; ask the user which entry to use and pass its name as item",
                    if app_name.is_empty() { "this app" } else { app_name }
                ),
            },
        )),
        many => Err(LoginError::new(
            409,
            "ambiguous_vault_entry",
            "several vault entries match; ask the user which one, then pass its name as item (and user when names repeat)",
        )
        .with(
            "candidates",
            Value::Array(many.iter().take(10).map(|e| e.public()).collect()),
        )),
    }
}

/// Fill and submit the login form on screen.
pub async fn sign_in(
    w: &mut WdaClient,
    vault: &Vault,
    request: &LoginRequest,
) -> Result<Value, LoginError> {
    // A password sheet iOS opened over the page hides the form from the tree:
    // close it before looking for fields, not only before typing (hardware,
    // 17 Pro Max: GitHub's web sign-in answered no_login_form behind it).
    if !close_autofill_sheet(w).await? {
        return Err(LoginError::new(
            409,
            "autofill_sheet",
            "iOS's password picker is over the login form and would not close; nothing was typed. Close it on the phone and try again",
        ));
    }
    let mut rows = w.elements().await.map_err(|_| phone_error("screen read"))?;
    let app_name = rows
        .iter()
        .find(|r| r.kind == "Application")
        .map(|r| r.label.clone())
        .unwrap_or_default();
    let active = w.active_bundles().await.unwrap_or_default();
    let (bundle, web_sheet) = presenting_app(&active);
    let page_host = if web_sheet { sheet_host(&rows) } else { None };
    let matched = matched_app(bundle.as_deref(), web_sheet, page_host.as_deref());
    let mut form = read_form(&rows);
    if form.secure_fields >= 2 {
        return Err(not_a_login_form());
    }
    if form.account.is_none() && form.password.is_none() {
        return Err(if form.code.is_some() {
            LoginError::new(
                409,
                "code_screen",
                "this screen asks for a verification code: use POST /agent/login/code (phone_login with code_via)",
            )
        } else {
            LoginError::new(
                422,
                "no_login_form",
                "no account or password field on this screen; open the app's login page first",
            )
        });
    }

    let entries = vault.entries().await?;
    let entry = pick_entry(
        &entries,
        request,
        bundle.as_deref(),
        &app_name,
        page_host.as_slice(),
    )
    .map_err(|e| e.with("matched_app", matched.clone()))?
    .clone();
    drop(entries);
    let credentials = vault.credentials(&entry.id).await?;
    let submit = request.submit.unwrap_or(true);
    let mut filled: Vec<&str> = Vec::new();

    if let (Some(index), Some(username)) = (form.account, &credentials.username) {
        // Found on hardware: a site username typed into a phone-number field
        // loses its letters, and the failure then read as a keyboard problem.
        if phone_only_field(&rows[index]) && !phone_shaped(username.expose()) {
            return Err(LoginError::new(
                422,
                "account_not_a_phone_number",
                "this app's account field takes a phone number, but the vault entry's username is not one; nothing was typed. Pick an entry that holds the phone number, or use another way to sign in that the app offers",
            ));
        }
        fill(w, &rows[index], username, "account").await?;
        filled.push("account");
        if form.password.is_some() {
            // Focusing a small-font web input zooms Safari (hardware: an admin
            // login), which moves and resizes the password field. Find it again
            // by its role on a fresh read, not by the frame it had before.
            rows = w.elements().await.map_err(|_| phone_error("screen read"))?;
            form = read_form(&rows);
            if form.secure_fields >= 2 {
                return Err(not_a_login_form());
            }
        }
    }
    if form.password.is_none() && form.code.is_some() {
        if filled.is_empty() {
            return Err(LoginError::new(
                422,
                "nothing_to_fill",
                "the vault entry has no username for the account field on screen",
            ));
        }
        // Account and one-time code on one screen (an SMS or email login): there
        // is no password page behind a Next button. Pressing Log in now would
        // submit an empty code, and apps that sign up unknown numbers on first
        // login (found on hardware) could create an account. Stop after the
        // account: the caller sends the code with the app's own button, then
        // calls POST /agent/login/code, which needs this session.
        remember_session(&entry);
        return Ok(done(
            &entry,
            &matched,
            &filled,
            false,
            &rows,
            Some(code_channel(&rows)),
            CODE_LOGIN_HINT,
        ));
    }
    if form.password.is_none() {
        // A two-step login: the account page first.
        if !submit {
            return Ok(done(
                &entry,
                &matched,
                &filled,
                false,
                &rows,
                None,
                "filled the account; the password page comes after Next",
            ));
        }
        let Some(before) = press(w, LOGIN_VERBS).await? else {
            return Ok(done(
                &entry,
                &matched,
                &filled,
                false,
                &rows,
                None,
                "filled the account but found no Next or Log in button; read the screen",
            ));
        };
        rows = next_screen(w, &before, Duration::from_secs(6)).await;
        form = read_form(&rows);
        if form.secure_fields >= 2 {
            return Err(not_a_login_form());
        }
    }
    let mut submitted = false;
    if let (Some(index), Some(password)) = (form.password, &credentials.password) {
        fill(w, &rows[index], password, "password").await?;
        filled.push("password");
        if submit {
            let before = match press(w, LOGIN_VERBS).await? {
                Some(before) => before,
                None => {
                    // No button: Return submits most forms.
                    w.named_key("return")
                        .await
                        .map_err(|_| phone_error("submit"))?;
                    rows.clone()
                }
            };
            submitted = true;
            rows = next_screen(w, &before, Duration::from_secs(8)).await;
            if let Some(after) = decline_save_password(w, &rows).await? {
                rows = after;
            }
        }
    }
    if filled.is_empty() {
        return Err(LoginError::new(
            422,
            "nothing_to_fill",
            "the vault entry has no username or password for the fields on screen",
        ));
    }

    remember_session(&entry);

    // A second factor, when the app asks for one.
    let mut needs_code = None;
    if submitted {
        let after = read_form(&rows);
        if after.code.is_some() && after.password.is_none() {
            if credentials.has_totp {
                let code = vault.totp(&entry.id).await?;
                if let Some(index) = after.code {
                    fill(w, &rows[index], &code, "verification code").await?;
                    filled.push("one_time_code");
                    if let Some(before) = press(w, VERIFY_VERBS).await? {
                        rows = next_screen(w, &before, Duration::from_secs(8)).await;
                    }
                }
            } else {
                needs_code = Some(code_channel(&rows));
            }
        }
    }
    drop(credentials);
    let hint = match needs_code {
        Some(_) => "the app asks for a verification code: call POST /agent/login/code with via sms or mail (and from: the sender) right after it was sent",
        None => "read the screen to confirm the login went through",
    };
    Ok(done(
        &entry, &matched, &filled, submitted, &rows, needs_code, hint,
    ))
}

const CODE_LOGIN_HINT: &str = "this login uses a one-time code instead of a password: the account is filled and nothing was submitted; send the code with the app's own button (it may sign up a number it does not know — check the screen's wording with the user first), then call POST /agent/login/code";

/// The login in progress, for a follow-up `POST /agent/login/code`.
fn remember_session(entry: &VaultEntry) {
    *SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(Session {
        started: Instant::now(),
        entry_id: entry.id.clone(),
        code_requests: 0,
    });
}

fn not_a_login_form() -> LoginError {
    LoginError::new(
        422,
        "not_a_login_form",
        "this screen has more than one password field (sign-up or a password change); only an existing account's login is filled",
    )
}

/// Which app the vault entry was matched for, reported with the result: the
/// app presenting a web sheet stands in for the sheet's view service.
fn matched_app(bundle: Option<&str>, web_sheet: bool, sheet_host: Option<&str>) -> Value {
    json!({ "bundle": bundle, "web_sheet": web_sheet, "sheet_host": sheet_host })
}

fn done(
    entry: &VaultEntry,
    matched: &Value,
    filled: &[&str],
    submitted: bool,
    rows: &[ElementRow],
    needs_code: Option<&str>,
    hint: &str,
) -> Value {
    let after = read_form(rows);
    // A screen the runner cannot read proves nothing either way (hardware: a
    // password manager's sheet hid the form and it read as "gone").
    let blind = submitted && unreadable(rows);
    let still: Value = if blind {
        Value::Null
    } else {
        json!(submitted && after.password.is_some())
    };
    json!({
        "ok": true,
        "entry": entry.name,
        "matched_app": matched,
        "account": entry.user.as_deref().map(mask_account),
        "filled": filled,
        "submitted": submitted,
        "login_form_still_visible": still,
        "screen_unreadable": blind,
        "needs_code": needs_code.map(|via| json!({ "via": via })),
        "hint": if blind {
            "the screen could not be read after the submit (a system or password-manager sheet may cover it): take a screenshot before assuming the login went through"
        } else if submitted && after.password.is_some() {
            "the login form is still on screen: read it for an error (wrong password, captcha) and tell the user"
        } else {
            hint
        },
    })
}

/// Fetch an SMS / email / authenticator code for the login in progress and
/// enter it.
pub async fn enter_code(w: &mut WdaClient, request: &CodeRequest) -> Result<Value, LoginError> {
    let (entry_id, elapsed) = {
        let mut session = SESSION.lock().unwrap_or_else(|e| e.into_inner());
        let Some(current) = session
            .as_mut()
            .filter(|s| s.started.elapsed() < SESSION_TTL)
        else {
            return Err(LoginError::new(
                409,
                "no_login_in_progress",
                "no recent login to finish; run the login first",
            ));
        };
        if current.code_requests >= MAX_CODE_REQUESTS {
            return Err(LoginError::new(
                429,
                "too_many_code_requests",
                "two codes were already requested for this login; ask the user before asking the app for another (repeat requests lock accounts)",
            ));
        }
        current.code_requests += 1;
        (current.entry_id.clone(), current.started.elapsed())
    };
    let rows = w.elements().await.map_err(|_| phone_error("screen read"))?;
    let form = read_form(&rows);
    let Some(index) = form.code else {
        return Err(LoginError::new(
            422,
            "no_code_field",
            "no verification-code field on this screen; read the screen",
        ));
    };
    if let Some(from) = &request.from {
        if from.starts_with('-') || from.len() > 80 {
            return Err(LoginError::new(
                400,
                "invalid_request",
                "from must be a sender, brand or subject fragment",
            ));
        }
    }
    let via = request.via.as_deref().unwrap_or("auto");
    let wait = Duration::from_secs(request.wait_secs.unwrap_or(20).min(25));
    let code = match via {
        "totp" => Vault::locate()?.totp(&entry_id).await?,
        "sms" | "mail" | "auto" => {
            wait_for_code(via, request.from.as_deref(), elapsed, wait).await?
        }
        _ => {
            return Err(LoginError::new(
                400,
                "invalid_request",
                "via must be sms, mail, totp or auto",
            ));
        }
    };
    fill(w, &rows[index], &code, "verification code").await?;
    drop(code);
    let mut submitted = false;
    let mut after = rows.clone();
    if let Some(before) = press(w, VERIFY_VERBS).await? {
        submitted = true;
        after = next_screen(w, &before, Duration::from_secs(8)).await;
    }
    let still = read_form(&after).code.is_some();
    Ok(json!({
        "ok": true,
        "filled": ["one_time_code"],
        "submitted": submitted,
        "code_field_still_visible": submitted && still,
        "hint": if submitted && still {
            "the code field is still on screen: read it for an error before asking for another code"
        } else {
            "read the screen to confirm the login went through"
        },
    }))
}

/// Poll message-use / mail-use for a code that arrived during this login.
async fn wait_for_code(
    via: &str,
    from: Option<&str>,
    since_login: Duration,
    wait: Duration,
) -> Result<Secret, LoginError> {
    let sources: &[&str] = match via {
        "sms" => &["sms"],
        "mail" => &["mail"],
        _ => &["sms", "mail"],
    };
    let tools: Vec<(&str, PathBuf)> = sources
        .iter()
        .filter_map(|source| {
            let (name, var) = if *source == "sms" {
                ("message-use", "IPHONE_USE_MESSAGE_USE")
            } else {
                ("mail-use", "IPHONE_USE_MAIL_USE")
            };
            find_tool(&[name], var).map(|path| (*source, path))
        })
        .collect();
    if tools.is_empty() {
        return Err(LoginError::new(
            503,
            "code_unavailable",
            "neither message-use nor mail-use is installed on this Mac; ask the user for the code",
        ));
    }
    let deadline = Instant::now() + wait;
    loop {
        // Only a code from this login counts, with a little grace for one
        // sent just before the login call returned.
        let window = since_login
            + Duration::from_secs(30)
            + (wait - deadline.saturating_duration_since(Instant::now()));
        let minutes = window.as_secs().div_ceil(60).max(1).to_string();
        let since = format!("{minutes}m");
        for (source, path) in &tools {
            let mut args = vec!["code", "--since", since.as_str()];
            if *source == "sms" {
                args.push("--json");
                if let Some(from) = from {
                    args.extend(["--from", from]);
                }
            } else {
                args.push("--all");
            }
            if let Some(mut output) = run_tool(path, &args, Duration::from_secs(20)).await {
                let code = if output.status.success() {
                    if *source == "sms" {
                        sms_code(&output.stdout)
                    } else {
                        mail_code(&output.stdout, from)
                    }
                } else {
                    None
                };
                output.stdout.fill(0);
                if let Some(code) = code {
                    return Ok(code);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(LoginError::new(
                504,
                "no_code_yet",
                "no new code arrived; make sure the app sent one, then call again (at most two requests per login)",
            ));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// `message-use code --json`: one object, or `null`.
pub fn sms_code(stdout: &[u8]) -> Option<Secret> {
    let value: Value = serde_json::from_slice(stdout).ok()?;
    value
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
        .map(|code| Secret::new(code.to_string()))
}

/// `mail-use code --all`: the newest confident candidate, filtered by sender
/// or subject when one was given.
pub fn mail_code(stdout: &[u8], from: Option<&str>) -> Option<Secret> {
    let value: Value = serde_json::from_slice(stdout).ok()?;
    if value.get("success") == Some(&Value::Bool(false)) {
        return None;
    }
    let mut items: Vec<&Value> = value
        .get("candidates")
        .and_then(Value::as_array)
        .map(|list| list.iter().collect())
        .unwrap_or_default();
    if items.is_empty() {
        items.extend(value.get("newest"));
    }
    let from = from.map(str::to_lowercase);
    items
        .into_iter()
        .filter(|item| {
            from.is_some() || item.get("confidence").and_then(Value::as_str) != Some("low")
        })
        .filter(|item| match &from {
            Some(from) => {
                let text = format!(
                    "{} {}",
                    item.get("from").and_then(Value::as_str).unwrap_or(""),
                    item.get("subject").and_then(Value::as_str).unwrap_or("")
                )
                .to_lowercase();
                text.contains(from.as_str())
            }
            None => true,
        })
        .find_map(|item| item.get("code").and_then(Value::as_str))
        .filter(|code| !code.is_empty())
        .map(|code| Secret::new(code.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, label: &str, y: f64) -> ElementRow {
        ElementRow {
            kind: kind.to_string(),
            label: label.to_string(),
            rect: [20.0, y, 300.0, 44.0],
            ..Default::default()
        }
    }

    fn entry(id: &str, name: &str, user: &str, uris: &[&str]) -> VaultEntry {
        VaultEntry {
            id: id.to_string(),
            name: name.to_string(),
            user: Some(user.to_string()),
            uris: uris.iter().map(|u| Value::String(u.to_string())).collect(),
        }
    }

    #[test]
    fn the_system_password_sheet_is_recognised_with_its_close_button() {
        // Hardware: Safari on iOS 27 opened 自动填充密码 over a web login.
        let mut close = row("Button", "取消", 111.0);
        close.rect = [20.0, 111.0, 36.0, 36.0];
        let rows = vec![
            row("NavigationBar", "自动填充密码", 73.0),
            close,
            row("Button", "创建新密码…", 111.0),
        ];
        assert_eq!(autofill_sheet_close(&rows), Some(1));

        // A form's own Cancel button is not the sheet's.
        let rows = vec![row("StaticText", "管理员登录", 143.0), row("Button", "取消", 111.0)];
        assert_eq!(autofill_sheet_close(&rows), None);
    }

    #[test]
    fn the_password_suggestion_sheet_is_recognised_lower_down() {
        // Hardware, 17 Pro Max: over GitHub's web sign-in iOS offered one
        // saved login in a bottom sheet with 关闭 / 填充密码.
        let rows = vec![
            row("StaticText", "通过 Bitwarden 中已存的密码登录", 560.0),
            row("Button", "关闭", 520.0),
            row("Button", "填充密码", 640.0),
        ];
        assert!(autofill_sheet_up(&rows));
        assert_eq!(autofill_sheet_close(&rows), Some(1));
        // Without the fill button a lone 关闭 is the page's own.
        let rows = vec![row("Button", "关闭", 520.0)];
        assert!(!autofill_sheet_up(&rows));
        assert_eq!(autofill_sheet_close(&rows), None);
    }

    #[test]
    fn a_token_paste_field_is_not_a_second_password() {
        // Hardware: an admin login with "或直接粘贴 Bearer Token" under the
        // password was refused as a sign-up page.
        let mut token = row("SecureTextField", "", 535.0);
        token.value = Some("或直接粘贴 Bearer Token".to_string());
        let rows = vec![
            row("TextField", "账号", 280.0),
            row("SecureTextField", "", 363.0),
            token,
        ];
        let form = read_form(&rows);
        assert_eq!(form.secure_fields, 1);
        assert_eq!(form.password, Some(1));
        assert_eq!(form.account, Some(0));

        // A real sign-up page still counts both password fields.
        let rows = vec![
            row("TextField", "Email", 200.0),
            row("SecureTextField", "Password", 260.0),
            row("SecureTextField", "Confirm password", 320.0),
        ];
        assert_eq!(read_form(&rows).secure_fields, 2);
    }

    #[test]
    fn a_phone_only_field_refuses_a_username_that_is_not_a_number() {
        // Hardware: PlayPop's 电话号码 field dropped a site username's letters.
        assert!(phone_only_field(&row("TextField", "电话号码", 240.0)));
        assert!(phone_only_field(&row("TextField", "Phone number", 240.0)));
        assert!(!phone_only_field(&row("TextField", "手机号/邮箱", 240.0)));
        assert!(!phone_only_field(&row("TextField", "Email or phone", 240.0)));
        assert!(!phone_only_field(&row("TextField", "Username", 240.0)));

        assert!(phone_shaped("+81 90-1234-5678"));
        assert!(phone_shaped("13800138000"));
        assert!(!phone_shaped("someone@example.com"));
        assert!(!phone_shaped("player_one99"));
        assert!(!phone_shaped("123"));
    }

    #[test]
    fn bundle_ids_imply_web_hosts() {
        assert_eq!(bundle_hosts("com.taobao.taobao4iphone"), vec!["taobao.com"]);
        assert_eq!(bundle_hosts("jp.co.rakuten.mobile"), vec!["rakuten.co.jp"]);
        assert_eq!(bundle_hosts("com.net.app"), vec!["net.com"]);
        assert!(bundle_hosts("Pastyx").is_empty());
        assert_eq!(
            uri_host("https://www.Example.com:8443/login?x").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            uri_host("sub.example.com").as_deref(),
            Some("sub.example.com")
        );
    }

    #[test]
    fn the_strongest_tier_wins_and_ties_stay_ambiguous() {
        let entries = vec![
            entry("1", "Taobao", "a@b.com", &["https://login.taobao.com"]),
            entry("2", "淘宝 old", "c@d.com", &[]),
            entry("3", "Other", "e@f.com", &["iosapp://com.other.app"]),
        ];
        let found = candidates(&entries, Some("com.taobao.taobao4iphone"), "淘宝", &[]);
        assert_eq!(
            found.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["1"]
        );
        let found = candidates(&entries, Some("com.other.app"), "Other", &[]);
        assert_eq!(found[0].id, "3", "an iosapp:// URI beats a name");
        let found = candidates(&entries, Some("cn.unknown.app"), "淘宝", &[]);
        assert_eq!(found[0].id, "2", "the display name is the last resort");
        assert!(candidates(&entries, None, "x", &[]).is_empty());
    }

    #[test]
    fn a_web_sheet_matches_the_app_presenting_it_and_its_page_host() {
        // Hardware, 17 Pro Max: GitHub's web sign-in sheet put the view
        // service first in the active apps and GitHub second.
        let active = vec![
            "com.apple.SafariViewService".to_string(),
            "com.github.stormbreaker.prod".to_string(),
        ];
        assert_eq!(
            presenting_app(&active),
            (Some("com.github.stormbreaker.prod".to_string()), true)
        );
        assert_eq!(
            presenting_app(&["com.apple.Preferences".to_string()]),
            (Some("com.apple.Preferences".to_string()), false)
        );
        assert_eq!(presenting_app(&[]), (None, false));
        // The address bar holds the page host; page text lower down does not count.
        let row = |kind: &str, label: &str, value: Option<&str>, y: f64| ElementRow {
            kind: kind.to_string(),
            label: label.to_string(),
            value: value.map(str::to_string),
            rect: [0.0, y, 440.0, 30.0],
            ..Default::default()
        };
        let rows = vec![
            ElementRow {
                kind: "Application".into(),
                label: "SafariViewService".into(),
                rect: [0.0, 0.0, 440.0, 956.0],
                ..Default::default()
            },
            row("Button", "完成", None, 60.0),
            row("TextField", "地址", Some("github.com"), 60.0),
            row("StaticText", "Sign in to GitHub", None, 300.0),
            row("Link", "docs.github.com", None, 700.0),
        ];
        assert_eq!(sheet_host(&rows).as_deref(), Some("github.com"));
        assert_eq!(sheet_host(&rows[..2]), None);
        // GitHub keeps two vault entries: the account with its TOTP, and the
        // TOTP alone. The sheet's host finds both; the one with a username wins.
        let entries = vec![
            entry("a", "github.com", "me@x.com", &["https://github.com/login"]),
            VaultEntry {
                id: "b".into(),
                name: "github.com".into(),
                user: None,
                uris: vec![json!("https://github.com")],
            },
            entry("c", "gitlab", "me@x.com", &["https://gitlab.com"]),
        ];
        let found = candidates(
            &entries,
            Some("com.apple.SafariViewService"),
            "",
            &["github.com".into()],
        );
        assert_eq!(found.len(), 2);
        let picked = prefer_with_username(found);
        assert_eq!(
            picked.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["a"]
        );
        let request = LoginRequest::default();
        let one = pick_entry(
            &entries,
            &request,
            Some("com.github.stormbreaker.prod"),
            "",
            &[],
        )
        .unwrap();
        assert_eq!(
            one.id, "a",
            "the presenting app's bundle implies github.com"
        );
    }

    #[test]
    fn a_save_password_prompt_is_declined_and_a_blank_screen_proves_nothing() {
        // Hardware, GitHub's web sheet after Sign in: iOS asks where to update
        // the password (密码 / Bitwarden / 以后); "later" is the only choice taken.
        let r = |kind: &str, label: &str| ElementRow {
            kind: kind.to_string(),
            label: label.to_string(),
            rect: [0.0, 400.0, 300.0, 44.0],
            ..Default::default()
        };
        let prompt = vec![
            r("Application", "Safari浏览器"),
            r("Alert", "更新密码？"),
            r("StaticText", "选取密码的更新位置。"),
            r("Button", "密码"),
            r("Button", "Bitwarden"),
            r("Button", "以后"),
        ];
        assert_eq!(save_prompt_later(&prompt), Some(5));
        let page = vec![r("Application", "Safari浏览器"), r("Button", "以后")];
        assert_eq!(
            save_prompt_later(&page),
            None,
            "a page's own 以后 is not the prompt"
        );
        // A password manager's sheet over the page reads as the root alone.
        assert!(unreadable(&[r("Application", "Safari浏览器")]));
        assert!(unreadable(&[]));
        assert!(!unreadable(&prompt));
    }

    #[test]
    fn a_field_is_found_again_where_it_is_now_after_a_zoom() {
        // Hardware, GitHub's web sheet: Safari zoomed after the account was
        // typed and every field moved; the password field is the same-kind,
        // same-label row nearest to where it was.
        let field = |kind: &str, label: &str, y: f64| ElementRow {
            kind: kind.to_string(),
            label: label.to_string(),
            rect: [16.0, y, 408.0, 40.0],
            ..Default::default()
        };
        let before = field("SecureTextField", "Password", 480.0);
        let now = vec![
            field("TextField", "Username or email address", 249.0),
            field("SecureTextField", "Password", 330.0),
            field("SecureTextField", "Password", 900.0),
        ];
        assert_eq!(live_frame(&now, &before), Some([16.0, 330.0, 408.0, 40.0]));
        assert_eq!(live_frame(&now[..1], &before), None);
    }

    #[test]
    fn a_real_sheet_address_bar_and_a_second_entry_for_the_same_host() {
        // Hardware, 17 Pro Max (v0.17.6): the address bar is a Button whose
        // value starts with an LRM mark, and the vault holds a second entry
        // with a username for github.com (another tool's token entry).
        let row = |kind: &str, label: &str, value: Option<&str>, y: f64| ElementRow {
            kind: kind.to_string(),
            label: label.to_string(),
            value: value.map(str::to_string),
            rect: [26.0, y, 388.0, 44.0],
            ..Default::default()
        };
        let rows = vec![
            ElementRow {
                kind: "Application".into(),
                label: "Safari浏览器".into(),
                rect: [0.0, 0.0, 440.0, 956.0],
                ..Default::default()
            },
            row("Button", "地址", Some("\u{200e}github.com"), 78.0),
            row("StaticText", "Sign in to GitHub", None, 300.0),
        ];
        assert_eq!(sheet_host(&rows).as_deref(), Some("github.com"));
        let entries = vec![
            entry(
                "tok",
                "cookie-use sync",
                "cookie@x.com",
                &["https://github.com"],
            ),
            entry(
                "gh",
                "github.com",
                "me@x.com",
                &["https://github.com/login"],
            ),
        ];
        let request = LoginRequest::default();
        let one = pick_entry(
            &entries,
            &request,
            Some("com.github.stormbreaker.prod"),
            "",
            &["github.com".into()],
        )
        .unwrap();
        assert_eq!(one.id, "gh", "the entry named after the page host wins");
        // Without a page host nothing breaks the tie: still ambiguous.
        let tied = pick_entry(
            &entries,
            &request,
            Some("com.github.stormbreaker.prod"),
            "",
            &[],
        );
        assert!(tied.is_err());
    }

    #[test]
    fn an_explicit_item_is_an_id_or_an_exact_name() {
        let entries = vec![
            entry("u-1", "apple.com", "a@qq.com", &[]),
            entry("u-2", "apple.com", "b@qq.com", &[]),
        ];
        assert_eq!(by_item(&entries, "u-2", None).len(), 1);
        assert_eq!(by_item(&entries, "Apple.com", None).len(), 2);
        assert_eq!(
            by_item(&entries, "apple.com", Some("B@qq.com"))[0].id,
            "u-2"
        );
        assert!(
            by_item(&entries, "apple", None).is_empty(),
            "no prefix guessing"
        );
    }

    #[test]
    fn accounts_are_masked() {
        assert_eq!(mask_account("leeguoo@qq.com"), "le***@qq.com");
        assert_eq!(mask_account("18612130974"), "186***74");
        assert_eq!(mask_account("abc"), "***");
    }

    #[test]
    fn a_login_form_is_found_by_hints_and_position() {
        let rows = vec![
            row("Application", "Shop", 0.0),
            row("TextField", "Search", 60.0),
            row("TextField", "", 200.0),
            row("SecureTextField", "密码", 260.0),
            row("Button", "忘记密码", 320.0),
            row("Button", "登录", 380.0),
            row("Button", "注册", 440.0),
        ];
        let form = read_form(&rows);
        assert_eq!(
            form.account,
            Some(2),
            "the text field just above the password"
        );
        assert_eq!(form.password, Some(3));
        assert_eq!(submit_button(&rows, LOGIN_VERBS), Some(5));

        let mut hinted = rows.clone();
        hinted[1].placeholder = Some("手机号/邮箱".to_string());
        assert_eq!(
            read_form(&hinted).account,
            Some(1),
            "a hinted field beats position"
        );
    }

    #[test]
    fn sign_up_and_password_changes_are_not_login_forms() {
        let rows = vec![
            row("TextField", "Email", 100.0),
            row("SecureTextField", "Password", 160.0),
            row("SecureTextField", "Confirm password", 220.0),
            row("Button", "Sign up", 300.0),
        ];
        let form = read_form(&rows);
        assert_eq!(form.secure_fields, 2);
        assert_eq!(form.password, None);
        assert_eq!(
            submit_button(&rows, LOGIN_VERBS),
            None,
            "never a sign-up button"
        );
    }

    #[test]
    fn two_step_and_code_pages_are_told_apart() {
        let account_page = vec![
            row("TextField", "Apple ID", 200.0),
            row("Button", "Continue", 300.0),
        ];
        let form = read_form(&account_page);
        assert_eq!(
            (form.account, form.password, form.code),
            (Some(0), None, None)
        );
        let code_page = vec![
            row("StaticText", "我们已向你的手机发送短信验证码", 100.0),
            row("TextField", "验证码", 200.0),
            row("Button", "确认", 300.0),
        ];
        let form = read_form(&code_page);
        assert_eq!((form.account, form.code), (None, Some(1)));
        assert_eq!(code_channel(&code_page), "sms");
        assert_eq!(submit_button(&code_page, VERIFY_VERBS), Some(2));
    }

    #[test]
    fn third_party_and_reset_buttons_never_submit() {
        let rows = vec![
            row("Button", "Sign in with Apple", 100.0),
            row("Button", "微信登录", 160.0),
            row("Button", "Reset password", 220.0),
        ];
        assert_eq!(submit_button(&rows, LOGIN_VERBS), None);
    }

    #[test]
    fn vault_output_parses_without_keeping_the_seed() {
        let raw = br#"{"data":{"username":"u","password":"p","totp":"otpauth://x","uris":[]},"id":"x","name":"n"}"#;
        let credentials = parse_credentials(raw).unwrap();
        assert_eq!(credentials.username.unwrap().expose(), "u");
        assert_eq!(credentials.password.unwrap().expose(), "p");
        assert!(credentials.has_totp);
        let secret = Secret::new("hunter2".to_string());
        assert_eq!(format!("{secret:?}"), "Secret([redacted])");
    }

    #[test]
    fn codes_parse_from_message_use_and_mail_use() {
        assert_eq!(
            sms_code(br#"{"code":"123456","brand":"X"}"#)
                .unwrap()
                .expose(),
            "123456"
        );
        assert!(sms_code(b"null").is_none());
        let mail = br#"{"success":true,"candidates":[
            {"code":"111111","from":"news@shop.com","subject":"Sale","confidence":"low"},
            {"code":"222222","from":"no-reply@acme.com","subject":"Your code","confidence":"high"}]}"#;
        assert_eq!(mail_code(mail, None).unwrap().expose(), "222222");
        assert_eq!(mail_code(mail, Some("shop")).unwrap().expose(), "111111");
        assert!(mail_code(br#"{"success":false,"error":"x"}"#, None).is_none());
    }

    #[test]
    fn a_shifted_page_still_finds_its_field_but_never_guesses() {
        let email = [20.0, 200.0, 350.0, 44.0];
        let search = [20.0, 60.0, 350.0, 44.0];
        let address = [10.0, 780.0, 370.0, 50.0];
        assert_eq!(pick_frame(&[search, email, address], email), Ok(1));
        // Safari's toolbar settled and the form moved up 30 pt.
        let moved = [20.0, 170.0, 350.0, 44.0];
        assert_eq!(pick_frame(&[search, moved, address], email), Ok(1));
        // Two same-size fields within reach: no guess.
        let below = [20.0, 230.0, 350.0, 44.0];
        assert_eq!(
            pick_frame(&[moved, below], email),
            Err(FrameMatch::Ambiguous)
        );
        assert_eq!(pick_frame(&[address], email), Err(FrameMatch::Missing));
        assert_eq!(
            pick_frame(&[email, email], email),
            Err(FrameMatch::Ambiguous)
        );
    }

    #[test]
    fn only_vault_ids_reach_bwu_argv() {
        assert!(safe_id("6e9663e2-0a84-4633-a413-d825a6b6c312"));
        assert!(!safe_id("--reveal"));
        assert!(!safe_id("name with space"));
    }
}
