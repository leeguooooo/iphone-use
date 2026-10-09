//! PID records: the only authority setup has to signal a process.
//!
//! A record is one line, `pid|lstart|expected`, where `expected` is
//! `<role>:<exact argv>` (role runner, relay or mjpeg). A process is "ours"
//! only while its pid, its start time and its full command line all still
//! match, and the command line has the exact shape setup itself launches.
//! A PID-only file from an old release is matched against a legacy contract
//! (`legacy-runner:<udid>:<team>:<bundle>`, `legacy-relay:<port>:<dport>:<udid>`).
//! Same format and rules as `setup-wda.sh`, so either can read the other's
//! records, and uninstall.sh keeps working.

use std::path::Path;
use std::time::Duration;

use super::checks::{valid_bundle_id, valid_team_id};
use super::ctx::{valid_port, Ctx};
use super::sys;
use super::term::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Runner,
    Relay,
    Mjpeg,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Runner => "runner",
            Role::Relay => "relay",
            Role::Mjpeg => "mjpeg",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub pid: u32,
    /// Empty for a legacy PID-only record.
    pub lstart: String,
    pub expected: String,
    pub legacy: bool,
}

/// No `|`, CR or LF: the record format's separators.
pub fn safe_expected(text: &str) -> bool {
    !text.is_empty() && !text.contains(['|', '\n', '\r'])
}

fn pid_number(text: &str) -> Option<u32> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>().ok().filter(|pid| *pid > 1)
}

/// The legacy contracts this run would accept for PID-only records.
#[derive(Debug, Clone)]
pub struct Legacy {
    pub runner: String,
    pub relay: String,
    pub mjpeg: String,
}

impl Legacy {
    pub fn new(ctx: &Ctx, team: &str, bundle: &str) -> Legacy {
        let or = |value: &str, missing: &str| {
            if value.is_empty() {
                missing.to_string()
            } else {
                value.to_string()
            }
        };
        let udid = or(&ctx.udid, "__missing_udid__");
        Legacy {
            runner: format!(
                "legacy-runner:{udid}:{}:{}",
                or(team, "__missing_team__"),
                or(bundle, "__missing_bundle__")
            ),
            relay: format!("legacy-relay:{}:8100:{udid}", ctx.wda_port),
            mjpeg: format!("legacy-mjpeg:{}:9100:{udid}", ctx.mjpeg_port),
        }
    }

    pub fn for_role(&self, role: Role) -> &str {
        match role {
            Role::Runner => &self.runner,
            Role::Relay => &self.relay,
            Role::Mjpeg => &self.mjpeg,
        }
    }
}

pub fn parse(file: &Path, legacy_expected: &str) -> Option<Record> {
    let text = std::fs::read_to_string(file).ok()?;
    // Exactly one line.
    let line = text.strip_suffix('\n').unwrap_or(&text);
    if line.contains('\n') {
        return None;
    }
    let record = if let Some((pid, rest)) = line.split_once('|') {
        let (lstart, expected) = rest.split_once('|')?;
        if lstart.is_empty() || !safe_expected(expected) {
            return None;
        }
        Record {
            pid: pid_number(pid)?,
            lstart: lstart.to_string(),
            expected: expected.to_string(),
            legacy: false,
        }
    } else {
        Record {
            pid: pid_number(line)?,
            lstart: String::new(),
            expected: legacy_expected.to_string(),
            legacy: true,
        }
    };
    safe_expected(&record.expected).then_some(record)
}

pub fn role_valid(expected: &str, role: Role) -> bool {
    let name = role.name();
    [format!("{name}:"), format!("legacy-{name}:")]
        .iter()
        .any(|prefix| {
            expected
                .strip_prefix(prefix.as_str())
                .is_some_and(|rest| !rest.is_empty())
        })
}

