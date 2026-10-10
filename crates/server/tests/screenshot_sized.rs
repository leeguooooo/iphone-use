//! `/agent/screenshot` against a scripted device runner: a sized request asks
//! the runner for a JPEG shrunk on the phone, an old runner's full PNG still
//! works, and a capture the link cuts mid-body is retried once, smaller.
//!
//! The runner here is a raw TCP script (not `support::mock_wda`) because these
//! tests need response headers and bodies cut short.

mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state_with_wda};

/// A 236×512 JPEG of a Settings screen, standing in for what a runner sizes
/// on the phone.
const SIZED_JPEG: &[u8] = include_bytes!("fixtures/screen-512.jpg");

/// What the script sends back for one `GET /screenshot`.
enum Reply {
    /// A full envelope; `headers` are extra response header lines.
    Image { bytes: Vec<u8>, headers: Vec<String> },
    /// Headers promising the whole envelope, then only part of it, then the
    /// connection closes: what the relay did on the degraded Wi-Fi link.
    Truncated { bytes: Vec<u8> },
}

fn envelope(bytes: &[u8]) -> String {
    use base64::Engine as _;
    format!(
        r#"{{"value":"{}"}}"#,
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn sized_headers(width: u32, height: u32) -> Vec<String> {
    vec![
        format!("X-IPU-Image-Width: {width}"),
        format!("X-IPU-Image-Height: {height}"),
        "X-IPU-Image-Source-Width: 1320".to_string(),
        "X-IPU-Image-Source-Height: 2868".to_string(),
        "X-IPU-Image-Format: jpeg".to_string(),
    ]
}

/// Serves `/screenshot` from `replies` in order and records each request
/// target; anything else gets a WDA error envelope.
fn scripted_runner(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buffer = [0_u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let target = request
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            if !target.contains("/screenshot") {
                let body = r#"{"value":{"error":"unknown command","message":"not scripted"}}"#;
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                continue;
            }
            log.lock().unwrap().push(target);
            match replies.next() {
                Some(Reply::Image { bytes, headers }) => {
                    let body = envelope(&bytes);
                    let mut head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for line in headers {
                        head.push_str(&line);
                        head.push_str("\r\n");
                    }
                    head.push_str("\r\n");
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                }
                Some(Reply::Truncated { bytes }) => {
                    let body = envelope(&bytes);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body.as_bytes()[..body.len() / 3]);
                    let _ = stream.flush();
                    // Dropping the stream closes it mid-body.
                }
                None => return,
            }
        }
    });
    (format!("http://{address}"), seen)
}

struct Shot {
    status: StatusCode,
    content_type: Option<String>,
    degraded: Option<String>,
    body: Vec<u8>,
}

fn screenshot(base: &str, uri: &str) -> Shot {
    let state = build_state_with_wda(base);
    let uri = uri.to_string();
    block(async move {
        let response = server::http::router(state)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_string())
        };
        let status = response.status();
        let content_type = header("content-type");
        let degraded = header("x-screenshot-degraded");
        let body = response.into_body().collect().await.unwrap().to_bytes().to_vec();
        Shot {
            status,
            content_type,
            degraded,
            body,
        }
    })
}

/// A full-resolution capture that is not one flat colour (so the hidden-
/// screen check never reads a tree): stripes over a 1320×2868 screen.
fn full_png() -> Vec<u8> {
    let (width, height) = (1320_u32, 2868_u32);
    let mut rgba = vec![255_u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            let v = if (x / 40 + y / 60) % 2 == 0 { 30 } else { 220 };
            rgba[i] = v;
            rgba[i + 1] = (y % 256) as u8;
            rgba[i + 2] = (x % 256) as u8;
        }
    }
    server::redaction::encode_png(&server::redaction::Image {
        width,
        height,
        rgba,
    })
    .unwrap()
}

#[test]
fn a_sized_request_asks_the_runner_for_a_jpeg_shrunk_on_the_phone() {
    let (base, seen) = scripted_runner(vec![Reply::Image {
        bytes: SIZED_JPEG.to_vec(),
        headers: sized_headers(236, 512),
    }]);
    let shot = screenshot(&base, "/agent/screenshot?max_side=512");
    assert_eq!(shot.status, StatusCode::OK);
    assert_eq!(seen.lock().unwrap().as_slice(), ["/screenshot?max_side=512&format=jpeg&quality=0.70"]);
    // Callers keep getting PNG, at the size the phone made.
    assert_eq!(shot.content_type.as_deref(), Some("image/png"));
    let image = server::redaction::decode_png(&shot.body).expect("a PNG");
    assert_eq!((image.width, image.height), (236, 512));
    assert_eq!(shot.degraded, None);
}

