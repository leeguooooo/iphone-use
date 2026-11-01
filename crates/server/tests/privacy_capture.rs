//! Real router + delayed scripted runner. No personal daemon or phone is used.
mod support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::Engine as _;
use http_body_util::BodyExt;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use support::{block, build_state_with_runner, mock_runner};
use tower::ServiceExt;

async fn hold(state: Arc<server::http::AppState>, private: bool) {
    let response = server::http::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agent/hold")
                .header("x-phone-control", "1")
                .header("x-phone-owner", "privacy-fixture")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"secs":if private {60} else {0},"private":private}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

async fn race(clear: bool) {
    for uri in [
        "/agent/screenshot?raw=1",
        "/agent/screenshot?overlay=elements&fresh=1",
        "/agent/elements?ocr=off",
    ] {
        let started = Arc::new(tokio::sync::Notify::new());
        let signal = started.clone();
        let png = base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("fixtures/privacy.png"));
        let delay_source = !uri.contains("raw=1");
        let runner = mock_runner(move |request, _| {
            if request.starts_with("POST /session ") {
                return Some((
                    Duration::ZERO,
                    json!({"value":{"sessionId":"SESSION"}}).to_string(),
                ));
            }
            if request.contains("/screenshot") {
                if !delay_source {
                    signal.notify_one();
                }
                return Some((
                    if delay_source {
                        Duration::ZERO
                    } else {
                        Duration::from_millis(400)
                    },
                    json!({"value":png}).to_string(),
                ));
            }
            if request.contains("/source") {
                if delay_source {
                    signal.notify_one();
                }
                return Some((Duration::from_millis(400),json!({"value":{"type":"XCUIElementTypeApplication","label":"PRIVATE_FIXTURE","rect":{"x":0,"y":0,"width":390,"height":844},"children":[{"type":"XCUIElementTypeButton","label":"PRIVATE_LABEL","rect":{"x":10,"y":10,"width":100,"height":40},"enabled":true,"visible":true}]}}).to_string()));
            }
            Some((Duration::ZERO, json!({"value":null}).to_string()))
        });
        let state = build_state_with_runner(runner.url());
        let app = server::http::router(state.clone());
        let read = tokio::spawn(async move {
            app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(4), started.notified())
            .await
            .expect("delayed screen read started");
        hold(state.clone(), true).await;
        if clear {
            hold(state.clone(), false).await;
        }
        let response = read.await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            StatusCode::LOCKED,
            "{uri}, clear={clear}: privacy transition must discard the completed screen read"
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("PRIVATE_LABEL") && !text.contains("PRIVATE_FIXTURE"));
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.get("image").is_none());
        assert!(state.element_snapshots.lock().unwrap().is_empty());
        assert!(state.owner_snapshots.lock().unwrap().is_empty());
    }
}

#[test]
fn in_flight_screen_reads_are_discarded_when_a_private_hold_begins() {
    block(race(false));
}
#[test]
fn in_flight_screen_reads_are_discarded_even_after_private_hold_is_lifted() {
    block(race(true));
}

#[test]
fn a_private_transition_invalidates_the_settled_frame_cache() {
    block(async {
        let fresh = include_bytes!("fixtures/privacy.png").to_vec();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&fresh);
        let runner = mock_runner(move |request, _| {
            Some((
                Duration::ZERO,
                if request.starts_with("POST /session ") {
                    json!({"value":{"sessionId":"SESSION"}}).to_string()
                } else if request.contains("/screenshot") {
                    json!({"value":b64}).to_string()
                } else {
                    json!({"value":null}).to_string()
                },
            ))
        });
        let state = build_state_with_runner(runner.url());
        state
            .runner
            .as_ref()
            .unwrap()
            .lock()
            .await
            .remember_settled_frame(server::runner::Capture::full_png(
                include_bytes!("fixtures/privacy-old.png").to_vec(),
            ));
        hold(state.clone(), true).await;
        hold(state.clone(), false).await;
        let response = server::http::router(state)
            .oneshot(
                Request::builder()
                    .uri("/agent/screenshot?max_side=600")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            &bytes[..],
            &fresh[..],
            "a private transition must not reuse the previous settled frame"
        );
    });
}

