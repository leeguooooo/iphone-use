//! Token-only continuation through actual HTTP entry points. The runner is synthetic and no
//! physical device is touched. Each test records all native calls, including refused attempts.
mod support;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{block, build_state_with_runner, mock_runner, MockRunner};
use tower::ServiceExt;

#[derive(Default)]
struct PrivacyHooks {
    key_started: tokio::sync::Notify,
    target_started: tokio::sync::Notify,
    clear_started: tokio::sync::Notify,
    session_started: tokio::sync::Notify,
    delay: Mutex<Option<&'static str>>,
    atomic: std::sync::atomic::AtomicBool,
}
struct Fixture {
    hooks: Arc<PrivacyHooks>,
    _runner: MockRunner,
    state: Arc<server::http::AppState>,
    calls: Arc<Mutex<Vec<String>>>,
    text: Arc<Mutex<String>>,
    failure: Arc<Mutex<Option<&'static str>>>,
}
impl Fixture {
    fn new(guarded: bool) -> Self {
        Self::with_capabilities(guarded, guarded)
    }
    fn with_capabilities(guarded: bool, continuation: bool) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let text = Arc::new(Mutex::new(String::new()));
        let failure = Arc::new(Mutex::new(None));
        let hooks=Arc::new(PrivacyHooks::default());
        let hook=hooks.clone();
        let (seen, typed, fail) = (calls.clone(), text.clone(), failure.clone());
        let runner = mock_runner(move |request, _| {
            seen.lock().unwrap().push(request.to_string());
            let response = if request.starts_with("POST /session ") {
                hook.session_started.notify_one();
                let body=json!({"value":{"sessionId":"SESSION","capabilities":{"ipu:guardedInput":guarded,"ipu:inputContinuation":continuation,"ipu:atomicText":hook.atomic.load(std::sync::atomic::Ordering::Acquire)}}});
                return Some((if *hook.delay.lock().unwrap()==Some("session") {Duration::from_millis(500)}else{Duration::ZERO},body.to_string()));
            } else if request.contains("/wda/input-target") {
                hook.target_started.notify_one();
                return Some((if *hook.delay.lock().unwrap()==Some("target") {Duration::from_millis(500)}else{Duration::ZERO},json!({"value":{"token":"bound-field"}}).to_string()));
            } else if request.contains("/element/active") {
                hook.target_started.notify_one();
                return Some((if *hook.delay.lock().unwrap()==Some("legacy-active") {Duration::from_millis(500)}else{Duration::ZERO},json!({"value":{"ELEMENT":"bound-field"}}).to_string()));
            } else if request.contains("/element/bound-field/clear") {
                typed.lock().unwrap().clear();
                hook.clear_started.notify_one();
                return Some((if *hook.delay.lock().unwrap()==Some("clear") {Duration::from_millis(500)}else{Duration::ZERO},json!({"value":null}).to_string()));
            } else if request.contains("/ipu/text") {
                let body:Value=serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                if body["clear"]==true {typed.lock().unwrap().clear();}
                typed.lock().unwrap().push_str(body["text"].as_str().unwrap());
                json!({"value":null})
            } else if request.contains("/wda/keys") {
                let body: Value =
                    serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                assert_eq!(body["inputTarget"], "bound-field");
                match *fail.lock().unwrap() {
                    Some("transport") => return None,
                    Some("focus") => {
                        let body = r#"{"value":{"error":"invalid argument","message":"input_target_focus_changed; nothing was sent"}}"#;
                        return Some((Duration::ZERO,format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body)));
                    }
                    _ => {}
                }
                typed
                    .lock()
                    .unwrap()
                    .push_str(body["value"][0].as_str().unwrap());
                hook.key_started.notify_one();
                return Some((
                    Duration::from_millis(250),
                    json!({"value":null}).to_string(),
                ));
            } else if request.contains("/source") {
                json!({"value":{"type":"XCUIElementTypeApplication","label":"Fixture","rect":{"x":0,"y":0,"width":390,"height":844},
                    "children":[{"type":"XCUIElementTypeTextField","label":"Draft","value":typed.lock().unwrap().clone(),
                        "isFocused":"1","isEnabled":"1","rect":{"x":10,"y":100,"width":300,"height":50},"children":[]}]}})
            } else if request.contains("/window/size") {
                json!({"value":{"width":390,"height":844}})
            } else {
                json!({"value":null})
            };
            Some((Duration::ZERO, response.to_string()))
        });
        let state = build_state_with_runner(runner.url());
        Self {
            hooks,
            _runner: runner,
            state,
            calls,
            text,
            failure,
        }
    }
    async fn call(
        &self,
        path: &str,
        owner: Option<&str>,
        body: Option<Value>,
        takeover: bool,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(if body.is_some() { "POST" } else { "GET" })
            .uri(path)
            .header("x-phone-control", "1")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(owner) = owner {
            request = request.header("x-phone-owner", owner);
        }
        if takeover {
            request = request.header("x-phone-owner-takeover", "1");
        }
        let response = server::http::router(self.state.clone())
            .oneshot(
                request
                    .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn start(&self, batch: bool) -> String {
        let action = json!({"type":"text","text":"字".repeat(500),"budget_ms":6000});
        let payload = if batch {
            json!({"steps":[{"kind":"action","action":action},{"kind":"action","action":{"type":"home"}}]})
        } else {
            action
        };
        let (status, body) = self
            .call(
                if batch {
                    "/agent/actions"
                } else {
                    "/agent/input"
                },
                Some("a"),
                Some(payload),
                false,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["characters_confirmed"], 200, "{body}");
        assert_eq!(body["characters_uncertain"], 0, "{body}");
        assert_eq!(body["remaining_text"], "字".repeat(300), "{body}");
        assert_eq!(body["continuation_safe"], true, "{body}");
        assert_eq!(
            body["retry_safe"], false,
            "whole-request replay stays unsafe: {body}"
        );
        body["continue_token"]
            .as_str()
            .expect("safe continuation token")
            .to_string()
    }
    fn count(&self, path: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.contains(path))
            .count()
    }
}

