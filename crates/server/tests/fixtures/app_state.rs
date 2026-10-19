// Shared `AppState` fixture.
//
// One source of truth for both test layers. It used to exist twice — once in
// `crates/server/tests/http_auth.rs` and once copied into the unit tests in
// `crates/server/src/http.rs` — so a new field had to be added in two places
// and the copies could drift apart silently.
//
// `include!`d rather than imported, because the two layers reach the same
// items by different paths: inside the crate they are `crate::…`, from an
// integration test they are `server::…`. Each include site defines the two
// alias below first, so this file can name everything one way.
//
//   use crate as srv;   // in-crate
//   use server as srv;  // integration
//
// Kept in `tests/` on purpose: nothing here is compiled into the shipped
// binary, and no test-only constructor is exposed from production code.

/// A minimal state with no runner client attached.
#[allow(dead_code)]
fn fixture_app_state(password: Option<&str>) -> Arc<AppState> {
    Arc::new(AppState {
        password: password.map(|s| s.to_string()),
        secret: b"test-secret-key-0123456789abcdef".to_vec(),
        session_ttl_secs: 3600,
        cookie_secure: false,
        base_path: String::new(),
        auth_limiter: Arc::new(Mutex::new(srv::http::AuthLimiter::new())),
        agent_token: None,
        device_udid: None,
        simulator: None,
        inbox: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        runner: None,
        managed_runner: false,
        managed_runner_pending: false,
        latest_release: std::sync::Arc::new(std::sync::Mutex::new(None)),
        mjpeg_url: None,
        video: None,
        runner_actionable: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        runner_read_degraded: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pairing: std::sync::Arc::new(srv::pairing::Pairing::new(true)),
        runner_health: std::sync::Arc::new(std::sync::Mutex::new(srv::runner::RunnerHealth::down())),
        runner_death: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
        runner_health_probe: std::sync::Arc::new(std::sync::Mutex::new(None)),
        settled_frame_prefetch: std::sync::Arc::new(std::sync::Mutex::new(None)),
        runner_control_pending: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        last_activity: std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
        released: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        runner_lifecycle: std::sync::Arc::new(srv::http::RunnerLifecycle::new()),
        live_streams: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        mjpeg_stream_activity: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        agent_focus: std::sync::Arc::new(std::sync::Mutex::new(
            srv::focus::AgentFocus::default(),
        )),
        flow_trail: std::sync::Arc::new(std::sync::Mutex::new(
            srv::flows::FlowTrail::default(),
        )),
        element_snapshots: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::new(),
        )),
        owner_snapshots: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        hold_until: std::sync::Arc::new(std::sync::Mutex::new(None)),
        private_until: std::sync::Arc::new(std::sync::Mutex::new(None)),
        owner: std::sync::Arc::new(std::sync::Mutex::new(None)),
        owner_lease_secs: 300,
        // Never launchd: a test that reconnects swaps in its own fake.
        runner_bootstrap: |_, _, _| false,
    })
}