#[test]
fn open_mjpeg_and_h264_feeds_do_not_resume_after_a_brief_private_hold() {
    block(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for h264 in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let source = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0u8; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                let head = if h264 {
                    "HTTP/1.0 200 OK\r\nX-Video-Format: iphone-use-h264-annexb-v1\r\n\r\n"
                } else {
                    "HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n"
                };
                socket.write_all(head.as_bytes()).await.unwrap();
                let frame = |byte: u8| {
                    if h264 {
                        server::video::frame_message(&server::video::H264Frame {
                            data: bytes::Bytes::from(vec![0, 0, 0, 1, 0x65, byte]),
                            keyframe: true,
                            pts_micros: byte as u64,
                        })
                        .to_vec()
                    } else {
                        vec![byte; 32]
                    }
                };
                socket.write_all(&frame(b'A')).await.unwrap();
                let _ = release_rx.await;
                let _ = socket.write_all(&frame(b'B')).await;
                let _ =
                    tokio::time::timeout(Duration::from_secs(2), socket.read(&mut request)).await;
            });
            let mut state = support::build_state(None);
            let inner = Arc::get_mut(&mut state).unwrap();
            inner.mjpeg_url = Some(format!("http://{address}"));
            if h264 {
                inner.video = Some(server::video::VideoHub::new(format!("http://{address}")));
            }
            let response = server::http::router(state.clone())
                .oneshot(
                    Request::builder()
                        .uri(if h264 { "/agent/h264" } else { "/agent/mjpeg" })
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let mut body = response.into_body();
            let first = tokio::time::timeout(Duration::from_secs(4), body.frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap();
            assert_eq!(first.last().copied(), Some(b'A'));
            hold(state.clone(), true).await;
            hold(state.clone(), false).await;
            let _ = release_tx.send(());
            let next = tokio::time::timeout(Duration::from_secs(3), body.frame())
                .await
                .unwrap();
            assert!(
                next.is_none(),
                "h264={h264}: an old feed must end across private on/off rather than forwarding B"
            );
            drop(body);
            assert_eq!(
                state
                    .live_streams
                    .load(std::sync::atomic::Ordering::Acquire),
                0
            );
            if let Some(hub) = &state.video {
                assert_eq!(hub.subscriber_count(), 0);
            }
            source.abort();
        }
    });
}

#[test]
fn a_queued_response_body_cannot_publish_after_a_private_transition() {
    block(async {
        let png = base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("fixtures/privacy.png"));
        let runner = mock_runner(move |request, _| {
            Some((
                Duration::ZERO,
                if request.starts_with("POST /session ") {
                    json!({"value":{"sessionId":"SESSION"}}).to_string()
                } else if request.contains("/screenshot") {
                    json!({"value":png}).to_string()
                } else {
                    json!({"value":null}).to_string()
                },
            ))
        });
        let state = build_state_with_runner(runner.url());
        let response = server::http::router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/agent/screenshot?raw=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // Headers/response already exist, but no body bytes have been committed.
        hold(state.clone(), true).await;
        hold(state.clone(), false).await;
        assert!(
            response.into_body().collect().await.is_err(),
            "queued PNG bytes must be discarded, not published after unhide"
        );
    });
}

