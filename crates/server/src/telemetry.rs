//! Anonymous usage telemetry — infrastructure only, OFF until a project token
//! is configured. See `docs/telemetry.md`.
//!
//! When enabled, the daemon counts its own `/agent/*` calls: which endpoint
//! (from a fixed list), how it ended, a failure class (from a fixed list,
//! else `other`), and how long it took. Events go in batches to the PostHog
//! capture API from a background task. Nothing else is ever sent: no text,
//! labels, screenshots, bundle ids, UDIDs, phone names, owner names or paths.
//!
//! Enabled only when ALL of these hold:
//! - a project token: `IPHONE_USE_TELEMETRY_TOKEN`, else [`BUILT_IN_TOKEN`]
//!   (empty in this build, so nothing is sent unless the environment sets one);
//! - `IPHONE_USE_TELEMETRY` is not `0`/`false`/`off`/`no`;
//! - `DO_NOT_TRACK` is not `1`/`true`/`yes`.
//!
//! A disabled daemon creates no install id and spawns no task. Delivery can
//! never affect a phone action: [`record`] only `try_send`s onto a bounded
//! queue (full = dropped), and the sender runs on its own task with a short
//! timeout.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// The project token shipped in this build. Deliberately empty: telemetry is
/// off for every install until a release sets one (and says so in the docs).
pub const BUILT_IN_TOKEN: &str = "";

const CAPTURE_URL: &str = "https://us.i.posthog.com/batch/";
/// The capture hosts a project may live on (PostHog's US and EU clouds).
/// `IPHONE_USE_TELEMETRY_HOST` picks one; anything else falls back to US.
const CAPTURE_HOSTS: &[&str] = &["us.i.posthog.com", "eu.i.posthog.com"];
const ID_FILE: &str = "telemetry-id";
const QUEUE_CAP: usize = 256;
const BATCH_MAX: usize = 20;
const FLUSH_DELAY: Duration = Duration::from_secs(5);
const SEND_TIMEOUT: Duration = Duration::from_secs(3);
/// Durations are capped (an hour) so an outlier cannot carry anything odd.
const MAX_DURATION_MS: u64 = 3_600_000;

/// Endpoints that are counted. Anything else — status polls, video streams,
/// the browser UI, schedules (their paths carry ids) — is never recorded.
pub const ENDPOINTS: &[&str] = &[
    "/agent/input",
    "/agent/actions",
    "/agent/elements",
    "/agent/screenshot",
    "/agent/collect",
    "/agent/scroll_find",
    "/agent/mode",
    "/agent/hold",
    "/agent/owner",
    "/agent/prewarm",
    "/agent/login",
    "/agent/login/code",
    "/agent/apps",
    "/agent/intents",
    "/agent/intent",
    "/agent/capabilities",
    "/agent/flow/draft",
    "/agent/reference",
];

/// Error codes that may be sent; [`crate::metrics::FailureClass`] plus the
/// two non-failure outcomes. Everything else is `other`.
pub const ERROR_CODES: &[&str] = &[
    "none",
    "element_not_found",
    "element_not_visible",
    "element_occluded",
    "ambiguous_element_label",
    "value_not_applied",
    "expectation_timeout",
    "phone_owned",
    "not_drivable",
    "transport",
    "stale_element_snapshot",
    "outcome_unknown",
    "other",
];

/// Property keys an event may carry. Checked again just before sending.
const PROPERTY_KEYS: &[&str] = &[
    "$lib",
    "$process_person_profile",
    "$geoip_disable",
    "app_version",
    "platform",
    "arch",
    "session_id",
    "endpoint",
    "outcome",
    "error_code",
    "duration_ms",
    "via",
];

