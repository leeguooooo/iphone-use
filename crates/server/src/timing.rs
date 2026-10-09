//! Per-request timing for the agent API.
//!
//! Every WDA call made while serving an `/agent/*` request is recorded —
//! which route, how long until WDA answered, how many bytes it sent — and the
//! sum is returned with the response: a `timing` object in JSON bodies (agents
//! read bodies through curl/jq, not headers) and a `Server-Timing` header.
//! With it, a slow step can be split into WDA building the answer (time to
//! the response headers: WDA serialises the whole tree before sending),
//! transfer (bytes), and the daemon's own work (total minus WDA).
//!
//! The daemon also appends one JSON line per request to
//! `<state dir>/agent-timing.jsonl` (see [`set_log_path`]) — timings, route
//! and the caller's `X-Phone-Owner`, never request or screen content — so
//! real sessions can be analysed without raising the log level, which would
//! need a restart.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

tokio::task_local! {
    static RECORDER: Mutex<Recorder>;
}

#[derive(Default)]
struct Recorder {
    calls: Vec<Call>,
    /// This request's task-metrics start, registered only once the request
    /// authenticates (see [`authenticated`]); a refused request never
    /// touches anyone's metrics.
    pending: Option<crate::metrics::CallStart>,
    /// Set by [`authenticated`]. Dropped unfinished (a cancelled request), it
    /// records the call as cancelled.
    guard: Option<crate::metrics::CallGuard>,
}

/// Called by the auth checks when a request authenticates: register its
/// task-metrics call now. A no-op outside a timed request, and after the
/// first call in the same request.
pub fn authenticated() {
    let _ = RECORDER.try_with(|recorder| {
        if let Ok(mut recorder) = recorder.lock() {
            if let Some(start) = recorder.pending.take() {
                recorder.guard = Some(crate::metrics::begin_call(start));
            }
        }
    });
}

struct Call {
    route: String,
    /// When the call started (now minus its duration at record time).
    started: Instant,
    elapsed: Duration,
    bytes: Option<u64>,
    /// Dropped before it answered: a deadline (or a cancelled request) cut
    /// it short. Its time was still the runner's.
    cancelled: bool,
}

/// Record one WDA call. A no-op outside a timed request.
pub fn record(route: String, elapsed: Duration, bytes: Option<u64>) {
    record_call(route, elapsed, bytes, false);
}

fn record_call(route: String, elapsed: Duration, bytes: Option<u64>, cancelled: bool) {
    let _ = RECORDER.try_with(|recorder| {
        if let Ok(mut recorder) = recorder.lock() {
            let now = Instant::now();
            recorder.calls.push(Call {
                route,
                started: now.checked_sub(elapsed).unwrap_or(now),
                elapsed,
                bytes,
                cancelled,
            });
        }
    });
}

/// A WDA call in flight: recorded as cancelled if its future is dropped
/// before [`InFlight::finish`].
struct InFlight {
    route: Option<String>,
    started: Instant,
}

impl InFlight {
    fn finish(mut self, bytes: Option<u64>) {
        if let Some(route) = self.route.take() {
            record_call(route, self.started.elapsed(), bytes, false);
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(route) = self.route.take() {
            record_call(route, self.started.elapsed(), None, true);
        }
    }
}

/// `send()` that records the call: method + normalised path, time until the
/// LAST byte of the body arrived, and the body size. The body is read here
/// (every caller reads it whole anyway): timed to the headers only, a 1 MB
/// `/source` or screenshot spent most of its transfer in `daemon_ms`.
pub trait SendTimed {
    fn send_timed(
        self,
    ) -> impl std::future::Future<Output = reqwest::Result<reqwest::Response>> + Send;
    /// [`Self::send_timed`] for the device runner: the request is signed with
    /// the runner's per-launch token first (see [`crate::runner_token`]).
    fn send_signed(
        self,
        auth: &crate::runner_token::TokenSource,
    ) -> impl std::future::Future<Output = reqwest::Result<reqwest::Response>> + Send;
}

impl SendTimed for reqwest::RequestBuilder {
    async fn send_timed(self) -> reqwest::Result<reqwest::Response> {
        let (client, request) = self.build_split();
        execute_timed(client, request?).await
    }