#[test]
fn an_unsized_request_stays_the_webdriveragent_screenshot() {
    let png = full_png();
    let (base, seen) = scripted_runner(vec![Reply::Image {
        bytes: png.clone(),
        headers: vec![],
    }]);
    let shot = screenshot(&base, "/agent/screenshot");
    assert_eq!(shot.status, StatusCode::OK);
    assert_eq!(seen.lock().unwrap().as_slice(), ["/screenshot"], "no query at all");
    assert_eq!(shot.body, png, "the full PNG, untouched");
}

#[test]
fn an_old_runner_that_ignores_the_query_still_gets_shrunk_here() {
    // A runner from before sized screenshots: full PNG, no X-IPU-Image-*.
    let (base, seen) = scripted_runner(vec![Reply::Image {
        bytes: full_png(),
        headers: vec![],
    }]);
    let shot = screenshot(&base, "/agent/screenshot?max_side=1024");
    assert_eq!(shot.status, StatusCode::OK);
    assert!(seen.lock().unwrap()[0].contains("max_side=1024"));
    let image = server::redaction::decode_png(&shot.body).expect("a PNG");
    assert_eq!(image.width.max(image.height), 1024, "fitted on the Mac");
    assert_eq!((image.width, image.height), (471, 1024));
}

#[test]
fn a_capture_cut_mid_body_is_retried_once_smaller() {
    let (base, seen) = scripted_runner(vec![
        Reply::Truncated {
            bytes: SIZED_JPEG.to_vec(),
        },
        Reply::Image {
            bytes: SIZED_JPEG.to_vec(),
            headers: sized_headers(236, 512),
        },
    ]);
    let shot = screenshot(&base, "/agent/screenshot?max_side=800");
    assert_eq!(shot.status, StatusCode::OK, "{}", String::from_utf8_lossy(&shot.body));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "one retry: {seen:?}");
    assert!(seen[0].contains("max_side=800"), "{seen:?}");
    assert!(
        seen[1].contains("max_side=400") && seen[1].contains("format=jpeg"),
        "the retry asks for half the size: {seen:?}"
    );
    assert_eq!(shot.degraded.as_deref(), Some("max_side=400"));
    let image = server::redaction::decode_png(&shot.body).expect("a PNG");
    assert!(image.width.max(image.height) <= 400);
}

#[test]
fn a_full_capture_cut_mid_body_falls_back_to_a_sized_one() {
    let (base, seen) = scripted_runner(vec![
        Reply::Truncated { bytes: full_png() },
        Reply::Image {
            bytes: SIZED_JPEG.to_vec(),
            headers: sized_headers(236, 512),
        },
    ]);
    let shot = screenshot(&base, "/agent/screenshot");
    assert_eq!(shot.status, StatusCode::OK);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0], "/screenshot");
    assert!(seen[1].contains("max_side=1024"), "{seen:?}");
    assert_eq!(shot.degraded.as_deref(), Some("max_side=1024"));
}

#[test]
fn raw_is_never_retried_smaller() {
    let (base, seen) = scripted_runner(vec![Reply::Truncated { bytes: full_png() }]);
    let shot = screenshot(&base, "/agent/screenshot?raw=1");
    assert_eq!(shot.status, StatusCode::BAD_GATEWAY);
    assert_eq!(seen.lock().unwrap().len(), 1);
    let json: serde_json::Value = serde_json::from_slice(&shot.body).unwrap();
    assert_eq!(json["attempts"], 1);
}

#[test]
fn two_cut_captures_fail_with_a_structured_cause() {
    let (base, seen) = scripted_runner(vec![
        Reply::Truncated {
            bytes: SIZED_JPEG.to_vec(),
        },
        Reply::Truncated {
            bytes: SIZED_JPEG.to_vec(),
        },
    ]);
    let shot = screenshot(&base, "/agent/screenshot?max_side=1200");
    assert_eq!(shot.status, StatusCode::BAD_GATEWAY);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(shot.content_type.as_deref(), Some("application/json"));
    let json: serde_json::Value = serde_json::from_slice(&shot.body).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"], "screenshot_failed");
    assert_eq!(json["cause"], "body_truncated", "{json}");
    assert_eq!(json["attempts"], 2);
    // The test state manages no runner of its own.
    assert_eq!(json["transport"], "external");
    assert!(json["hint"].as_str().unwrap().contains("max_side"));
}

#[test]
fn a_runner_error_is_not_retried() {
    // The runner refuses the capture: a WDA error envelope, not a link fault.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let count = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&count);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buffer = [0_u8; 8192];
            let _ = stream.read(&mut buffer);
            *counter.lock().unwrap() += 1;
            let body = r#"{"value":{"error":"unknown error","message":"screenshot failed"}}"#;
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    let shot = screenshot(&format!("http://{address}"), "/agent/screenshot?max_side=600");
    assert_eq!(shot.status, StatusCode::BAD_GATEWAY);
    assert_eq!(*count.lock().unwrap(), 1);
    let json: serde_json::Value = serde_json::from_slice(&shot.body).unwrap();
    assert_eq!(json["cause"], "runner_error", "{json}");
}