#[test]
fn a_private_hold_cancels_a_stalled_capture_without_waiting_for_the_runner() {
    block(async {
        let started = Arc::new(tokio::sync::Notify::new());
        let signal = started.clone();
        let runner = mock_runner(move |request, _| {
            if request.starts_with("POST /session ") {
                return Some((
                    Duration::ZERO,
                    json!({"value":{"sessionId":"SESSION"}}).to_string(),
                ));
            }
            if request.contains("/screenshot") {
                signal.notify_one();
                return Some((Duration::from_secs(5), json!({"value":""}).to_string()));
            }
            Some((Duration::ZERO, json!({"value":null}).to_string()))
        });
        let state = build_state_with_runner(runner.url());
        let app = server::http::router(state.clone());
        let pending = tokio::spawn(async move {
            app.oneshot(
                Request::builder()
                    .uri("/agent/screenshot?raw=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        hold(state, true).await;
        let response = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("private capture must not wait for the stalled runner")
            .unwrap();
        assert_eq!(response.status(), StatusCode::LOCKED);
    });
}

#[test]
fn normalizing_an_expired_hold_preserves_a_new_permitted_capture() {
    block(async {
        let png = base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("fixtures/privacy.png"));
        let runner = mock_runner(move |request, _| {
            Some((
                Duration::ZERO,
                if request.starts_with("POST /session ") {
                    json!({"value":{"sessionId":"SESSION"}}).to_string()
                } else if request.contains("/screenshot") {
                    json!({"value":png}).to_string()
                } else {
                    json!({"value":null}).to_string()
                },
            ))
        });
        let state = build_state_with_runner(runner.url());
        *state.private_until.lock().unwrap() =
            Some(std::time::Instant::now() - Duration::from_secs(1));
        let response = server::http::router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/agent/screenshot?raw=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        hold(state, false).await;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), include_bytes!("fixtures/privacy.png"));
    });
}

async fn observed_input_revocation(during_read: bool, clear: bool, owner_change: bool) {
    let started = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    let revoked=Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marker=revoked.clone();
    let late_reads=Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count=late_reads.clone();
    let runner = mock_runner(move |request, _| {
        if marker.load(std::sync::atomic::Ordering::SeqCst) {
            count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
        }
        if request.starts_with("POST /session ") {
            return Some((Duration::ZERO, json!({"value":{"sessionId":"SESSION"}}).to_string()));
        }
        if request.contains("/source") {
            signal.notify_one();
            return Some((if during_read {Duration::from_millis(150)} else {Duration::ZERO},
                json!({"value":{"type":"XCUIElementTypeApplication","label":"PRIVATE_INPUT_OBSERVATION",
                    "rect":{"x":0,"y":0,"width":390,"height":844},"children":[]}}).to_string()));
        }
        if request.contains("/window/size") {
            return Some((Duration::ZERO, json!({"value":{"width":390,"height":844}}).to_string()));
        }
        Some((Duration::ZERO, json!({"value":null}).to_string()))
    });
    let state = build_state_with_runner(runner.url());
    let app = server::http::router(state.clone());
    let pending = tokio::spawn(async move {
        app.oneshot(Request::builder().method("POST")
            .uri("/agent/input?return=delta&settle_ms=1000")
            .header("x-phone-control","1").header("x-phone-owner","privacy-fixture")
            .header("content-type","application/json").body(Body::from(r#"{"type":"home"}"#)).unwrap()).await.unwrap()
    });
    if during_read {
        tokio::time::timeout(Duration::from_secs(4),started.notified()).await.unwrap();
        revoked.store(true,std::sync::atomic::Ordering::SeqCst);
        hold(state.clone(),true).await;
        if clear {hold(state.clone(),false).await;}
    }
    let response = pending.await.unwrap();
    assert_eq!(response.status(),StatusCode::OK);
    if !during_read {
        if owner_change {
            state.owner_epoch.fetch_add(1,std::sync::atomic::Ordering::AcqRel);
            state.owner_snapshots.lock().unwrap().insert("new-owner".into(),"new-snapshot".into());
            state.element_snapshots.lock().unwrap().push_back(("new-snapshot".into(),Arc::new(vec![])));
        }
        else { hold(state.clone(),true).await; }
    }
    {
        let runner = state.runner.as_ref().unwrap().lock().await;
        assert!(runner.recent_tree(Duration::from_secs(10)).is_none(),"revoked tree cannot be reused before body publication");
        assert!(runner.settled_frame(Duration::from_secs(10)).is_none(),"revoked frame cannot be reused before body publication");
    }
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("PRIVATE_INPUT_OBSERVATION"),"private action observation leaked: {text}");
    let body:serde_json::Value=serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["ok"],true,"privacy must not rewrite a confirmed applied action");
    assert_eq!(body["observation_omitted"],true);
    assert_eq!(late_reads.load(std::sync::atomic::Ordering::SeqCst),0,"revoked observation must not dispatch internal retries or later reads");
    if owner_change {
        assert!(state.element_snapshots.lock().unwrap().iter().any(|(id,_)|id=="new-snapshot"));
        assert_eq!(state.owner_snapshots.lock().unwrap().get("new-owner").map(String::as_str),Some("new-snapshot"));
    } else {
        assert!(state.element_snapshots.lock().unwrap().is_empty());
        assert!(state.owner_snapshots.lock().unwrap().is_empty());
    }
}
#[test]
fn observed_input_is_fenced_until_its_body_is_published() {
    block(observed_input_revocation(false,false,false));
}
#[test]
fn observed_input_does_not_cache_a_read_crossing_a_private_hold() {
    block(observed_input_revocation(true,true,false));
}

#[test]
fn observed_input_owner_change_invalidates_cache_before_publication() {
    block(observed_input_revocation(false,false,true));
}

#[test]
fn private_hold_during_action_skips_post_action_observation() {
    block(async {
        let started=Arc::new(tokio::sync::Notify::new());
        let signal=started.clone();
        let reads=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count=reads.clone();
        let runner=mock_runner(move |request,_| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO,json!({"value":{"sessionId":"SESSION"}}).to_string()));
            }
            if request.contains("/wda/pressButton") {
                signal.notify_one();
                return Some((Duration::from_millis(150),json!({"value":null}).to_string()));
            }
            if request.contains("/source") || request.contains("/screenshot") || request.contains("/alert/") || request.contains("/wda/settle") {
                count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            }
            Some((Duration::ZERO,json!({"value":null}).to_string()))
        });
        let state=build_state_with_runner(runner.url());
        let app=server::http::router(state.clone());
        let pending=tokio::spawn(async move {
            app.oneshot(Request::builder().method("POST").uri("/agent/input?return=delta")
                .header("x-phone-control","1").header("x-phone-owner","privacy-fixture")
                .header("content-type","application/json").body(Body::from(r#"{"type":"home"}"#)).unwrap()).await.unwrap()
        });
        tokio::time::timeout(Duration::from_secs(4),started.notified()).await.unwrap();
        hold(state,true).await;
        let bytes=pending.await.unwrap().into_body().collect().await.unwrap().to_bytes();
        let value:serde_json::Value=serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["ok"],true);
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst),0,"no new observation after private hold");
    });
}

