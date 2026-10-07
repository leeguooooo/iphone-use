//! The device-layer setup, in Rust: what `scripts/setup-wda.sh` did, moving
//! over phase by phase. `setup-wda.sh` stays the entry point launchd, the
//! daemon and the installer know; for every command this binary implements,
//! the script execs `iphone-use setup-native <command>` with the same
//! environment, so file names, launchd labels, the status file and every
//! `WDA_*` variable keep working unchanged.
//!
//! Implemented here so far: `doctor` and the setup status protocol
//! (`status-watch` is the heartbeat watcher a setup run starts).

pub mod checks;
pub mod ctx;
pub mod doctor;
pub mod status;
pub mod sys;
pub mod term;

/// Commands `setup-native` accepts; the shim asks before handing one over
/// (`iphone-use setup-native --supports <command>`), so a script newer than
/// its binary keeps running the command itself.
pub const COMMANDS: &[&str] = &["doctor"];

/// `iphone-use setup-native <command> [args…]`. Returns the exit code.
pub fn main(args: &[String]) -> i32 {
    let Some((command, rest)) = args.split_first() else {
        eprintln!("usage: iphone-use setup-native <{}>", COMMANDS.join("|"));
        return 2;
    };
    match command.as_str() {
        "--supports" => i32::from(!rest.first().is_some_and(|c| COMMANDS.contains(&c.as_str()))),
        "status-watch" => status::watch(rest),
        "doctor" => {
            sys::extend_path();
            term::start_clock();
            match ctx::Ctx::resolve() {
                Ok(ctx) => doctor::run(&ctx),
                Err((message, code)) => {
                    eprintln!("{message}");
                    code
                }
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
