//! App lookup by name for `GET /agent/apps?query=` and `launch_app {"app":…}`.
//!
//! Agents used to guess bundle identifiers. This module answers "which bundle
//! is 招商银行 / WeChat / Settings" from three sources, in this order:
//!
//! * `installed` — the phone's own inventory (`apps.rs`, from devicectl), the
//!   only source that proves an app is on the phone;
//! * `catalog` — a bundled list of popular apps whose bundle ids were read
//!   from Apple's lookup API (`apps_catalog.json`), plus Apple's built-in apps;
//! * `apple` — Apple's public iTunes Search API, for anything else. A store
//!   hit is a candidate, never installation evidence.
//!
//! Everything here except [`search_apple`] is pure so tests can feed sources
//! directly; the devicectl and network calls live in `http.rs` / below.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

/// Apple's built-in apps: bundle id, English name, other names (Chinese UI
/// names first). Bundle ids were read from a real iPhone's devicectl
/// inventory (`defaultApp: true`), so they need no store lookup.
pub const SYSTEM_APPS: &[(&str, &str, &[&str])] = &[
    ("com.apple.Preferences", "Settings", &["设置", "设定"]),
    (
        "com.apple.mobilesafari",
        "Safari",
        &["浏览器", "Safari浏览器"],
    ),
    (
        "com.apple.MobileSMS",
        "Messages",
        &["信息", "短信", "iMessage"],
    ),
    ("com.apple.mobileslideshow", "Photos", &["照片", "相册"]),
    ("com.apple.camera", "Camera", &["相机"]),
    ("com.apple.mobiletimer", "Clock", &["时钟", "闹钟"]),
    ("com.apple.mobilenotes", "Notes", &["备忘录"]),
    ("com.apple.reminders", "Reminders", &["提醒事项"]),
    ("com.apple.mobilecal", "Calendar", &["日历"]),
    ("com.apple.mobilephone", "Phone", &["电话"]),
    ("com.apple.mobilemail", "Mail", &["邮件"]),
    ("com.apple.Maps", "Maps", &["地图", "Apple Maps"]),
    ("com.apple.AppStore", "App Store", &["应用商店", "appstore"]),
    ("com.apple.Passbook", "Wallet", &["钱包"]),
    ("com.apple.Health", "Health", &["健康"]),
    ("com.apple.DocumentsApp", "Files", &["文件"]),
    ("com.apple.shortcuts", "Shortcuts", &["快捷指令"]),
    ("com.apple.Music", "Music", &["音乐", "Apple Music"]),
    ("com.apple.findmy", "Find My", &["查找"]),
    ("com.apple.calculator", "Calculator", &["计算器"]),
    (
        "com.apple.MobileAddressBook",
        "Contacts",
        &["通讯录", "联系人"],
    ),
    ("com.apple.facetime", "FaceTime", &["FaceTime通话"]),
    ("com.apple.weather", "Weather", &["天气"]),
    ("com.apple.iBooks", "Books", &["图书", "Apple Books"]),
    ("com.apple.podcasts", "Podcasts", &["播客"]),
    ("com.apple.tv", "TV", &["Apple TV", "视频"]),
    ("com.apple.Translate", "Translate", &["翻译"]),
    ("com.apple.VoiceMemos", "Voice Memos", &["语音备忘录"]),
    ("com.apple.Home", "Home", &["家庭"]),
    ("com.apple.Fitness", "Fitness", &["健身"]),
    ("com.apple.freeform", "Freeform", &["无边记"]),
    ("com.apple.Bridge", "Watch", &["Apple Watch", "手表"]),
    ("com.apple.compass", "Compass", &["指南针"]),
    ("com.apple.measure", "Measure", &["测距仪"]),
    ("com.apple.Magnifier", "Magnifier", &["放大器"]),
    ("com.apple.stocks", "Stocks", &["股市"]),
    ("com.apple.tips", "Tips", &["提示"]),
    ("com.apple.news", "News", &["Apple News"]),
    ("com.apple.Passwords", "Passwords", &["密码"]),
    ("com.apple.journal", "Journal", &["手记"]),
    ("com.apple.TestFlight", "TestFlight", &[]),
    ("com.apple.MobileStore", "iTunes Store", &[]),
    ("com.apple.games", "Games", &["游戏"]),
];