    async fn send_signed(
        self,
        auth: &crate::runner_token::TokenSource,
    ) -> reqwest::Result<reqwest::Response> {
        let (client, request) = self.build_split();
        let mut request = request?;
        crate::runner_token::sign_request(auth, &mut request);
        execute_timed(client, request).await
    }
}

async fn execute_timed(
    client: reqwest::Client,
    request: reqwest::Request,
) -> reqwest::Result<reqwest::Response> {
    let route = format!(
        "{} {}",
        request.method(),
        normalize_route(request.url().path())
    );
    let call = InFlight {
        route: Some(route),
        started: Instant::now(),
    };
    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) => {
            call.finish(None);
            return Err(error);
        }
    };
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => {
            call.finish(None);
            return Err(error);
        }
    };
    call.finish(Some(body.len() as u64));
    let mut buffered = axum::http::Response::new(body);
    *buffered.status_mut() = status;
    *buffered.version_mut() = version;
    *buffered.headers_mut() = headers;
    Ok(reqwest::Response::from(buffered))
}

/// `/session/<id>/element/<id>/rect` → `/element/:id/rect`: one bucket per
/// WDA route, not per session or element.
pub fn normalize_route(path: &str) -> String {
    let mut out = Vec::new();
    let mut parts = path.split('/').filter(|p| !p.is_empty()).peekable();
    while let Some(part) = parts.next() {
        match part {
            "session" if parts.peek().is_some() => {
                parts.next(); // the session id carries no information here
            }
            "element" | "elements" => {
                out.push(part.to_string());
                if let Some(next) = parts.peek() {
                    if looks_like_id(next) {
                        parts.next();
                        out.push(":id".to_string());
                    }
                }
            }
            _ => out.push(part.to_string()),
        }
    }
    format!("/{}", out.join("/"))
}

fn looks_like_id(segment: &str) -> bool {
    segment.len() >= 8 && segment.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Routes whose bodies are long-lived streams: never buffered or rewritten.
fn is_stream(path: &str) -> bool {
    matches!(path, "/agent/mjpeg" | "/agent/h264")
}

/// Bodies larger than this are passed through with the header only.
const MAX_REWRITE_BYTES: usize = 32 << 20;

/// Where the per-request timing log goes. Unset (tests, one-shot commands):
/// nothing is written.
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
/// Serialises appends and rotation across requests.
static LOG_LOCK: Mutex<()> = Mutex::new(());
/// Past this size the log is moved to `agent-timing.jsonl.1` (one generation).
const LOG_ROTATE_BYTES: u64 = 20 << 20;

/// Called once by the daemon at startup.
pub fn set_log_path(path: PathBuf) {
    let _ = LOG_PATH.set(path);
}

fn append_log_line(line: String) {
    if let Some(path) = LOG_PATH.get() {
        append_to(path, &line);
    }
}

fn append_to(path: &std::path::Path, line: &str) {
    let Ok(_guard) = LOG_LOCK.lock() else {
        return;
    };
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= LOG_ROTATE_BYTES) {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = std::fs::rename(path, rotated);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    if let Ok(mut file) = file {
        let _ = writeln!(file, "{line}");
    }
}

/// The caller's lease name, if it sent one: short, printable, nothing else.
fn owner_of(request: &Request) -> String {
    request
        .headers()
        .get("x-phone-owner")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.chars()
                .filter(|c| c.is_ascii_graphic())
                .take(64)
                .collect::<String>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "-".to_string())
}

/// Query parameter names only: their values can be tokens or text.
fn query_keys(request: &Request) -> Vec<String> {
    request
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter_map(|pair| pair.split('=').next())
        .filter(|key| !key.is_empty())
        .map(|key| key.chars().take(32).collect())
        .collect()
}

