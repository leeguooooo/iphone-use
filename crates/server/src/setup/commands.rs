//! `status`, `stop`, `pause`, `resume`: the lifecycle commands the daemon
//! (idle release runs `stop`) and operators call. None of them kills a
//! process without a matching PID record.

use std::time::Duration;

use super::ctx::{valid_port, Ctx};
use super::launchd;
use super::pid::{self, Legacy, Role};
use super::retry;
use super::sys;
use super::term::{info, ok, warn};

fn legacy(ctx: &Ctx) -> Legacy {
    Legacy::new(ctx, &ctx.team_id, &ctx.bundle_id)
}

fn stop_all(ctx: &Ctx, legacy: &Legacy) -> bool {
    let runner = pid::stop(ctx, &ctx.runner_pid_file, &legacy.runner, Role::Runner);
    let relay = pid::stop(ctx, &ctx.relay_pid_file, &legacy.relay, Role::Relay);
    let mjpeg = pid::stop(ctx, &ctx.mjpeg_relay_pid_file, &legacy.mjpeg, Role::Mjpeg);
    runner && relay && mjpeg
}

pub fn stop(ctx: &Ctx) -> i32 {
    if !ctx.state_dir().is_dir() {
        warn(&format!(
            "setup state is not initialized at {}; there are no PID-owned processes to stop safely",
            ctx.state_dir().display()
        ));
        return 1;
    }
    info("Stopping the dedicated runner supervisor and its managed processes");
    let label = &ctx.instance.wda_label;
    launchd::bootout(ctx, label);
    let mut failed = !launchd::wait_gone(ctx, label);
    if !stop_all(ctx, &legacy(ctx)) {
        failed = true;
    }
    if launchd::loaded(ctx, label) {
        failed = true;
    }
    if failed {
        warn("stop was not fully verified; no unowned process was killed");
        return 1;
    }
    ok("runner supervisor and all PID-verified managed processes stopped");
    0
}

pub fn pause(ctx: &Ctx) -> i32 {
    if !ctx.state_dir().is_dir() {
        warn(&format!(
            "setup state is not initialized at {}; there is no managed runner stack to pause",
            ctx.state_dir().display()
        ));
        return 1;
    }
    info("Pausing the managed device runner and giving the phone back to the user");
    let label = &ctx.instance.wda_label;
    // Disable before bootout so KeepAlive cannot race the shutdown.
    let mut failed = !launchd::disable(ctx, label);
    launchd::bootout(ctx, label);
    if !launchd::wait_gone(ctx, label) {
        failed = true;
    }
    if !stop_all(ctx, &legacy(ctx)) {
        failed = true;
    }
    if let Err(error) = retry::reset(&ctx.retry_state) {
        warn(&error);
        failed = true;
    }
    if launchd::disabled_state(ctx, label) != Some(true) || launchd::loaded(ctx, label) {
        failed = true;
    }
    if failed {
        warn("pause was not fully verified; no process without a matching PID/argv record was killed");
        return 1;
    }
    ok("device runner paused: supervisor disabled and all PID-verified runner/relay processes stopped");
    println!("  Resume: {} resume", ctx.self_install.display());
    0
}

