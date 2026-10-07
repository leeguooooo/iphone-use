//! Children setup starts, and how setup stops.
//!
//! SIGTERM (launchd booting the supervisor out) and SIGINT end a run with
//! exit code 130 after its cleanup, exactly as the script's `trap 'exit 130'`
//! did: every wait goes through [`sleep`], which notices the signal within
//! 100 ms. The runner and relays are detached (SIGHUP ignored, output to their
//! logs) and always reaped by a thread, so a dead child never lingers as a
//! zombie that `ps` would still report.

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::term::{Exit, Step};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

pub fn install_signal_handlers() {
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
}

pub fn stopping() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Return `Exit(130)` once a stop signal arrived.
pub fn check() -> Step {
    if stopping() {
        Err(Exit(130))
    } else {
        Ok(())
    }
}

/// Sleep, waking every 100 ms to honour a stop signal.
pub fn sleep(duration: Duration) -> Step {
    let deadline = Instant::now() + duration;
    loop {
        check()?;
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// How a child's XCODE_XCCONFIG_FILE is set.
#[derive(Debug, Clone, Default)]
pub struct XcconfigEnv(pub Option<std::path::PathBuf>);

impl XcconfigEnv {
    pub fn apply(&self, command: &mut Command) {
        match &self.0 {
            Some(path) => command.env("XCODE_XCCONFIG_FILE", path),
            None => command.env_remove("XCODE_XCCONFIG_FILE"),
        };
    }
}

fn open_log(log: &Path, append: bool) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(log)
}

fn prepare(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    log: &Path,
    append: bool,
) -> std::io::Result<Command> {
    let out = open_log(log, append)?;
    let err = out.try_clone()?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
    Ok(command)
}

/// Start a long-lived child (the runner, a relay) and return its pid. A
/// thread reaps it whenever it exits.
pub fn spawn_detached(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    log: &Path,
    xcconfig: Option<&XcconfigEnv>,
) -> std::io::Result<u32> {
    let mut command = prepare(program, args, cwd, log, false)?;
    if let Some(xcconfig) = xcconfig {
        xcconfig.apply(&mut command);
    }
    let mut child: Child = command.spawn()?;
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// Run a child to completion, output appended to `log`. A stop signal kills
/// it and returns `Exit(130)`.
pub fn run_logged(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    log: &Path,
    xcconfig: &XcconfigEnv,
) -> Result<bool, Exit> {
    let Ok(mut command) = prepare(program, args, cwd, log, true) else {
        return Ok(false);
    };
    xcconfig.apply(&mut command);
    let Ok(mut child) = command.spawn() else {
        return Ok(false);
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success()),
            Ok(None) => {}
            Err(_) => return Ok(false),
        }
        if stopping() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Exit(130));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