fn hex_udid(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// `-destination platform=iOS,id=<hex>` value check.
fn destination(value: &str) -> bool {
    value.strip_prefix("platform=iOS,id=").is_some_and(hex_udid)
}

/// `[A-Za-z0-9]+`, `[A-Za-z0-9-]+`.
fn alnum(text: &str, dash: bool) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || (dash && b == b'-'))
}

/// Split a trailing ASC signing suffix off `command`, validating it.
/// `Ok((body, true))` when it was present and complete.
fn split_asc_suffix(command: &str) -> Result<(&str, bool), ()> {
    const TAIL: &str = " -allowProvisioningDeviceRegistration";
    let Some(start) = command.find(" -authenticationKeyPath ") else {
        return Ok((command, false));
    };
    let body = &command[..start];
    let suffix = &command[start + " -authenticationKeyPath ".len()..];
    let rest = suffix.strip_suffix(TAIL).ok_or(())?;
    let (rest, issuer) = rest.rsplit_once(" -authenticationKeyIssuerID ").ok_or(())?;
    let (path, key_id) = rest.rsplit_once(" -authenticationKeyID ").ok_or(())?;
    let path_ok = path.starts_with('/')
        && path.ends_with(".p8")
        && path.len() > 4
        && !path.chars().any(|c| c == '|' || c.is_control());
    if path_ok && alnum(key_id, false) && alnum(issuer, true) {
        Ok((body, true))
    } else {
        Err(())
    }
}

/// `(/…/)?xcodebuild ` prefix; the rest of the command.
fn strip_xcodebuild(command: &str) -> Option<&str> {
    let (program, rest) = command.split_once(' ')?;
    let ok =
        program == "xcodebuild" || (program.starts_with('/') && program.ends_with("/xcodebuild"));
    ok.then_some(rest)
}

/// The argv shapes setup has launched a runner with: the device runner
/// (test-without-building its .xctestrun), and the two WebDriverAgent forms
/// releases before it used, so the first run after an upgrade can still stop
/// a WDA runner the previous release left. The optional ASC suffix must be
/// complete and in the order setup emits it.
pub fn runner_signature_valid(signature: &str) -> bool {
    if !safe_expected(signature) {
        return false;
    }
    if go_ios_runtest_valid(signature) {
        return true;
    }
    let Some(rest) = strip_xcodebuild(signature) else {
        return false;
    };
    let Ok((body, asc)) = split_asc_suffix(rest) else {
        return false;
    };
    let tokens: Vec<&str> = body.split(' ').collect();
    if tokens.iter().any(|t| t.is_empty()) {
        return false;
    }
    let xctestrun = |path: &str, product: &str| {
        path.starts_with('/')
            && path.ends_with(".xctestrun")
            && path.rsplit_once('/').is_some_and(|(dir, file)| {
                dir.len() > 1
                    && file.starts_with(product)
                    && file.len() > product.len() + ".xctestrun".len()
            })
    };
    // Device runner.
    if let ["-destination", dest, "test-without-building", "-xctestrun", run, "-only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe", tail @ ..] =
        tokens.as_slice()
    {
        let tail_ok = match tail {
            [] => !asc,
            ["-allowProvisioningUpdates"] => asc,
            _ => false,
        };
        return destination(dest) && xctestrun(run, "IPhoneUseRunner_") && tail_ok;
    }
    // WebDriverAgent `test` form.
    if let ["-project", "WebDriverAgent.xcodeproj", "-scheme", "WebDriverAgentRunner", "-destination", dest, "-allowProvisioningUpdates", team, bundle, "test"] =
        tokens.as_slice()
    {
        return destination(dest)
            && team.strip_prefix("DEVELOPMENT_TEAM=").is_some_and(|t| {
                t.len() == 10
                    && t.bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            })
            && bundle
                .strip_prefix("PRODUCT_BUNDLE_IDENTIFIER=")
                .is_some_and(|b| {
                    !b.is_empty()
                        && b.bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
                });
    }
    // WebDriverAgent xctestrun form.
    if let ["-destination", dest, "test-without-building", "-xctestrun", run, tail @ ..] =
        tokens.as_slice()
    {
        let tail_ok = match tail {
            [] => !asc,
            ["-allowProvisioningUpdates"] => asc,
            _ => false,
        };
        return destination(dest) && xctestrun(run, "WebDriverAgentRunner_") && tail_ok;
    }
    false
}