/// Whether telemetry is on, and with which token. `env` reads a variable
/// (injected so tests never touch the process environment).
pub fn token_from(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    let off = |name: &str, values: &[&str]| {
        env(name).is_some_and(|v| values.contains(&v.trim().to_ascii_lowercase().as_str()))
    };
    if off("IPHONE_USE_TELEMETRY", &["0", "false", "off", "no"])
        || off("DO_NOT_TRACK", &["1", "true", "yes"])
    {
        return None;
    }
    let token = env("IPHONE_USE_TELEMETRY_TOKEN").unwrap_or_else(|| BUILT_IN_TOKEN.to_string());
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// A random version-4 UUID.
fn random_uuid() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The install's random id, kept in `state_dir` (mode 0600). It identifies
/// an install, not a person or a phone; deleting the file makes a new one.
pub fn install_id(state_dir: &Path) -> Option<String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = state_dir.join(ID_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let text = text.trim();
        if is_uuid(text) {
            return Some(text.to_string());
        }
    }
    let id = random_uuid()?;
    std::fs::create_dir_all(state_dir).ok()?;
    let temporary = state_dir.join(format!("{ID_FILE}.{}.tmp", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| file.write_all(id.as_bytes()));
    let renamed = written.and_then(|()| std::fs::rename(&temporary, &path));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return None;
    }
    Some(id)
}

/// Delivers one batch; answers the HTTP status. Boxed so tests can inject a
/// fake behind `dyn`.
pub trait Sender: Send + Sync + 'static {
    fn send<'a>(
        &'a self,
        token: &'a str,
        batch: &'a [serde_json::Value],
    ) -> Pin<Box<dyn Future<Output = Result<u16, String>> + Send + 'a>>;
}

/// The real sender: PostHog's batch capture endpoint.
pub struct PosthogSender {
    client: reqwest::Client,
    url: String,
}

/// The batch URL for `host` when it is one of [`CAPTURE_HOSTS`].
pub fn capture_url(host: Option<&str>) -> String {
    match host.map(str::trim).filter(|h| CAPTURE_HOSTS.contains(h)) {
        Some(host) => format!("https://{host}/batch/"),
        None => CAPTURE_URL.to_string(),
    }
}

impl PosthogSender {
    pub fn new() -> Option<Self> {
        let client = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .build()
            .ok()?;
        Some(Self {
            client,
            url: capture_url(std::env::var("IPHONE_USE_TELEMETRY_HOST").ok().as_deref()),
        })
    }
}

impl Sender for PosthogSender {
    fn send<'a>(
        &'a self,
        token: &'a str,
        batch: &'a [serde_json::Value],
    ) -> Pin<Box<dyn Future<Output = Result<u16, String>> + Send + 'a>> {
        Box::pin(async move {
            let body = serde_json::json!({"api_key": token, "batch": batch});
            let response = self
                .client
                .post(&self.url)
                .json(&body)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            Ok(response.status().as_u16())
        })
    }
}

/// How a counted call ended, from the daemon's own classification.
pub fn outcome_fields(outcome: crate::metrics::Outcome) -> (&'static str, &'static str) {
    use crate::metrics::{FailureClass, Outcome};
    match outcome {
        Outcome::Ok => ("ok", "none"),
        Outcome::Stale => ("error", "stale_element_snapshot"),
        Outcome::OutcomeUnknown => ("unknown", "outcome_unknown"),
        Outcome::Failed(class) => (
            "error",
            match class {
                FailureClass::ElementNotFound => "element_not_found",
                FailureClass::ElementNotVisible => "element_not_visible",
                FailureClass::ElementOccluded => "element_occluded",
                FailureClass::AmbiguousElementLabel => "ambiguous_element_label",
                FailureClass::ValueNotApplied => "value_not_applied",
                FailureClass::ExpectationTimeout => "expectation_timeout",
                FailureClass::PhoneOwned => "phone_owned",
                FailureClass::NotDrivable => "not_drivable",
                FailureClass::Transport => "transport",
                FailureClass::Other => "other",
            },
        ),
    }
}