/// The bundle of an Apple built-in app named exactly (case-insensitively for
/// ASCII) by its English name or one of its listed names.
pub fn system_app_bundle(name: &str) -> Option<&'static str> {
    let wanted = normalize(name);
    if wanted.is_empty() {
        return None;
    }
    SYSTEM_APPS.iter().find_map(|(bundle, english, others)| {
        std::iter::once(*english)
            .chain(others.iter().copied())
            .any(|candidate| normalize(candidate) == wanted)
            .then_some(*bundle)
    })
}

/// Case-, width- and spacing-insensitive form used for every comparison.
/// Invisible format characters are dropped: devicectl reports WhatsApp as
/// "\u{200E}WhatsApp".
pub fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|c| {
            !c.is_whitespace()
                && !matches!(*c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
        })
        .map(|c| match c {
            // Fullwidth ASCII (ＱＱ) folds to ASCII.
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            _ => c,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// A reverse-DNS bundle identifier, as the launch validators accept it.
pub fn valid_bundle(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.contains('.')
        && !value.starts_with('.')
        && !value.ends_with('.')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub aliases: Vec<String>,
    pub bundle_id: String,
    /// `installed`, `catalog`, or `apple` — where the row came from.
    pub source: &'static str,
    pub system: bool,
    pub publisher: Option<String>,
    pub country: Option<String>,
    pub track_id: Option<u64>,
}

impl Candidate {
    fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str())
            .chain(self.aliases.iter().map(String::as_str))
            .chain(std::iter::once(self.bundle_id.as_str()))
    }

    /// 2 = an exact name, alias or bundle id; 1 = a substring of one; 0 = no match.
    fn score(&self, wanted: &str) -> u8 {
        if wanted.is_empty() {
            return 0;
        }
        let mut best = 0;
        for name in self.names() {
            let name = normalize(name);
            if name == wanted {
                return 2;
            }
            if name.contains(wanted) {
                best = 1;
            }
        }
        best
    }
}

#[derive(Deserialize)]
struct CatalogFile {
    apps: Vec<CatalogRow>,
}

#[derive(Deserialize)]
struct CatalogRow {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    bundle_id: String,
    track_id: u64,
    publisher: String,
    country: String,
}

/// The bundled catalog plus Apple's built-in apps. Parsed once.
pub fn catalog() -> &'static [Candidate] {
    static CATALOG: OnceLock<Vec<Candidate>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let file: CatalogFile = serde_json::from_str(include_str!("apps_catalog.json"))
            .expect("apps_catalog.json is valid");
        let mut rows: Vec<Candidate> = file
            .apps
            .into_iter()
            .filter(|row| valid_bundle(&row.bundle_id))
            .map(|row| Candidate {
                name: row.name,
                aliases: row.aliases,
                bundle_id: row.bundle_id,
                source: "catalog",
                system: false,
                publisher: Some(row.publisher),
                country: Some(row.country),
                track_id: Some(row.track_id),
            })
            .collect();
        rows.extend(
            SYSTEM_APPS
                .iter()
                .map(|(bundle, english, others)| Candidate {
                    name: (*english).to_string(),
                    aliases: others.iter().map(|s| (*s).to_string()).collect(),
                    bundle_id: (*bundle).to_string(),
                    source: "catalog",
                    system: true,
                    publisher: Some("Apple".to_string()),
                    country: None,
                    track_id: None,
                }),
        );
        rows
    })
}

/// The phone's apps from an `/agent/apps` inventory body. Catalog names for
/// the same bundle become aliases, so "微信" finds an installed "WeChat".
pub fn installed_from_inventory(body: &Value) -> Vec<Candidate> {
    let known: HashMap<&str, &Candidate> = catalog()
        .iter()
        .map(|row| (row.bundle_id.as_str(), row))
        .collect();
    body["apps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|app| {
            let bundle = app["bundle"].as_str().filter(|b| valid_bundle(b))?;
            let reference = known.get(bundle);
            let name = app["name"]
                .as_str()
                .map(|n| n.trim_matches(|c: char| normalize(&c.to_string()).is_empty()))
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .or_else(|| reference.map(|r| r.name.clone()))
                .unwrap_or_else(|| bundle.to_string());
            let mut aliases = Vec::new();
            if let Some(reference) = reference {
                aliases.push(reference.name.clone());
                aliases.extend(reference.aliases.iter().cloned());
            }
            Some(Candidate {
                name,
                aliases,
                bundle_id: bundle.to_string(),
                source: "installed",
                system: app["system"].as_bool().unwrap_or(false),
                publisher: reference.and_then(|r| r.publisher.clone()),
                country: None,
                track_id: reference.and_then(|r| r.track_id),
            })
        })
        .collect()
}

