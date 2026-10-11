//! C entry point for the device runner: starts the iphone-use daemon inside
//! the runner process, on a thread of its own, so the phone serves the whole
//! agent API (`/agent/*`, flows, schedules) without a Mac in the request path.
//!
//! The runner is the phone's only control path and only a Mac can relaunch
//! it, so nothing here may take it down: a panic is caught and reported as a
//! status, never unwound into Swift.

use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;

/// 0 not started, 1 starting/serving, 2 failed (see `ipu_daemon_error`).
static STATE: AtomicI32 = AtomicI32::new(0);
static ERROR: OnceLock<std::ffi::CString> = OnceLock::new();

fn string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller passes NUL-terminated strings that outlive this call.
    let value = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn fail(message: String) {
    let _ = ERROR.set(std::ffi::CString::new(message.replace('\0', " ")).unwrap_or_default());
    STATE.store(2, Ordering::SeqCst);
}

/// Starts the daemon on `host` (NULL: 127.0.0.1) port `port` against the runner on `runner_port`
/// / `mjpeg_port`. Returns at once: 0 when the serving thread started, 1 when
/// it was already started, -1 when the thread could not be spawned. Every
/// string may be NULL.
#[no_mangle]
pub extern "C" fn ipu_daemon_start(
    host: *const c_char,
    port: u16,
    runner_port: u16,
    mjpeg_port: u16,
    runner_token: *const c_char,
    agent_token: *const c_char,
    password: *const c_char,
) -> i32 {
    if STATE.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return 1;
    }
    let options = server::boot::EmbeddedOptions {
        host: string(host).unwrap_or_else(|| "127.0.0.1".into()),
        port,
        runner_port,
        runner_mjpeg_port: mjpeg_port,
        runner_token: string(runner_token),
        agent_token: string(agent_token),
        password: string(password),
    };
    let spawned = std::thread::Builder::new()
        .name("ipu-daemon".into())
        .stack_size(8 << 20)
        .spawn(move || {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .with_target(false)
                .with_ansi(false)
                .try_init();
            match std::panic::catch_unwind(move || server::boot::serve_embedded(options)) {
                Ok(Ok(())) => fail("embedded daemon stopped".into()),
                Ok(Err(error)) => fail(format!("{error:#}")),
                Err(panic) => fail(format!(
                    "embedded daemon panicked: {}",
                    panic
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown panic")
                )),
            }
        });
    match spawned {
        Ok(_) => 0,
        Err(error) => {
            fail(format!("spawn daemon thread: {error}"));
            -1
        }
    }
}

/// 0 not started, 1 serving (or starting), 2 failed.
#[no_mangle]
pub extern "C" fn ipu_daemon_state() -> i32 {
    STATE.load(Ordering::SeqCst)
}

/// Mints a one-time pairing code for the iPhone app on this phone, writing it
/// NUL-terminated into `out` (capacity `capacity` bytes). Returns the code's
/// length, -1 while the daemon is not serving, -2 when `out` is NULL or too
/// small. The code is valid for five minutes and redeems once at
/// `POST /pair` on the daemon (`{"code"}`).
///
/// # Safety
/// A non-NULL `out` must point to at least `capacity` writable bytes and
/// remain valid for the duration of this call, with no concurrent access.
#[no_mangle]
pub unsafe extern "C" fn ipu_daemon_pair_code(out: *mut c_char, capacity: usize) -> i32 {
    if STATE.load(Ordering::SeqCst) != 1 {
        return -1;
    }
    let Ok(Some(code)) = std::panic::catch_unwind(server::boot::embedded_pair_code) else {
        return -1;
    };
    let bytes = code.as_bytes();
    if out.is_null() || bytes.len() + 1 > capacity {
        return -2;
    }
    // SAFETY: the caller guarantees `capacity` writable bytes; the check
    // above establishes that the code and its NUL fit in that buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast::<u8>(), bytes.len());
        *out.add(bytes.len()) = 0;
    }
    bytes.len() as i32
}

/// Why the daemon stopped, or NULL. Valid for the life of the process.
#[no_mangle]
pub extern "C" fn ipu_daemon_error() -> *const c_char {
    ERROR.get().map_or(std::ptr::null(), |e| e.as_ptr())
}