/// The legacy (iOS 15/16) runner: go-ios `runtest` of the runner's test
/// (`super::legacy_ios::runtest_argv`), from an absolute go-ios path.
fn go_ios_runtest_valid(signature: &str) -> bool {
    let tokens: Vec<&str> = signature.split(' ').collect();
    let bundle_ok = |value: &str| {
        value.ends_with(".xctrunner")
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
    };
    match tokens.as_slice() {
        [program, "runtest", udid, bundle, runner, "--xctest-config=iPhoneUse.xctest", "--test-to-run=RunnerTests/testServe"] =>
        {
            let bundle = bundle.strip_prefix("--bundle-id=");
            program.starts_with('/')
                && program.ends_with("/ios")
                && udid.strip_prefix("--udid=").is_some_and(hex_udid)
                && bundle.is_some_and(bundle_ok)
                && runner.strip_prefix("--test-runner-bundle-id=") == bundle
        }
        _ => false,
    }
}

fn relay_signature_valid(signature: &str) -> bool {
    let tokens: Vec<&str> = signature.split(' ').collect();
    match tokens.as_slice() {
        [program, "relay", "--udid", udid, "--listen", listen, "--device-port", port] => {
            program.starts_with('/')
                && program.ends_with("/iphone-use")
                && program.len() > "/iphone-use".len() + 1
                && hex_udid(udid)
                && listen.strip_prefix("127.0.0.1:").is_some_and(digits)
                && digits(port)
        }
        [program, "relay", "--udid", udid, "--listen", listen, "--device-port", port, "--lan-host", host] => {
            program.starts_with('/')
                && program.ends_with("/iphone-use")
                && program.len() > "/iphone-use".len() + 1
                && hex_udid(udid)
                && listen.strip_prefix("127.0.0.1:").is_some_and(digits)
                && digits(port)
                && host.parse::<std::net::Ipv4Addr>().is_ok()
        }
        [program, "-s", "127.0.0.1", ports, "-u", udid] => {
            (*program == "iproxy" || (program.starts_with('/') && program.ends_with("/iproxy")))
                && ports
                    .split_once(':')
                    .is_some_and(|(a, b)| digits(a) && digits(b))
                && hex_udid(udid)
        }
        [program, listen, target] => {
            let listen_ok = listen
                .strip_prefix("TCP-LISTEN:")
                .and_then(|rest| rest.strip_suffix(",fork,reuseaddr,bind=127.0.0.1"))
                .is_some_and(digits);
            let target_ok = target
                .strip_prefix("TCP:")
                .and_then(|rest| rest.rsplit_once(':'))
                .is_some_and(|(host, port)| {
                    !host.is_empty()
                        && host
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._:%_-".contains(&b))
                        && digits(port)
                });
            (*program == "socat" || (program.starts_with('/') && program.ends_with("/socat")))
                && listen_ok
                && target_ok
        }
        _ => false,
    }
}

