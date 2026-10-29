//! The device-layer setup, in Rust: what `scripts/setup-runner.sh` did, moving
//! over phase by phase. `setup-runner.sh` stays the entry point launchd, the
//! daemon and the installer know; for every command this binary implements,
//! the script execs `iphone-use setup-native <command>` with the same
//! environment, so file names, launchd labels, the status file and every
//! `IPU_RUNNER_*` variable keep working unchanged.
//!
//! Implemented here so far: `doctor`, `status`, `stop`, `pause`, `resume`,
//! and `setup` as the launchd supervisor runs it (`IPU_RUNNER_KEEPALIVE=1`), plus
//! the setup status protocol (`status-watch` is a run's heartbeat watcher),
//! and `prebuild` (the runner product only, after an upgrade; no phone).

/// Developer-certificate diagnostics for setup; computer pairing is a separate step.
pub const DEVELOPER_TRUST_ZH: &str = "请在 iPhone 上打开 设置 › 通用 › VPN 与设备管理，选择对应的 Apple Development 开发者证书并信任；这不同于「信任此电脑」。若没有证书或提示，运行 iphone-use doctor 并查看 runner 日志，不要反复重连。之后仍可能出现密码或 UI 自动化授权，请只在手机上处理，不要把密码发给代理；输入期间暂停截图和屏幕读取，完成后检查 phone_status。";
pub const DEVELOPER_TRUST_EN: &str = "On the iPhone: Settings > General > VPN & Device Management > matching Apple Development certificate > Trust (separate from Trust This Computer). If no certificate or prompt appears, run iphone-use doctor and inspect runner logs; do not loop reconnect. A passcode/UI automation prompt may appear later: handle it only on the phone, never disclose the passcode, pause screenshots and screen reads during entry, then check phone_status.";

pub mod app;
pub mod asc;
pub mod checks;
pub mod commands;
pub mod coordination;
pub mod ctx;
pub mod doctor;
pub mod embed;
pub mod flow;
pub mod icon;
pub mod launchd;
pub mod legacy_ios;
pub mod owner;
pub mod pid;
pub mod proc;
pub mod retry;
pub mod runner;
pub mod status;
pub mod sys;
pub mod term;
pub mod usbdiag;

/// Commands `setup-native` accepts; the shim asks before handing one over
/// (`iphone-use setup-native --supports <command>`), so a script newer than
/// its binary keeps running the command itself. `setup` is supported for the
/// launchd supervisor's run only; an interactive setup stays in the script.
pub const COMMANDS: &[&str] = &[
    "doctor",
    "status",
    "stop",
    "pause",
    "resume",
    "setup",
    "owner-check",
    "instance-context",
    "prebuild",
];

/// `iphone-use setup-native <command> [args…]`. Returns the exit code.
pub fn main(args: &[String]) -> i32 {
    let Some((command, rest)) = args.split_first() else {
        eprintln!("usage: iphone-use setup-native <{}>", COMMANDS.join("|"));
        return 2;
    };
    match command.as_str() {
        "--supports" => i32::from(!rest.first().is_some_and(|c| COMMANDS.contains(&c.as_str()))),
        "status-watch" => status::watch(rest),
        name if COMMANDS.contains(&name) => {
            sys::extend_path();
            term::start_clock();
            let ctx = match ctx::Ctx::resolve() {
                Ok(ctx) => ctx,
                Err((message, code)) => {
                    eprintln!("{message}");
                    return code;
                }
            };
            // This phone's own Xcode reaches every child: xcodebuild, xcrun,
            // devicectl, actool, codesign. Set before any thread exists.
            match &ctx.developer_dir {
                Some(dir) => std::env::set_var("DEVELOPER_DIR", dir),
                None => std::env::remove_var("DEVELOPER_DIR"),
            }
            // An interactive setup remembers this phone's Xcode before anything
            // can fail: a first setup that rolls back must not lose the choice,
            // or every later supervisor attempt runs the Mac's Xcode again.
            if name == "setup" && !ctx.keepalive {
                if let Some(choice) = &ctx.xcode_to_persist {
                    if let Err(error) = ctx::write_xcode_choice(ctx.state_dir(), choice.as_deref())
                    {
                        eprintln!("could not remember this phone's Xcode: {error}");
                        return 1;
                    }
                }
            }
            match name {
                "doctor" => doctor::run(&ctx),
                "status" => commands::status(&ctx),
                "stop" => commands::stop(&ctx),
                "pause" => commands::pause(&ctx),
                "resume" => commands::resume(&ctx),
                "owner-check" => owner::check(&ctx),
                "instance-context" => commands::instance_context(&ctx),
                "prebuild" => flow::Setup::new(ctx).run_prebuild(),
                _ if ctx.keepalive => flow::Setup::new(ctx).run_keepalive(),
                _ => flow::Setup::new(ctx).run_interactive(),
            }
        }
        other => {
            eprintln!(
                "unknown setup-native command: {other} (implemented: {})",
                COMMANDS.join("|")
            );
            2
        }
    }
}