/// Keep only allowed keys, and only allowed values for the enumerated ones.
fn sanitize(properties: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(map) = properties else {
        return serde_json::json!({});
    };
    let mut clean = serde_json::Map::new();
    for (key, value) in map {
        if !PROPERTY_KEYS.contains(&key.as_str()) {
            continue;
        }
        let value = match key.as_str() {
            "endpoint" => match value.as_str() {
                Some(endpoint) if ENDPOINTS.contains(&endpoint) => value,
                _ => continue,
            },
            "error_code" => match value.as_str() {
                Some(code) if ERROR_CODES.contains(&code) => value,
                _ => "other".into(),
            },
            "outcome" => match value.as_str() {
                Some("ok" | "error" | "unknown") => value,
                _ => continue,
            },
            "via" => match value.as_str() {
                Some("flow" | "direct") => value,
                _ => continue,
            },
            "duration_ms" => match value.as_u64() {
                Some(ms) => ms.min(MAX_DURATION_MS).into(),
                None => continue,
            },
            _ => value,
        };
        clean.insert(key, value);
    }
    serde_json::Value::Object(clean)
}

/// A running telemetry pipeline: a bounded queue and its delivery task.
pub struct Telemetry {
    tx: tokio::sync::mpsc::Sender<serde_json::Value>,
    distinct_id: String,
    session_id: String,
}

impl Telemetry {
    /// Start delivery on the current tokio runtime. `None` when the install
    /// id cannot be made (then nothing is ever sent).
    pub fn start(
        token: String,
        state_dir: &Path,
        sender: Arc<dyn Sender>,
        flush_delay: Duration,
    ) -> Option<Self> {
        let distinct_id = install_id(state_dir)?;
        let session_id = random_uuid()?;
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_CAP);
        tokio::spawn(deliver(rx, token, sender, flush_delay));
        let telemetry = Self {
            tx,
            distinct_id,
            session_id,
        };
        telemetry.capture("iphone_use_daemon_started", serde_json::json!({}));
        Some(telemetry)
    }

    /// Queue one event; never blocks. `false` when it was dropped.
    fn capture(&self, event: &str, properties: serde_json::Value) -> bool {
        let mut properties = sanitize(properties);
        let common = serde_json::json!({
            "$lib": "iphone-use-daemon",
            "$process_person_profile": false,
            "$geoip_disable": true,
            "app_version": env!("CARGO_PKG_VERSION"),
            "platform": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "session_id": self.session_id,
        });
        if let (Some(target), serde_json::Value::Object(common)) =
            (properties.as_object_mut(), common)
        {
            target.extend(common);
        }
        let Some(uuid) = random_uuid() else {
            return false;
        };
        let event = serde_json::json!({
            "event": event,
            "distinct_id": self.distinct_id,
            "uuid": uuid,
            "properties": properties,
        });
        self.tx.try_send(event).is_ok()
    }

    /// Count one finished `/agent/*` call. Unlisted endpoints are ignored.
    pub fn record(
        &self,
        endpoint: &str,
        outcome: crate::metrics::Outcome,
        duration: Duration,
        via_flow: bool,
    ) -> bool {
        if !ENDPOINTS.contains(&endpoint) {
            return false;
        }
        let (outcome, error_code) = outcome_fields(outcome);
        self.capture(
            "iphone_use_request",
            serde_json::json!({
                "endpoint": endpoint,
                "outcome": outcome,
                "error_code": error_code,
                "duration_ms": duration.as_millis() as u64,
                "via": if via_flow { "flow" } else { "direct" },
            }),
        )
    }
}

/// Batch and send until the queue closes. One retry for a network error,
/// 429 or 5xx; any other answer drops the batch. Nothing is written to disk.
async fn deliver(
    mut rx: tokio::sync::mpsc::Receiver<serde_json::Value>,
    token: String,
    sender: Arc<dyn Sender>,
    flush_delay: Duration,
) {
    while let Some(first) = rx.recv().await {
        // Collect for up to `flush_delay`, but send as soon as a batch is full,
        // so a burst drains instead of overflowing the queue.
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + flush_delay;
        while batch.len() < BATCH_MAX {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(event)) => batch.push(event),
                _ => break,
            }
        }
        for attempt in 0..2 {
            match sender.send(&token, &batch).await {
                Ok(status) if status != 429 && status < 500 => break,
                _ if attempt == 0 => tokio::time::sleep(Duration::from_secs(1)).await,
                _ => {}
            }
        }
    }
}