/// One ranked match.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub candidate: Candidate,
    pub exact: bool,
    pub installed_verified: bool,
}

impl Ranked {
    pub fn to_json(&self) -> Value {
        let c = &self.candidate;
        let mut row = json!({
            "name": c.name,
            "bundle_id": c.bundle_id,
            "source": c.source,
            "installed_verified": self.installed_verified,
            "match": if self.exact { "exact" } else { "partial" },
        });
        if c.system {
            row["system"] = json!(true);
        }
        if let Some(publisher) = &c.publisher {
            row["publisher"] = json!(publisher);
        }
        if let Some(country) = &c.country {
            row["country"] = json!(country);
        }
        if let (Some(track), Some(country)) = (c.track_id, &c.country) {
            row["track_id"] = json!(track);
            row["store_url"] = json!(format!("https://apps.apple.com/{country}/app/id{track}"));
        }
        row
    }
}

/// Rank every source's matches for `query`. One row per bundle id (the
/// earliest source wins, so installed > catalog > apple); exact matches first,
/// then installed ones. When any row matches exactly, only exact rows are
/// returned — "CMB" must not drag in every app whose name contains "cmb".
///
/// `installed` is `None` when the inventory was not read: then nothing is
/// `installed_verified`, which means "unknown", not "absent".
pub fn rank(
    query: &str,
    installed: Option<&[Candidate]>,
    catalog_rows: &[Candidate],
    apple: &[Candidate],
) -> Vec<Ranked> {
    let wanted = normalize(query);
    let installed_ids: HashSet<&str> = installed
        .into_iter()
        .flatten()
        .map(|row| row.bundle_id.as_str())
        .collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let all = installed
        .into_iter()
        .flatten()
        .chain(catalog_rows.iter())
        .chain(apple.iter());
    for row in all {
        let score = row.score(&wanted);
        if score == 0 || !seen.insert(row.bundle_id.clone()) {
            continue;
        }
        out.push(Ranked {
            candidate: row.clone(),
            exact: score == 2,
            installed_verified: installed_ids.contains(row.bundle_id.as_str()),
        });
    }
    // Stable sort keeps source order inside each group.
    out.sort_by_key(|r| (!r.exact, !r.installed_verified));
    if out.iter().any(|r| r.exact) {
        out.retain(|r| r.exact);
    }
    out
}

/// Why an `app` name did not resolve to one bundle.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    /// Nothing is named that. `suggestions` are partial matches, if any.
    NotFound { suggestions: Vec<Ranked> },
    /// Known, but the phone's inventory (which was read) does not have it.
    NotInstalled { candidates: Vec<Ranked> },
    /// More than one app carries that exact name.
    Ambiguous { candidates: Vec<Ranked> },
}

/// Resolve an app name for `launch_app` without guessing: only an exact name
/// or alias counts, and it has to be unique. With an inventory, only apps on
/// the phone qualify. The Apple store is never consulted here — a store hit
/// is not evidence the app is installed.
pub fn resolve_for_launch(
    query: &str,
    installed: Option<&[Candidate]>,
) -> Result<Ranked, ResolveError> {
    let ranked = rank(query, installed, catalog(), &[]);
    let exact: Vec<Ranked> = ranked.iter().filter(|r| r.exact).cloned().collect();
    if exact.is_empty() {
        return Err(ResolveError::NotFound {
            suggestions: ranked.into_iter().take(5).collect(),
        });
    }
    let pool: Vec<Ranked> = if installed.is_some() {
        let on_phone: Vec<Ranked> = exact
            .iter()
            .filter(|r| r.installed_verified)
            .cloned()
            .collect();
        if on_phone.is_empty() {
            return Err(ResolveError::NotInstalled { candidates: exact });
        }
        on_phone
    } else {
        exact
    };
    match pool.len() {
        1 => Ok(pool.into_iter().next().expect("one")),
        _ => Err(ResolveError::Ambiguous { candidates: pool }),
    }
}