#[test]
fn continuation_reuses_one_native_target_after_reads_and_cannot_be_replayed() {
    block(async {
        for batch in [false, true] {
            let f = Fixture::new(true);
            let token = f.start(batch).await;
            assert_eq!(f.count("/wda/homescreen"), 0);
            let (status, body) = f.call("/agent/elements", Some("a"), None, false).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (status, body) = f
                .call(
                    "/agent/input",
                    Some("a"),
                    Some(json!({"type":"text","continue_token":token})),
                    false,
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["ok"], true, "{body}");
            assert_eq!(*f.text.lock().unwrap(), "字".repeat(500));
            assert_eq!(
                f.count("/wda/input-target"),
                1,
                "resume never rebinds focus"
            );
            assert_eq!(f.count("/wda/keys"), 3);
            let before = f.calls.lock().unwrap().len();
            let (status, body) = f
                .call(
                    "/agent/input",
                    Some("a"),
                    Some(json!({"type":"text","continue_token":token})),
                    false,
                )
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["error"], "input_continuation_expired");
            assert_eq!(
                f.calls.lock().unwrap().len(),
                before,
                "consumed token sends no native request"
            );
        }
    });
}

#[test]
fn wrong_owner_does_not_consume_but_release_takeover_and_session_changes_invalidate() {
    block(async {
        for reason in ["wrong-owner", "release", "takeover", "session", "mutation"] {
            let f = Fixture::new(true);
            let token = f.start(false).await;
            if reason == "wrong-owner" {
                let (status, _) = f
                    .call(
                        "/agent/input",
                        Some("b"),
                        Some(json!({"type":"text","continue_token":token})),
                        false,
                    )
                    .await;
                assert_eq!(status, StatusCode::CONFLICT);
            } else if reason == "release" {
                let (status, _) = f
                    .call(
                        "/agent/owner",
                        Some("a"),
                        Some(json!({"release":true})),
                        false,
                    )
                    .await;
                assert_eq!(status, StatusCode::OK);
            } else if reason == "takeover" {
                let (status, _) = f
                    .call(
                        "/agent/input",
                        Some("b"),
                        Some(json!({"type":"text","continue_token":token})),
                        true,
                    )
                    .await;
                assert_eq!(status, StatusCode::CONFLICT);
            } else if reason == "session" {
                f.state
                    .runner
                    .as_ref()
                    .unwrap()
                    .lock()
                    .await
                    .invalidate_session();
            } else {
                let (status, body) = f
                    .call(
                        "/agent/input",
                        Some("a"),
                        Some(json!({"type":"home"})),
                        false,
                    )
                    .await;
                assert_eq!(status, StatusCode::OK, "{body}");
            }
            let before = f.count("/wda/keys");
            let (status, body) = f
                .call(
                    "/agent/input",
                    Some("a"),
                    Some(json!({"type":"text","continue_token":token})),
                    reason == "takeover",
                )
                .await;
            if reason == "wrong-owner" {
                assert_eq!(status, StatusCode::OK, "{body}");
            } else {
                assert_eq!(status, StatusCode::CONFLICT, "{reason}: {body}");
                assert_eq!(body["error"], "input_continuation_expired");
                assert_eq!(f.count("/wda/keys"), before);
            }
            assert_eq!(f.count("/wda/input-target"), 1);
        }
    });
}