/// axum middleware: time `/agent/*` requests and attach the breakdown.
pub async fn layer(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    if !path.starts_with("/agent/") || is_stream(&path) {
        return next.run(request).await;
    }
    let started = Instant::now();
    let started_ms = crate::metrics::now_ms();
    let method = request.method().to_string();
    let owner = owner_of(&request);
    let query = query_keys(&request);
    // Task metrics: attributed now, at the start, from the caller's own
    // headers (validated by the aggregator). Polls and the metrics routes
    // themselves are not agent work.
    let (pending, flow_call) = {
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let counted =
            !matches!(path.as_str(), "/agent/status" | "/agent/metrics" | "/agent/run");
        let flow_call = header("x-agent-call").as_deref() == Some("flow");
        let pending = counted.then(|| crate::metrics::CallStart {
            start_ms: started_ms,
            owner: header("x-phone-owner"),
            run_id: header("x-agent-run"),
        });
        (pending, flow_call)
    };
    let (response, recorder) = RECORDER
        .scope(
            Mutex::new(Recorder {
                pending,
                ..Recorder::default()
            }),
            async move {
            let response = next.run(request).await;
            let recorder = RECORDER.with(|r| match r.lock() {
                Ok(mut r) => std::mem::take(&mut *r),
                Err(_) => Recorder::default(),
            });
            (response, recorder)
        },
        )
        .await;
    let summary = Summary::new(&recorder, started.elapsed());
    if path != "/agent/status" && summary.total_ms >= 1000 {
        tracing::info!(
            path = %path,
            total_ms = summary.total_ms,
            wda_ms = summary.wda_ms,
            wda_calls = summary.calls,
            slowest = %summary.slowest(),
            "agent request timing"
        );
    }
    // Status polls (every client, every few seconds) would drown the log.
    if path != "/agent/status" && LOG_PATH.get().is_some() {
        let line = serde_json::json!({
            "ts": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            "owner": owner,
            "method": method,
            "path": path,
            "query": query,
            "status": response.status().as_u16(),
            "timing": summary.json(),
        })
        .to_string();
        tokio::task::spawn_blocking(move || append_log_line(line));
    }
    let status = response.status();
    let (response, json) = attach(response, &summary).await;
    let mut recorder = recorder;
    if let Some(guard) = recorder.guard.take() {
        {
            let end_ms = started_ms.saturating_add(started.elapsed().as_millis() as u64);
            let runner_intervals = recorder
                .calls
                .iter()
                .map(|call| {
                    let offset = call.started.saturating_duration_since(started).as_millis() as u64;
                    let from = started_ms.saturating_add(offset);
                    (from, from.saturating_add(call.elapsed.as_millis() as u64))
                })
                .collect();
            let kind = if flow_call {
                crate::metrics::CallKind::Flow
            } else if path == "/agent/actions" {
                crate::metrics::CallKind::Batch
            } else {
                crate::metrics::CallKind::Single
            };
            guard.finish(
                crate::metrics::CallEnd {
                    end_ms,
                    kind,
                    // Only an action can be observed; a read's diff is a read.
                    observed: matches!(path.as_str(), "/agent/input" | "/agent/actions")
                        && json
                            .as_ref()
                            .is_some_and(|j| j.get("delta").is_some() || j.get("settle").is_some()),
                    outcome: classify(status, json.as_ref()),
                    runner_intervals,
                },
            );
        }
    }
    response
}

/// What a response says about its call, from its status and JSON body.
fn classify(
    status: axum::http::StatusCode,
    json: Option<&serde_json::Value>,
) -> crate::metrics::Outcome {
    use crate::metrics::{FailureClass, Outcome};
    let str_of = |key: &str| json.and_then(|j| j.get(key)).and_then(serde_json::Value::as_str);
    let error = str_of("error");
    if error == Some("stale_element_snapshot") {
        return Outcome::Stale;
    }
    if error == Some("outcome_unknown") || str_of("outcome") == Some("unknown") {
        return Outcome::OutcomeUnknown;
    }
    let ok = json.and_then(|j| j.get("ok")).and_then(serde_json::Value::as_bool);
    match (status.is_success(), ok) {
        (true, Some(true)) | (true, None) => Outcome::Ok,
        _ => Outcome::Failed(error.map_or(FailureClass::Other, FailureClass::from_code)),
    }
}

struct Summary {
    total_ms: u64,
    wda_ms: u64,
    calls: usize,
    /// Calls a deadline dropped before they answered.
    cancelled: usize,
    /// Per route, in first-call order: (route, count, ms, bytes).
    routes: Vec<(String, usize, u64, u64)>,
}

