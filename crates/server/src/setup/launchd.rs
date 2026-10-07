//! launchctl, the way setup drives it: by exact label in the user's GUI
//! domain, with every state change verified by reading it back.

use std::path::Path;
use std::time::Duration;

use super::ctx::Ctx;
use super::sys;

fn quiet(args: &[&str]) -> bool {
    std::process::Command::new("launchctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn service(ctx: &Ctx, label: &str) -> String {
    format!("{}/{label}", ctx.gui_domain)
}

pub fn loaded(ctx: &Ctx, label: &str) -> bool {
    quiet(&["print", &service(ctx, label)])
}

pub fn bootout(ctx: &Ctx, label: &str) {
    quiet(&["bootout", &service(ctx, label)]);
}

pub fn bootstrap(ctx: &Ctx, plist: &Path) -> bool {
    quiet(&["bootstrap", &ctx.gui_domain, &plist.to_string_lossy()])
}

pub fn enable(ctx: &Ctx, label: &str) -> bool {
    quiet(&["enable", &service(ctx, label)])
}

pub fn disable(ctx: &Ctx, label: &str) -> bool {
    quiet(&["disable", &service(ctx, label)])
}

/// Wait up to ~5 s for a booted-out job to disappear.
pub fn wait_gone(ctx: &Ctx, label: &str) -> bool {
    for _ in 0..10 {
        if !loaded(ctx, label) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    !loaded(ctx, label)
}

/// Whether `print-disabled` lists `label` as disabled. Newer macOS prints
/// `=> disabled`, older releases `=> true`.
pub fn disabled_in(print_disabled: &str, label: &str) -> bool {
    let quoted = format!("\"{label}\"");
    print_disabled.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.len() >= 3
            && fields[0] == quoted
            && fields[1] == "=>"
            && matches!(fields[2], "true" | "disabled")
    })
}

/// `Some(true)` disabled, `Some(false)` enabled, `None` unreadable.
pub fn disabled_state(ctx: &Ctx, label: &str) -> Option<bool> {
    let out = sys::run("launchctl", &["print-disabled", &ctx.gui_domain])?;
    out.status
        .success()
        .then(|| disabled_in(&String::from_utf8_lossy(&out.stdout), label))
}

/// Put a label's persistent enable/disable policy back, and verify it.
pub fn restore_policy(ctx: &Ctx, label: &str, disabled: bool) -> bool {
    let changed = if disabled {
        disable(ctx, label)
    } else {
        enable(ctx, label)
    };
    changed && disabled_state(ctx, label) == Some(disabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_disabled_parsing() {
        let text = "disabled services = {\n\t\"com.leeguoo.iphone-use.wda\" => disabled\n\t\"com.leeguoo.iphone-use.wda.i13\" => enabled\n\t\"old\" => true\n}\n";
        assert!(disabled_in(text, "com.leeguoo.iphone-use.wda"));
        assert!(!disabled_in(text, "com.leeguoo.iphone-use.wda.i13"));
        assert!(disabled_in(text, "old"));
        assert!(!disabled_in(text, "missing"));
    }
}