#[test]
fn resume_never_rebinds_after_focus_loss_or_unknown_delivery() {
    block(async {
        for failure in ["focus", "transport"] {
            let f = Fixture::new(true);
            let token = f.start(false).await;
            *f.failure.lock().unwrap() = Some(failure);
            let (status, body) = f
                .call(
                    "/agent/input",
                    Some("a"),
                    Some(json!({"type":"text","continue_token":token})),
                    false,
                )
                .await;
            assert!(!status.is_success(), "{body}");
            assert!(body.get("continue_token").is_none(), "{body}");
            if failure == "transport" {
                assert_eq!(body["characters_total"], 300, "resumed-call counts: {body}");
                assert_eq!(body["characters_uncertain"], 200, "{body}");
                assert_eq!(body["remaining_text"], "字".repeat(300));
            } else {
                assert_eq!(body["error"], "runner_rejected");
            }
            let (_, body) = f
                .call(
                    "/agent/input",
                    Some("a"),
                    Some(json!({"type":"text","continue_token":token})),
                    false,
                )
                .await;
            assert_eq!(body["error"], "input_continuation_expired");
            assert_eq!(f.count("/wda/keys"), 2, "failed chunk is never replayed");
            assert_eq!(f.count("/wda/input-target"), 1);
        }
    });
}

#[test]
fn invalid_continuations_and_anonymous_budgets_never_reach_runner() {
    block(async {
        let f = Fixture::new(true);
        for body in [
            json!({"type":"text","continue_token":"bad"}),
            json!({"type":"text","continue_token":"a".repeat(64),"text":"extra"}),
            json!({"type":"text","text":"x","budget_ms":5999}),
        ] {
            let (status, _) = f.call("/agent/input", Some("a"), Some(body), false).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        let (status, _) = f
            .call(
                "/agent/input",
                None,
                Some(json!({"type":"text","text":"x","budget_ms":6000})),
                false,
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT); // The named owner already holds this fixture.
        assert!(f.calls.lock().unwrap().is_empty());
        let anonymous = Fixture::new(true);
        let (status, body) = anonymous
            .call(
                "/agent/input",
                None,
                Some(json!({"type":"text","text":"x","budget_ms":6000})),
                false,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "input_owner_required");
        assert!(anonymous.calls.lock().unwrap().is_empty());
        for guarded in [false, true] {
            let f = Fixture::with_capabilities(guarded, false);
            let (status, body) = f
                .call(
                    "/agent/input",
                    Some("a"),
                    Some(json!({"type":"text","text":"x".repeat(300),"budget_ms":6000})),
                    false,
                )
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["error"], "input_continuation_unsupported");
            assert_eq!(f.count("/wda/keys"), 0);
        }
    });
}