impl Summary {
    fn new(recorder: &Recorder, total: Duration) -> Self {
        let mut routes: Vec<(String, usize, u64, u64)> = Vec::new();
        let mut wda_ms = 0;
        for call in &recorder.calls {
            let ms = call.elapsed.as_millis() as u64;
            wda_ms += ms;
            match routes.iter_mut().find(|r| r.0 == call.route) {
                Some(r) => {
                    r.1 += 1;
                    r.2 += ms;
                    r.3 += call.bytes.unwrap_or(0);
                }
                None => routes.push((call.route.clone(), 1, ms, call.bytes.unwrap_or(0))),
            }
        }
        Summary {
            total_ms: total.as_millis() as u64,
            wda_ms,
            calls: recorder.calls.len(),
            cancelled: recorder.calls.iter().filter(|call| call.cancelled).count(),
            routes,
        }
    }

    fn slowest(&self) -> String {
        self.routes
            .iter()
            .max_by_key(|r| r.2)
            .map(|r| format!("{} x{} {}ms", r.0, r.1, r.2))
            .unwrap_or_default()
    }

    fn json(&self) -> serde_json::Value {
        let mut json = serde_json::json!({
            "total_ms": self.total_ms,
            "wda_ms": self.wda_ms,
            "daemon_ms": self.total_ms.saturating_sub(self.wda_ms),
            "wda": self.routes.iter().map(|(route, count, ms, bytes)| serde_json::json!({
                "call": route, "count": count, "ms": ms, "bytes": bytes,
            })).collect::<Vec<_>>(),
        });
        if self.cancelled > 0 {
            json["wda_cancelled"] = serde_json::json!(self.cancelled);
        }
        json
    }

    /// `total;dur=…, wda;dur=…, wda-get-source;dur=…;desc="1x 812345B"`.
    fn server_timing(&self) -> String {
        let mut parts = vec![
            format!("total;dur={}", self.total_ms),
            format!("wda;dur={}", self.wda_ms),
        ];
        for (route, count, ms, bytes) in &self.routes {
            let mut name = String::new();
            for c in format!("wda-{route}").to_ascii_lowercase().chars() {
                let c = if c.is_ascii_alphanumeric() { c } else { '-' };
                if !(c == '-' && name.ends_with('-')) {
                    name.push(c);
                }
            }
            parts.push(format!(
                "{};dur={ms};desc=\"{count}x {bytes}B\"",
                name.trim_matches('-')
            ));
        }
        parts.join(", ")
    }
}

/// Add the timing header (and, for a JSON body that reached WDA, the timing
/// field), and hand back the parsed body for the task metrics. A body that
/// is not JSON, too large, or of unknown size passes through unread.
async fn attach(response: Response, summary: &Summary) -> (Response, Option<serde_json::Value>) {
    let (mut parts, body) = response.into_parts();
    if let Ok(value) = HeaderValue::from_str(&summary.server_timing()) {
        parts.headers.insert("server-timing", value);
    }
    let is_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));
    if !is_json {
        return (Response::from_parts(parts, body), None);
    }
    // Buffer only a body of known size under the cap: reading anything else
    // and failing part-way would leave nothing to send but an empty 200.
    let exact = axum::body::HttpBody::size_hint(&body).exact();
    if exact.is_none_or(|len| len > MAX_REWRITE_BYTES as u64) {
        return (Response::from_parts(parts, body), None);
    }
    let bytes = match axum::body::to_bytes(body, MAX_REWRITE_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            // The handler already ran: whatever it did may have happened.
            // Say the outcome is unknown rather than inventing a failure.
            parts.headers.remove(header::CONTENT_LENGTH);
            return (
                Response::from_parts(parts, Body::from(UNKNOWN_BODY)),
                serde_json::from_str(UNKNOWN_BODY).ok(),
            );
        }
    };
    let json = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
    // A request that never reached WDA keeps its body byte for byte; the
    // header still carries its total.
    if summary.calls == 0 {
        return (Response::from_parts(parts, Body::from(bytes)), json);
    }
    match with_timing_field(&bytes, &summary.json()) {
        Some(body) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            (Response::from_parts(parts, Body::from(body)), json)
        }
        None => (Response::from_parts(parts, Body::from(bytes)), json),
    }
}