pub fn resume(ctx: &Ctx) -> i32 {
    if !ctx.state_dir().is_dir() {
        warn(&format!(
            "setup state is not initialized at {}; run setup before resume",
            ctx.state_dir().display()
        ));
        return 1;
    }
    let plist = &ctx.wda_agent_plist;
    if !sys::marker_file_secure(plist) || !super::flow::plist_lints(plist) {
        warn(&format!(
            "managed runner supervisor plist is missing or unsafe: {}; run setup again",
            plist.display()
        ));
        return 1;
    }
    let label = &ctx.instance.wda_label;
    let identity_ok = sys::plist_top(plist, "Label") == *label
        && sys::plist_program_argument(plist, 0) == "/bin/bash"
        && sys::plist_program_argument(plist, 1) == ctx.self_install.to_string_lossy()
        && sys::is_executable(&ctx.self_install);
    if !identity_ok {
        warn(
            "managed runner supervisor identity is not the expected setup helper; run setup again",
        );
        return 1;
    }
    info("Resuming the managed runner supervisor");
    if let Err(error) = retry::reset(&ctx.retry_state) {
        warn(&error);
        return 1;
    }
    if !launchd::enable(ctx, label) {
        warn("could not enable the runner supervisor");
        return 1;
    }
    if !launchd::loaded(ctx, label) && !launchd::bootstrap(ctx, plist) {
        launchd::disable(ctx, label);
        warn("could not bootstrap the runner supervisor; it remains paused");
        return 1;
    }
    if launchd::disabled_state(ctx, label) != Some(false) || !launchd::loaded(ctx, label) {
        launchd::disable(ctx, label);
        warn("resume was not verified; the runner supervisor remains paused");
        return 1;
    }
    ok("device runner resume requested; lock-screen failures will retry with quiet backoff");
    println!("  Status: {} status", ctx.self_install.display());
    println!("  Log   : {}", ctx.wda_agent_log.display());
    0
}

pub fn status(ctx: &Ctx) -> i32 {
    if !ctx.state_dir().is_dir() {
        warn(&format!(
            "setup state is not initialized at {}; run setup before requesting runtime status",
            ctx.state_dir().display()
        ));
        return 1;
    }
    let (Some(wda_port), Some(mjpeg_port)) =
        (valid_port(&ctx.wda_port), valid_port(&ctx.mjpeg_port))
    else {
        warn("WDA_PORT and MJPEG_PORT must be distinct decimal TCP ports from 1 to 65535");
        return 1;
    };
    if wda_port == mjpeg_port {
        warn("WDA_PORT and MJPEG_PORT must be distinct decimal TCP ports from 1 to 65535");
        return 1;
    }
    let label = &ctx.instance.wda_label;
    if launchd::disabled_state(ctx, label) == Some(true) {
        warn(&format!(
            "the device runner is paused; run {} resume before the next agent session",
            ctx.self_install.display()
        ));
        return 1;
    }
    if sys::which("lsof").is_none() {
        warn("lsof is required to verify relay PID ownership and loopback-only binds");
        return 1;
    }
    let legacy = legacy(ctx);
    let mut failed = false;
    if launchd::loaded(ctx, label) {
        ok(&format!(
            "runner supervisor loaded: {}",
            launchd::service(ctx, label)
        ));
    } else {
        warn("runner supervisor not loaded");
        failed = true;
    }
    match pid::validate(
        ctx,
        &ctx.runner_pid_file,
        &legacy.runner,
        Role::Runner,
        false,
    ) {
        Some(pid) => ok(&format!("PID-verified device runner alive: {pid}")),
        None => {
            warn("device runner PID record is absent, stale, or does not match its process");
            failed = true;
        }
    }
    if pid::verify_loopback_listener(
        ctx,
        &ctx.relay_pid_file,
        &legacy.relay,
        Role::Relay,
        wda_port,
    ) {
        ok(&format!("control relay PID owns only 127.0.0.1:{wda_port}"));
    } else {
        warn("control relay ownership/bind could not be verified");
        failed = true;
    }
    if pid::verify_loopback_listener(
        ctx,
        &ctx.mjpeg_relay_pid_file,
        &legacy.mjpeg,
        Role::Mjpeg,
        mjpeg_port,
    ) {
        ok(&format!("video relay PID owns only 127.0.0.1:{mjpeg_port}"));
    } else {
        warn("video relay ownership/bind could not be verified");
        failed = true;
    }
    if sys::http_ok(
        &format!("http://127.0.0.1:{wda_port}/status"),
        Duration::from_secs(4),
    ) {
        ok("device runner /status reachable through the loopback relay");
    } else {
        warn("device runner /status is not reachable");
        failed = true;
    }
    i32::from(failed)
}
