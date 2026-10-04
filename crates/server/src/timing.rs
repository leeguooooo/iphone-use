//! Per-request timing for the agent API.
//!
//! Every WDA call made while serving an `/agent/*` request is recorded —
//! which route, how long until WDA answered, how many bytes it sent — and the
//! sum is returned with the response: a `timing` object in JSON bodies (agents
//! read bodies through curl/jq, not headers) and a `Server-Timing` header.
//! With it, a slow step can be split into WDA building the answer (time to
//! the response headers: WDA serialises the whole tree before sending),
//! transfer (bytes), and the daemon's own work (total minus WDA).

use std::sync::Mutex;
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
}

struct Call {
    route: String,
    elapsed: Duration,
    bytes: Option<u64>,
}

/// Record one WDA call. A no-op outside a timed request.
pub fn record(route: String, elapsed: Duration, bytes: Option<u64>) {
    let _ = RECORDER.try_with(|recorder| {
        if let Ok(mut recorder) = recorder.lock() {
            recorder.calls.push(Call {
                route,
                elapsed,
                bytes,
            });
        }
    });
}

/// `send()` that records the call: method + normalised path, time until the
/// response headers arrived, and the body size WDA declared.
pub trait SendTimed {
    fn send_timed(
        self,
    ) -> impl std::future::Future<Output = reqwest::Result<reqwest::Response>> + Send;
}

impl SendTimed for reqwest::RequestBuilder {
    async fn send_timed(self) -> reqwest::Result<reqwest::Response> {
        let (client, request) = self.build_split();
        let request = request?;
        let route = format!(
            "{} {}",
            request.method(),
            normalize_route(request.url().path())
        );
        let started = Instant::now();
        let result = client.execute(request).await;
        let bytes = result.as_ref().ok().and_then(|r| r.content_length());
        record(route, started.elapsed(), bytes);
        result
    }
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

/// axum middleware: time `/agent/*` requests and attach the breakdown.
pub async fn layer(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    if !path.starts_with("/agent/") || is_stream(&path) {
        return next.run(request).await;
    }
    let started = Instant::now();
    let (response, recorder) = RECORDER
        .scope(Mutex::new(Recorder::default()), async move {
            let response = next.run(request).await;
            let recorder = RECORDER.with(|r| match r.lock() {
                Ok(mut r) => std::mem::take(&mut *r),
                Err(_) => Recorder::default(),
            });
            (response, recorder)
        })
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
    attach(response, &summary).await
}

struct Summary {
    total_ms: u64,
    wda_ms: u64,
    calls: usize,
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
        serde_json::json!({
            "total_ms": self.total_ms,
            "wda_ms": self.wda_ms,
            "daemon_ms": self.total_ms.saturating_sub(self.wda_ms),
            "wda": self.routes.iter().map(|(route, count, ms, bytes)| serde_json::json!({
                "call": route, "count": count, "ms": ms, "bytes": bytes,
            })).collect::<Vec<_>>(),
        })
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

async fn attach(response: Response, summary: &Summary) -> Response {
    let (mut parts, body) = response.into_parts();
    if let Ok(value) = HeaderValue::from_str(&summary.server_timing()) {
        parts.headers.insert("server-timing", value);
    }
    let is_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));
    // A request that never reached WDA keeps its body untouched; the header
    // still carries its total.
    if !is_json || summary.calls == 0 {
        return Response::from_parts(parts, body);
    }
    // Buffer only a body of known size under the cap: reading anything else
    // and failing part-way would leave nothing to send but an empty 200.
    let exact = axum::body::HttpBody::size_hint(&body).exact();
    if !exact.is_some_and(|len| len <= MAX_REWRITE_BYTES as u64) {
        return Response::from_parts(parts, body);
    }
    let bytes = match axum::body::to_bytes(body, MAX_REWRITE_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            parts.status = axum::http::StatusCode::INTERNAL_SERVER_ERROR;
            parts.headers.remove(header::CONTENT_LENGTH);
            return Response::from_parts(
                parts,
                Body::from(r#"{"ok":false,"error":"response_body_unreadable"}"#),
            );
        }
    };
    match with_timing_field(&bytes, &summary.json()) {
        Some(body) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(body))
        }
        None => Response::from_parts(parts, Body::from(bytes)),
    }
}

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
                    elapsed: Duration::from_millis(6000),
                    bytes: Some(800_000),
                },
                Call {
                    route: "GET /window/size".into(),
                    elapsed: Duration::from_millis(180),
                    bytes: Some(90),
                },
                Call {
                    route: "GET /source".into(),
                    elapsed: Duration::from_millis(5000),
                    bytes: Some(700_000),
                },
            ],
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
                    elapsed: Duration::from_millis(5),
                    bytes: None,
                }],
            },
            Duration::from_millis(9),
        );
        let big = vec![b' '; MAX_REWRITE_BYTES + 1];
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(big.clone()))
            .unwrap();
        let out = attach(response, &summary).await;
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
        let out = attach(response, &summary).await;
        let body = axum::body::to_bytes(out.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], br#"{"ok":true}"#);
    }

    #[test]
    fn record_outside_a_request_is_harmless() {
        record("GET /status".into(), Duration::from_millis(1), None);
    }
}