/// The unique exact match among Apple's built-in apps, which needs no
/// inventory read: their bundle ids are the same on every iPhone.
pub fn resolve_system(query: &str) -> Option<Ranked> {
    let wanted = normalize(query);
    let mut hits = catalog()
        .iter()
        .filter(|row| row.system && row.score(&wanted) == 2);
    let first = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    Some(Ranked {
        candidate: first.clone(),
        exact: true,
        installed_verified: false,
    })
}

// ---------------------------------------------------------------------------
// Apple iTunes Search API
// ---------------------------------------------------------------------------

/// Apple's guidance is about 20 requests a minute; stay under it.
pub const APPLE_RATE_PER_MINUTE: usize = 18;
pub const APPLE_TIMEOUT: Duration = Duration::from_secs(5);
pub const APPLE_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const APPLE_CACHE_TTL: Duration = Duration::from_secs(900);
const APPLE_CACHE_MAX_ENTRIES: usize = 256;

/// Parse an iTunes search/lookup answer into candidates. Rows without a valid
/// bundle id, name or track id are dropped.
pub fn parse_apple(body: &Value, country: &str) -> Result<Vec<Candidate>, String> {
    let results = body["results"]
        .as_array()
        .ok_or_else(|| "Apple returned an unexpected JSON shape".to_string())?;
    let mut seen = HashSet::new();
    Ok(results
        .iter()
        .filter(|app| app["kind"].as_str().is_none_or(|k| k == "software"))
        .filter_map(|app| {
            let bundle = app["bundleId"].as_str().filter(|b| valid_bundle(b))?;
            let name = app["trackName"]
                .as_str()
                .map(str::trim)
                .filter(|n| !n.is_empty())?;
            let track = app["trackId"].as_u64().filter(|t| *t > 0)?;
            seen.insert(bundle.to_string()).then(|| Candidate {
                name: name.chars().take(200).collect(),
                aliases: Vec::new(),
                bundle_id: bundle.to_string(),
                source: "apple",
                system: false,
                publisher: app["artistName"]
                    .as_str()
                    .map(|p| p.chars().take(200).collect()),
                country: Some(country.to_string()),
                track_id: Some(track),
            })
        })
        .collect())
}

/// A sliding one-minute window of request times.
#[derive(Default)]
pub struct RateWindow(VecDeque<Instant>);

impl RateWindow {
    /// Record a request at `now` if the window has room.
    pub fn try_take(&mut self, now: Instant, per_minute: usize) -> bool {
        while self
            .0
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= Duration::from_secs(60))
        {
            self.0.pop_front();
        }
        if self.0.len() >= per_minute {
            return false;
        }
        self.0.push_back(now);
        true
    }
}

type AppleCache = HashMap<(String, String, usize), (Instant, Vec<Candidate>)>;

fn apple_cache() -> &'static Mutex<AppleCache> {
    static CACHE: OnceLock<Mutex<AppleCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn apple_rate() -> &'static Mutex<RateWindow> {
    static RATE: OnceLock<Mutex<RateWindow>> = OnceLock::new();
    RATE.get_or_init(|| Mutex::new(RateWindow::default()))
}

