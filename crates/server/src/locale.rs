//! Which language — Chinese or English — a person reads.
//!
//! Two audiences: a browser hitting the daemon's own pages (the
//! `iphone_use_lang` cookie the web pages' language switch sets, else
//! `Accept-Language`), and the person at this Mac (the POSIX locale
//! variables, else macOS's preferred languages — what decides for the daemon,
//! which launchd starts with no `LANG`).

use axum::http::{header, HeaderMap};

/// The cookie the web pages' 中文/English switch sets: `zh` or `en`.
pub const LANG_COOKIE: &str = "iphone_use_lang";

/// Whether a page served for this request should be in Chinese. The
/// `iphone_use_lang` cookie wins; else the first tag of `Accept-Language`
/// (`zh…` → Chinese, anything else → English); a request with neither gets
/// Chinese, as every page did before it had an English half.
pub fn request_prefers_chinese(headers: &HeaderMap) -> bool {
    if let Some(chinese) = cookie_language(headers) {
        return chinese;
    }
    match headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => accept_language_prefers_chinese(value),
        None => true,
    }
}

/// `Some(true)` for `iphone_use_lang=zh`, `Some(false)` for `=en`, `None`
/// when the cookie is absent or holds anything else.
fn cookie_language(headers: &HeaderMap) -> Option<bool> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().strip_prefix(LANG_COOKIE)?.strip_prefix('='))
        .find_map(|value| match value.trim() {
            "zh" => Some(true),
            "en" => Some(false),
            _ => None,
        })
}

/// The browser lists languages in the person's order of preference, so the
/// first real tag decides, like `navigator.languages[0]` does on the pages.
/// A header naming no language at all keeps the Chinese default.
fn accept_language_prefers_chinese(value: &str) -> bool {
    value
        .split(',')
        .map(|item| item.split(';').next().unwrap_or("").trim())
        .find(|tag| !tag.is_empty() && *tag != "*")
        .is_none_or(|tag| tag.to_ascii_lowercase().starts_with("zh"))
}

/// Whether the person at this Mac reads Chinese. The POSIX locale
/// variables decide, in their own precedence (`LC_ALL`, `LC_MESSAGES`,
/// `LANG`); when none names a language (unset, `C`, `POSIX`), the first of
/// macOS's preferred languages does, since a Chinese Mac often starts a shell
/// with no `LANG` at all — and launchd starts the daemon with none.
pub fn prefers_chinese() -> bool {
    prefers_chinese_from(|key| std::env::var(key).ok(), apple_language)
}

fn prefers_chinese_from(
    get: impl Fn(&str) -> Option<String>,
    system: impl FnOnce() -> Option<String>,
) -> bool {
    let named = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(&get)
        .map(|value| value.trim().to_ascii_lowercase())
        .find(|value| !value.is_empty());
    match named {
        Some(value) if value != "c" && value != "posix" && !value.starts_with("c.") => {
            value.starts_with("zh")
        }
        _ => system().is_some_and(|lang| lang.trim().to_ascii_lowercase().starts_with("zh")),
    }
}

/// The first entry of `AppleLanguages` (`zh-Hans-CN`, `en-US`), if any.
fn apple_language() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = std::process::Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        .ok()?;
    // `(\n    "zh-Hans-CN",\n    en\n)`: entries may or may not be quoted.
    String::from_utf8_lossy(&output.stdout)
        .split(|c: char| matches!(c, '(' | ')' | ',' | '"') || c.is_whitespace())
        .find(|token| !token.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn a_request_with_no_language_stays_chinese() {
        assert!(request_prefers_chinese(&HeaderMap::new()));
        assert!(request_prefers_chinese(&headers(&[(
            header::ACCEPT_LANGUAGE,
            ""
        )])));
        assert!(request_prefers_chinese(&headers(&[(
            header::ACCEPT_LANGUAGE,
            "*"
        )])));
    }

    #[test]
    fn the_first_accept_language_tag_decides() {
        let lang = |value| request_prefers_chinese(&headers(&[(header::ACCEPT_LANGUAGE, value)]));
        assert!(lang("zh-CN,zh;q=0.9,en;q=0.8"));
        assert!(lang("zh-Hans-CN"));
        assert!(lang("ZH-tw"));
        assert!(!lang("en-US,en;q=0.9,zh-CN;q=0.8"));
        assert!(!lang("ja"));
        assert!(!lang(" en ; q=1 , zh"));
    }

    #[test]
    fn the_cookie_outranks_the_browser() {
        let en_browser = (header::ACCEPT_LANGUAGE, "en-US");
        let zh_browser = (header::ACCEPT_LANGUAGE, "zh-CN");
        assert!(request_prefers_chinese(&headers(&[
            en_browser.clone(),
            (header::COOKIE, "phone_session=x; iphone_use_lang=zh"),
        ])));
        assert!(!request_prefers_chinese(&headers(&[
            zh_browser.clone(),
            (header::COOKIE, "iphone_use_lang=en"),
        ])));
        assert!(!request_prefers_chinese(&headers(&[(
            header::COOKIE,
            "iphone_use_lang=en"
        )])));
        // A value the switch never writes is ignored.
        assert!(!request_prefers_chinese(&headers(&[
            en_browser,
            (header::COOKIE, "iphone_use_lang=fr; iphone_use_language=zh"),
        ])));
        // Split across two Cookie headers (HTTP/2 does this).
        assert!(!request_prefers_chinese(&headers(&[
            zh_browser,
            (header::COOKIE, "a=b"),
            (header::COOKIE, "iphone_use_lang=en"),
        ])));
    }

    #[test]
    fn the_locale_variables_decide_before_the_system_language() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            }
        };
        let zh_system = || Some("zh-Hans-CN".to_string());
        let en_system = || Some("en-US".to_string());
        assert!(prefers_chinese_from(
            env(&[("LANG", "zh_CN.UTF-8")]),
            en_system
        ));
        assert!(!prefers_chinese_from(
            env(&[("LANG", "en_US.UTF-8")]),
            zh_system
        ));
        // LC_ALL outranks LANG.
        assert!(!prefers_chinese_from(
            env(&[("LC_ALL", "en_US.UTF-8"), ("LANG", "zh_CN.UTF-8")]),
            zh_system
        ));
        // No language named: the Mac's own language decides.
        assert!(prefers_chinese_from(env(&[]), zh_system));
        assert!(prefers_chinese_from(env(&[("LANG", "C.UTF-8")]), zh_system));
        assert!(prefers_chinese_from(env(&[("LC_ALL", "POSIX")]), zh_system));
        assert!(!prefers_chinese_from(env(&[]), en_system));
        assert!(!prefers_chinese_from(env(&[]), || None));
    }
}