/// Sent when a response body could not be read back after its handler ran.
pub const UNKNOWN_BODY: &str =
    r#"{"ok":false,"error":"outcome_unknown","outcome":"unknown","retry_safe":false,"hint":"the response could not be read back; re-read the screen before retrying"}"#;

/// Splice `"timing":{…}` in before the object's closing brace, leaving every
/// other byte (and so the field order) as the handler wrote it. `None` for
/// anything that is not a JSON object or already has a `timing` field.
fn with_timing_field(body: &[u8], timing: &serde_json::Value) -> Option<Vec<u8>> {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(object)) if !object.contains_key("timing") => {
            let end = body.iter().rposition(|b| *b == b'}')?;
            let mut out = Vec::with_capacity(body.len() + 256);
            out.extend_from_slice(&body[..end]);
            if !object.is_empty() {
                out.push(b',');
            }
            out.extend_from_slice(b"\"timing\":");
            out.extend_from_slice(timing.to_string().as_bytes());
            out.extend_from_slice(&body[end..]);
            Some(out)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_are_bucketed_without_ids() {
        assert_eq!(
            normalize_route("/session/0A1B2C3D-AAAA-BBBB-CCCC-0123456789AB/source"),
            "/source"
        );
        assert_eq!(
            normalize_route("/session/abc12345/element/1F000000-0000-0000-0000-000000000042/rect"),
            "/element/:id/rect"
        );
        assert_eq!(normalize_route("/window/size"), "/window/size");
        assert_eq!(normalize_route("/session/x1/elements"), "/elements");
        assert_eq!(normalize_route("/wda/locked"), "/wda/locked");
    }

    #[test]
    fn summary_groups_calls_by_route() {
        let recorder = Recorder {
            calls: vec![
                Call {
                    route: "GET /source".into(),
                    started: Instant::now(),
                    elapsed: Duration::from_millis(6000),
                    bytes: Some(800_000),
                    cancelled: false,
                },
                Call {
                    route: "GET /window/size".into(),
                    started: Instant::now(),
                    elapsed: Duration::from_millis(180),
                    bytes: Some(90),
                    cancelled: false,
                },
                Call {
                    route: "GET /source".into(),
                    started: Instant::now(),
                    elapsed: Duration::from_millis(5000),
                    bytes: Some(700_000),
                    cancelled: false,
                },
            ],
            ..Recorder::default()
        };
        let s = Summary::new(&recorder, Duration::from_millis(11_500));
        assert_eq!(s.wda_ms, 11_180);
        assert_eq!(s.calls, 3);
        assert_eq!(s.routes[0], ("GET /source".into(), 2, 11_000, 1_500_000));
        let json = s.json();
        assert_eq!(json["daemon_ms"], 320);
        assert_eq!(json["wda"][0]["count"], 2);
        let header = s.server_timing();
        assert!(
            header.starts_with(
                "total;dur=11500, wda;dur=11180, wda-get-source;dur=11000;desc=\"2x 1500000B\""
            ),
            "{header}"
        );
    }

    #[test]
    fn timing_is_spliced_in_without_reordering() {
        let t = serde_json::json!({"total_ms": 5});
        let out = with_timing_field(br#"{"ok":true,"b":1,"a":2}"#, &t).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"ok":true,"b":1,"a":2,"timing":{"total_ms":5}}"#
        );
        let out = with_timing_field(b"{ }", &t).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{ "timing":{"total_ms":5}}"#
        );
        assert!(with_timing_field(b"[1,2]", &t).is_none());
        assert!(with_timing_field(br#"{"timing":1}"#, &t).is_none());
        assert!(with_timing_field(b"not json", &t).is_none());
    }

    /// A server that sends its headers at once and the body after `delay`.
    fn slow_body_server(delay: Duration) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                std::thread::spawn(move || {
                    let mut buffer = [0_u8; 4096];
                    let _ = stream.read(&mut buffer);
                    let body = r#"{"value":null}"#;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.flush();
                    std::thread::sleep(delay);
                    let _ = stream.write_all(body.as_bytes());
                });
            }
        });
        base
    }

    /// A WDA call is timed to the last byte of its body, and one a deadline
    /// drops is still recorded (as cancelled) instead of vanishing into
    /// `daemon_ms`.
    #[test]
    fn calls_are_timed_to_the_end_of_the_body_and_kept_when_cancelled() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let base = slow_body_server(Duration::from_millis(300));
                let client = reqwest::Client::new();
                let recorder = RECORDER
                    .scope(Mutex::new(Recorder::default()), async {
                        let response = client
                            .get(format!("{base}/status"))
                            .send_timed()
                            .await
                            .unwrap();
                        assert_eq!(response.text().await.unwrap(), r#"{"value":null}"#);
                        let cut = tokio::time::timeout(
                            Duration::from_millis(100),
                            client.get(format!("{base}/source")).send_timed(),
                        )
                        .await;
                        assert!(cut.is_err());
                        RECORDER.with(|r| std::mem::take(&mut *r.lock().unwrap()))
                    })
                    .await;
                let [whole, cut] = recorder.calls.as_slice() else {
                    panic!("two calls recorded");
                };
                assert_eq!(whole.route, "GET /status");
                assert!(
                    whole.elapsed >= Duration::from_millis(280),
                    "{:?}",
                    whole.elapsed
                );
                assert_eq!(whole.bytes, Some(14));
                assert!(!whole.cancelled);
                assert_eq!(cut.route, "GET /source");
                assert!(cut.cancelled);
                let summary = Summary::new(&recorder, Duration::from_millis(500));
                assert_eq!(summary.json()["wda_cancelled"], 1);
            });
    }

    // A hand-built runtime: `#[tokio::test]` resolves `core::` to this
    // workspace's own `core` crate.
    #[test]
    fn an_oversized_or_unsized_json_body_passes_through_untouched() {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(oversized_or_unsized_body_case());
    }

    async fn oversized_or_unsized_body_case() {
        let summary = Summary::new(
            &Recorder {
                calls: vec![Call {
                    route: "GET /source".into(),
                    started: Instant::now(),
                    elapsed: Duration::from_millis(5),
                    bytes: None,
                    cancelled: false,
                }],
                ..Recorder::default()
            },
            Duration::from_millis(9),
        );
        let big = vec![b' '; MAX_REWRITE_BYTES + 1];
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(big.clone()))
            .unwrap();
        let (out, json) = attach(response, &summary).await;
        assert!(json.is_none(), "an oversized body is passed through unread");
        assert_eq!(out.status(), axum::http::StatusCode::OK);
        assert!(out.headers().contains_key("server-timing"));
        let body = axum::body::to_bytes(out.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            body.len(),
            big.len(),
            "the body must reach the client intact"
        );

        let stream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
            bytes::Bytes::from_static(br#"{"ok":true}"#),
        )]);
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(stream))
            .unwrap();
        let (out, _) = attach(response, &summary).await;
        let body = axum::body::to_bytes(out.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], br#"{"ok":true}"#);
    }

    #[test]
    fn log_lines_name_the_owner_and_query_keys_but_never_values() {
        let request = Request::builder()
            .uri("/agent/elements?since=SECRETTOKEN&filter=hello")
            .header("x-phone-owner", "paypay-check")
            .body(Body::empty())
            .unwrap();
        assert_eq!(owner_of(&request), "paypay-check");
        assert_eq!(query_keys(&request), vec!["since", "filter"]);
        let anonymous = Request::builder()
            .uri("/agent/elements")
            .body(Body::empty())
            .unwrap();
        assert_eq!(owner_of(&anonymous), "-");
        assert!(query_keys(&anonymous).is_empty());
    }

    #[test]
    fn the_log_appends_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-timing.jsonl");
        append_to(&path, r#"{"n":1}"#);
        append_to(&path, r#"{"n":2}"#);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"n\":1}\n{\"n\":2}\n"
        );
        // Past the cap the current file becomes .1 and a fresh one starts.
        std::fs::write(&path, vec![b'x'; LOG_ROTATE_BYTES as usize]).unwrap();
        append_to(&path, r#"{"n":3}"#);
        assert_eq!(
            std::fs::metadata(dir.path().join("agent-timing.jsonl.1"))
                .unwrap()
                .len(),
            LOG_ROTATE_BYTES
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"n\":3}\n");
    }

    #[test]
    fn record_outside_a_request_is_harmless() {
        record("GET /status".into(), Duration::from_millis(1), None);
    }
}