#[test]
fn repeated_safe_boundaries_rotate_tokens_without_reselecting_the_field() {
    block(async {
        let f = Fixture::new(true);
        let (mut status, mut body) = f
            .call(
                "/agent/input",
                Some("a"),
                Some(json!({"type":"text","text":"字".repeat(900),"budget_ms":6000})),
                false,
            )
            .await;
        let mut tokens = Vec::new();
        for _ in 0..6 {
            if status == StatusCode::OK {
                break;
            }
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
            let token = body["continue_token"].as_str().unwrap().to_string();
            assert!(!tokens.contains(&token));
            tokens.push(token.clone());
            (status, body) = f
                .call(
                    "/agent/input",
                    Some("a"),
                    Some(json!({"type":"text","continue_token":token})),
                    false,
                )
                .await;
            if status != StatusCode::OK {
                let (old_status, old_body) = f
                    .call(
                        "/agent/input",
                        Some("a"),
                        Some(json!({"type":"text","continue_token":token})),
                        false,
                    )
                    .await;
                assert_eq!(old_status, StatusCode::CONFLICT, "{old_body}");
            }
        }
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(*f.text.lock().unwrap(), "字".repeat(900));
        assert_eq!(f.count("/wda/input-target"), 1);
        assert_eq!(f.count("/wda/keys"), 5);
    });
}

#[test]
fn ownership_change_during_a_chunk_stops_before_the_next_chunk_without_a_token() {
    block(async {
        let f = Fixture::new(true);
        let calls = f.calls.clone();
        let state = f.state.clone();
        let handoff = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.contains("/wda/keys"))
                {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap();
            state
                .owner_epoch
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        });
        let (_, body) = f
            .call(
                "/agent/input",
                Some("a"),
                Some(json!({"type":"text","text":"字".repeat(500),"budget_ms":15000})),
                false,
            )
            .await;
        handoff.await.unwrap();
        assert_eq!(body["characters_confirmed"], 200, "{body}");
        assert_eq!(body["characters_uncertain"], 0, "{body}");
        assert!(body.get("continue_token").is_none(), "{body}");
        assert_eq!(
            body["detail"]["input_error"], "input_owner_changed",
            "{body}"
        );
        assert_eq!(f.count("/wda/keys"), 1);
    });
}

#[test]
fn cancellation_after_resuming_consumes_the_token_and_never_replays_the_request() {
    block(async {
        let f = Fixture::new(true);
        let token = f.start(false).await;
        {
            let request = f.call(
                "/agent/input",
                Some("a"),
                Some(json!({"type":"text","continue_token":token})),
                false,
            );
            tokio::pin!(request);
            tokio::select! {
                reply=&mut request => panic!("resume completed before cancellation: {reply:?}"),
                _=async {
                    tokio::time::timeout(Duration::from_secs(5),async {
                        while f.count("/wda/keys")<2 {tokio::time::sleep(Duration::from_millis(2)).await;}
                    }).await.unwrap();
                } => {}
            }
            // Dropping this request may leave the chunk on the phone. No acknowledgement,
            // and therefore no new token, exists; the consumed token must never be replayed.
        }
        let (status, body) = f
            .call(
                "/agent/input",
                Some("a"),
                Some(json!({"type":"text","continue_token":token})),
                false,
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "input_continuation_expired");
        assert_eq!(f.count("/wda/keys"), 2);
        assert_eq!(f.count("/wda/input-target"), 1);
    });
}

