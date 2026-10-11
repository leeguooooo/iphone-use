//! `GET /agent/screenshot?overlay=elements` (numbered element outlines on
//! the picture) and `GET /agent/elements?ocr=…` (Vision text rows when the
//! tree is unusable), against a scripted runner. Vision itself needs macOS,
//! so OCR runs through `server::ocr::set_stand_in`.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use server::redaction::Image;
use support::{block, build_state_with_wda, mock_wda};

const TREE: &str = r#"{"value":{"type":"XCUIElementTypeApplication","label":"设置","rect":{"x":0,"y":0,"width":440,"height":956},"children":[
{"type":"XCUIElementTypeButton","label":"Sign In","rect":{"x":20,"y":100,"width":200,"height":44},"isEnabled":"1"},
{"type":"XCUIElementTypeTextField","label":"","placeholderValue":"Email address","rect":{"x":20,"y":160,"width":300,"height":44},"isEnabled":"1"},
{"type":"XCUIElementTypeStaticText","label":"通用设置","rect":{"x":20,"y":280,"width":200,"height":44},"isEnabled":"1"},
{"type":"XCUIElementTypeButton","label":"Sign Out","rect":{"x":20,"y":340,"width":200,"height":44},"isEnabled":"1"}]}}"#;

/// A canvas or game: containers only, nothing to act on.
const OPAQUE_TREE: &str = r#"{"value":{"type":"XCUIElementTypeApplication","label":"Game","rect":{"x":0,"y":0,"width":440,"height":956},"children":[
{"type":"XCUIElementTypeOther","label":"","rect":{"x":0,"y":0,"width":440,"height":956}}]}}"#;

const MAGENTA: [u8; 3] = [230, 0, 126];

/// A 2x capture of the 440x956-point screen: grey and white bands, so it
/// is neither blank nor magenta anywhere.
fn capture() -> Vec<u8> {
    let (width, height) = (880u32, 1912u32);
    let rgba = (0..height)
        .flat_map(|y| {
            let v = if (y / 16) % 2 == 0 { 255 } else { 40 };
            (0..width).flat_map(move |_| [v, v, v, 255])
        })
        .collect();
    server::redaction::encode_png(&Image { width, height, rgba }).unwrap()
}

fn pixel(image: &Image, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * image.width + x) * 4) as usize;
    [image.rgba[i], image.rgba[i + 1], image.rgba[i + 2]]
}