#[test]
fn system_shortcuts_refuse_private_hold_at_both_http_entries() {
    block(async {
        for uri in ["/agent/input", "/control"] {
            for name in ["save_screenshot","volume_up","volume_down"] {
                let calls=Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let count=calls.clone();
                let runner=mock_runner(move |_,_| {count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                    Some((Duration::ZERO,json!({"value":null}).to_string()))});
                let state=build_state_with_runner(runner.url());
                hold(state.clone(),true).await;
                let response=server::http::router(state).oneshot(Request::builder().method("POST").uri(uri)
                    .header("x-phone-control","1").header("x-phone-owner","privacy-fixture")
                    .header("content-type","application/json")
                    .body(Body::from(json!({"type":"shortcut","name":name}).to_string())).unwrap()).await.unwrap();
                assert_eq!(response.status(),StatusCode::LOCKED,"{uri}/{name}");
                let bytes=response.into_body().collect().await.unwrap().to_bytes();
                let value:serde_json::Value=serde_json::from_slice(&bytes).unwrap();
                assert_eq!(value["outcome"],"not_sent");
                assert_eq!(value["retry_safe"],true);
                assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst),0);
            }
        }
    });
}

#[test]
fn batch_owner_change_redacts_queued_observation() {
    block(async {
        let runner=mock_runner(|request,_| Some((Duration::ZERO,if request.contains("/source") {
            json!({"value":{"type":"XCUIElementTypeApplication","label":"PRIVATE_BATCH_OWNER",
                "rect":{"x":0,"y":0,"width":390,"height":844},"children":[]}}).to_string()
        } else if request.starts_with("POST /session ") {
            json!({"value":{"sessionId":"SESSION"}}).to_string()
        } else {json!({"value":null}).to_string()})));
        let state=build_state_with_runner(runner.url());
        let response=server::http::router(state.clone()).oneshot(Request::builder().method("POST").uri("/agent/actions")
            .header("x-phone-control","1").header("x-phone-owner","privacy-fixture")
            .header("content-type","application/json")
            .body(Body::from(r#"{"steps":[{"kind":"action","action":{"type":"home"}}],"observe":true}"#)).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::OK);
        state.owner_epoch.fetch_add(1,std::sync::atomic::Ordering::AcqRel);
        let bytes=response.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE_BATCH_OWNER"));
        let value:serde_json::Value=serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["observation_omitted"],true);
        assert_eq!(value["applied_actions"],1);
        assert_eq!(value["retry_safe"],false);
    });
}

#[test]
fn system_shortcut_setup_cannot_outlive_its_privacy_permission() {
    block(async {
        for name in ["save_screenshot","volume_up","volume_down"] {
            let started=Arc::new(tokio::sync::Notify::new());
            let signal=started.clone();
            let mutations=Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count=mutations.clone();
            let runner=mock_runner(move |request,_| {
                if request.starts_with("POST /session ") {
                    signal.notify_one();
                    return Some((Duration::from_millis(150),json!({"value":{"sessionId":"SESSION",
                        "capabilities":{"ipu:systemCaptureRequest":true,"ipu:volumeButtons":true}}}).to_string()));
                }
                if request.contains("/ipu/system-capture") || request.contains("/wda/pressButton") {
                    count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                }
                Some((Duration::ZERO,json!({"value":null}).to_string()))
            });
            let state=build_state_with_runner(runner.url());
            let app=server::http::router(state.clone());
            let body=json!({"type":"shortcut","name":name}).to_string();
            let pending=tokio::spawn(async move {app.oneshot(Request::builder().method("POST").uri("/agent/input")
                .header("x-phone-control","1").header("x-phone-owner","privacy-fixture")
                .header("content-type","application/json").body(Body::from(body)).unwrap()).await.unwrap()});
            tokio::time::timeout(Duration::from_secs(4),started.notified()).await.unwrap();
            hold(state,true).await;
            let bytes=pending.await.unwrap().into_body().collect().await.unwrap().to_bytes();
            let value:serde_json::Value=serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["outcome"],"not_sent","{name}: {value}");
            assert_eq!(value["retry_safe"],true);
            assert_eq!(mutations.load(std::sync::atomic::Ordering::SeqCst),0);
        }
    });
}

#[test]
fn fresh_elements_restore_cache_reuse_after_owner_or_privacy_revocation() {
    block(async {
        for revoke in ["owner", "privacy"] {
            let runner = mock_runner(|request, _| Some((Duration::ZERO,
                if request.contains("/source") {
                    json!({"value":{"type":"XCUIElementTypeApplication","label":"fresh-cache-fixture",
                        "rect":{"x":0,"y":0,"width":390,"height":844},"children":[]}}).to_string()
                } else if request.contains("/window/size") {
                    json!({"value":{"width":390,"height":844}}).to_string()
                } else { json!({"value":null}).to_string() }
            )));
            let state = build_state_with_runner(runner.url());
            state.runner.as_ref().unwrap().lock().await.bind_observation_scope(Arc::new(|| false));
            for round in 0..2 {
                let response = server::http::router(state.clone()).oneshot(Request::builder()
                    .uri("/agent/elements?ocr=off").body(Body::empty()).unwrap()).await.unwrap();
                assert_eq!(response.status(),StatusCode::OK);
                response.into_body().collect().await.unwrap();
                assert!(state.runner.as_ref().unwrap().lock().await.recent_tree(Duration::from_secs(10)).is_some(),
                    "fresh read must restore reusable observations after {revoke}, round {round}");
                // A fresh tree must not reauthorize independent frame observations.
                if round == 0 {
                    let mut client = state.runner.as_ref().unwrap().lock().await;
                    client.remember_settled_frame(server::runner::Capture::full_png(include_bytes!("fixtures/privacy.png").to_vec()));
                    assert!(client.settled_frame(Duration::from_secs(10)).is_none());
                }
                if round == 0 {
                    if revoke == "owner" { state.owner_epoch.fetch_add(1,std::sync::atomic::Ordering::AcqRel); }
                    else { hold(state.clone(),true).await; hold(state.clone(),false).await; }
                    assert!(state.runner.as_ref().unwrap().lock().await.recent_tree(Duration::from_secs(10)).is_none(),
                        "revocation must still reject the previous tree");
                }
            }
        }
    });
}

#[test]
fn a_queued_read_cannot_gain_permission_from_a_later_private_hold() {
    block(async {
        let reads=Arc::new(std::sync::atomic::AtomicUsize::new(0));let count=reads.clone();
        let runner=mock_runner(move |request,_| {
            if request.contains("/source") {count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);}
            Some((Duration::ZERO,json!({"value":{"type":"XCUIElementTypeApplication",
                "label":"private-queued-fixture","rect":{"x":0,"y":0,"width":390,"height":844},"children":[]}}).to_string()))
        });
        let state=build_state_with_runner(runner.url());
        hold(state.clone(),false).await; // Keep owner identity unchanged across the private interval.
        state.runner_actionable.store(true,std::sync::atomic::Ordering::Release);
        *state.runner_health.lock().unwrap()=server::runner::RunnerHealth{up:true,actionable:true,locked:Some(false)};
        let guard=state.runner.as_ref().unwrap().lock().await;
        let app=server::http::router(state.clone());
        let pending=tokio::spawn(async move {
            app.oneshot(Request::builder().uri("/agent/elements?ocr=off").body(Body::empty()).unwrap()).await.unwrap()
        });
        tokio::time::timeout(Duration::from_secs(2),async {
            while state.runner_control_pending.load(std::sync::atomic::Ordering::Acquire)==0 {tokio::task::yield_now().await;}
        }).await.unwrap();
        let held=server::http::router(state.clone()).oneshot(Request::builder().method("POST").uri("/agent/hold")
            .header("x-phone-control","1").header("x-phone-owner","privacy-fixture")
            .header("content-type","application/json").body(Body::from(r#"{"secs":1,"private":true}"#)).unwrap()).await.unwrap();
        assert_eq!(held.status(),StatusCode::OK);drop(guard);
        let response=pending.await.unwrap();assert_eq!(response.status(),StatusCode::LOCKED);
        response.into_body().collect().await.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst),0,"revoked queued read must not reach Native after acquiring the lock");
        assert!(state.runner_actionable.load(std::sync::atomic::Ordering::Acquire),"scope refusal must not mark healthy hardware unavailable");
        assert!(state.runner_health.lock().unwrap().up);
        assert!(state.runner.as_ref().unwrap().lock().await.recent_tree(Duration::from_secs(10)).is_none(),"hold expiry must not authorize a revoked request's cache");
    });
}

#[test]
fn private_list_operations_refuse_before_reading_or_swiping() {
    block(async {
        for (uri, body) in [
            ("/agent/collect", json!({"max_pages":1})),
            (
                "/agent/scroll_find",
                json!({"label":"PRIVATE_LIST_SECRET","max_swipes":0}),
            ),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let state =
                build_state_with_runner(&format!("http://{}", listener.local_addr().unwrap()));
            hold(state.clone(), true).await;
            let response = server::http::router(state)
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("x-phone-control", "1")
                        .header("x-phone-owner", "privacy-fixture")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::LOCKED);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["error"], "screen_private");
            assert_eq!(value["swipes_confirmed"], 0);
            assert_eq!(value["swipes_uncertain"], 0);
            assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE_LIST_SECRET"));
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
                "private admission must not make any Native connection, including setup"
            );
        }
    });
}