#[test]
fn input_that_loses_ownership_while_waiting_for_the_runner_never_binds_or_types() {
    block(async {
        for characters in [3, 500] {
            let f = Fixture::new(true);
            let guard = f.state.runner.as_ref().unwrap().lock().await;
            let request = f.call(
                "/agent/input",
                Some("a"),
                Some(json!({"type":"text","text":"字".repeat(characters)})),
                false,
            );
            tokio::pin!(request);
            tokio::select! {
                reply=&mut request => panic!("request bypassed the runner lock: {reply:?}"),
                _=async {
                    tokio::time::timeout(Duration::from_secs(5),async {
                        loop {
                            let claimed = f.state.owner.lock().unwrap().as_ref().is_some_and(|o| o.name=="a");
                            if claimed { break; }
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                    }).await.unwrap();
                } => {}
            }
            // The takeover itself is authorized by the header; the invalid body then fails before
            // waiting for runner. This changes the owner while A is still queued at the real lock.
            let (status, _) = f
                .call(
                    "/agent/input",
                    Some("b"),
                    Some(json!({"type":"text","text":"x","budget_ms":1})),
                    true,
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            drop(guard);
            let (status, body) = tokio::time::timeout(Duration::from_secs(5), &mut request)
                .await
                .unwrap();
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["error"], "input_owner_changed");
            assert!(
                f.calls.lock().unwrap().is_empty(),
                "no target read, focus change, unlock or text request"
            );
        }
    });
}

#[test]
fn completing_input_only_renews_its_existing_lease_and_never_reclaims_ownership() {
    block(async {
        for transition in ["takeover", "release", "expiry"] {
            let mut f = Fixture::new(true);
            Arc::get_mut(&mut f.state).unwrap().owner_lease_secs = 1;
            let request = f.call(
                "/agent/input",
                Some("a"),
                Some(json!({"type":"text","text":"字".repeat(500),"budget_ms":6000})),
                true,
            );
            tokio::pin!(request);
            tokio::select! {
                reply=&mut request => panic!("input finished before ownership transition: {reply:?}"),
                _=async {
                    tokio::time::timeout(Duration::from_secs(5),async {
                        while f.count("/wda/keys")==0 {tokio::time::sleep(Duration::from_millis(2)).await;}
                    }).await.unwrap();
                } => {}
            }
            let epoch = f
                .state
                .owner_epoch
                .load(std::sync::atomic::Ordering::Acquire);
            if transition == "takeover" {
                let (status, _) = f
                    .call(
                        "/agent/input",
                        Some("b"),
                        Some(json!({"type":"text","text":"x","budget_ms":1})),
                        true,
                    )
                    .await;
                assert_eq!(status, StatusCode::BAD_REQUEST);
            } else if transition == "release" {
                let (status, _) = f
                    .call(
                        "/agent/owner",
                        Some("a"),
                        Some(json!({"release":true})),
                        false,
                    )
                    .await;
                assert_eq!(status, StatusCode::OK);
            } else {
                f.state.owner.lock().unwrap().as_mut().unwrap().last_seen =
                    std::time::Instant::now() - Duration::from_secs(2);
            }
            let (_, body) = (&mut request).await;
            assert_eq!(body["characters_confirmed"], 200, "{body}");
            let owner = f
                .state
                .owner
                .lock()
                .unwrap()
                .as_ref()
                .map(|owner| owner.name.clone());
            match transition {
                "takeover" => {
                    assert_eq!(owner.as_deref(), Some("b"));
                    assert!(body.get("continue_token").is_none());
                }
                "release" => {
                    assert!(owner.is_none());
                    assert!(body.get("continue_token").is_none());
                }
                "expiry" => {
                    assert_eq!(owner.as_deref(), Some("a"));
                    assert_eq!(
                        f.state
                            .owner_epoch
                            .load(std::sync::atomic::Ordering::Acquire),
                        epoch,
                        "in-flight renewal must not start a new ownership incarnation"
                    );
                    let (status, result) = f
                        .call(
                            "/agent/input",
                            Some("a"),
                            Some(json!({"type":"text","continue_token":body["continue_token"]})),
                            false,
                        )
                        .await;
                    assert_eq!(status, StatusCode::OK, "{result}");
                    assert_eq!(*f.text.lock().unwrap(), "字".repeat(500));
                }
                _ => unreachable!(),
            }
        }
    });
}


async fn private_hold(f:&Fixture, private:bool) {
    let (status,body)=f.call("/agent/hold",Some("a"),Some(json!({"secs":if private{60}else{0},"private":private})),false).await;
    assert_eq!(status,StatusCode::OK,"{body}");
}
fn input_payload(batch:bool, action:Value)->Value {
    if batch {json!({"observe":false,"steps":[{"kind":"action","action":action},{"kind":"action","action":{"type":"home"}}]})} else {action}
}
fn input_path(batch:bool)->&'static str {if batch {"/agent/actions"}else{"/agent/input"}}

#[test]
fn privacy_transition_invalidates_saved_continuation_without_native_input() {
    block(async {
        for batch in [false,true] {for expire in [false,true] {
            let f=Fixture::new(true);let token=f.start(false).await;
            if expire {
                let (status,_)=f.call("/agent/hold",Some("a"),Some(json!({"secs":1,"private":true})),false).await;
                assert_eq!(status,StatusCode::OK);
                tokio::time::sleep(Duration::from_millis(1100)).await;
            } else {private_hold(&f,true).await;private_hold(&f,false).await;}
            let before=f.calls.lock().unwrap().len();
            let (status,body)=f.call(input_path(batch),Some("a"),Some(input_payload(batch,json!({"type":"text","continue_token":token}))),false).await;
            assert_eq!(status,StatusCode::CONFLICT,"{body}");
            assert_eq!(body["error"],"input_continuation_expired","{body}");
            assert_eq!(f.calls.lock().unwrap().len(),before,"an invalid token must send no runner request");
            assert_eq!(*f.text.lock().unwrap(),"字".repeat(200));
        }}
    });
}

#[test]
fn privacy_transition_stops_long_input_after_the_current_acknowledged_chunk() {
    block(async {
        for batch in [false,true] {for lift in [false,true] {
            let f=Arc::new(Fixture::new(true));let caller=f.clone();
            let pending=tokio::spawn(async move {caller.call(input_path(batch),Some("a"),Some(input_payload(batch,json!({"type":"text","text":"字".repeat(800)}))),false).await});
            tokio::time::timeout(Duration::from_secs(3),f.hooks.key_started.notified()).await.unwrap();
            private_hold(&f,true).await;if lift{private_hold(&f,false).await;}
            let (status,body)=pending.await.unwrap();
            assert_eq!(status,StatusCode::BAD_GATEWAY,"{body}");
            assert_eq!(body["characters_confirmed"],200,"{body}");
            assert_eq!(body["characters_uncertain"],0,"{body}");
            assert_eq!(body["detail"]["input_error"],"input_privacy_changed","{body}");
            assert!(body.get("continue_token").is_none());
            assert_eq!(f.count("/wda/keys"),1);
            assert_eq!(f.count("/wda/homescreen"),0);
        }}
    });
}

#[test]
fn privacy_transition_while_queued_refuses_short_input_before_any_runner_request() {
    block(async {
        for batch in [false,true] {
            let f=Arc::new(Fixture::new(true));f.hooks.atomic.store(true,std::sync::atomic::Ordering::Release);
            let guard=f.state.runner.as_ref().unwrap().lock().await;
            let caller=f.clone();
            let pending=tokio::spawn(async move {caller.call(input_path(batch),Some("a"),Some(input_payload(batch,json!({"type":"text","text":"x"}))),false).await});
            tokio::time::timeout(Duration::from_secs(3),async {loop {
                if f.state.owner.lock().unwrap().as_ref().is_some_and(|owner|owner.name=="a") {break;}
                tokio::task::yield_now().await;
            }}).await.unwrap();
            private_hold(&f,true).await;private_hold(&f,false).await;drop(guard);
            let (status,body)=pending.await.unwrap();
            assert_eq!(status,StatusCode::CONFLICT,"{body}");
            assert_eq!(body["error"],"input_privacy_changed","{body}");
            assert_eq!(body["outcome"],"not_sent");
            assert!(f.calls.lock().unwrap().is_empty(),"queued input must be refused before session/binding/typing");
        }
    });
}

#[test]
fn privacy_transition_during_session_binding_or_clear_never_dispatches_later_text() {
    block(async {
        for batch in [false,true] {for phase in ["session","target","clear","legacy-active"] {
            let f=Arc::new(Fixture::with_capabilities(phase!="legacy-active",phase!="legacy-active"));*f.hooks.delay.lock().unwrap()=Some(phase);
            f.hooks.atomic.store(phase=="session",std::sync::atomic::Ordering::Release);
            let caller=f.clone();
            let action=if phase=="session" || phase=="legacy-active" {json!({"type":"text","text":"x","clear":true})}else{json!({"type":"text","text":"字".repeat(500),"clear":true})};
            let pending=tokio::spawn(async move {caller.call(input_path(batch),Some("a"),Some(input_payload(batch,action)),false).await});
            let signal=match phase {"session"=>&f.hooks.session_started,"target"|"legacy-active"=>&f.hooks.target_started,_=>&f.hooks.clear_started};
            tokio::time::timeout(Duration::from_secs(3),signal.notified()).await.unwrap();
            private_hold(&f,true).await;private_hold(&f,false).await;
            let (status,body)=pending.await.unwrap();
            if phase=="clear" {
                assert_eq!(status,StatusCode::BAD_GATEWAY,"{body}");
                assert_eq!(body["characters_confirmed"],0);
                assert_eq!(body["characters_uncertain"],0);
                assert_eq!(body["retry_safe"],false);
                assert_eq!(body["detail"]["input_error"],"input_privacy_changed");
                assert_eq!(body["detail"]["clear_applied"],true);
                if batch {assert_eq!(body["batch_outcome"],"partially_applied");}
            } else {
                assert_eq!(status,StatusCode::CONFLICT,"{body}");
                assert_eq!(body["error"],"input_privacy_changed");
            }
            assert!(body.get("continue_token").is_none());
            assert_eq!(f.count("/wda/keys"),0);
            assert_eq!(f.count("/ipu/text"),0);
            assert_eq!(f.count("/wda/homescreen"),0);
            assert_eq!(f.count("/clear"),usize::from(phase=="clear"));
        }}
    });
}

#[test]
fn private_hold_refuses_unobserved_text_and_nonprivate_hold_preserves_continuation() {
    block(async {
        for batch in [false,true] {
            let f=Fixture::new(true);f.hooks.atomic.store(true,std::sync::atomic::Ordering::Release);private_hold(&f,true).await;
            let (status,body)=f.call(input_path(batch),Some("a"),Some(input_payload(batch,json!({"type":"text","text":"x"}))),false).await;
            assert_eq!(status,StatusCode::CONFLICT,"{body}");
            assert_eq!(body["error"],"input_privacy_changed");
            assert!(f.calls.lock().unwrap().is_empty());
        }
        let f=Fixture::new(true);let token=f.start(false).await;
        let (status,_)=f.call("/agent/hold",Some("a"),Some(json!({"secs":60})),false).await;
        assert_eq!(status,StatusCode::OK);
        let (status,body)=f.call("/agent/input",Some("a"),Some(json!({"type":"text","continue_token":token})),false).await;
        assert_eq!(status,StatusCode::OK,"{body}");
        assert_eq!(*f.text.lock().unwrap(),"字".repeat(500));
    });
}

#[test]
fn privacy_transition_during_resumed_input_preserves_acknowledged_delivery_only() {
    block(async {
        for batch in [false,true] {
            let f=Arc::new(Fixture::new(true));let token=f.start(false).await;
            f.hooks.key_started.notified().await; // consume the initial request's signal
            let caller=f.clone();let resume=token.clone();
            let pending=tokio::spawn(async move {caller.call(input_path(batch),Some("a"),Some(input_payload(batch,json!({"type":"text","continue_token":resume}))),false).await});
            tokio::time::timeout(Duration::from_secs(3),f.hooks.key_started.notified()).await.unwrap();
            private_hold(&f,true).await;private_hold(&f,false).await;
            let (status,body)=pending.await.unwrap();
            assert_eq!(status,StatusCode::BAD_GATEWAY,"{body}");
            assert_eq!(body["characters_confirmed"],200);
            assert_eq!(body["characters_uncertain"],0);
            assert_eq!(body["remaining_text"],"字".repeat(100));
            assert_eq!(*f.text.lock().unwrap(),"字".repeat(400));
            assert!(body.get("continue_token").is_none());
            assert_eq!(f.count("/wda/input-target"),1);
            assert_eq!(f.count("/wda/keys"),2);
            let before=f.calls.lock().unwrap().len();
            let (status,body)=f.call("/agent/input",Some("a"),Some(json!({"type":"text","continue_token":token})),false).await;
            assert_eq!(status,StatusCode::CONFLICT,"{body}");
            assert_eq!(f.calls.lock().unwrap().len(),before);
        }
    });
}

#[test]
fn queued_input_cannot_cross_owner_aba_even_when_the_name_matches_again() {
    block(async {
        for batch in [false,true] {for length in [1,500] {
            let f=Arc::new(Fixture::new(true));f.hooks.atomic.store(true,std::sync::atomic::Ordering::Release);
            let guard=f.state.runner.as_ref().unwrap().lock().await;let caller=f.clone();
            let pending=tokio::spawn(async move {caller.call(input_path(batch),Some("a"),Some(input_payload(batch,json!({"type":"text","text":"x".repeat(length)}))),false).await});
            tokio::time::timeout(Duration::from_secs(3),async {loop {
                if f.state.owner.lock().unwrap().as_ref().is_some_and(|owner|owner.name=="a") {break;}
                tokio::task::yield_now().await;
            }}).await.unwrap();
            for owner in ["b","a"] {
                let (status,_)=f.call("/agent/input",Some(owner),Some(json!({"type":"text","text":"invalid","budget_ms":1})),true).await;
                assert_eq!(status,StatusCode::BAD_REQUEST);
            }
            let last_seen=f.state.owner.lock().unwrap().as_ref().unwrap().last_seen;
            drop(guard);let (status,body)=pending.await.unwrap();
            assert_eq!(status,StatusCode::CONFLICT,"{body}");
            assert_eq!(body["error"],"input_owner_changed");
            assert_eq!(f.state.owner.lock().unwrap().as_ref().unwrap().last_seen,last_seen,"an old request cannot renew the restored owner incarnation");
            assert!(f.calls.lock().unwrap().is_empty(),"a restored owner name must not authorize an old queued request");
        }}
    });
}

#[test]
fn expired_privacy_normalization_preserves_fresh_continuation() {
    block(async {
        let f=Fixture::new(true);
        let (status,_)=f.call("/agent/hold",Some("a"),Some(json!({"secs":1,"private":true})),false).await;
        assert_eq!(status,StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let token=f.start(false).await;
        let (status,_)=f.call("/agent/hold",Some("a"),Some(json!({"secs":60})),false).await;
        assert_eq!(status,StatusCode::OK);
        let (status,body)=f.call("/agent/input",Some("a"),Some(json!({"type":"text","continue_token":token})),false).await;
        assert_eq!(status,StatusCode::OK,"work started after privacy expired must survive ordinary hold: {body}");
        assert_eq!(*f.text.lock().unwrap(),"字".repeat(500));
    });
}