async fn get(state: &Arc<server::http::AppState>, uri: &str) -> axum::response::Response {
    server::http::router(state.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn get_json(state: &Arc<server::http::AppState>, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = get(state, uri).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

#[test]
fn overlay_numbers_elements_and_ocr_fills_in_for_an_unusable_tree() {
    block(async {
        use base64::Engine as _;
        let png = capture();
        let envelope = format!(
            r#"{{"value":"{}"}}"#,
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        let opaque = Arc::new(AtomicBool::new(false));
        let tree_is_opaque = opaque.clone();
        let wda = mock_wda(move |request, _| {
            let body = if request.starts_with("POST /session ") {
                r#"{"value":{"sessionId":"SESSION"}}"#.to_string()
            } else if request.contains("/source") {
                let tree = if tree_is_opaque.load(Ordering::SeqCst) { OPAQUE_TREE } else { TREE };
                tree.replace('\n', "")
            } else if request.contains("/window/size") {
                r#"{"value":{"width":440,"height":956}}"#.to_string()
            } else if request.starts_with("GET /screenshot") {
                envelope.clone()
            } else {
                r#"{"value":null}"#.to_string()
            };
            Some((Duration::ZERO, body))
        });
        let state = build_state_with_wda(wda.url());

        // The plain picture is untouched.
        let response = get(&state, "/agent/screenshot").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get("x-overlay").is_none());
        let plain = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(plain.as_ref(), png.as_slice());

        let response = get(&state, "/agent/screenshot?overlay=elements").await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        assert_eq!(headers["content-type"], "image/png");
        assert_eq!(headers["x-overlay"], "elements");
        // Two buttons and the text field; the static text is not numbered.
        assert_eq!(headers["x-overlay-marks"], "3");
        let snapshot = headers["x-elements-snapshot"].to_str().unwrap().to_string();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let drawn = server::redaction::decode_png(&bytes).unwrap();
        let original = server::redaction::decode_png(&png).unwrap();
        assert_eq!((drawn.width, drawn.height), (880, 1912));
        // "Sign In" is [20,100,200,44] points: its bottom edge in pixels.
        let (x, y) = (240, (100 + 44) * 2 - 1);
        assert_ne!(pixel(&original, x, y), MAGENTA);
        assert_eq!(pixel(&drawn, x, y), MAGENTA);
        // Its badge at the top-left corner, and nothing far from any element.
        assert_eq!(pixel(&drawn, 20 * 2 + 1, 100 * 2 + 1), MAGENTA);
        assert_eq!(pixel(&drawn, 860, 1800), pixel(&original, 860, 1800));
        // The static text's rect is not outlined.
        assert_eq!(pixel(&drawn, 240, (280 + 44) * 2 - 1), pixel(&original, 240, (280 + 44) * 2 - 1));

        // The numbers are the full read's indexes, with its snapshot.
        let (status, elements) = get_json(&state, "/agent/elements").await;
        assert_eq!(status, StatusCode::OK, "{elements}");
        assert_eq!(elements["snapshot"], snapshot.as_str());

        // Shrunk to max_side, the marks scale with the picture.
        let response = get(&state, "/agent/screenshot?overlay=elements&max_side=440&fresh=1").await;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let small = server::redaction::decode_png(&bytes).unwrap();
        assert_eq!(small.height, 440);
        let scale = f64::from(small.width) / 440.0;
        let bottom = ((100.0 + 44.0) * scale).floor() as u32 - 1;
        assert_eq!(pixel(&small, (120.0 * scale) as u32, bottom), MAGENTA);

        let (status, json) = get_json(&state, "/agent/screenshot?overlay=boxes").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(json["error"], "invalid_overlay");

        // OCR, with a stand-in for Vision that records the scale it got.
        let scales = Arc::new(Mutex::new(Vec::new()));
        let seen = scales.clone();
        server::ocr::set_stand_in(Some(Arc::new(move |png: &[u8], scale: f64| {
            assert!(png.starts_with(b"\x89PNG"));
            seen.lock().unwrap().push(scale);
            Ok(vec![server::ocr::OcrRow {
                text: "Start game".into(),
                confidence: 0.97,
                rect: [100.0, 400.0, 120.0, 30.0],
            }])
        })));
        let (status, json) = get_json(&state, "/agent/elements?ocr=1").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(json.get("ocr").is_none(), "a usable tree needs no OCR: {json}");
        assert!(scales.lock().unwrap().is_empty());

        let (status, json) = get_json(&state, "/agent/elements?ocr=force&scope=interactive").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ocr"]["source"], "vision", "{json}");
        let row = &json["ocr"]["rows"][0];
        assert_eq!(row["kind"], "OCRText");
        assert_eq!(row["label"], "Start game");
        assert_eq!(row["source"], "ocr");
        assert_eq!(row["rect"], serde_json::json!([100, 400, 120, 30]));
        assert_eq!(*scales.lock().unwrap(), vec![2.0], "pixels per point");
        assert_eq!(json["scope"]["name"], "interactive", "the scope still applies: {json}");

        opaque.store(true, Ordering::SeqCst);
        let (status, json) = get_json(&state, "/agent/elements?ocr=auto").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ocr"]["rows"][0]["label"], "Start game", "{json}");

        // A failed recognition is reported, never a failed read.
        server::ocr::set_stand_in(Some(Arc::new(|_: &[u8], _: f64| {
            Err(anyhow::anyhow!("OCR needs macOS"))
        })));
        let (status, json) = get_json(&state, "/agent/elements?ocr=1").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(json["ocr_error"].as_str().unwrap().contains("OCR needs macOS"), "{json}");
        assert!(json["elements"].is_array());
        server::ocr::set_stand_in(None);

        let (status, json) = get_json(&state, "/agent/elements?ocr=sometimes").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(json["error"], "invalid_ocr");
    });
}