async fn list_response_race(
    uri: &str,
    request: serde_json::Value,
    during_swipe: bool,
    drop_swipe: bool,
    private_publish: bool,
) {
    let sent = Arc::new(tokio::sync::Notify::new());
    let signal = sent.clone();
    let actions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = actions.clone();
    let source = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = source.clone();
    let runner = mock_runner(move |request, _| {
        if request.starts_with("POST /session ") {
            return Some((
                Duration::ZERO,
                json!({"value":{"sessionId":"SESSION"}}).to_string(),
            ));
        }
        if request.contains("/actions") {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            signal.notify_one();
            if drop_swipe {
                std::thread::sleep(Duration::from_millis(150));
                return None;
            }
            return Some((
                Duration::from_millis(150),
                json!({"value":null}).to_string(),
            ));
        }
        if request.contains("/source") {
            let page = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Some((Duration::ZERO,json!({"value":{"type":"XCUIElementTypeApplication","label":"PRIVATE_LIST_APP",
                "rect":{"x":0,"y":0,"width":390,"height":844},"children":[
                {"type":"XCUIElementTypeCell","label":format!("PRIVATE_LIST_ROW_{page}"),"rect":{"x":20,"y":100,"width":300,"height":50}}]}}).to_string()));
        }
        if request.contains("/window/size") {
            return Some((
                Duration::ZERO,
                json!({"value":{"width":390,"height":844}}).to_string(),
            ));
        }
        Some((Duration::ZERO, json!({"value":null}).to_string()))
    });
    let state = build_state_with_runner(runner.url());
    let app = server::http::router(state.clone());
    let uri = uri.to_string();
    let pending = tokio::spawn(async move {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("x-phone-control", "1")
                .header("x-phone-owner", "privacy-fixture")
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    });
    if during_swipe {
        tokio::time::timeout(Duration::from_secs(5), sent.notified())
            .await
            .unwrap();
        hold(state.clone(), true).await;
    }
    let response = pending.await.unwrap();
    if !during_swipe && private_publish {
        hold(state.clone(), true).await;
    }
    if !during_swipe && !private_publish {
        state
            .owner_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        state
            .owner_snapshots
            .lock()
            .unwrap()
            .insert("new-owner".into(), "NEW_SAFE_SNAPSHOT".into());
        state
            .element_snapshots
            .lock()
            .unwrap()
            .push_back(("NEW_SAFE_SNAPSHOT".into(), Arc::new(vec![])));
    }
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("PRIVATE_LIST"),
        "revoked list response leaked: {text}"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["observation_omitted"], true);
    assert_eq!(value["retry_safe"], false);
    if during_swipe {
        assert_eq!(
            actions.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "revocation must not issue another swipe"
        );
        assert_eq!(
            source.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "revocation must not read another page"
        );
        assert_eq!(value["swipes_confirmed"], if drop_swipe { 0 } else { 1 });
        assert_eq!(value["swipes_uncertain"], if drop_swipe { 1 } else { 0 });
    }
    assert!(
        value.get("rows").is_none()
            && value.get("found").is_none()
            && value.get("complete").is_none()
    );
    if !during_swipe && !private_publish {
        assert_eq!(
            state
                .owner_snapshots
                .lock()
                .unwrap()
                .get("new-owner")
                .map(String::as_str),
            Some("NEW_SAFE_SNAPSHOT")
        );
        assert!(state
            .element_snapshots
            .lock()
            .unwrap()
            .iter()
            .any(|(id, _)| id == "NEW_SAFE_SNAPSHOT"));
    }
}
#[test]
fn list_scope_changes_preserve_delivery_and_hide_observations() {
    block(async {
        for (uri, body) in [
            ("/agent/collect", json!({"max_pages":3})),
            (
                "/agent/scroll_find",
                json!({"label":"NOT_PRESENT","max_swipes":2}),
            ),
        ] {
            list_response_race(uri, body.clone(), true, false, false).await;
            list_response_race(uri, body, true, true, false).await;
        }
        list_response_race(
            "/agent/collect",
            json!({"max_pages":1}),
            false,
            false,
            false,
        )
        .await;
        list_response_race("/agent/collect", json!({"max_pages":1}), false, false, true).await;
        list_response_race(
            "/agent/scroll_find",
            json!({"label":"PRIVATE_LIST_ROW_0","max_swipes":0}),
            false,
            false,
            true,
        )
        .await;
        list_response_race(
            "/agent/scroll_find",
            json!({"label":"PRIVATE_LIST_ROW_0","max_swipes":0}),
            false,
            false,
            false,
        )
        .await;
    });
}

