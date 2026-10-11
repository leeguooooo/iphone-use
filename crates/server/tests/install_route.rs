//! `POST /agent/input {"type":"install",…}`: everything checked before
//! devicectl runs — the request, the bundle id read from the `.app` or
//! `.ipa`, and the owner's blocked apps.
//!
//! The policy is read from the environment once per process, so this binary
//! holds a single test that sets it first.

mod support;

use std::path::Path;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state};

async fn install(
    state: &std::sync::Arc<server::http::AppState>,
    body: serde_json::Value,
    control: bool,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/agent/input")
        .header(header::CONTENT_TYPE, "application/json");
    if control {
        request = request.header("x-phone-control", "1");
    }
    let response = server::http::router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// A binary Info.plist, as Xcode writes it into a built app.
fn info_plist(bundle: &str) -> Vec<u8> {
    let mut dict = plist::Dictionary::new();
    dict.insert("CFBundleIdentifier".into(), plist::Value::String(bundle.into()));
    dict.insert("CFBundleExecutable".into(), plist::Value::String("Demo".into()));
    let mut out = Vec::new();
    plist::Value::Dictionary(dict).to_writer_binary(&mut out).unwrap();
    out
}

fn app(dir: &Path, name: &str, bundle: Option<&str>) -> String {
    let app = dir.join(format!("{name}.app"));
    std::fs::create_dir_all(&app).unwrap();
    if let Some(bundle) = bundle {
        std::fs::write(app.join("Info.plist"), info_plist(bundle)).unwrap();
    }
    app.to_str().unwrap().to_string()
}

/// An .ipa built with the `zip` tool, with a plugin's plist beside the
/// app's own; `None` where `zip`/`unzip` are missing.
fn ipa(dir: &Path, bundle: &str) -> Option<String> {
    let has = |tool: &str| {
        std::process::Command::new(tool)
            .arg("-v")
            .output()
            .is_ok_and(|out| out.status.success())
    };
    if !has("zip") || !has("unzip") {
        return None;
    }
    let root = dir.join("ipa-root");
    let app = root.join("Payload").join("Demo [Beta].app");
    let plugin = app.join("PlugIns").join("Widget.appex");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(plugin.join("Info.plist"), info_plist("com.example.widget")).unwrap();
    std::fs::write(app.join("Info.plist"), info_plist(bundle)).unwrap();
    let out = dir.join(format!("{bundle}.ipa"));
    let status = std::process::Command::new("zip")
        .current_dir(&root)
        .args(["-qr"])
        .arg(&out)
        .arg("Payload")
        .status()
        .unwrap();
    assert!(status.success());
    Some(out.to_str().unwrap().to_string())
}

#[test]
fn install_checks_the_request_bundle_and_blocklist_before_devicectl() {
    std::env::set_var("IPHONE_USE_BLOCKED_APPS", "com.example.bank");
    let dir = tempfile::tempdir().unwrap();
    block(async {
        let state = build_state(None);
        let fine = app(dir.path(), "Notes", Some("com.example.notes"));

        let (status, json) = install(&state, serde_json::json!({"type":"install","path":fine}), false).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["error"], "missing_control_header");

        for bad in [
            serde_json::json!({"type":"install"}),
            serde_json::json!({"type":"install","path":"relative/App.ipa"}),
            serde_json::json!({"type":"install","url":"http://example.com/App.ipa"}),
            serde_json::json!({"type":"install","path":fine,"fresh":1}),
        ] {
            let (status, json) = install(&state, bad.clone(), true).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {json}");
            assert_eq!(json["error"], "invalid_install", "{bad}");
            assert_eq!(json["outcome"], "not_sent");
            assert!(json["hint"].is_string());
        }

        let bank = app(dir.path(), "Bank", Some("com.example.bank"));
        let (status, json) = install(&state, serde_json::json!({"type":"install","path":bank}), true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["error"], "blocked_app");

        // No readable bundle id while apps are blocked: refused, and "fresh"
        // without one is a request error.
        let anonymous = app(dir.path(), "Anonymous", None);
        let (status, json) = install(&state, serde_json::json!({"type":"install","path":anonymous}), true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(json["error"], "install_unverified");
        let (status, json) =
            install(&state, serde_json::json!({"type":"install","path":anonymous,"fresh":true}), true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert!(json["hint"].as_str().unwrap().contains("bundle id"), "{json}");

        if cfg!(not(target_os = "macos")) {
            let (status, json) =
                install(&state, serde_json::json!({"type":"install","path":fine,"fresh":true}), true).await;
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{json}");
            assert_eq!(json["error"], "install_needs_macos");
            assert_eq!(json["bundle"], "com.example.notes", "read from Info.plist: {json}");
        }

        // The .ipa's own Payload/<App>.app/Info.plist, not a plugin's.
        match ipa(dir.path(), "com.example.bank") {
            Some(path) => {
                let (status, json) =
                    install(&state, serde_json::json!({"type":"install","path":path}), true).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
                assert_eq!(json["error"], "blocked_app", "{json}");
                let notes = ipa(&dir.path().join("second"), "com.example.notes").unwrap();
                if cfg!(not(target_os = "macos")) {
                    let (_, json) =
                        install(&state, serde_json::json!({"type":"install","path":notes}), true).await;
                    assert_eq!(json["bundle"], "com.example.notes", "{json}");
                }
            }
            None => eprintln!("zip/unzip missing: .ipa extraction not exercised"),
        }

        // Batches never carry an install.
        let batch = serde_json::json!({"steps":[{"kind":"action","action":{"type":"install","path":fine}}]});
        let response = server::http::router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/actions")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(batch.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    });
}