/// Search Apple's store for `query` in `country` (two letters, validated by
/// the caller). Cached 15 minutes, rate limited, 5 s timeout, 2 MiB cap, no
/// redirects. Only the query leaves the Mac — never the phone's inventory.
pub async fn search_apple(
    query: &str,
    country: &str,
    limit: usize,
) -> Result<Vec<Candidate>, String> {
    let key = (normalize(query), country.to_string(), limit);
    if let Ok(cache) = apple_cache().lock() {
        if let Some((at, rows)) = cache.get(&key) {
            if at.elapsed() < APPLE_CACHE_TTL {
                return Ok(rows.clone());
            }
        }
    }
    let allowed = apple_rate()
        .lock()
        .map(|mut window| window.try_take(Instant::now(), APPLE_RATE_PER_MINUTE))
        .unwrap_or(false);
    if !allowed {
        return Err("Apple store request budget reached (18 a minute); use source=catalog or installed, or retry in a minute".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(APPLE_TIMEOUT)
        .connect_timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("iphone-use/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("Apple store client: {e}"))?;
    let limit_text = limit.to_string();
    let mut response = client
        .get("https://itunes.apple.com/search")
        .query(&[
            ("term", query),
            ("country", country),
            ("media", "software"),
            ("entity", "software"),
            ("limit", limit_text.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("Apple store request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            403 | 429 => "Apple throttled the store search; do not retry right away — use source=catalog or installed".to_string(),
            code => format!("Apple store search answered HTTP {code}"),
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("Apple store response: {e}"))?
    {
        if bytes.len() + chunk.len() > APPLE_MAX_BYTES {
            return Err("Apple store response exceeded 2 MiB".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    let body: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("Apple store JSON: {e}"))?;
    let rows = parse_apple(&body, country)?;
    if let Ok(mut cache) = apple_cache().lock() {
        cache.retain(|_, (at, _)| at.elapsed() < APPLE_CACHE_TTL);
        if cache.len() >= APPLE_CACHE_MAX_ENTRIES {
            cache.clear();
        }
        cache.insert(key, (Instant::now(), rows.clone()));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(apps: Value) -> Vec<Candidate> {
        installed_from_inventory(&json!({ "ok": true, "apps": apps }))
    }

    #[test]
    fn catalog_rows_are_valid_and_unique() {
        let rows = catalog();
        assert!(rows.len() > 100, "catalog + system apps: {}", rows.len());
        let mut seen = HashSet::new();
        for row in rows {
            assert!(valid_bundle(&row.bundle_id), "{}", row.bundle_id);
            assert!(seen.insert(&row.bundle_id), "duplicate {}", row.bundle_id);
            assert!(!row.name.is_empty());
            if !row.system {
                assert!(row.track_id.is_some_and(|t| t > 0), "{}", row.name);
                assert!(row.country.as_deref().is_some_and(|c| c.len() == 2));
            }
        }
    }

    #[test]
    fn common_apps_resolve_from_the_catalog() {
        for (query, bundle) in [
            ("微信", "com.tencent.xin"),
            ("wechat", "com.tencent.xin"),
            ("支付宝", "com.alipay.iphoneclient"),
            ("淘宝", "com.taobao.taobao4iphone"),
            ("京东", "com.360buy.jdmobile"),
            ("拼多多", "com.xunmeng.pinduoduo"),
            ("小红书", "com.xingin.discover"),
            ("抖音", "com.ss.iphone.ugc.Aweme"),
            ("B站", "tv.danmaku.bilianime"),
            ("微博", "com.sina.weibo"),
            ("美团", "com.meituan.imeituan"),
            ("钉钉", "com.laiwang.DingTalk"),
            ("飞书", "com.bytedance.ee.lark"),
            ("CMB", "com.cmbchina.MPBBank"),
            ("招商银行", "com.cmbchina.MPBBank"),
            ("健康", "com.apple.Health"),
            ("Settings", "com.apple.Preferences"),
            ("Safari", "com.apple.mobilesafari"),
            ("信息", "com.apple.MobileSMS"),
            ("照片", "com.apple.mobileslideshow"),
            ("WhatsApp", "net.whatsapp.WhatsApp"),
        ] {
            let resolved =
                resolve_for_launch(query, None).unwrap_or_else(|e| panic!("{query}: {e:?}"));
            assert_eq!(resolved.candidate.bundle_id, bundle, "{query}");
        }
    }

    #[test]
    fn an_exact_alias_hides_substring_matches() {
        // 招商银行 also appears inside 掌上生活's alias 招商银行信用卡.
        let ranked = rank("招商银行", None, catalog(), &[]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].candidate.bundle_id, "com.cmbchina.MPBBank");
        // Without an exact hit, partial matches come back as suggestions.
        let partial = rank("招商", None, catalog(), &[]);
        assert!(partial.len() >= 2);
        assert!(partial.iter().all(|r| !r.exact));
    }

    #[test]
    fn normalization_ignores_case_width_spacing_and_marks() {
        assert_eq!(normalize("\u{200E}WhatsApp"), "whatsapp");
        assert_eq!(normalize("ＱＱ 音乐"), "qq音乐");
        assert_eq!(normalize("  Google   Maps "), "googlemaps");
    }

    #[test]
    fn installed_rows_win_and_carry_catalog_aliases() {
        let phone = installed(json!([
            {"bundle":"com.tencent.xin","name":"WeChat","system":false},
            {"bundle":"net.whatsapp.WhatsApp","name":"\u{200E}WhatsApp","system":false},
            {"bundle":"com.example.private","name":"内部工具","system":false},
        ]));
        let ranked = rank("微信", Some(&phone), catalog(), &[]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].candidate.source, "installed");
        assert!(ranked[0].installed_verified);
        assert_eq!(ranked[0].candidate.name, "WeChat");

        let wa = rank("whatsapp", Some(&phone), catalog(), &[]);
        assert_eq!(
            wa[0].candidate.name, "WhatsApp",
            "format characters trimmed"
        );

        // An app only the phone knows about.
        let private = resolve_for_launch("内部工具", Some(&phone)).unwrap();
        assert_eq!(private.candidate.bundle_id, "com.example.private");
    }

    #[test]
    fn launch_resolution_refuses_to_guess() {
        let phone = installed(json!([
            {"bundle":"com.example.notes.a","name":"Notes Pro"},
            {"bundle":"com.example.notes.b","name":"Notes Pro"},
            {"bundle":"com.tencent.xin","name":"WeChat"},
        ]));
        match resolve_for_launch("Notes Pro", Some(&phone)) {
            Err(ResolveError::Ambiguous { candidates }) => assert_eq!(candidates.len(), 2),
            other => panic!("{other:?}"),
        }
        // Known app, but the inventory that was read does not have it.
        match resolve_for_launch("支付宝", Some(&phone)) {
            Err(ResolveError::NotInstalled { candidates }) => {
                assert_eq!(candidates[0].candidate.bundle_id, "com.alipay.iphoneclient")
            }
            other => panic!("{other:?}"),
        }
        // A partial name is never launched.
        match resolve_for_launch("WeCh", Some(&phone)) {
            Err(ResolveError::NotFound { suggestions }) => {
                assert_eq!(suggestions[0].candidate.bundle_id, "com.tencent.xin")
            }
            other => panic!("{other:?}"),
        }
        match resolve_for_launch("no such app anywhere", None) {
            Err(ResolveError::NotFound { suggestions }) => assert!(suggestions.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn system_apps_resolve_without_an_inventory() {
        assert_eq!(
            resolve_system("设置").unwrap().candidate.bundle_id,
            "com.apple.Preferences"
        );
        assert_eq!(system_app_bundle("settings"), Some("com.apple.Preferences"));
        assert_eq!(system_app_bundle("App Store"), Some("com.apple.AppStore"));
        assert_eq!(system_app_bundle("查找"), Some("com.apple.findmy"));
        assert_eq!(system_app_bundle("微信"), None);
        assert_eq!(system_app_bundle(""), None);
        assert!(resolve_system("微信").is_none());
    }

    #[test]
    fn apple_results_parse_defensively() {
        let body = json!({"resultCount":4,"results":[
            {"kind":"software","trackId":392899425,"trackName":"招商银行","bundleId":"com.cmbchina.MPBBank","artistName":"招商银行"},
            {"kind":"software","trackId":392899425,"trackName":"dup","bundleId":"com.cmbchina.MPBBank"},
            {"kind":"software","trackId":1,"trackName":"bad bundle","bundleId":"not a bundle"},
            {"kind":"podcast","trackId":2,"trackName":"podcast","bundleId":"com.example.pod"},
            {"trackName":"no track id","bundleId":"com.example.none"}
        ]});
        let rows = parse_apple(&body, "cn").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "apple");
        assert_eq!(rows[0].publisher.as_deref(), Some("招商银行"));
        assert!(parse_apple(&json!({"oops":true}), "cn").is_err());

        // Apple rows rank after the catalog and are never installed_verified.
        let ranked = rank("招商银行", Some(&[] as &[Candidate]), catalog(), &rows);
        assert_eq!(ranked.len(), 1, "same bundle merges into the catalog row");
        assert_eq!(ranked[0].candidate.source, "catalog");
        assert!(!ranked[0].installed_verified);
        assert_eq!(
            ranked[0].to_json()["store_url"],
            "https://apps.apple.com/cn/app/id392899425"
        );
    }

    #[test]
    fn the_rate_window_slides() {
        let mut window = RateWindow::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(window.try_take(t0, 3));
        }
        assert!(!window.try_take(t0 + Duration::from_secs(30), 3));
        assert!(window.try_take(t0 + Duration::from_secs(61), 3));
    }

    #[test]
    fn bundle_validation_matches_the_launch_validators() {
        assert!(valid_bundle("com.tencent.xin"));
        assert!(valid_bundle("ctrip.com"));
        assert!(valid_bundle("notion.id"));
        assert!(!valid_bundle("not a bundle"));
        assert!(!valid_bundle("nodot"));
        assert!(!valid_bundle(".com.x"));
        assert!(!valid_bundle(&"a.".repeat(150)));
    }
}