#[test]
fn queued_list_operations_do_not_gain_new_permission_after_a_brief_hold() {
    block(async {
        for (uri, body) in [
            ("/agent/collect", json!({"max_pages":2})),
            (
                "/agent/scroll_find",
                json!({"label":"missing","max_swipes":1}),
            ),
        ] {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = calls.clone();
            let runner = mock_runner(move |_, _| {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some((Duration::ZERO, json!({"value":null}).to_string()))
            });
            let state = build_state_with_runner(runner.url());
            hold(state.clone(), false).await;
            state
                .runner_actionable
                .store(true, std::sync::atomic::Ordering::Release);
            *state.runner_health.lock().unwrap() = server::runner::RunnerHealth {
                up: true,
                actionable: true,
                locked: Some(false),
            };
            let client = state.runner.as_ref().unwrap().lock().await;
            let app = server::http::router(state.clone());
            let pending = tokio::spawn(async move {
                app.oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("x-phone-control", "1")
                        .header("x-phone-owner", "privacy-fixture")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap()
            });
            tokio::time::timeout(Duration::from_secs(2), async {
                while state
                    .runner_control_pending
                    .load(std::sync::atomic::Ordering::Acquire)
                    == 0
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            hold(state.clone(), true).await;
            hold(state.clone(), false).await;
            drop(client);
            let bytes = pending
                .await
                .unwrap()
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["ok"], false);
            assert_eq!(value["retry_safe"], false);
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "queued list sent a scripted Native request after revocation"
            );
            assert!(state
                .runner_actionable
                .load(std::sync::atomic::Ordering::Acquire));
        }
    });
}