/// Whether `command` (a live process's full argv) is the process `expected`
/// describes.
pub fn command_matches(command: &str, expected: &str) -> bool {
    if let Some(signature) = expected.strip_prefix("runner:") {
        return runner_signature_valid(signature) && command == signature;
    }
    if let Some(signature) = expected
        .strip_prefix("relay:")
        .or_else(|| expected.strip_prefix("mjpeg:"))
    {
        return relay_signature_valid(signature) && command == signature;
    }
    if let Some(rest) = expected.strip_prefix("legacy-runner:") {
        let mut parts = rest.splitn(3, ':');
        let (Some(udid), Some(team), Some(bundle)) = (parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        if udid == "__missing_udid__"
            || !hex_udid(udid)
            || !valid_team_id(team)
            || !valid_bundle_id(bundle)
        {
            return false;
        }
        if !runner_signature_valid(command) {
            return false;
        }
        let mut base = command
            .split(" -authenticationKeyPath ")
            .next()
            .unwrap_or(command);
        base = base
            .strip_suffix(" -allowProvisioningUpdates")
            .unwrap_or(base);
        let legacy_argv = format!(
            "xcodebuild -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id={udid} -allowProvisioningUpdates DEVELOPMENT_TEAM={team} PRODUCT_BUNDLE_IDENTIFIER={bundle}"
        );
        let test_form = format!("{legacy_argv} test");
        if base == test_form || base.ends_with(&format!("/{test_form}")) {
            return true;
        }
        let xctestrun_argv = format!(
            "xcodebuild -destination platform=iOS,id={udid} test-without-building -xctestrun "
        );
        let after = if let Some(rest) = base.strip_prefix(&xctestrun_argv) {
            Some(rest)
        } else {
            base.find(&format!("/{xctestrun_argv}"))
                .map(|i| &base[i + 1 + xctestrun_argv.len()..])
        };
        return after.is_some_and(|path| {
            path.starts_with('/')
                && !path.contains(' ')
                && path.rsplit('/').next().is_some_and(|file| {
                    file.starts_with("WebDriverAgentRunner_") && file.ends_with(".xctestrun")
                })
        });
    }
    if let Some(rest) = expected
        .strip_prefix("legacy-relay:")
        .or_else(|| expected.strip_prefix("legacy-mjpeg:"))
    {
        let mut parts = rest.splitn(3, ':');
        let (Some(local), Some(device), Some(udid)) = (parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        if valid_port(local).is_none()
            || valid_port(device).is_none()
            || udid == "__missing_udid__"
            || !hex_udid(udid)
        {
            return false;
        }
        // A legacy socat argv names no phone; it is never adopted or killed.
        let forms = [
            format!("iproxy {local} {device} -u {udid}"),
            format!("iproxy -s 127.0.0.1 {local}:{device} -u {udid}"),
        ];
        return forms
            .iter()
            .any(|form| command == form || command.ends_with(&format!("/{form}")));
    }
    false
}

fn legacy_runner_cwd_ok(ctx: &Ctx, pid: u32) -> bool {
    if sys::which("lsof").is_none() || !ctx.wda_dir.is_dir() {
        return false;
    }
    let Ok(expected) = ctx.wda_dir.canonicalize() else {
        return false;
    };
    let pid = pid.to_string();
    let out = sys::stdout_of("lsof", &["-nP", "-a", "-p", &pid, "-d", "cwd", "-Fn"]);
    out.lines()
        .find_map(|line| line.strip_prefix('n'))
        .is_some_and(|cwd| Path::new(cwd) == expected)
}

fn store(file: &Path, pid: u32, lstart: &str, expected: &str) -> bool {
    if !sys::pid_exists(pid) || lstart.is_empty() || !safe_expected(expected) {
        return false;
    }
    sys::write_atomic(
        file,
        format!("{pid}|{lstart}|{expected}\n").as_bytes(),
        0o600,
    )
    .is_ok()
}

/// The pid `file` names, when it is still the process recorded there.
/// `adopt_legacy` (mutating paths only) rewrites a verified legacy PID-only
/// record into the full format.
pub fn validate(
    ctx: &Ctx,
    file: &Path,
    legacy_expected: &str,
    role: Role,
    adopt_legacy: bool,
) -> Option<u32> {
    let record = parse(file, legacy_expected)?;
    if !role_valid(&record.expected, role) || !sys::pid_exists(record.pid) {
        return None;
    }
    if sys::ps_uid(record.pid) != Some(ctx.uid) {
        return None;
    }
    let lstart = sys::ps_lstart(record.pid);
    if lstart.is_empty() || (!record.lstart.is_empty() && lstart != record.lstart) {
        return None;
    }
    let command = sys::ps_command(record.pid);
    if !command_matches(&command, &record.expected) {
        return None;
    }
    if record.legacy {
        if record.expected.starts_with("legacy-runner:") && !legacy_runner_cwd_ok(ctx, record.pid) {
            return None;
        }
        if adopt_legacy {
            if !store(file, record.pid, &lstart, &record.expected) {
                return None;
            }
            return validate(ctx, file, legacy_expected, role, false);
        }
    }
    (sys::ps_lstart(record.pid) == lstart && sys::ps_command(record.pid) == command)
        .then_some(record.pid)
}

/// Record a process this run just started, once `ps` shows it as exactly
/// `expected` (an exec can take a moment to replace the argv).
pub fn write(ctx: &Ctx, file: &Path, pid: u32, expected: &str, role: Role) -> Option<u32> {
    if !role_valid(expected, role) || !safe_expected(expected) {
        return None;
    }
    let mut lstart = String::new();
    let mut matched = false;
    for _ in 0..30 {
        if !sys::pid_exists(pid) {
            return None;
        }
        lstart = sys::ps_lstart(pid);
        let uid = sys::ps_uid(pid);
        let command = sys::ps_command(pid);
        if uid == Some(ctx.uid) && !lstart.is_empty() && command_matches(&command, expected) {
            matched = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !matched || !store(file, pid, &lstart, expected) {
        return None;
    }
    validate(ctx, file, expected, role, false)
}

fn listener_pids(port: u16) -> Option<Vec<u32>> {
    let port = format!("-iTCP:{port}");
    let out = sys::run("lsof", &["-nP", "-a", &port, "-sTCP:LISTEN", "-Fp"])?;
    if !out.status.success() {
        return None;
    }
    let mut pids: Vec<u32> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('p'))
        .filter_map(|pid| pid.parse().ok())
        .collect();
    pids.sort_unstable();
    pids.dedup();
    Some(pids)
}

pub fn legacy_migration_hint(ctx: &Ctx) {
    warn(&format!(
        "legacy pid-only state was found but could not be proven safe to stop automatically.
   Retry with the exact old values (do not guess):
     WDA_UDID=<old-device-udid> WDA_TEAM_ID=<10-char-team> \\
     WDA_BUNDLE_ID=<old-runner-bundle-id> WDA_DIR=<old-wda-checkout> \\
       {} stop
   If the old relay used socat, inspect its numeric PID with both:
     ps -ww -p <pid> -o uid=,lstart=,command=
     lsof -nP -a -p <pid> -iTCP:<8100-or-9100> -sTCP:LISTEN
   This script intentionally will not turn an unproven legacy PID into a global kill.",
        ctx.self_install.display()
    ));
}

/// Stop the process a record names, if and only if it is still that process.
/// `true` when nothing of ours is left running (including "no record").
pub fn stop(ctx: &Ctx, file: &Path, legacy_expected: &str, role: Role) -> bool {
    if !file.exists() {
        return true;
    }
    let Some(record) = parse(file, legacy_expected) else {
        warn(&format!(
            "invalid managed PID record; refusing to act: {}",
            file.display()
        ));
        return false;
    };
    let Some(pid) = validate(ctx, file, legacy_expected, role, true) else {
        if sys::pid_exists(record.pid) {
            if record.legacy {
                legacy_migration_hint(ctx);
            }
            warn(&format!(
                "PID {} does not match the current-user {} identity; refusing to kill it",
                record.pid,
                role.name()
            ));
            return false;
        }
        let _ = std::fs::remove_file(file);
        return true;
    };
    if record.legacy
        && (legacy_expected.starts_with("legacy-relay:")
            || legacy_expected.starts_with("legacy-mjpeg:"))
    {
        let port = legacy_expected
            .split(':')
            .nth(1)
            .and_then(valid_port)
            .unwrap_or(0);
        if listener_pids(port).is_none_or(|pids| pids.is_empty() || pids.iter().any(|p| *p != pid))
        {
            warn(&format!(
                "legacy {} PID does not exclusively own TCP {port}; refusing to kill it",
                role.name()
            ));
            legacy_migration_hint(ctx);
            return false;
        }
        if validate(ctx, file, legacy_expected, role, false).is_none() {
            return false;
        }
    }
    // SAFETY: kill with a validated pid this user owns.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    for _ in 0..20 {
        if !sys::pid_exists(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if sys::pid_exists(pid) {
        warn(&format!(
            "managed {} pid {pid} did not stop after SIGTERM",
            role.name()
        ));
        return false;
    }
    let _ = std::fs::remove_file(file);
    true
}

/// TCP `port` has no listener at all (lsof says so without diagnostics).
pub fn assert_port_free(ctx: &Ctx, port: u16) -> bool {
    if sys::which("lsof").is_none() {
        warn(&format!("lsof is required to prove TCP {port} is free"));
        return false;
    }
    let _ = ctx;
    let selector = format!("-iTCP:{port}");
    let Some(out) = sys::run("lsof", &["-nP", "-a", &selector, "-sTCP:LISTEN"]) else {
        warn(&format!("lsof could not prove TCP {port} is free:"));
        return false;
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let code = out.status.code().unwrap_or(-1);
    if code == 1 && stderr.trim().is_empty() {
        return true;
    }
    let indent = |text: &str| {
        text.lines()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if code != 0 {
        warn(&format!("lsof could not prove TCP {port} is free:"));
        eprintln!("{}", indent(&stderr));
        return false;
    }
    if !stderr.trim().is_empty() {
        warn(&format!(
            "lsof returned diagnostics, so TCP {port} is not proven free:"
        ));
        eprintln!("{}", indent(&stderr));
        return false;
    }
    if stdout.trim().is_empty() {
        return true;
    }
    warn(&format!(
        "TCP {port} is already owned by a non-managed listener:"
    ));
    eprintln!("{}", indent(stdout.trim_end()));
    false
}

/// The recorded relay is alive and is the only listener on `port`, bound to
/// 127.0.0.1 only.
pub fn verify_loopback_listener(
    ctx: &Ctx,
    file: &Path,
    legacy_expected: &str,
    role: Role,
    port: u16,
) -> bool {
    if sys::which("lsof").is_none() {
        return false;
    }
    let Some(pid) = validate(ctx, file, legacy_expected, role, false) else {
        return false;
    };
    match listener_pids(port) {
        Some(pids) if !pids.is_empty() && pids.iter().all(|p| *p == pid) => {}
        _ => return false,
    }
    let pid_text = pid.to_string();
    let selector = format!("-iTCP:{port}");
    let Some(out) = sys::run(
        "lsof",
        &[
            "-nP",
            "-a",
            "-p",
            &pid_text,
            &selector,
            "-sTCP:LISTEN",
            "-Fn",
        ],
    ) else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .map(str::to_string)
        .collect();
    let want = format!("127.0.0.1:{port}");
    if names.is_empty() || names.iter().any(|name| *name != want) {
        return false;
    }
    validate(ctx, file, legacy_expected, role, false) == Some(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNER: &str = "/Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild -destination platform=iOS,id=00008110-0002346211A0401E test-without-building -xctestrun /Users/leo/.iphone-use/instances/i13/runner-build/Build/Products/IPhoneUseRunner_iphoneos27.0-arm64.xctestrun -only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe";

    #[test]
    fn runner_signatures() {
        assert!(runner_signature_valid(RUNNER));
        let asc = format!(
            "{RUNNER} -allowProvisioningUpdates -authenticationKeyPath /Users/leo/keys/My Key.p8 -authenticationKeyID ABC123 -authenticationKeyIssuerID 1a2b-3c -allowProvisioningDeviceRegistration"
        );
        assert!(
            runner_signature_valid(&asc),
            "a key path may contain spaces"
        );
        assert!(
            !runner_signature_valid(&format!("{RUNNER} -allowProvisioningUpdates")),
            "updates flag only with the ASC suffix"
        );
        assert!(!runner_signature_valid(&format!("{RUNNER} -authenticationKeyPath /k.p8 -authenticationKeyID A -authenticationKeyIssuerID B -allowProvisioningDeviceRegistration")));
        assert!(!runner_signature_valid(
            &RUNNER.replace("testServe", "testOther")
        ));
        assert!(!runner_signature_valid(
            &RUNNER.replace("id=00008110", "id=zz")
        ));
        assert!(!runner_signature_valid(&format!("{RUNNER} extra")));
        assert!(!runner_signature_valid(
            &RUNNER.replace("xcodebuild -dest", "xcodebuild  -dest")
        ));
        assert!(!runner_signature_valid(&RUNNER.replace(
            "/Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild",
            "/bin/sh"
        )));
        assert!(runner_signature_valid(&RUNNER.replace(
            "/Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild",
            "xcodebuild"
        )));
        let wda = "xcodebuild -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=00008110-0002346211A0401E -allowProvisioningUpdates DEVELOPMENT_TEAM=6ZPXG4KVVS PRODUCT_BUNDLE_IDENTIFIER=com.x.wda test";
        assert!(runner_signature_valid(wda));
        assert!(!runner_signature_valid(
            &wda.replace("6ZPXG4KVVS", "6zpxg4kvvs")
        ));
        let wda_run = "/usr/bin/xcodebuild -destination platform=iOS,id=0000 test-without-building -xctestrun /tmp/d/WebDriverAgentRunner_iphoneos17.0-arm64.xctestrun";
        assert!(runner_signature_valid(wda_run));
    }

    #[test]
    fn go_ios_runtest_is_a_runner_and_a_lan_relay_is_a_relay() {
        let runtest = "/Users/leo/.iphone-use/tools/go-ios-v1.3.2/ios runtest --udid=00008101-000409443404001E --bundle-id=com.leeguoo.iphone-use.wda.6zpxg4kvvs.xctrunner --test-runner-bundle-id=com.leeguoo.iphone-use.wda.6zpxg4kvvs.xctrunner --xctest-config=iPhoneUse.xctest --test-to-run=RunnerTests/testServe";
        assert!(runner_signature_valid(runtest));
        assert!(command_matches(runtest, &format!("runner:{runtest}")));
        assert!(!runner_signature_valid(
            &runtest.replace("ios runtest", "ios kill")
        ));
        assert!(!runner_signature_valid(&runtest.replace(
            "--test-runner-bundle-id=com.leeguoo",
            "--test-runner-bundle-id=org.other"
        )));
        assert!(!runner_signature_valid(&runtest.replace(
            "/Users/leo/.iphone-use/tools/go-ios-v1.3.2/ios",
            "ios"
        )));
        assert!(!runner_signature_valid(&format!("{runtest} --env=X=1")));
        let lan = "/Users/leo/.iphone-use/instances/i12/runtime/iPhoneUse.app/Contents/MacOS/iphone-use relay --udid 00008101-000409443404001E --listen 127.0.0.1:8410 --device-port 8100 --lan-host 192.168.0.59";
        assert!(command_matches(lan, &format!("relay:{lan}")));
        assert!(!command_matches(
            &lan.replace("192.168.0.59", "evil;host"),
            &format!("relay:{}", lan.replace("192.168.0.59", "evil;host"))
        ));
    }

    #[test]
    fn relay_and_legacy_matching() {
        let relay = "/Users/leo/.iphone-use/instances/i13/runtime/iPhoneUse.app/Contents/MacOS/iphone-use relay --udid 00008110-0002346211A0401E --listen 127.0.0.1:8538 --device-port 8100";
        assert!(command_matches(relay, &format!("relay:{relay}")));
        assert!(command_matches(relay, &format!("mjpeg:{relay}")));
        assert!(!command_matches(
            &relay.replace("8538", "8539"),
            &format!("relay:{relay}")
        ));
        assert!(!command_matches(
            &relay.replace("127.0.0.1", "0.0.0.0"),
            &format!("relay:{}", relay.replace("127.0.0.1", "0.0.0.0"))
        ));
        let iproxy = "/opt/homebrew/bin/iproxy -s 127.0.0.1 8538:8100 -u 00008110-0002346211A0401E";
        assert!(command_matches(iproxy, &format!("relay:{iproxy}")));
        let socat = "/opt/homebrew/bin/socat TCP-LISTEN:8100,fork,reuseaddr,bind=127.0.0.1 TCP:192.168.0.236:8100";
        assert!(command_matches(socat, &format!("relay:{socat}")));
        assert!(command_matches(
            "iproxy 8100 8100 -u 0000AB",
            "legacy-relay:8100:8100:0000AB"
        ));
        assert!(command_matches(
            "/usr/local/bin/iproxy -s 127.0.0.1 8100:8100 -u 0000AB",
            "legacy-relay:8100:8100:0000AB"
        ));
        assert!(
            !command_matches(socat, "legacy-relay:8100:8100:0000AB"),
            "a legacy socat names no phone"
        );
        assert!(!command_matches(
            "iproxy 8100 8100 -u 0000AB",
            "legacy-relay:8100:8100:__missing_udid__"
        ));
        let legacy = "xcodebuild -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=0000AB -allowProvisioningUpdates DEVELOPMENT_TEAM=6ZPXG4KVVS PRODUCT_BUNDLE_IDENTIFIER=com.x.wda test";
        assert!(command_matches(
            legacy,
            "legacy-runner:0000AB:6ZPXG4KVVS:com.x.wda"
        ));
        assert!(!command_matches(
            legacy,
            "legacy-runner:0000AC:6ZPXG4KVVS:com.x.wda"
        ));
        assert!(command_matches(RUNNER, &format!("runner:{RUNNER}")));
        assert!(!command_matches(RUNNER, "runner:/bin/sh -c x"));
    }

    #[test]
    fn records_parse() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.pid");
        std::fs::write(
            &file,
            format!("123|Wed Oct  7 01:02:03 2026|runner:{RUNNER}\n"),
        )
        .unwrap();
        let record = parse(&file, "legacy-runner:x").unwrap();
        assert_eq!(record.pid, 123);
        assert!(!record.legacy);
        std::fs::write(&file, "456\n").unwrap();
        let record = parse(&file, "legacy-relay:8100:8100:AB").unwrap();
        assert!(record.legacy);
        assert_eq!(record.expected, "legacy-relay:8100:8100:AB");
        for bad in [
            "0\n",
            "1\n",
            "012\n",
            "12|a\n",
            "12||runner:x\n",
            "12|a|b\n13\n",
            "abc\n",
        ] {
            std::fs::write(&file, bad).unwrap();
            let parsed = parse(&file, "legacy-relay:8100:8100:AB");
            assert!(
                parsed.is_none() || !role_valid(&parsed.unwrap().expected, Role::Runner),
                "{bad:?}"
            );
        }
        assert!(role_valid("runner:x", Role::Runner));
        assert!(role_valid("legacy-mjpeg:x", Role::Mjpeg));
        assert!(!role_valid("relay:x", Role::Mjpeg));
        assert!(!role_valid("runner:", Role::Runner));
    }
}
