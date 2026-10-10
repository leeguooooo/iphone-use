//! The device runner's per-launch token, Mac side: where it lives, how setup
//! rotates it, and how every client signs a request with it.
//!
//! Setup writes a fresh token to `<state dir>/runner-token` (0600) right
//! before each runner launch and hands it to the runner in its test
//! environment; nothing else stores it and nothing logs it. The daemon, setup's
//! probes, `iphone-use device runner-status` and the video paths read the file
//! and sign each request (`core::runner_auth`). The relays never see the
//! token: they move bytes, and the signature is already in them.
//!
//! An older runner (no request signing) ignores the header, so a new daemon
//! keeps working with it through an upgrade; a new runner refuses any request
//! that is not signed with its own launch's token.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

pub use core::runner_auth::{SCHEME, TOKEN_ENV, TOKEN_FILE, XCODEBUILD_TOKEN_ENV};

/// The token file in `state_dir`.
pub fn path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(TOKEN_FILE)
}

/// Generate a fresh token and store it (0600, atomically) for this launch.
/// The previous launch's token stops working with its runner.
pub fn rotate(state_dir: &Path) -> Result<String, String> {
    let token = core::runner_auth::new_token()?;
    write(state_dir, &token)?;
    Ok(token)
}

/// Store `token` (0600, atomically, never through a symlink).
pub fn write(state_dir: &Path, token: &str) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    let path = path_in(state_dir);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("refusing to replace symlinked {}", path.display()));
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(".runner-token.")
        .tempfile_in(state_dir)
        .map_err(|e| format!("{}: {e}", state_dir.display()))?;
    std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    temporary
        .write_all(format!("{token}\n").as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    temporary
        .persist(&path)
        .map_err(|e| format!("{}: {}", path.display(), e.error))?;
    Ok(())
}

/// The token stored in `state_dir`, when there is a valid one.
pub fn read(state_dir: &Path) -> Option<String> {
    let path = path_in(state_dir);
    // Only a plain file of ours: never a symlink planted in the state dir.
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    core::runner_auth::read_token_file(&path)
}

/// A token file read on demand. Setup replaces the file at every runner
/// launch, so a long-lived client (the daemon) re-reads it whenever its
/// modification time or size changes; otherwise a request costs one stat.
#[derive(Debug)]
pub struct TokenSource {
    dir: Dir,
    cache: Mutex<Option<Cached>>,
}

/// The file's (mtime, size, inode) when read, and the token it held.
type Cached = (SystemTime, u64, u64, Option<String>);

#[derive(Debug)]
enum Dir {
    /// This process's instance state dir, resolved on first use.
    Instance,
    At(PathBuf),
    Nowhere,
}

impl TokenSource {
    /// Reads `<state_dir>/runner-token`.
    pub fn at(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Dir::At(state_dir.into()),
            cache: Mutex::new(None),
        }
    }

    /// Never signs (tests against a fake runner without auth).
    pub fn none() -> Self {
        Self {
            dir: Dir::Nowhere,
            cache: Mutex::new(None),
        }
    }

    /// This process's instance state dir.
    pub fn instance() -> Self {
        Self {
            dir: Dir::Instance,
            cache: Mutex::new(None),
        }
    }

    pub fn token(&self) -> Option<String> {
        use std::os::unix::fs::MetadataExt as _;
        let dir: &Path = match &self.dir {
            Dir::Instance => &crate::instance::current().state_dir,
            Dir::At(dir) => dir,
            Dir::Nowhere => return None,
        };
        let path = path_in(dir);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return None;
        };
        let stamp = (
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            meta.len(),
            meta.ino(),
        );
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((mtime, len, ino, token)) = cache.as_ref() {
            if (*mtime, *len, *ino) == stamp {
                return token.clone();
            }
        }
        let token = read(dir);
        *cache = Some((stamp.0, stamp.1, stamp.2, token.clone()));
        token
    }

    /// Drop the cached token so the next request reads the file again, even
    /// when its (mtime, size, inode) look unchanged.
    pub fn forget(&self) {
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The Authorization value for one request, when there is a token.
    pub fn authorization(&self, method: &str, target: &str, body: &[u8]) -> Option<String> {
        self.token()
            .map(|token| core::runner_auth::authorization(&token, method, target, body))
    }
}

