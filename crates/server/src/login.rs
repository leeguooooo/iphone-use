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
) -> Vec<&'a VaultEntry> {
    let mut tiers: [Vec<&VaultEntry>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let hosts = bundle.map(bundle_hosts).unwrap_or_default();
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

pub fn read_form(rows: &[ElementRow]) -> Form {
    let secure: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].kind == "SecureTextField" && usable(&rows[i]))
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
async fn element_for(w: &mut WdaClient, row: &ElementRow) -> Result<String, LoginError> {
    let ids = w
        .find_elements("class chain", &format!("**/XCUIElementType{}", row.kind))
        .await
        .map_err(|_| phone_error("field lookup"))?;
    let mut found = None;
    for id in ids {
        if let Ok(rect) = w.element_rect(&id).await {
            if rect
                .iter()
                .zip(row.rect.iter())
                .all(|(a, b)| (a - b).abs() <= 2.0)
            {
                if found.is_some() {
                    return Err(LoginError::new(
                        422,
                        "field_ambiguous",
                        "two fields share that frame; finish this login by hand",
                    ));
                }
                found = Some(id);
            }
        }
    }
    found.ok_or_else(|| {
        LoginError::new(
            422,
            "field_not_found",
            "the login field moved; read the screen and try again",
        )
    })
}

fn same_text(a: &str, b: &str) -> bool {
    let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    strip(a) == strip(b)
}

/// Replace a field's contents with a secret. `verify` reads a non-secure
/// field back and compares in the daemon; nothing read is returned.
async fn fill(
    w: &mut WdaClient,
    row: &ElementRow,
    secret: &Secret,
    verify: bool,
    what: &'static str,
) -> Result<(), LoginError> {
    let id = element_for(w, row).await?;
    let _ = w.clear_element(&id).await;
    w.type_into(&id, secret.expose())
        .await
        .map_err(|_| phone_error(&format!("{what} entry")))?;
    if verify {
        if let Ok(Some(now)) = w.element_value(&id).await {
            if !same_text(&now, secret.expose()) {
                return Err(LoginError::new(
                    422,
                    "value_not_applied",
                    format!(
                        "the {what} field holds something other than what was typed (a keyboard \
                         language that composes letters, or autocorrect); switch the phone's \
                         keyboard to English and try again"
                    ),
                ));
            }
        }
    }
    Ok(())
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

fn pick_entry<'a>(
    entries: &'a [VaultEntry],
    request: &LoginRequest,
    bundle: Option<&str>,
    app_name: &str,
) -> Result<&'a VaultEntry, LoginError> {
    let matches = match &request.item {
        Some(item) => by_item(entries, item, request.user.as_deref()),
        None => {
            let found = candidates(entries, bundle, app_name);
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
    let mut rows = w.elements().await.map_err(|_| phone_error("screen read"))?;
    let app_name = rows
        .iter()
        .find(|r| r.kind == "Application")
        .map(|r| r.label.clone())
        .unwrap_or_default();
    let bundle = w.active_bundle().await.ok().flatten();
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
    let entry = pick_entry(&entries, request, bundle.as_deref(), &app_name)?.clone();
    drop(entries);
    let credentials = vault.credentials(&entry.id).await?;
    let submit = request.submit.unwrap_or(true);
    let mut filled: Vec<&str> = Vec::new();

    if let (Some(index), Some(username)) = (form.account, &credentials.username) {
        fill(w, &rows[index], username, true, "account").await?;
        filled.push("account");
    }
    if form.password.is_none() {
        // A two-step login: the account page first.
        if !submit {
            return Ok(done(
                &entry,
                &filled,
                false,
                &rows,
                None,
                "filled the account; the password page comes after Next",
            ));
        }
        let Some(next) = submit_button(&rows, LOGIN_VERBS) else {
            return Ok(done(
                &entry,
                &filled,
                false,
                &rows,
                None,
                "filled the account but found no Next or Log in button; read the screen",
            ));
        };
        tap_row(w, &rows[next]).await?;
        rows = next_screen(w, &rows, Duration::from_secs(6)).await;
        form = read_form(&rows);
        if form.secure_fields >= 2 {
            return Err(not_a_login_form());
        }
    }
    let mut submitted = false;
    if let (Some(index), Some(password)) = (form.password, &credentials.password) {
        fill(w, &rows[index], password, false, "password").await?;
        filled.push("password");
        if submit {
            match submit_button(&rows, LOGIN_VERBS) {
                Some(button) => tap_row(w, &rows[button]).await?,
                None => w
                    .named_key("return")
                    .await
                    .map_err(|_| phone_error("submit"))?,
            }
            submitted = true;
            rows = next_screen(w, &rows, Duration::from_secs(8)).await;
        }
    }
    if filled.is_empty() {
        return Err(LoginError::new(
            422,
            "nothing_to_fill",
            "the vault entry has no username or password for the fields on screen",
        ));
    }

    *SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(Session {
        started: Instant::now(),
        entry_id: entry.id.clone(),
        code_requests: 0,
    });

    // A second factor, when the app asks for one.
    let mut needs_code = None;
    if submitted {
        let after = read_form(&rows);
        if after.code.is_some() && after.password.is_none() {
            if credentials.has_totp {
                let code = vault.totp(&entry.id).await?;
                if let Some(index) = after.code {
                    fill(w, &rows[index], &code, false, "verification code").await?;
                    filled.push("one_time_code");
                    if let Some(button) = submit_button(&rows, VERIFY_VERBS) {
                        tap_row(w, &rows[button]).await?;
                        rows = next_screen(w, &rows, Duration::from_secs(8)).await;
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
    Ok(done(&entry, &filled, submitted, &rows, needs_code, hint))
}

fn not_a_login_form() -> LoginError {
    LoginError::new(
        422,
        "not_a_login_form",
        "this screen has more than one password field (sign-up or a password change); only an existing account's login is filled",
    )
}

fn done(
    entry: &VaultEntry,
    filled: &[&str],
    submitted: bool,
    rows: &[ElementRow],
    needs_code: Option<&str>,
    hint: &str,
) -> Value {
    let after = read_form(rows);
    json!({
        "ok": true,
        "entry": entry.name,
        "account": entry.user.as_deref().map(mask_account),
        "filled": filled,
        "submitted": submitted,
        "login_form_still_visible": submitted && after.password.is_some(),
        "needs_code": needs_code.map(|via| json!({ "via": via })),
        "hint": if submitted && after.password.is_some() {
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
    fill(w, &rows[index], &code, false, "verification code").await?;
    drop(code);
    let mut submitted = false;
    let mut after = rows.clone();
    if let Some(button) = submit_button(&rows, VERIFY_VERBS) {
        tap_row(w, &rows[button]).await?;
        submitted = true;
        after = next_screen(w, &rows, Duration::from_secs(8)).await;
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
        let found = candidates(&entries, Some("com.taobao.taobao4iphone"), "淘宝");
        assert_eq!(
            found.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["1"]
        );
        let found = candidates(&entries, Some("com.other.app"), "Other");
        assert_eq!(found[0].id, "3", "an iosapp:// URI beats a name");
        let found = candidates(&entries, Some("cn.unknown.app"), "淘宝");
        assert_eq!(found[0].id, "2", "the display name is the last resort");
        assert!(candidates(&entries, None, "x").is_empty());
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
    fn only_vault_ids_reach_bwu_argv() {
        assert!(safe_id("6e9663e2-0a84-4633-a413-d825a6b6c312"));
        assert!(!safe_id("--reveal"));
        assert!(!safe_id("name with space"));
    }
}
