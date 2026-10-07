//! Setup's terminal output: the same four line shapes `setup-wda.sh` printed,
//! so `wda-agent.log` keeps reading the same way (the daemon finds rounds by
//! the `== Checking prerequisites` text, and operators grep for `✓`/`⚠`/`✗`).

use std::io::Write as _;
use std::sync::OnceLock;
use std::time::Instant;

pub const BOLD: &str = "\x1b[1m";
pub const RED: &str = "\x1b[0;31m";
pub const GRN: &str = "\x1b[0;32m";
pub const YLW: &str = "\x1b[1;33m";
pub const RST: &str = "\x1b[0m";

static STARTED: OnceLock<Instant> = OnceLock::new();

/// Start the `(+Ns)` clock. The first output call starts it otherwise.
pub fn start_clock() {
    STARTED.get_or_init(Instant::now);
}

/// Whole seconds since this run started, like bash's `$SECONDS`.
pub fn seconds() -> u64 {
    STARTED.get_or_init(Instant::now).elapsed().as_secs()
}

/// A stage header. The elapsed seconds are a suffix, not a prefix: the daemon
/// matches the header text from its start.
pub fn info(message: &str) {
    println!("{BOLD}== {message} (+{}s){RST}", seconds());
    flush();
}

pub fn ok(message: &str) {
    println!("{GRN}✓{RST} {message}");
    flush();
}

pub fn warn(message: &str) {
    println!("{YLW}⚠{RST}  {message}");
    flush();
}

/// The failure line, on stderr. Callers return [`Exit`] after it.
pub fn die_line(message: &str) {
    flush();
    eprintln!("{RED}✗ {message}{RST}");
    let _ = std::io::stderr().flush();
}

/// A checklist row: `✓ item` or `✗ item — fix`.
pub fn checklist_line(passed: bool, item: &str, fix: &str) {
    if passed {
        println!("  {GRN}✓{RST} {item}");
    } else {
        println!("  {RED}✗{RST} {item} — {fix}");
    }
    flush();
}

pub fn flush() {
    let _ = std::io::stdout().flush();
}

/// How a setup command ends. `die` prints the red line and yields exit 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit(pub i32);

pub type Step<T = ()> = Result<T, Exit>;

pub fn die<T>(message: impl AsRef<str>) -> Step<T> {
    die_line(message.as_ref());
    Err(Exit(1))
}
