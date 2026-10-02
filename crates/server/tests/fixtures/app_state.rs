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

/// A minimal state with no WDA client attached.
#[allow(dead_code)]
fn fixture_app_state(password: Option<&str>) -> Arc<AppState> {
    Arc::new(AppState {
        password: password.map(|s| s.to_string()),
        secret: b"test-secret-key-0123456789abcdef".to_vec(),
        session_ttl_secs: 3600,
        cookie_secure: false,
        auth_limiter: Arc::new(Mutex::new(srv::http::AuthLimiter::new())),
        agent_token: None,
        device_udid: None,
        inbox: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        wda: None,
        managed_wda: false,
        managed_wda_pending: false,
        latest_release: std::sync::Arc::new(std::sync::Mutex::new(None)),
        mjpeg_url: None,
        video: None,
        wda_actionable: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        wda_health: std::sync::Arc::new(std::sync::Mutex::new(srv::wda::WdaHealth::down())),
        wda_death: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
        wda_health_probe: std::sync::Arc::new(std::sync::Mutex::new(None)),
        wda_control_pending: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        last_activity: std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
        released: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        wda_lifecycle: std::sync::Arc::new(srv::http::WdaLifecycle::new()),
        live_streams: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        mjpeg_stream_activity: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        element_snapshots: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::new(),
        )),
        hold_until: std::sync::Arc::new(std::sync::Mutex::new(None)),
        owner: std::sync::Arc::new(std::sync::Mutex::new(None)),
        owner_lease_secs: 300,
    })
}