/// The process-wide source for this instance (daemon and CLI).
pub fn instance_source() -> &'static TokenSource {
    static SOURCE: std::sync::OnceLock<TokenSource> = std::sync::OnceLock::new();
    SOURCE.get_or_init(TokenSource::instance)
}

/// The request target of `url` as it goes on the request line: path plus
/// `?query`.
pub fn target_of(url: &reqwest::Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    }
}

/// Sign a built request in place (no-op without a token). The body must be
/// in memory, which every runner request's is.
pub fn sign_request(source: &TokenSource, request: &mut reqwest::Request) {
    let body = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .unwrap_or_default()
        .to_vec();
    let target = target_of(request.url());
    if let Some(value) = source.authorization(request.method().as_str(), &target, &body) {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&value) {
            request
                .headers_mut()
                .insert(reqwest::header::AUTHORIZATION, value);
        }
    }
}

/// `builder.send()`, signed (no timing record; see
/// [`crate::timing::SendTimed::send_signed`] for the timed variant).
pub async fn send(
    builder: reqwest::RequestBuilder,
    source: &TokenSource,
) -> reqwest::Result<reqwest::Response> {
    let (client, request) = builder.build_split();
    let mut request = request?;
    sign_request(source, &mut request);
    client.execute(request).await
}

/// `Authorization: …\r\n` for a hand-written request, or nothing.
pub fn header_line(source: &TokenSource, method: &str, target: &str, body: &[u8]) -> String {
    source
        .authorization(method, target, body)
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_writes_a_private_file_the_source_follows() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let source = TokenSource::at(dir.path());
        assert_eq!(source.token(), None, "no file, no signing");
        let first = rotate(dir.path()).unwrap();
        let mode = std::fs::metadata(path_in(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(source.token().as_deref(), Some(first.as_str()));
        let second = rotate(dir.path()).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            source.token().as_deref(),
            Some(second.as_str()),
            "a new launch's token is picked up"
        );
        std::fs::remove_file(path_in(dir.path())).unwrap();
        assert_eq!(source.token(), None);
    }

    #[test]
    fn a_symlinked_token_file_is_ignored_and_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, format!("{}\n", "a".repeat(64))).unwrap();
        std::os::unix::fs::symlink(&elsewhere, path_in(dir.path())).unwrap();
        assert_eq!(read(dir.path()), None);
        assert!(rotate(dir.path()).is_err());
    }

    #[test]
    fn requests_are_signed_over_their_exact_target_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let token = rotate(dir.path()).unwrap();
        let source = TokenSource::at(dir.path());
        let client = reqwest::Client::new();
        let mut request = client
            .post("http://127.0.0.1:8100/session/S/actions?x=a%20b")
            .json(&serde_json::json!({"k": 1}))
            .build()
            .unwrap();
        sign_request(&source, &mut request);
        let value = request.headers()[reqwest::header::AUTHORIZATION]
            .to_str()
            .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(core::runner_auth::verify(
            &token,
            "POST",
            "/session/S/actions?x=a%20b",
            br#"{"k":1}"#,
            Some(value),
            now
        )
        .is_ok());
        let mut unsigned = client.get("http://127.0.0.1:8100/status").build().unwrap();
        sign_request(&TokenSource::none(), &mut unsigned);
        assert!(unsigned
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .is_none());
        assert_eq!(header_line(&TokenSource::none(), "GET", "/", b""), "");
        assert!(
            header_line(&source, "GET", "/", b"").starts_with("Authorization: IPU-HMAC-SHA256 ")
        );
    }
}