#[test]
fn list_revocation_during_first_read_hides_rows_without_degrading_hardware() {
    block(async {
        for uri in ["/agent/collect", "/agent/scroll_find"] {
            let started = Arc::new(tokio::sync::Notify::new());
            let signal = started.clone();
            let sources = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let reads = sources.clone();
            let actions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let swipes = actions.clone();
            let runner = mock_runner(move |request, _| {
                if request.contains("/actions") {
                    swipes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                if request.contains("/source") {
                    reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    signal.notify_one();
                    return Some((Duration::from_millis(150),json!({"value":{"type":"XCUIElementTypeApplication","label":"PRIVATE_LIST_APP","rect":{"x":0,"y":0,"width":390,"height":844}}}).to_string()));
                }
                Some((
                    Duration::ZERO,
                    if request.starts_with("POST /session ") {
                        json!({"value":{"sessionId":"SESSION"}}).to_string()
                    } else {
                        json!({"value":null}).to_string()
                    },
                ))
            });
            let state = build_state_with_runner(runner.url());
            state
                .runner_actionable
                .store(true, std::sync::atomic::Ordering::Release);
            let app = server::http::router(state.clone());
            let pending = tokio::spawn(async move {
                app.oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("x-phone-control", "1")
                        .header("x-phone-owner", "privacy-fixture")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            (if uri == "/agent/collect" {json!({"max_pages":2})} else {json!({"label":"missing","max_swipes":1})}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap()
            });
            tokio::time::timeout(Duration::from_secs(4), started.notified())
                .await
                .unwrap();
            hold(state.clone(), true).await;
            let bytes = pending
                .await
                .unwrap()
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes();
            assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE_LIST"));
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["ok"], false);
            assert_eq!(value["retry_safe"], false);
            assert_eq!(value["swipes_confirmed"], 0);
            assert_eq!(value["swipes_uncertain"], 0);
            assert_eq!(sources.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(actions.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(state
                .runner_actionable
                .load(std::sync::atomic::Ordering::Acquire));
        }
    });
}

#[test]
fn list_setup_failures_never_claim_safe_replay_or_global_not_sent() {
    block(async {
        for uri in ["/agent/collect", "/agent/scroll_find"] {
            for app_mismatch in [false, true] {
                let sources = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let reads = sources.clone();
                let runner = mock_runner(move |request, _| {
                    if request.contains("/source") {
                        let n = reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if !app_mismatch {
                            return None;
                        }
                        return Some((Duration::ZERO,json!({"value":{"type":"XCUIElementTypeApplication","label":if n==0 {"ExpectedApp"}else{"OtherApp"},"rect":{"x":0,"y":0,"width":390,"height":844}}}).to_string()));
                    }
                    Some((
                        Duration::ZERO,
                        if request.starts_with("POST /session ") {
                            json!({"value":{"sessionId":"SESSION"}}).to_string()
                        } else if request.contains("/window/size") {
                            json!({"value":{"width":390,"height":844}}).to_string()
                        } else {
                            json!({"value":null}).to_string()
                        },
                    ))
                });
                let state = build_state_with_runner(runner.url());
                let app = server::http::router(state);
                let mut body = if uri == "/agent/collect" {json!({"max_pages":1})} else {json!({"label":"missing","max_swipes":0})};
                if app_mismatch {
                    let response = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .uri("/agent/elements?ocr=off")
                                .header("x-phone-owner", "privacy-fixture")
                                .body(Body::empty())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    let bytes = response.into_body().collect().await.unwrap().to_bytes();
                    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    body["snapshot"] = value["snapshot"].clone();
                    assert!(body["snapshot"].is_string(), "seed snapshot: {value}");
                }
                let response = app
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(uri)
                            .header("x-phone-control", "1")
                            .header("x-phone-owner", "privacy-fixture")
                            .header("content-type", "application/json")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    value["error"],
                    if app_mismatch {
                        "app_changed"
                    } else {
                        "runner_source_failed"
                    },
                    "{value}"
                );
                assert_eq!(value["retry_safe"], false, "{value}");
                assert_ne!(value["outcome"], "not_sent");
                assert_eq!(value["swipes_confirmed"], 0);
                assert_eq!(value["swipes_uncertain"], 0);
                assert_eq!(value["swipe_outcome"], "not_sent");
            }
        }
    });
}