static GLOBAL: OnceLock<Telemetry> = OnceLock::new();

/// Called once by the daemon at startup, inside its runtime. Reads the
/// environment; does nothing (no id file, no task) when telemetry is off.
pub fn init_from_env(state_dir: PathBuf) -> bool {
    let Some(token) = token_from(|name| std::env::var(name).ok()) else {
        return false;
    };
    let Some(sender) = PosthogSender::new() else {
        return false;
    };
    match Telemetry::start(token, &state_dir, Arc::new(sender), FLUSH_DELAY) {
        Some(telemetry) => {
            tracing::info!("anonymous usage telemetry is on (IPHONE_USE_TELEMETRY=0 turns it off)");
            GLOBAL.set(telemetry).is_ok()
        }
        None => false,
    }
}

/// Count one finished call, if telemetry was started. A no-op otherwise.
pub fn record(
    endpoint: &str,
    outcome: crate::metrics::Outcome,
    duration: Duration,
    via_flow: bool,
) {
    if let Some(telemetry) = GLOBAL.get() {
        telemetry.record(endpoint, outcome, duration, via_flow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{FailureClass, Outcome};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        batches: Mutex<Vec<Vec<serde_json::Value>>>,
        tokens: Mutex<Vec<String>>,
        answer: Mutex<Vec<Result<u16, String>>>,
    }

    impl Sender for Fake {
        fn send<'a>(
            &'a self,
            token: &'a str,
            batch: &'a [serde_json::Value],
        ) -> Pin<Box<dyn Future<Output = Result<u16, String>> + Send + 'a>> {
            self.batches.lock().unwrap().push(batch.to_vec());
            self.tokens.lock().unwrap().push(token.to_string());
            let answer = self.answer.lock().unwrap().pop().unwrap_or(Ok(200));
            Box::pin(async move { answer })
        }
    }

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn off_without_a_token_and_under_either_opt_out() {
        assert_eq!(BUILT_IN_TOKEN, "", "this build ships no token");
        assert_eq!(token_from(env(&[])), None);
        assert_eq!(capture_url(None), "https://us.i.posthog.com/batch/");
        assert_eq!(capture_url(Some("eu.i.posthog.com")), "https://eu.i.posthog.com/batch/");
        assert_eq!(capture_url(Some("evil.example")), "https://us.i.posthog.com/batch/");
        assert_eq!(
            token_from(env(&[("IPHONE_USE_TELEMETRY_TOKEN", "  ")])),
            None
        );
        assert_eq!(
            token_from(env(&[("IPHONE_USE_TELEMETRY_TOKEN", "phc_x")])).as_deref(),
            Some("phc_x")
        );
        for opt_out in [
            &[
                ("IPHONE_USE_TELEMETRY_TOKEN", "phc_x"),
                ("IPHONE_USE_TELEMETRY", "0"),
            ][..],
            &[
                ("IPHONE_USE_TELEMETRY_TOKEN", "phc_x"),
                ("IPHONE_USE_TELEMETRY", "off"),
            ][..],
            &[
                ("IPHONE_USE_TELEMETRY_TOKEN", "phc_x"),
                ("DO_NOT_TRACK", "1"),
            ][..],
            &[
                ("IPHONE_USE_TELEMETRY_TOKEN", "phc_x"),
                ("DO_NOT_TRACK", "true"),
            ][..],
        ] {
            let pairs: Vec<(String, String)> = opt_out
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let lookup = move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            };
            assert_eq!(token_from(lookup), None, "{opt_out:?}");
        }
    }

    #[test]
    fn the_global_is_a_no_op_until_started() {
        // Never started in tests: recording must do nothing and not panic.
        record("/agent/input", Outcome::Ok, Duration::from_millis(5), false);
        assert!(GLOBAL.get().is_none());
    }

    #[test]
    fn the_install_id_is_random_private_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let first = install_id(dir.path()).unwrap();
        assert!(is_uuid(&first), "{first}");
        assert_eq!(install_id(dir.path()).unwrap(), first);
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir.path().join(ID_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let other = tempfile::tempdir().unwrap();
        assert_ne!(install_id(other.path()).unwrap(), first);
    }

    #[test]
    fn events_carry_only_allow_listed_fields() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        runtime().block_on(async {
            let telemetry =
                Telemetry::start("phc_test".into(), dir.path(), fake.clone(), Duration::from_millis(10)).unwrap();
            assert!(telemetry.record(
                "/agent/collect",
                Outcome::Failed(FailureClass::ElementOccluded),
                Duration::from_millis(1234),
                false
            ));
            assert!(telemetry.record("/agent/actions", Outcome::Ok, Duration::from_secs(99_999), true));
            assert!(!telemetry.record("/agent/status", Outcome::Ok, Duration::ZERO, false), "status polls are not counted");
            assert!(!telemetry.record("/agent/schedules/abc", Outcome::Ok, Duration::ZERO, false));
            // A caller slipping in a free-form field or value gets neither through.
            assert!(telemetry.capture(
                "iphone_use_request",
                serde_json::json!({"endpoint": "/agent/input", "error_code": "a label 设置", "label": "设置", "path": "/Users/x"})
            ));
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let batches = fake.batches.lock().unwrap();
        let events: Vec<&serde_json::Value> = batches.iter().flatten().collect();
        assert_eq!(events.len(), 4, "{events:?}");
        assert_eq!(fake.tokens.lock().unwrap()[0], "phc_test");
        assert_eq!(events[0]["event"], "iphone_use_daemon_started");
        let collect = &events[1]["properties"];
        assert_eq!(collect["endpoint"], "/agent/collect");
        assert_eq!(collect["outcome"], "error");
        assert_eq!(collect["error_code"], "element_occluded");
        assert_eq!(collect["duration_ms"], 1234);
        assert_eq!(collect["via"], "direct");
        assert_eq!(collect["$process_person_profile"], false);
        assert_eq!(events[2]["properties"]["duration_ms"], MAX_DURATION_MS);
        assert_eq!(events[3]["properties"]["error_code"], "other");
        let id = install_id(dir.path()).unwrap();
        for event in &events {
            assert_eq!(event["distinct_id"], id.as_str());
            for key in event["properties"].as_object().unwrap().keys() {
                assert!(
                    PROPERTY_KEYS.contains(&key.as_str()),
                    "unexpected property {key}"
                );
            }
            let text = event.to_string();
            assert!(!text.contains("设置") && !text.contains("/Users"), "{text}");
        }
    }

    #[test]
    fn a_failing_sender_is_retried_once_and_never_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        *fake.answer.lock().unwrap() = vec![Err("offline".into()), Ok(503)];
        runtime().block_on(async {
            let telemetry = Telemetry::start(
                "phc_test".into(),
                dir.path(),
                fake.clone(),
                Duration::from_millis(5),
            )
            .unwrap();
            telemetry.record("/agent/input", Outcome::Ok, Duration::from_millis(1), false);
            tokio::time::sleep(Duration::from_millis(1500)).await;
        });
        // started + input in one batch: 503, then the retry fails too — dropped.
        assert_eq!(fake.batches.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_full_queue_drops_instead_of_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        runtime().block_on(async {
            // A long flush delay keeps the queue full while we record.
            let telemetry = Telemetry::start(
                "phc_test".into(),
                dir.path(),
                fake.clone(),
                Duration::from_secs(60),
            )
            .unwrap();
            tokio::task::yield_now().await;
            let accepted = (0..QUEUE_CAP + 50)
                .filter(|_| telemetry.record("/agent/input", Outcome::Ok, Duration::ZERO, false))
                .count();
            assert!(accepted <= QUEUE_CAP, "{accepted}");
            assert!(accepted >= QUEUE_CAP - 1, "{accepted}");
        });
    }
}
