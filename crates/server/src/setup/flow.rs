//! `setup`: bring the device runner up on the phone, relay it to loopback,
//! point the daemon at it, and — under launchd (`WDA_KEEPALIVE=1`) — hold
//! while it stays healthy, exiting so launchd rebuilds it when it does not.
//!
//! Stage for stage, message for message, what `setup-wda.sh` did: the daemon
//! and the web client read the same status phases and blockers, and
//! `wda-agent.log` reads the same.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::checks::{self, Signing};
use super::ctx::{valid_port, Ctx, RUNNER_APP_NAME, XCODE_APP_STORE_URL};
use super::icon;
use super::launchd;
use super::owner;
use super::pid::{self, Legacy, Role};
use super::proc::{self, XcconfigEnv};
use super::retry::{self, Kind};
use super::runner;
use super::status;
use super::sys;
use super::term::{die, info, ok, warn, Exit, Step, BOLD, RST};

const RUNNER_DEVICE_PORT: u16 = 8100;
const MJPEG_DEVICE_PORT: u16 = 9100;
/// Published as `not_connected`; the daemon's hint says the same.
const NOT_CONNECTED_MESSAGE: &str = "the iPhone isn't connected to this Mac — plug it in over USB (or join the same Wi-Fi) and unlock it";
const AUTOMATION_MODE_HINT: &str = "enable UI automation on the iPhone: Settings › Developer › Enable UI Automation, then accept any passcode or Allow automation prompt while the phone is unlocked";

/// The state of one setup run that cleanup needs.
pub struct Setup {
    pub ctx: Ctx,
    run: Option<status::Run>,
    signing: Option<Signing>,
    legacy: Legacy,
    xcconfig: XcconfigEnv,
    failure_kind: Kind,
    keepalive_attempt_active: bool,
    lock_retry: bool,
    build_blocker: String,
    started_runner: bool,
    started_control: bool,
    started_mjpeg: bool,
    daemon: DaemonTx,
    /// Set once the runner/relays are verified and supervision is in place.
    pub handoff_complete: bool,
    repair_attempted: bool,
    build_locked: bool,
    validation_error: String,
    warned_link_drop: bool,
    sup: SupervisorTx,
    self_install: SelfInstallTx,
    /// Where the runner serves (for restarting only the relays).
    phone_url: String,
    /// The runner's Home Screen icon source, when one is injected.
    icon_source: Option<PathBuf>,
    /// Readiness came straight over USB (no LAN address known yet).
    from_probe: bool,
    interactive_lock: Option<InteractiveLock>,
}

/// The interactive lock wait's notice schedule.
struct InteractiveLock {
    started: u64,
    notice_at: u64,
    attempt: u32,
}

#[derive(Default)]
struct DaemonTx {
    active: bool,
    touched: bool,
    was_loaded: bool,
    was_disabled: bool,
    rollback: PathBuf,
    staged: Option<PathBuf>,
}

impl Setup {
    pub fn new(ctx: Ctx) -> Setup {
        let legacy = Legacy::new(&ctx, &ctx.team_id, &ctx.bundle_id);
        let rollback = ctx
            .state_dir()
            .join(format!("daemon.rollback.{}.plist", std::process::id()));
        Setup {
            ctx,
            run: None,
            signing: None,
            legacy,
            xcconfig: XcconfigEnv::default(),
            failure_kind: Kind::Generic,
            keepalive_attempt_active: false,
            lock_retry: false,
            build_blocker: String::new(),
            started_runner: false,
            started_control: false,
            started_mjpeg: false,
            daemon: DaemonTx {
                rollback,
                ..DaemonTx::default()
            },
            handoff_complete: false,
            repair_attempted: false,
            build_locked: false,
            validation_error: String::new(),
            warned_link_drop: false,
            sup: SupervisorTx::default(),
            self_install: SelfInstallTx::default(),
            phone_url: String::new(),
            icon_source: None,
            from_probe: false,
            interactive_lock: None,
        }
    }

    /// `_setstatus`.
    pub fn phase(&self, phase: &str, blocked: &str, message: &str) {
        if let Some(run) = &self.run {
            run.phase(phase, blocked, message);
        }
    }

    fn rel(&self, name: &str) -> PathBuf {
        self.ctx.state_dir().join(name)
    }

    // ── entry ───────────────────────────────────────────────────────────────

    /// The whole run, cleanup included. Returns the exit code.
    pub fn run_keepalive(mut self) -> i32 {
        proc::install_signal_handlers();
        if let Err(Exit(code)) = self.wait_for_retry() {
            return self.cleanup(code);
        }
        self.keepalive_attempt_active = true;
        if let Err(code) = self.begin_status() {
            return self.cleanup(code);
        }
        let code = match self.body().and_then(|()| self.hold()) {
            Ok(()) => 0,
            Err(Exit(code)) => code,
        };
        self.cleanup(code)
    }

    pub fn begin_status(&mut self) -> Result<(), i32> {
        let _ = std::fs::create_dir_all(self.ctx.state_dir());
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(
                self.ctx.state_dir(),
                std::fs::Permissions::from_mode(0o700),
            );
        }
        match status::Run::begin(&self.ctx.status_file) {
            Ok(run) => {
                self.run = Some(run);
                Ok(())
            }
            Err(_) => {
                super::term::die_line("could not initialize the setup status owner");
                Err(1)
            }
        }
    }

    fn wait_for_retry(&mut self) -> Step {
        let state = match retry::read(&self.ctx.retry_state) {
            Ok(state) => state,
            Err(()) => {
                warn(&format!(
                    "ignoring invalid KeepAlive retry state: {}",
                    self.ctx.retry_state.display()
                ));
                return Ok(());
            }
        };
        let Some(state) = state else {
            return Ok(());
        };
        if state.kind == Kind::Locked {
            self.lock_retry = true;
        }
        let now = retry::now();
        if state.next_at > now {
            let wait = state.next_at - now;
            if state.kind != Kind::Locked {
                info(&format!(
                    "KeepAlive retry backoff: waiting {wait}s before the next rebuild"
                ));
            }
            if state.kind == Kind::WifiAutomation {
                // Only USB clears a Wi-Fi refusal: stop waiting the moment
                // the phone is plugged in, checking usbmuxd every few seconds.
                while retry::now() < state.next_at {
                    if checks::transport(&self.ctx.udid) == checks::Transport::Usb {
                        info("the iPhone is now on USB; retrying the device runner");
                        break;
                    }
                    let left = state.next_at.saturating_sub(retry::now()).clamp(1, 5);
                    proc::sleep(Duration::from_secs(left))?;
                }
                return Ok(());
            }
            proc::sleep(Duration::from_secs(wait))?;
        }
        Ok(())
    }

    // ── cleanup (the script's EXIT trap) ────────────────────────────────────

    pub fn cleanup(mut self, status: i32) -> i32 {
        let mut failed = false;
        if status != 0 {
            if self.started_mjpeg
                && !pid::stop(
                    &self.ctx,
                    &self.ctx.mjpeg_relay_pid_file,
                    &self.legacy.mjpeg,
                    Role::Mjpeg,
                )
            {
                failed = true;
            }
            if self.started_control
                && !pid::stop(
                    &self.ctx,
                    &self.ctx.relay_pid_file,
                    &self.legacy.relay,
                    Role::Relay,
                )
            {
                failed = true;
            }
            if self.started_runner
                && !pid::stop(
                    &self.ctx,
                    &self.ctx.runner_pid_file,
                    &self.legacy.runner,
                    Role::Runner,
                )
            {
                failed = true;
            }
        }
        if !self.rollback_interactive(status) {
            failed = true;
        }
        if status != 0 && self.daemon.active && !self.daemon.touched {
            // Nothing was changed, so nothing is restored. Reloading anyway
            // killed the daemon that had just asked for the stop.
            let _ = std::fs::remove_file(&self.daemon.rollback);
        } else if status != 0 && self.daemon.active && !self.restore_daemon() {
            failed = true;
        }
        if let Some(staged) = self.daemon.staged.take() {
            let _ = std::fs::remove_file(staged);
        }
        if status != 0 && status != 130 && self.ctx.keepalive && self.keepalive_attempt_active {
            self.keepalive_attempt_active = false;
            if !self.record_failure() {
                failed = true;
            }
        }
        let code = if failed { 1 } else { status };
        if let Some(run) = self.run.take() {
            run.finish(code);
        }
        code
    }

    fn record_failure(&self) -> bool {
        match retry::record_failure(&self.ctx.retry_state, self.failure_kind) {
            Ok((attempt, delay, previous)) => {
                if self.failure_kind == Kind::Locked {
                    self.phase(
                        "lock-backoff",
                        "locked",
                        &format!(
                            "lock screen blocked the device runner; next quiet retry in {delay}s"
                        ),
                    );
                    if previous != Some(Kind::Locked) {
                        warn("iPhone lock screen blocked the device runner; retrying quietly every 5s to 1min until it is unlocked");
                    }
                } else if self.failure_kind == Kind::Automation {
                    if previous != Some(Kind::Automation) {
                        warn("the iPhone has not allowed UI automation; retrying quietly every 5s to 1min until it does");
                    }
                } else if self.failure_kind == Kind::WifiAutomation {
                    if previous != Some(Kind::WifiAutomation) {
                        warn("the iPhone refused UI automation over Wi-Fi; waiting 15 min between attempts, or until it is plugged in over USB");
                    }
                } else if self.failure_kind == Kind::NotConnected {
                    self.phase("waiting", "not_connected", NOT_CONNECTED_MESSAGE);
                    if previous != Some(Kind::NotConnected) {
                        warn("the iPhone is not connected to this Mac; waiting for it without rebuilding anything");
                    }
                } else if self.failure_kind == Kind::Owned {
                    warn(&format!(
                        "another session holds the phone; checking its lease again in {delay}s"
                    ));
                } else {
                    warn(&format!(
                        "KeepAlive rebuild failed; next retry in {delay}s (failure {attempt})"
                    ));
                }
                true
            }
            Err(_) => false,
        }
    }

    fn restore_daemon(&self) -> bool {
        let ctx = &self.ctx;
        let label = ctx.instance.daemon_label.clone();
        warn("setup failed — restoring the prior daemon configuration and loaded state");
        let mut ok_all = true;
        launchd::bootout(ctx, &label);
        if !launchd::wait_gone(ctx, &label) {
            ok_all = false;
            warn("new daemon job did not fully stop during rollback");
        }
        if !restore_backup(&self.daemon.rollback, &ctx.daemon_plist, 0o600) {
            ok_all = false;
            warn(&format!(
                "could not restore the prior daemon plist; rescue backup retained at:\n   {}",
                self.daemon.rollback.display()
            ));
        }
        if self.daemon.was_loaded {
            if ok_all && plist_lints(&ctx.daemon_plist) {
                launchd::enable(ctx, &label);
                if !launchd::bootstrap(ctx, &ctx.daemon_plist) || !launchd::loaded(ctx, &label) {
                    ok_all = false;
                    warn("prior daemon plist was restored, but its loaded state was not");
                }
            } else {
                ok_all = false;
            }
        }
        if !launchd::restore_policy(ctx, &label, self.daemon.was_disabled) {
            ok_all = false;
        }
        if ok_all {
            let _ = std::fs::remove_file(&self.daemon.rollback);
        } else if self.daemon.rollback.is_file() {
            warn(&format!(
                "daemon rescue backup retained at: {}",
                self.daemon.rollback.display()
            ));
        }
        ok_all
    }

    /// `_prepare_locked_retry` + `exit 1`.
    fn locked_retry<T>(&mut self) -> Step<T> {
        self.failure_kind = Kind::Locked;
        pid::stop(
            &self.ctx,
            &self.ctx.mjpeg_relay_pid_file,
            &self.legacy.mjpeg,
            Role::Mjpeg,
        );
        pid::stop(
            &self.ctx,
            &self.ctx.relay_pid_file,
            &self.legacy.relay,
            Role::Relay,
        );
        pid::stop(
            &self.ctx,
            &self.ctx.runner_pid_file,
            &self.legacy.runner,
            Role::Runner,
        );
        Err(Exit(1))
    }

    // ── the setup body ──────────────────────────────────────────────────────

    pub fn body(&mut self) -> Step {
        self.prerequisites()?;
        self.resolve_device()?;
        let source_hash = self.runner_source()?;
        let (deployment_override, sdk) = self.xcconfig()?;
        self.destination_fallback()?;
        self.legacy = Legacy::new(&self.ctx, &self.ctx.team_id, &self.ctx.bundle_id);
        self.announce_device();
        self.wait_for_developer_services()?;
        self.wait_for_unlock()?;
        self.phase(
            "building",
            &self.build_blocker.clone(),
            "building + launching the device runner",
        );
        info("Building + launching the device runner on the phone (the first build takes a minute or two)");
        // Replacing a live runner takes the phone from whoever drives it.
        // Recovering a dead one is fine: nobody can be using it.
        if !owner::overridden(&self.ctx)
            && pid::validate(
                &self.ctx,
                &self.ctx.runner_pid_file,
                &self.legacy.runner,
                Role::Runner,
                false,
            )
            .is_some()
        {
            if let Some(lease) =
                owner::foreign(owner::current(&self.ctx), owner::caller().as_deref())
            {
                self.failure_kind = Kind::Owned;
                self.phase(
                    "building",
                    "",
                    &format!(
                        "the phone is in use by session {}; not replacing its live runner",
                        lease.owner
                    ),
                );
                return die(owner::refusal(&lease, &self.ctx));
            }
        }
        if !pid::stop(
            &self.ctx,
            &self.ctx.runner_pid_file,
            &self.legacy.runner,
            Role::Runner,
        ) {
            return die("the prior runner PID record does not safely identify a process; refusing to kill anything");
        }
        let _ = std::fs::write(&self.ctx.run_log, b"");
        let xcodebuild = PathBuf::from(sys::stdout_of("xcrun", &["--find", "xcodebuild"]));
        if xcodebuild.as_os_str().is_empty() || !sys::is_executable(&xcodebuild) {
            return die("could not resolve the selected Xcode's xcodebuild executable");
        }
        let xcode_version =
            checks::xcode_version_cached(self.ctx.state_dir(), &xcodebuild.to_string_lossy());
        self.icon_source = icon::source(&self.ctx);
        // The icon is part of the product: a different icon rebuilds.
        let key = format!(
            "{}|{}",
            runner::cache_key(
                &self.ctx,
                &source_hash,
                &self.ctx.bundle_id,
                &self.ctx.team_id,
                &self.ctx.udid,
                &xcode_version,
                sdk.as_deref().unwrap_or(""),
                &deployment_override,
            ),
            icon::cache_component(self.icon_source.as_deref())
        );
        let (_products, xctestrun, from_cache) = self.product(&xcodebuild, &key)?;
        let url = self.launch(&xcodebuild, &xctestrun, from_cache)?;
        self.phone_url = url.clone();
        let target_url = self.relays(&url)?;
        let daemon = self.configure_daemon(&target_url)?;
        if self.ctx.keepalive {
            self.verify_supervision(&target_url)?;
        } else {
            self.handoff(&target_url)?;
        }
        self.verify_product(&daemon)?;
        self.handoff_complete = true;
        if self.ctx.keepalive {
            if let Err(error) = retry::reset(&self.ctx.retry_state) {
                warn(&error);
                warn("could not clear KeepAlive retry state after a verified recovery");
            }
        }
        let _ = std::fs::remove_file(&self.daemon.rollback);
        let _ = std::fs::remove_file(&self.sup.rollback);
        let _ = std::fs::remove_file(&self.self_install.rollback);
        self.daemon.active = false;
        self.sup.active = false;
        self.self_install.replaced = false;
        self.phase("ready", "", "device runner and launchd supervisor verified");
        self.summary(&url, &target_url, &daemon, &source_hash);
        Ok(())
    }

    fn prerequisites(&mut self) -> Step {
        let mut previous = status::previous_blocker(&self.ctx.status_file);
        // A USB blocker from a default-mode attempt is incompatible with an
        // explicit LAN run.
        if self.ctx.lan() && previous == "usb" {
            previous.clear();
        }
        // Blockers only an on-phone or Xcode action clears stay visible across
        // the next build pass; `serving` is the first evidence they cleared.
        self.build_blocker = if matches!(
            previous.as_str(),
            "trust"
                | "automation_mode_disabled"
                | "xcode_too_old"
                | "automation_not_allowed"
                | "wifi_automation_refused"
        ) {
            previous.clone()
        } else {
            String::new()
        };
        self.phase("prereq", &previous, "checking prerequisites");
        info("Checking prerequisites");
        match checks::warp_preflight() {
            Ok(()) => {
                if checks::warp_on() {
                    ok(checks::warp_ready_summary());
                }
            }
            Err(error) => {
                self.phase("prereq", "warp", "WARP is connected and breaks CoreDevice");
                return die(format!(
                    "{error}\n   WARP would otherwise invalidate the just-verified runner session and create a restart loop.\n   See docs/wda-setup.html pitfall (WARP)."
                ));
            }
        }
        if let Err(error) = checks::system_proxy_check() {
            self.phase(
                "prereq",
                "proxy",
                "macOS system proxy is enabled but unusable",
            );
            return die(error);
        }
        if sys::which("lsof").is_none() {
            return die("lsof is required to verify exclusive loopback relay ownership");
        }
        let Some(xcodebuild) = sys::which("xcodebuild") else {
            return die(format!(
                "Xcode is not installed. Get it from the App Store ({XCODE_APP_STORE_URL}), open it once, then rerun"
            ));
        };
        if valid_port(&self.ctx.wda_port).is_none() {
            return die(format!(
                "WDA_PORT must be a decimal TCP port from 1 to 65535 (got '{}')",
                self.ctx.wda_port
            ));
        }
        if valid_port(&self.ctx.mjpeg_port).is_none() {
            return die(format!(
                "MJPEG_PORT must be a decimal TCP port from 1 to 65535 (got '{}')",
                self.ctx.mjpeg_port
            ));
        }
        if self.ctx.wda_port == self.ctx.mjpeg_port {
            return die(format!(
                "WDA_PORT and MJPEG_PORT must be different (both are '{}')",
                self.ctx.wda_port
            ));
        }
        let version =
            checks::xcode_version_cached(self.ctx.state_dir(), &xcodebuild.to_string_lossy());
        if version.is_empty() {
            return die(format!(
                "full Xcode is not selected. Install it from the App Store ({XCODE_APP_STORE_URL}), then run: sudo xcode-select -s /Applications/Xcode.app"
            ));
        }
        ok(&format!("Xcode: {version}"));
        let signing = match checks::resolve_signing(&self.ctx) {
            Ok(signing) => signing,
            Err(error) => {
                checks::open_xcode_for_account(&self.ctx);
                return die(error);
            }
        };
        ok(&format!("Team: {}", signing.team));
        if signing.derived {
            ok(&format!(
                "Runner bundle ID: {} (derived for this team)",
                signing.bundle
            ));
        } else {
            ok(&format!(
                "Runner bundle ID: {} (explicit or persisted)",
                signing.bundle
            ));
        }
        self.ctx.team_id = signing.team.clone();
        self.ctx.bundle_id = signing.bundle.clone();
        self.signing = Some(signing);
        Ok(())
    }

    fn resolve_device(&mut self) -> Step {
        info("Resolving target device");
        let udid_ok = |udid: &str| udid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
        if !self.ctx.udid.is_empty() && !udid_ok(&self.ctx.udid) {
            return die("target UDID contains invalid characters (expected hex and dashes)");
        }
        if !self.ctx.instance.is_default() && self.ctx.udid.is_empty() {
            let name = &self.ctx.instance.name;
            return die(format!(
                "instance {name} has no target iPhone; set WDA_UDID or rerun install.sh --instance {name} --udid <UDID>"
            ));
        }
        if self.ctx.udid.is_empty() {
            let usb = checks::usb_udids();
            if usb.len() == 1 {
                self.ctx.udid = usb[0].clone();
                ok(&format!("using USB-connected iPhone: {}", self.ctx.udid));
            } else if !usb.is_empty() {
                return die(format!(
                    "multiple iPhones are connected over USB ({}). Set WDA_UDID=<one>; refusing to guess.",
                    usb.join(" ")
                ));
            }
        }
        if !self.ctx.udid.is_empty() {
            self.wait_until_connected()?;
        }
        if !self.ctx.lan() {
            if self.ctx.udid.is_empty() {
                self.phase("prereq", "usb", "no USB iPhone is connected");
                return die("the device layer defaults to USB, but no USB iPhone was found.\n   Plug in and unlock one iPhone, or set WDA_UDID=<USB UDID>; no build was started.");
            }
            if !checks::on_usb(&self.ctx.udid, &checks::usb_udids()) {
                self.phase(
                    "prereq",
                    "usb",
                    "the configured iPhone is not connected over USB",
                );
                return die(format!(
                    "target {} is not currently connected over USB.\n   Plug in that iPhone, or set WDA_UDID to the exact USB-connected device; refusing a slow Wi-Fi fallback.",
                    self.ctx.udid
                ));
            }
        } else if self.ctx.udid.is_empty() {
            warn("WDA_ALLOW_LAN=1: no USB target; paired destinations will be enumerated from the runner project");
        }
        let ports = [self.ctx.wda_port.clone(), self.ctx.mjpeg_port.clone()];
        let refs: Vec<&str> = ports.iter().map(String::as_str).collect();
        if let Err(error) = super::ctx::check_bindings_cached(&self.ctx, &refs) {
            println!("{error}");
            return die(format!(
                "refusing to set up instance {} (see above)",
                self.ctx.instance.name
            ));
        }
        self.phase("prereq", "", "prerequisites passed");
        Ok(())
    }

    fn runner_source(&mut self) -> Step<String> {
        info("Device runner source");
        if let Err(error) = checks::runner_source_valid(&self.ctx) {
            self.phase(
                "prereq",
                "wda",
                "device runner sources are missing or unsafe",
            );
            return die(error);
        }
        let Some(hash) = checks::runner_source_hash(&self.ctx.runner_src) else {
            self.phase("prereq", "wda", "device runner sources could not be read");
            return die(format!(
                "could not read the device runner sources in {}",
                self.ctx.runner_src.display()
            ));
        };
        ok(&format!(
            "Device runner source: {} (sha256 {})",
            self.ctx.runner_src.display(),
            &hash[..12]
        ));
        if std::fs::create_dir_all(&self.ctx.runner_derived_data).is_err() {
            return die(format!(
                "could not create {}",
                self.ctx.runner_derived_data.display()
            ));
        }
        Ok(hash)
    }

    /// Returns (deployment override, SDK version).
    fn xcconfig(&mut self) -> Step<(String, Option<String>)> {
        let sdk = checks::ios_sdk_version();
        match runner::prepare_xcconfig(&self.ctx) {
            Ok((target, env)) => {
                self.xcconfig = XcconfigEnv(env);
                if !target.is_empty() {
                    ok(&format!(
                        "iOS deployment target raised to {target} for this Xcode (via {}; sources untouched)",
                        self.ctx.xcconfig_file.display()
                    ));
                }
                Ok((target, sdk))
            }
            Err(_) => die(format!(
                "could not write the Xcode compatibility xcconfig at {}",
                self.ctx.xcconfig_file.display()
            )),
        }
    }

    /// LAN only, with no target: the one paired destination the runner
    /// project lists. Guessing among several could drive the wrong phone.
    fn destination_fallback(&mut self) -> Step {
        if !self.ctx.udid.is_empty() {
            return Ok(());
        }
        let xcodebuild = sys::which("xcodebuild").unwrap_or_else(|| PathBuf::from("xcodebuild"));
        let args =
            runner::project_argv(&self.ctx, &["-showdestinations".to_string()]).unwrap_or_default();
        let mut command = std::process::Command::new(&xcodebuild);
        command
            .args(&args)
            .current_dir(self.ctx.state_dir())
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        self.xcconfig.apply(&mut command);
        let text = command
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let mut udids: Vec<String> = text
            .lines()
            .filter(|line| line.contains("platform:iOS, arch:arm64"))
            .filter_map(|line| {
                let start = line.find("id:")? + 3;
                let rest = &line[start..];
                let end = rest.find(',')?;
                let id = &rest[..end];
                (!id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b) || b == b'-'))
                .then(|| id.to_string())
            })
            .collect();
        udids.sort();
        udids.dedup();
        match udids.len() {
            0 => die("no iOS device found — pair the iPhone, enable Developer Mode, and rerun"),
            1 => {
                self.ctx.udid = udids.remove(0);
                Ok(())
            }
            _ => {
                for udid in &udids {
                    eprintln!("    {udid}");
                }
                die("multiple paired iOS destinations are available; set WDA_UDID=<one exact UDID>")
            }
        }
    }

    fn announce_device(&self) {
        let udid = &self.ctx.udid;
        let info = sys::block_on(async {
            tokio::time::timeout(Duration::from_secs(5), crate::lockdown::device_info(udid))
                .await
                .ok()
                .and_then(Result::ok)
        });
        let (name, over_usb) = match info {
            Some(info) if info.connection == "usb" => {
                let mut name = info.name.unwrap_or_default();
                if let Some(version) = info.product_version.filter(|v| !v.is_empty()) {
                    name.push_str(&format!(", iOS {version}"));
                }
                (name, true)
            }
            _ => {
                let text = sys::devicectl(8, &["device", "info", "details", "--device", udid]);
                let name = text
                    .lines()
                    .find_map(|line| {
                        let lower = line.to_ascii_lowercase();
                        let index = lower
                            .find("marketing name:")
                            .or_else(|| lower.find("marketingname:"))?;
                        let colon = line[index..].find(':')? + index + 1;
                        Some(line[colon..].trim_start().to_string())
                    })
                    .unwrap_or_default();
                (name, false)
            }
        };
        if name.is_empty() {
            ok(&format!("Device UDID: {udid}"));
        } else {
            ok(&format!("Device UDID: {udid}  ({name})"));
        }
        if !over_usb {
            let count = sys::devicectl(8, &["list", "devices"])
                .lines()
                .filter(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower.contains("iphone") || lower.contains("ipad")
                })
                .count();
            if count > 1 {
                warn(&format!(
                    "{count} iOS devices are paired — if the wrong one was picked, re-run with WDA_UDID=<classic-udid> (the 00008…/8-… id)."
                ));
            }
        }
    }

    /// `not_connected` when the phone left this Mac entirely, else `usb`
    /// (it is still reachable, just not over the cable).
    fn disconnected_blocker(&mut self) -> &'static str {
        if checks::presence(&self.ctx.udid) == checks::Presence::Absent {
            self.failure_kind = Kind::NotConnected;
            "not_connected"
        } else {
            "usb"
        }
    }

    /// Holds while the phone is not connected to this Mac at all, without
    /// building or launching anything, and returns once it is back. usbmuxd
    /// is asked every 2 s (it sees a cable at once); CoreDevice, slower but
    /// also aware of a Wi-Fi phone, every 10 s. An unreadable CoreDevice
    /// answer never counts as absent. Interactive setup gives up after 10
    /// minutes; KeepAlive waits as long as it takes.
    fn wait_until_connected(&mut self) -> Step {
        const POLL: Duration = Duration::from_secs(2);
        const INTERACTIVE_LIMIT: u32 = 300;
        let udid = self.ctx.udid.clone();
        let mut tick: u32 = 0;
        loop {
            let present = if tick % 5 == 0 {
                checks::presence(&udid) != checks::Presence::Absent
            } else {
                checks::usbmux_lists(&udid)
            };
            if present {
                if tick > 0 {
                    ok("the iPhone is connected to this Mac again");
                }
                return Ok(());
            }
            if tick == 0 {
                self.phase("prereq", "not_connected", NOT_CONNECTED_MESSAGE);
                warn(&format!(
                    "{NOT_CONNECTED_MESSAGE}. Waiting for {udid}; nothing is built or launched until it is back."
                ));
            } else if !self.ctx.keepalive && tick % 30 == 0 {
                warn("still waiting for the iPhone — plug it in over USB and unlock it ...");
            }
            if !self.ctx.keepalive && tick >= INTERACTIVE_LIMIT {
                return die(format!(
                    "{NOT_CONNECTED_MESSAGE}; it did not come back within 10 minutes, and no build was started"
                ));
            }
            proc::sleep(POLL)?;
            tick += 1;
        }
    }

    fn ddi_ready(&self) -> bool {
        let udid = &self.ctx.udid;
        let mounted = sys::block_on(async {
            tokio::time::timeout(Duration::from_secs(5), crate::lockdown::ddi_status(udid))
                .await
                .ok()
                .and_then(Result::ok)
        })
        .is_some_and(|status| status.mounted);
        if mounted {
            return true;
        }
        let file = tempfile::NamedTempFile::new().ok();
        let path = file
            .as_ref()
            .map(|f| f.path().to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = sys::devicectl(
            10,
            &["device", "info", "details", "--device", udid, "-j", &path],
        );
        let json = std::fs::read_to_string(&path).unwrap_or_default();
        let json_true = json.lines().any(|line| {
            line.find("\"ddiServicesAvailable\"").is_some_and(|i| {
                line[i + "\"ddiServicesAvailable\"".len()..]
                    .trim_start()
                    .strip_prefix(':')
                    .is_some_and(|rest| rest.trim_start().starts_with("true"))
            })
        });
        json_true || text.contains("ddiServicesAvailable: true")
    }

    fn wait_for_developer_services(&mut self) -> Step {
        let mut blocker = self.build_blocker.clone();
        if self.ctx.lan() {
            self.phase(
                "ddi-wait",
                &self.build_blocker.clone(),
                "waiting for developer services — unlock and keep the iPhone awake",
            );
            if !self.lock_retry {
                info("Waiting for developer services (UNLOCK the iPhone and keep it awake)");
            }
        } else {
            blocker = "usb".into();
            self.phase(
                "ddi-wait",
                "usb",
                "waiting for developer services — unlock + USB",
            );
            if !self.lock_retry {
                info("Waiting for developer services (UNLOCK the iPhone, keep it awake, and plug it in via USB)");
            }
        }
        let mut tries = 0;
        while !self.ddi_ready() {
            proc::check()?;
            tries += 1;
            // Developer services never come up for a phone that left: wait
            // for it to come back instead of counting toward a DDI failure.
            if tries % 5 == 0 && checks::presence(&self.ctx.udid) == checks::Presence::Absent {
                self.wait_until_connected()?;
                tries = 0;
            }
            self.phase(
                "ddi-wait",
                &blocker,
                &format!("waiting for developer services (attempt {tries})"),
            );
            if tries > 45 {
                warn(&format!(
                    "developer services never became available for {}.",
                    self.ctx.udid
                ));
                warn("Most reliable fix: connect this iPhone to the Mac with a USB cable");
                warn("(Wi-Fi-only often sits in 'connecting' and never mounts the disk image),");
                warn("keep it unlocked + awake, then re-run. Devices the Mac currently sees:");
                for line in sys::devicectl(8, &["list", "devices"]).lines() {
                    eprintln!("    {line}");
                }
                warn("If the wrong phone was picked, re-run with WDA_UDID=<classic-udid>.");
                warn("If WARP is connected, verify its effective Excluded routes contain fe80::/10 and fd00::/8.");
                warn("Temporarily disconnect WARP only when those Zero Trust Split Tunnel exclusions cannot be added.");
                self.phase(
                    "ddi-fail",
                    "ddi",
                    "developer services never became available",
                );
                return die("developer services not available (docs/wda-setup.html pitfall ①; check WARP/USB)");
            }
            if self.ctx.keepalive {
                if tries == 1 && !self.lock_retry {
                    warn("developer services are not ready; KeepAlive will not repeat this prompt on every poll");
                }
            } else if tries % 8 == 1 {
                if self.ctx.lan() {
                    warn("still waiting — UNLOCK the phone and keep the screen on ...");
                } else {
                    warn(
                        "still waiting — UNLOCK the phone, keep the screen on, and plug in USB ...",
                    );
                }
            }
            proc::sleep(Duration::from_secs(4))?;
        }
        ok("Developer Disk Image mounted");
        Ok(())
    }

    /// `true`, `false`, or `None` when devicectl could not tell.
    fn passcode_required(&self) -> Option<bool> {
        let json = sys::devicectl_json(
            5,
            &["device", "info", "lockState", "--device", &self.ctx.udid],
        )?;
        let value: serde_json::Value = serde_json::from_str(&json).ok()?;
        let result = value.get("result")?;
        Some(result.get("passcodeRequired") == Some(&serde_json::Value::Bool(true)))
    }

    /// Launching onto a locked phone fails only after xcodebuild's ~70 s
    /// automation-mode timeout. Ask CoreDevice first, publish `locked`, and
    /// launch the moment it unlocks (two explicit "unlocked" reads in a row;
    /// an unreadable one proves nothing).
    fn wait_for_unlock(&mut self) -> Step {
        let (limit, complaint) =
            lock_wait_limit(std::env::var("WDA_LOCK_WAIT_SECS").ok().as_deref());
        if let Some(complaint) = complaint {
            warn(&complaint);
        }
        if self.passcode_required() != Some(true) {
            return Ok(());
        }
        info("Waiting for the iPhone to be unlocked");
        self.phase(
            "lock-wait",
            "locked",
            "the iPhone is locked — unlock it and connecting continues on its own",
        );
        let started = Instant::now();
        let mut wait = UnlockWait::default();
        loop {
            proc::check()?;
            let reading = self.passcode_required();
            if wait.observe(reading) {
                break;
            }
            if reading == Some(false) {
                // One unlocked reading: read again at once for the second.
                continue;
            }
            if started.elapsed().as_secs() >= limit {
                if self.ctx.keepalive {
                    return self.locked_retry();
                }
                self.phase(
                    "building-fail",
                    "locked",
                    &format!("the iPhone stayed locked for {limit}s"),
                );
                return die(format!(
                    "the iPhone stayed locked for {limit}s. Unlock it, then rerun setup."
                ));
            }
            proc::sleep(Duration::from_secs(1))?;
        }
        ok(&format!(
            "iPhone unlocked after {}s",
            started.elapsed().as_secs()
        ));
        Ok(())
    }

    // ── the product ─────────────────────────────────────────────────────────

    /// (products dir, .xctestrun, reused from the cache).
    fn product(&mut self, xcodebuild: &Path, key: &str) -> Step<(PathBuf, PathBuf, bool)> {
        let rebuild = std::env::var("WDA_RUNNER_REBUILD").is_ok_and(|v| v == "1");
        if !rebuild {
            if let Some((products, _app, xctestrun)) = runner::cache_read(&self.ctx, key) {
                ok("Reusing the runner product from the last bring-up (no rebuild; WDA_RUNNER_REBUILD=1 forces one)");
                self.phase(
                    "building",
                    &self.build_blocker.clone(),
                    "reusing the verified runner product from the last bring-up",
                );
                return Ok((products, xctestrun, true));
            }
        }
        let build_log = self.rel("wda-runner-product-build.log");
        match self.ensure_launchable(xcodebuild, &build_log)? {
            Some((products, xctestrun)) => {
                // Record as soon as it is verified, not after it serves: a
                // round that fails for a reason outside the product must not
                // rebuild every retry (#75).
                if runner::cache_write(&self.ctx, key, &products, &xctestrun) {
                    ok("Recorded the verified runner product; the next reconnect installs it without rebuilding");
                } else {
                    warn("could not record the runner product for reuse; the next reconnect rebuilds");
                }
                Ok((products, xctestrun, false))
            }
            None => {
                if self.build_locked {
                    if self.ctx.keepalive {
                        return self.locked_retry();
                    }
                    return die("the phone is locked and the runner build exited. Unlock it, then rerun setup.");
                }
                if runner::log_shows_no_accounts(&build_log) {
                    return self.report_missing_account();
                }
                if runner::log_shows_profile_failure(&build_log) {
                    self.phase(
                        "signing-fail",
                        "account",
                        "Xcode could not create the runner provisioning profile",
                    );
                    // "could not find or create a development provisioning" is
                    // the phrase the daemon maps to its `account` blocker.
                    return die(format!(
                        "Xcode could not find or create a development provisioning profile for the device runner.\n   In Xcode → Settings → Accounts, refresh the selected team, keep the iPhone\n   registered, then rerun. With WDA_ASC_* API-key signing, check that the key\n   can manage profiles. Build log: {}",
                        build_log.display()
                    ));
                }
                let error = self.validation_error.clone();
                self.phase("building-fail", "wda", &error);
                die(format!("device runner product is not launchable: {error}"))
            }
        }
    }

    fn prebuild(&mut self, xcodebuild: &Path, build_log: &Path) -> Step<bool> {
        self.phase(
            "building",
            &self.build_blocker.clone(),
            "building the device runner",
        );
        let _ = std::fs::write(build_log, b"");
        let signing = self.signing.clone().unwrap_or(Signing {
            team: self.ctx.team_id.clone(),
            bundle: self.ctx.bundle_id.clone(),
            derived: false,
        });
        let extra = [
            "-destination".to_string(),
            format!("platform=iOS,id={}", self.ctx.udid),
            "-allowProvisioningUpdates".into(),
            format!("DEVELOPMENT_TEAM={}", signing.team),
            format!("PRODUCT_BUNDLE_IDENTIFIER={}", signing.bundle),
            "build-for-testing".into(),
        ];
        let Ok(args) = runner::project_argv(&self.ctx, &extra) else {
            self.validation_error =
                format!("build-for-testing failed (log: {})", build_log.display());
            return Ok(false);
        };
        // Xcode reuses a cached profile until it has expired; one that is due
        // goes aside so this build carries a fresh one (7 days on a free
        // Apple ID), and comes back if the build fails.
        let aside = runner::set_aside_due_profiles(
            &signing.team,
            &signing.bundle,
            &self.ctx.state_dir().join("profiles-renewed"),
            super::retry::now(),
        );
        if !aside.is_empty() {
            ok(&format!(
                "Renewing the runner provisioning profile ({} due within {}h)",
                aside.len(),
                runner::PROFILE_RENEW_SECS / 3600
            ));
        }
        let result = proc::run_logged(
            xcodebuild,
            &args,
            Some(self.ctx.state_dir()),
            build_log,
            &self.xcconfig,
        );
        if matches!(result, Ok(true)) {
            for stale in &aside {
                let _ = std::fs::remove_file(stale);
            }
        } else {
            runner::restore_profiles(&aside);
        }
        let built = result?;
        if !built {
            if runner::log_shows_lock(build_log) {
                self.build_locked = true;
            }
            self.validation_error =
                format!("build-for-testing failed (log: {})", build_log.display());
        }
        Ok(built)
    }

    fn ensure_launchable(
        &mut self,
        xcodebuild: &Path,
        build_log: &Path,
    ) -> Step<Option<(PathBuf, PathBuf)>> {
        let products = self.ctx.runner_products_dir.clone();
        let app = products.join(RUNNER_APP_NAME);
        // An injected app is not a valid incremental-build input (#75).
        if let Err(error) = icon::discard_previous_injection(&self.ctx, &app) {
            self.validation_error = error;
            return Ok(None);
        }
        if !self.prebuild(xcodebuild, build_log)? {
            return Ok(None);
        }
        if std::fs::symlink_metadata(&app).is_err() {
            self.validation_error = format!(
                "build-for-testing produced no {RUNNER_APP_NAME} (log: {})",
                build_log.display()
            );
            return Ok(None);
        }
        if !self.repair_if_invalid(xcodebuild, &products, &app, build_log)? {
            return Ok(None);
        }
        let Some(xctestrun) = runner::resolve_xctestrun(&products, None) else {
            self.validation_error = format!(
                "could not resolve a unique .xctestrun next to {} (run doctor)",
                products.display()
            );
            return Ok(None);
        };
        if let Some(source) = self.icon_source.clone() {
            // Failure restores the pristine app; setup continues without it.
            icon::inject(&self.ctx, &app, &source);
        }
        Ok(Some((products, xctestrun)))
    }

    fn repair_if_invalid(
        &mut self,
        xcodebuild: &Path,
        products: &Path,
        app: &Path,
        build_log: &Path,
    ) -> Step<bool> {
        let reason = match runner::validate_bundle(app) {
            Ok(()) => return Ok(true),
            Err(reason) => reason,
        };
        self.validation_error = reason.clone();
        warn(&format!("Runner product invalid: {reason}"));
        self.phase(
            "building",
            &self.build_blocker.clone(),
            &format!("runner product invalid: {reason}; rebuilding once"),
        );
        if self.repair_attempted {
            self.phase(
                "building-fail",
                "wda",
                &format!("runner still invalid after one repair: {reason}"),
            );
            return Ok(false);
        }
        // Only this instance's own runner app, at its fixed products path.
        let canonical = products.to_string_lossy().contains("/Build/Products/")
            && products == self.ctx.runner_products_dir
            && app == products.join(RUNNER_APP_NAME)
            && !std::fs::symlink_metadata(app).is_ok_and(|m| m.file_type().is_symlink());
        if !canonical {
            self.validation_error = "refusing to remove an unowned runner product".into();
            return Ok(false);
        }
        let resolved_ok = products.is_absolute()
            && products
                .canonicalize()
                .is_ok_and(|p| p.to_string_lossy().contains("/Build/Products/"))
            && app.parent().and_then(|p| p.canonicalize().ok()) == products.canonicalize().ok();
        if !resolved_ok {
            self.validation_error =
                "refusing to remove a runner outside canonical build products".into();
            return Ok(false);
        }
        self.repair_attempted = true;
        if std::fs::remove_dir_all(app).is_err() {
            return Ok(false);
        }
        let repair_log = PathBuf::from(format!(
            "{}.repair.log",
            build_log.to_string_lossy().trim_end_matches(".log")
        ));
        warn(&format!(
            "Runner repair: keeping the failed build's log at {}; rebuild log at {}",
            build_log.display(),
            repair_log.display()
        ));
        let rebuilt = self.prebuild(xcodebuild, &repair_log)?
            && match runner::validate_bundle(app) {
                Ok(()) => true,
                Err(error) => {
                    self.validation_error = error;
                    false
                }
            };
        if !rebuilt {
            self.phase(
                "building-fail",
                "wda",
                &format!("runner repair failed: {}", self.validation_error),
            );
            return Ok(false);
        }
        self.phase(
            "building",
            &self.build_blocker.clone(),
            "runner product rebuilt and verified",
        );
        Ok(true)
    }

    // ── launch ──────────────────────────────────────────────────────────────

    fn report_missing_account<T>(&self) -> Step<T> {
        self.phase(
            "signing-fail",
            "account",
            "sign in to an Apple account in Xcode, or configure WDA_ASC_KEY_PATH / WDA_ASC_KEY_ID / WDA_ASC_ISSUER_ID for API key signing",
        );
        die("Xcode has no signed-in Apple account. Open Xcode → Settings → Accounts,\n   sign in and select the development team, or configure WDA_ASC_KEY_PATH,\n   WDA_ASC_KEY_ID and WDA_ASC_ISSUER_ID for App Store Connect API key signing,\n   then rerun.")
    }

    fn report_device_unavailable<T>(&mut self) -> Step<T> {
        // Gone from this Mac entirely is its own blocker; the next attempt
        // waits for the phone instead of building again.
        if self.disconnected_blocker() == "not_connected" {
            self.phase("building-fail", "not_connected", NOT_CONNECTED_MESSAGE);
            return die(format!(
                "{NOT_CONNECTED_MESSAGE}; xcodebuild timed out waiting for it. KeepAlive waits for it to come back. Log: {}",
                self.ctx.run_log.display()
            ));
        }
        self.phase(
            "building-fail",
            "usb",
            "this Mac cannot reach the iPhone (xcodebuild timed out waiting for it) — connect it with a cable, unlock it and keep it awake",
        );
        die(format!(
            "this Mac cannot reach the iPhone: xcodebuild timed out waiting for it to become available. Connect it with a USB cable, unlock it and keep it awake; KeepAlive retries on its own. Log: {}",
            self.ctx.run_log.display()
        ))
    }

    fn report_automation_disabled<T>(&self) -> Step<T> {
        self.phase(
            "building-fail",
            "automation_mode_disabled",
            AUTOMATION_MODE_HINT,
        );
        die(format!(
            "iOS did not enable UI automation for the device runner — {AUTOMATION_MODE_HINT}, then rerun setup (KeepAlive retries on its own). Log: {}",
            self.ctx.run_log.display()
        ))
    }

    /// The runner refused the IDE channel AND the phone's iOS is newer than
    /// the SDK (#126). The version gap alone never fails setup.
    fn xcode_too_old(&self) -> Option<String> {
        if !runner::log_shows_ide_refusal(&self.ctx.run_log) {
            return None;
        }
        xcode_too_old_message(
            checks::ios_sdk_version().as_deref(),
            checks::device_ios_version(&self.ctx.udid).as_deref(),
        )
    }

    /// testmanagerd refused the IDE channel with an Xcode that supports this
    /// iOS. Over USB the phone wants a passcode or Allow prompt answered;
    /// over Wi-Fi iOS cannot show that prompt at all, so it is its own
    /// blocker with a long backoff instead of a minute-by-minute relaunch.
    fn report_ide_refusal<T>(&mut self) -> Step<T> {
        let wait = runner::ide_refusal_wait_secs(&self.ctx.run_log);
        let transport = checks::transport(&self.ctx.udid);
        let log = self.ctx.run_log.display().to_string();
        match ide_refusal_blocker(transport) {
            "wifi_automation_refused" => {
                let message = wifi_automation_message(wait);
                self.phase("building-fail", "wifi_automation_refused", &message);
                self.failure_kind = Kind::WifiAutomation;
                die(format!(
                    "{message}. Retrying over Wi-Fi cannot fix this; KeepAlive waits 15 minutes between attempts and starts again as soon as the iPhone is plugged in over USB. Log: {log}"
                ))
            }
            _ => {
                let message = automation_message(wait);
                self.phase("building-fail", "automation_not_allowed", &message);
                self.failure_kind = Kind::Automation;
                die(format!(
                    "{message}. KeepAlive retries quietly every 5 s to 1 min. Log: {log}"
                ))
            }
        }
    }

    fn report_xcode_too_old<T>(&mut self, message: &str) -> Step<T> {
        self.phase("building-fail", "xcode_too_old", message);
        self.failure_kind = Kind::XcodeTooOld;
        die(format!(
            "{message}. Retrying or reconnecting cannot fix this; KeepAlive waits 15 minutes between attempts. Log: {}",
            self.ctx.run_log.display()
        ))
    }

    fn runner_session(&self) -> Option<String> {
        let udid = &self.ctx.udid;
        sys::block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                crate::lockdown::runner_status(udid, RUNNER_DEVICE_PORT),
            )
            .await
            .ok()
            .and_then(Result::ok)
        })
        .and_then(|status| status.session_id)
        .filter(|id| !id.is_empty())
    }

    /// Launch the runner from its product and wait until it serves. Returns
    /// the URL it serves on (its LAN address, or 127.0.0.1:8100 when the
    /// answer came straight over USB).
    fn launch(&mut self, xcodebuild: &Path, xctestrun: &Path, from_cache: bool) -> Step<String> {
        let probe = checks::on_usb(&self.ctx.udid, &checks::usb_udids());
        let previous_session = if probe { self.runner_session() } else { None };
        let Ok(argv) = runner::runner_argv(&self.ctx, &self.ctx.udid, xctestrun) else {
            return die("could not prepare the device runner launch arguments");
        };
        let command = format!("{} {}", xcodebuild.display(), argv.join(" "));
        let expected = format!("runner:{command}");
        let Ok(spawned) = proc::spawn_detached(
            xcodebuild,
            &argv,
            Some(self.ctx.state_dir()),
            &self.ctx.run_log,
            Some(&self.xcconfig),
        ) else {
            return die(format!(
                "xcodebuild did not become the exact expected runner process; no unverified PID was signalled.\n   Inspect {} and any listener before retrying.",
                self.ctx.run_log.display()
            ));
        };
        let Some(runner_pid) = pid::write(
            &self.ctx,
            &self.ctx.runner_pid_file,
            spawned,
            &expected,
            Role::Runner,
        ) else {
            return die(format!(
                "xcodebuild did not become the exact expected runner process; no unverified PID was signalled.\n   Inspect {} and any listener before retrying.",
                self.ctx.run_log.display()
            ));
        };
        self.started_runner = true;
        ok(&format!(
            "PID-verified runner {runner_pid} (log: {})",
            self.ctx.run_log.display()
        ));
        info("Waiting for ServerURLHere (or a trust error) ...");
        let started = Instant::now();
        let mut tick: u64 = 0;
        let mut tries = 0;
        let mut from_probe_session = None;
        let url = loop {
            proc::check()?;
            if let Some(url) = runner::server_url(&self.ctx.run_log) {
                break url;
            }
            if probe {
                if let Some(session) = self.runner_session() {
                    if Some(&session) != previous_session.as_ref() {
                        from_probe_session = Some(session);
                        break format!("http://127.0.0.1:{RUNNER_DEVICE_PORT}");
                    }
                }
            }
            tick += 1;
            if tick % 5 != 1 {
                proc::sleep(Duration::from_millis(200))?;
                continue;
            }
            tries += 1;
            if tries > 360 {
                if runner::log_shows_product_failure(&self.ctx.run_log) {
                    runner::cache_drop(&self.ctx);
                    warn("the recorded runner product failed to install or launch; the next round rebuilds it");
                }
                if let Some(message) = self.xcode_too_old() {
                    return self.report_xcode_too_old(&message);
                }
                // The same refusal with an Xcode that supports this iOS: the
                // phone did not authorize the UI-automation session.
                if runner::log_shows_ide_refusal(&self.ctx.run_log) {
                    return self.report_ide_refusal();
                }
                if runner::log_shows_automation_disabled(&self.ctx.run_log) {
                    return self.report_automation_disabled();
                }
                self.phase(
                    "building-fail",
                    "wda",
                    "the device runner did not report its server URL before the startup timeout",
                );
                return die(format!(
                    "timed out waiting for the device runner to start — check {}",
                    self.ctx.run_log.display()
                ));
            }
            // Actionable xcodebuild failures before process liveness: fast
            // failures exit between polls and would read as "runner exited".
            if runner::log_shows_no_accounts(&self.ctx.run_log) {
                return self.report_missing_account();
            }
            if runner::log_shows_profile_failure(&self.ctx.run_log) {
                // The product was recorded before launch; one its launch
                // rejected for signing must not be reused.
                runner::cache_drop(&self.ctx);
                self.phase(
                    "signing-fail",
                    "account",
                    "Xcode could not create the runner provisioning profile",
                );
                return die("Xcode could not find or create a development provisioning profile for the device runner.\n   In Xcode → Settings → Accounts, refresh the selected team, keep the iPhone\n   registered, then rerun. If WARP is connected, its effective Excluded routes\n   must contain fe80::/10 and fd00::/8 (otherwise disconnect it temporarily).");
            }
            if !self.warned_link_drop && runner::log_shows_link_dropped(&self.ctx.run_log) {
                self.warned_link_drop = true;
                warn("the device link dropped while starting the runner (CoreDevice tunnel). Over Wi-Fi this is usually the phone's address changing or the phone sleeping; a USB cable makes it immune.");
            }
            if runner::log_shows_untrusted(&self.ctx.run_log) {
                self.phase(
                    "trust",
                    "trust",
                    "trust the Apple Development cert on the iPhone",
                );
                return die("Developer cert not trusted. On the iPhone: 设置 → 通用 → VPN与设备管理 → 信任 'Apple Development: …', then re-run. (pitfall ②)");
            }
            if pid::validate(
                &self.ctx,
                &self.ctx.runner_pid_file,
                &self.legacy.runner,
                Role::Runner,
                false,
            )
            .is_none()
            {
                let locked = runner::log_shows_lock(&self.ctx.run_log);
                if self.ctx.keepalive && locked {
                    return self.locked_retry();
                }
                if locked {
                    self.phase(
                        "building-fail",
                        "wda",
                        "phone is locked and the device runner exited",
                    );
                    return die(
                        "the phone is locked and xcodebuild exited. Unlock it, then rerun setup.",
                    );
                }
                // Only a failure of the product itself evicts it; a dropped
                // device link says nothing about the product.
                if runner::log_shows_product_failure(&self.ctx.run_log) {
                    runner::cache_drop(&self.ctx);
                    warn("the recorded runner product failed to install or launch; the next round rebuilds it");
                }
                if let Some(message) = self.xcode_too_old() {
                    return self.report_xcode_too_old(&message);
                }
                // The same refusal with an Xcode that supports this iOS: the
                // phone did not authorize the UI-automation session.
                if runner::log_shows_ide_refusal(&self.ctx.run_log) {
                    return self.report_ide_refusal();
                }
                if runner::log_shows_automation_disabled(&self.ctx.run_log) {
                    return self.report_automation_disabled();
                }
                // The phone vanished from this Mac before the runner could
                // start: say so, instead of the generic exit that sends the
                // operator to a log (#166).
                if runner::log_shows_device_unavailable(&self.ctx.run_log) {
                    return self.report_device_unavailable();
                }
                self.phase(
                    "building-fail",
                    "wda",
                    "the device runner exited before reporting its server URL",
                );
                return die(format!(
                    "the PID-verified device runner exited before reporting its server URL — check {}",
                    self.ctx.run_log.display()
                ));
            }
            if runner::log_shows_lock(&self.ctx.run_log)
                && runner::server_url(&self.ctx.run_log).is_none()
            {
                if self.ctx.keepalive {
                    return self.locked_retry();
                }
                self.interactive_lock_wait_tick()?;
            } else if tries % 30 == 0 {
                self.phase(
                    "building",
                    &self.build_blocker.clone(),
                    &format!(
                        "launching the device runner ({}s elapsed)",
                        started.elapsed().as_secs()
                    ),
                );
            }
            proc::sleep(Duration::from_millis(200))?;
        };
        let _ = from_cache;
        if !url.starts_with("http://") {
            return die(format!(
                "the device runner reported an unexpected server URL '{url}' (plain http:// expected)"
            ));
        }
        match &from_probe_session {
            Some(session) => ok(&format!(
                "device runner serving on device port {RUNNER_DEVICE_PORT} (answered over USB, session {}…)",
                session.chars().take(8).collect::<String>()
            )),
            None => ok(&format!("device runner serving at {url}")),
        }
        self.phase("serving", "", "device runner serving — starting relay");
        self.from_probe = from_probe_session.is_some();
        Ok(url)
    }
}

/// The daemon side of a run: where it listens and whether it answered.
pub struct DaemonEndpoint {
    pub port: String,
    pub http_ready: bool,
}

impl Setup {
    /// Interactive runs only: wait up to five minutes for an unlock, with
    /// prompts that back off instead of repeating every poll.
    fn interactive_lock_wait_tick(&mut self) -> Step {
        let now = retry::now();
        let lock = self.interactive_lock.get_or_insert_with(|| {
            warn("phone is locked; waiting up to 5 minutes without repeating this prompt every poll (Ctrl-C to stop)");
            InteractiveLock {
                started: now,
                notice_at: now + retry::exponential_delay(30, 120, 1),
                attempt: 1,
            }
        });
        let elapsed = now.saturating_sub(lock.started);
        if elapsed >= 300 {
            self.phase(
                "building-fail",
                "wda",
                "phone remained locked for 5 minutes",
            );
            return die("the phone remained locked for 5 minutes. Unlock it, then rerun setup.");
        }
        if now >= lock.notice_at {
            warn(&format!(
                "phone is still locked after {elapsed}s; unlock it, or press Ctrl-C and rerun setup later"
            ));
            lock.attempt += 1;
            lock.notice_at = now + retry::exponential_delay(30, 120, lock.attempt);
        }
        self.phase(
            "building",
            "wda",
            "phone is locked; interactive setup is waiting up to 5 minutes",
        );
        Ok(())
    }

    // ── relays ──────────────────────────────────────────────────────────────

    fn wait_tcp_listening(port: u16, limit: Duration) -> Step {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if sys::tcp_listening(port) {
                return Ok(());
            }
            proc::sleep(Duration::from_millis(50))?;
        }
        Ok(())
    }

    /// Start one relay (control or video) and prove it owns its port.
    #[allow(clippy::too_many_arguments)]
    fn start_relay(
        &mut self,
        role: Role,
        local: u16,
        device_port: u16,
        phone_ip: &str,
        tool: &RelayTool,
        pid_file: &Path,
        log: &Path,
    ) -> Step<(u32, String)> {
        let udid = self.ctx.udid.clone();
        let (program, args, desc): (PathBuf, Vec<String>, String) = match tool {
            RelayTool::Native(bin) => (
                bin.clone(),
                vec![
                    "relay".into(),
                    "--udid".into(),
                    udid.clone(),
                    "--listen".into(),
                    format!("127.0.0.1:{local}"),
                    "--device-port".into(),
                    device_port.to_string(),
                ],
                format!("USB relay (usbmuxd) on 127.0.0.1:{local}"),
            ),
            RelayTool::Iproxy(bin) => (
                bin.clone(),
                vec![
                    "-s".into(),
                    "127.0.0.1".into(),
                    format!("{local}:{device_port}"),
                    "-u".into(),
                    udid.clone(),
                ],
                format!("USB iproxy on 127.0.0.1:{local}"),
            ),
            RelayTool::Socat(bin) => (
                bin.clone(),
                vec![
                    format!("TCP-LISTEN:{local},fork,reuseaddr,bind=127.0.0.1"),
                    format!("TCP:{phone_ip}:{device_port}"),
                ],
                format!("LAN socat on 127.0.0.1:{local} to {phone_ip}:{device_port}"),
            ),
        };
        let expected = format!("{}:{} {}", role.name(), program.display(), args.join(" "));
        let log_name = log.display().to_string();
        let (what, retry_hint) = match role {
            Role::Relay => (
                "control relay",
                format!("Inspect {log_name} and TCP {local} before retrying."),
            ),
            _ => (
                "video relay",
                format!("Inspect {log_name} and TCP {local} before retrying."),
            ),
        };
        let spawned = proc::spawn_detached(&program, &args, None, log, None).ok();
        let recorded =
            spawned.and_then(|spawned| pid::write(&self.ctx, pid_file, spawned, &expected, role));
        let Some(relay_pid) = recorded else {
            return die(format!(
                "{what} did not become the exact expected process; no unverified PID was signalled.\n   {retry_hint}"
            ));
        };
        match role {
            Role::Relay => self.started_control = true,
            _ => self.started_mjpeg = true,
        }
        Self::wait_tcp_listening(local, Duration::from_secs(3))?;
        let legacy = self.legacy.for_role(role).to_string();
        if !pid::verify_loopback_listener(&self.ctx, pid_file, &legacy, role, local) {
            return die(format!(
                "{what} ownership/bind verification failed.\n   Expected only PID {relay_pid} on 127.0.0.1:{local}; inspect {log_name}"
            ));
        }
        Ok((relay_pid, desc))
    }

    /// Relay the runner's control and video ports to loopback. The daemon is
    /// a background LaunchAgent that macOS Local Network privacy keeps off the
    /// LAN, and the runner has no authentication: a USB relay is the default,
    /// a LAN socat relay only behind WDA_ALLOW_LAN=1. Returns the control URL.
    fn relays(&mut self, url: &str) -> Step<String> {
        let wda_port = valid_port(&self.ctx.wda_port).unwrap_or(8100);
        let mjpeg_port = valid_port(&self.ctx.mjpeg_port).unwrap_or(9100);
        info(&format!("Starting localhost relay on 127.0.0.1:{wda_port}"));
        let (mut phone_ip, mut device_port) = split_host_port(url);
        if valid_port(&device_port).is_none() {
            return die(format!(
                "the device runner reported an invalid device port in '{url}'"
            ));
        }
        if !pid::stop(
            &self.ctx,
            &self.ctx.relay_pid_file,
            &self.legacy.relay,
            Role::Relay,
        ) {
            return die(format!(
                "the prior control-relay PID record is not safe to stop; refusing to reuse TCP {wda_port}"
            ));
        }
        if !pid::assert_port_free(&self.ctx, wda_port) {
            return die(format!(
                "TCP {wda_port} must be free before starting the managed control relay"
            ));
        }
        let relay_log = self.rel("wda-relay.log");
        let _ = std::fs::write(&relay_log, b"");
        let target_is_usb = checks::on_usb(&self.ctx.udid, &checks::usb_udids());
        // Readiness seen over USB carries no LAN address; only the socat
        // fallback needs one, from the log line once xcodebuild flushes it.
        if self.from_probe && !target_is_usb {
            let mut lan_url = None;
            for _ in 0..50 {
                lan_url = runner::server_url(&self.ctx.run_log);
                if lan_url.is_some() {
                    break;
                }
                proc::sleep(Duration::from_millis(200))?;
            }
            match lan_url.filter(|u| u.starts_with("http://")) {
                Some(lan_url) => {
                    (phone_ip, device_port) = split_host_port(&lan_url);
                }
                None => {
                    let blocker = self.disconnected_blocker();
                    self.phase(
                        "serving",
                        blocker,
                        "the configured iPhone disconnected before the control relay started",
                    );
                    return die("the iPhone left USB after its runner started, and its network address is not known yet; reconnect the cable and retry");
                }
            }
        }
        let device_port: u16 = valid_port(&device_port).unwrap_or(RUNNER_DEVICE_PORT);
        let mut tool = None;
        if target_is_usb {
            tool = checks::relay_binary(&self.ctx)
                .map(RelayTool::Native)
                .or_else(|| sys::which("iproxy").map(RelayTool::Iproxy));
        }
        if tool.is_none() && self.ctx.lan() {
            if let Some(socat) = sys::which("socat") {
                warn("WDA_ALLOW_LAN=1: the device runner has no authentication; use only on a trusted, isolated LAN");
                if phone_ip.is_empty()
                    || !phone_ip
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._:%-".contains(&b))
                {
                    return die(format!("the device runner reported a LAN host that is unsafe for socat: '{phone_ip}'"));
                }
                tool = Some(RelayTool::Socat(socat));
            }
        }
        let Some(tool) = tool else {
            if !self.ctx.lan() && !target_is_usb {
                let blocker = self.disconnected_blocker();
                self.phase(
                    "serving",
                    blocker,
                    "the configured iPhone disconnected before the control relay started",
                );
            } else {
                self.phase(
                    "serving",
                    "wda",
                    "no permitted control relay tool is available",
                );
            }
            return die("the device layer relays over USB by default. Keep this iPhone connected over USB; if it is,\n   the iPhoneUse app is missing or too old to relay — reinstall it or run: iphone-use upgrade.\n   The on-phone runner has no HTTP authentication. A LAN relay is therefore disabled\n   unless WDA_ALLOW_LAN=1 is explicitly set for a trusted, isolated network.");
        };
        let relay_pid_file = self.ctx.relay_pid_file.clone();
        let (relay_pid, desc) = self.start_relay(
            Role::Relay,
            wda_port,
            device_port,
            &phone_ip,
            &tool,
            &relay_pid_file,
            &relay_log,
        )?;
        ok(&format!("PID-verified control relay {relay_pid}: {desc}"));
        let target_url = format!("http://127.0.0.1:{wda_port}");
        if !sys::http_ok(&format!("{target_url}/status"), Duration::from_secs(5)) {
            return die(format!(
                "relay up but the device runner is not answering through it — check {}",
                relay_log.display()
            ));
        }
        ok(&format!("device runner reachable at {target_url}"));
        warn("The Mac relay is loopback-only, but the runner on the iPhone has no HTTP authentication.\n   Keep the iPhone on a trusted, isolated network even when the Mac relay uses USB.");

        // Live video: the runner's MJPEG stream on the device's :9100, in the
        // same XCUITest session as control.
        if !pid::stop(
            &self.ctx,
            &self.ctx.mjpeg_relay_pid_file,
            &self.legacy.mjpeg,
            Role::Mjpeg,
        ) {
            return die(format!(
                "the prior video-relay PID record is not safe to stop; refusing to reuse TCP {mjpeg_port}"
            ));
        }
        if !pid::assert_port_free(&self.ctx, mjpeg_port) {
            return die(format!(
                "TCP {mjpeg_port} must be free before starting the managed video relay"
            ));
        }
        let mjpeg_log = self.rel("wda-mjpeg-relay.log");
        let _ = std::fs::write(&mjpeg_log, b"");
        let mjpeg_pid_file = self.ctx.mjpeg_relay_pid_file.clone();
        let (mjpeg_pid, mjpeg_desc) = self.start_relay(
            Role::Mjpeg,
            mjpeg_port,
            MJPEG_DEVICE_PORT,
            &phone_ip,
            &tool,
            &mjpeg_pid_file,
            &mjpeg_log,
        )?;
        let first_byte = sys::http_get_prefix(
            &format!("http://127.0.0.1:{mjpeg_port}"),
            Duration::from_secs(8),
            1,
        )
        .is_some_and(|(status, body)| status < 400 && !body.is_empty());
        if !first_byte {
            return die(format!(
                "video relay owns 127.0.0.1:{mjpeg_port} but no MJPEG data arrived within 8s.\n   The daemon configuration was not changed; inspect {}.",
                mjpeg_log.display()
            ));
        }
        ok(&format!(
            "PID-verified video relay {mjpeg_pid}: {mjpeg_desc}"
        ));
        Ok(target_url)
    }

    // ── the daemon ──────────────────────────────────────────────────────────

    /// Point the daemon at the verified endpoints. It restarts only for the
    /// settings it reads at startup: a restart drops every in-flight request,
    /// hold and owner lease.
    fn configure_daemon(&mut self, target_url: &str) -> Step<DaemonEndpoint> {
        let ctx = self.ctx.clone();
        let label = ctx.instance.daemon_label.clone();
        let mjpeg_url = format!("http://127.0.0.1:{}", ctx.mjpeg_port);
        if !ctx.daemon_plist.is_file() {
            warn("daemon LaunchAgent not found; the runner can be verified, but the product daemon cannot be started");
            println!(
                "    PHONE_REMOTE_BACKEND=direct PHONE_REMOTE_WDA_URL={target_url} PHONE_REMOTE_WDA_MJPEG_URL={mjpeg_url} iphone-use serve"
            );
            return Ok(DaemonEndpoint {
                port: "44321".into(),
                http_ready: false,
            });
        }
        info("Configuring the iphone-use daemon for the direct backend");
        let Some(was_disabled) = launchd::disabled_state(&ctx, &label) else {
            return die("could not snapshot the daemon's launchd disabled policy");
        };
        self.daemon.was_disabled = was_disabled;
        if !copy_preserving(&ctx.daemon_plist, &self.daemon.rollback) {
            return die("could not back up the daemon plist before changing its backend");
        }
        self.daemon.was_loaded = launchd::loaded(&ctx, &label);
        self.daemon.active = true;
        let staged = PathBuf::from(format!(
            "{}.install.{}",
            ctx.daemon_plist.display(),
            std::process::id()
        ));
        if !copy_preserving(&ctx.daemon_plist, &staged) {
            return die("could not stage the daemon plist for an atomic update");
        }
        self.daemon.staged = Some(staged.clone());
        {
            use std::os::unix::fs::PermissionsExt as _;
            if std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600)).is_err() {
                return die("could not secure the staged daemon plist");
            }
        }
        if !plist_buddy(&staged, "Print :EnvironmentVariables") {
            plist_buddy(&staged, "Add :EnvironmentVariables dict");
        }
        let current = |key: &str| sys::plist_env(&staged, key);
        let mut changed: Vec<&str> = Vec::new();
        if current("PHONE_REMOTE_BACKEND") != "direct" {
            changed.push("PHONE_REMOTE_BACKEND");
        }
        if current("PHONE_REMOTE_UDID") != ctx.udid {
            changed.push("PHONE_REMOTE_UDID");
        }
        if current("PHONE_REMOTE_WDA_URL") != target_url {
            changed.push("PHONE_REMOTE_WDA_URL");
        }
        if current("PHONE_REMOTE_WDA_MJPEG_URL") != mjpeg_url {
            changed.push("PHONE_REMOTE_WDA_MJPEG_URL");
        }
        if current("PHONE_REMOTE_WDA_MANAGED") != "true" {
            changed.push("PHONE_REMOTE_WDA_MANAGED");
        }
        // A per-phone Xcode reaches the daemon's own devicectl/xcrun calls
        // through its environment, which it reads at startup.
        let want_dir = ctx
            .developer_dir
            .as_ref()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();
        if current("DEVELOPER_DIR") != want_dir {
            changed.push("DEVELOPER_DIR");
        }
        let needs_restart = !changed.is_empty();
        if current("WDA_ALLOW_LAN") != ctx.allow_lan {
            changed.push("WDA_ALLOW_LAN");
        }
        let config_changed = !changed.is_empty();
        if config_changed {
            for (key, value) in [
                ("PHONE_REMOTE_BACKEND", "direct"),
                ("PHONE_REMOTE_UDID", ctx.udid.as_str()),
                ("PHONE_REMOTE_WDA_URL", target_url),
                ("PHONE_REMOTE_WDA_MJPEG_URL", mjpeg_url.as_str()),
                ("PHONE_REMOTE_WDA_MANAGED", "true"),
                ("WDA_ALLOW_LAN", ctx.allow_lan.as_str()),
            ] {
                plist_set_env(&staged, key, value);
            }
            if want_dir.is_empty() {
                plist_buddy(&staged, "Delete :EnvironmentVariables:DEVELOPER_DIR");
            } else {
                plist_set_env(&staged, "DEVELOPER_DIR", &want_dir);
            }
            ok(&format!(
                "daemon plist set to managed direct + fixed device + runner control/video endpoints (changed: {})",
                changed.join(" ")
            ));
        } else {
            ok("daemon plist already has the managed direct + fixed device + runner endpoint configuration");
        }
        if !plist_lints(&staged) {
            return die("staged daemon LaunchAgent plist is invalid after configuration");
        }
        if config_changed {
            self.daemon.touched = true;
            if std::fs::rename(&staged, &ctx.daemon_plist).is_err() {
                return die("could not atomically install the configured daemon plist");
            }
        } else {
            let _ = std::fs::remove_file(&staged);
        }
        self.daemon.staged = None;
        if needs_restart || !launchd::loaded(&ctx, &label) {
            self.daemon.touched = true;
            launchd::bootout(&ctx, &label);
            if !launchd::wait_gone(&ctx, &label) {
                return die("daemon LaunchAgent did not finish stopping");
            }
            launchd::enable(&ctx, &label);
            if !launchd::bootstrap(&ctx, &ctx.daemon_plist) {
                return die("the device runner is reachable, but the daemon LaunchAgent could not be bootstrapped");
            }
        }
        let loaded = launchd::loaded(&ctx, &label);
        if loaded {
            ok("daemon LaunchAgent job loaded");
        } else {
            warn("daemon LaunchAgent loaded state could not be verified");
        }
        let mut port = sys::plist_env(&ctx.daemon_plist, "PHONE_REMOTE_PORT");
        if port.is_empty() {
            port = "44321".into();
        }
        let mut http_ready = false;
        if loaded {
            for _ in 0..10 {
                if sys::http_get(&format!("http://127.0.0.1:{port}/"), Duration::from_secs(2))
                    .is_some()
                {
                    http_ready = true;
                    break;
                }
                proc::sleep(Duration::from_millis(500))?;
            }
            if http_ready {
                ok(&format!(
                    "daemon HTTP endpoint verified on 127.0.0.1:{port}"
                ));
            } else {
                warn("daemon job is loaded, but its HTTP endpoint is not verified; check its error log");
            }
        }
        Ok(DaemonEndpoint { port, http_ready })
    }

    /// Under launchd this run IS the supervisor: verify it is loaded and owns
    /// the runner and both relays.
    fn verify_supervision(&mut self, target_url: &str) -> Step {
        let ctx = &self.ctx;
        let wda_port = valid_port(&ctx.wda_port).unwrap_or(8100);
        let mjpeg_port = valid_port(&ctx.mjpeg_port).unwrap_or(9100);
        let verified = launchd::loaded(ctx, &ctx.instance.wda_label)
            && pid::validate(
                ctx,
                &ctx.runner_pid_file,
                &self.legacy.runner,
                Role::Runner,
                false,
            )
            .is_some()
            && pid::verify_loopback_listener(
                ctx,
                &ctx.relay_pid_file,
                &self.legacy.relay,
                Role::Relay,
                wda_port,
            )
            && pid::verify_loopback_listener(
                ctx,
                &ctx.mjpeg_relay_pid_file,
                &self.legacy.mjpeg,
                Role::Mjpeg,
                mjpeg_port,
            )
            && sys::http_ok(&format!("{target_url}/status"), Duration::from_secs(4));
        if !verified {
            self.phase(
                "supervisor-fail",
                "wda",
                "the device runner is reachable but launchd supervision is unverified",
            );
            return die("the device runner endpoint is up, but dedicated launchd supervision could not be verified");
        }
        Ok(())
    }

    /// Poll the daemon's own `/agent/status` until it reaches the runner.
    /// `wda:true` is the setup verdict; `drivable:true` (unlocked, automation
    /// granted) is only waited for briefly — a locked phone is a runtime hint,
    /// never a reason to tear a healthy runner down.
    fn verify_product(&mut self, daemon: &DaemonEndpoint) -> Step {
        if !daemon.http_ready {
            return Ok(());
        }
        let env_number = |key: &str, default: u32| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(default)
        };
        let max_tries = env_number("DAEMON_STATUS_MAX_TRIES", 120);
        let grace = env_number("DAEMON_REACHABLE_GRACE_TRIES", 20);
        let mut token = sys::plist_env(&self.ctx.daemon_plist, "PHONE_REMOTE_AGENT_TOKEN");
        if token.is_empty() {
            token = sys::plist_env(&self.ctx.daemon_plist, "PHONE_REMOTE_PASSWORD");
        }
        let url = format!("http://127.0.0.1:{}/agent/status", daemon.port);
        let mut verdict = Verdict::Down;
        let mut status = serde_json::Value::Null;
        let mut reachable_tries = 0;
        let mut ready = false;
        for _ in 0..max_tries {
            let body = if token.is_empty() {
                sys::http_get(&url, Duration::from_secs(2))
            } else {
                sys::http_get_auth(&url, Duration::from_secs(2), &token)
            };
            status = body
                .and_then(|(_, body)| serde_json::from_slice(&body).ok())
                .unwrap_or(serde_json::Value::Null);
            verdict = Verdict::of(&status);
            if verdict == Verdict::Drivable {
                ready = true;
                break;
            }
            if verdict == Verdict::Reachable {
                reachable_tries += 1;
                if reachable_tries >= grace {
                    ready = true;
                    break;
                }
            }
            proc::sleep(Duration::from_millis(500))?;
        }
        token.clear();
        let locked = status.get("wda_locked") == Some(&serde_json::Value::Bool(true));
        if !ready {
            self.phase(
                "daemon-fail",
                "wda",
                "daemon never reached the device runner after a verified handoff",
            );
            return die(format!(
                "the device runner, relays, and launchd supervision are verified, but the daemon did not report wda=true within {}s.\n   Inspect: ~/Library/Logs/iPhoneUse/iphone-use.err",
                max_tries / 2
            ));
        }
        if verdict == Verdict::Drivable {
            ok("daemon product status verified: drivable=true");
        } else if locked {
            ok("daemon product status verified: device runner reachable through the relays");
            warn("the iPhone is locked — unlock it once; the daemon keeps probing and reports drivable=true as soon as the runner can act");
        } else {
            ok("daemon product status verified: device runner reachable through the relays");
            warn("the device runner answers but cannot act yet (drivable=false) — keep the iPhone unlocked and awake; the daemon keeps probing");
        }
        Ok(())
    }

    fn summary(&self, url: &str, target_url: &str, daemon: &DaemonEndpoint, source_hash: &str) {
        let ctx = &self.ctx;
        println!("\n{BOLD}━━━ Device layer verified ━━━{RST}");
        println!("  Runner    : {url} (on-phone), {target_url} (verified relay)");
        println!(
            "  Supervisor: {} (job, runner, and /status verified)",
            launchd::service(ctx, &ctx.instance.wda_label)
        );
        println!(
            "  Video     : http://127.0.0.1:{} (startup stream + relay ownership verified)",
            ctx.mjpeg_port
        );
        if daemon.http_ready {
            println!("  Daemon    : http://127.0.0.1:{} (verified)", daemon.port);
        } else {
            println!(
                "  Daemon    : not HTTP-verified; inspect ~/Library/Logs/iPhoneUse/iphone-use.err"
            );
        }
        println!(
            "  Try       : curl -H \"Authorization: Bearer $PW\" http://127.0.0.1:{}/agent/elements",
            daemon.port
        );
        println!("  Stop      : {} stop", ctx.self_install.display());
        println!(
            "  Pause     : {} pause  (give the phone back without auto-restart)",
            ctx.self_install.display()
        );
        println!("  Resume    : {} resume", ctx.self_install.display());
        println!(
            "  Source    : {} (sha256 {})",
            ctx.runner_src.display(),
            &source_hash[..12.min(source_hash.len())]
        );
        println!("  Signing   : free Apple ID profiles may expire after 7 days; re-run setup when needed.");
        super::term::flush();
    }

    // ── KeepAlive: hold while healthy ───────────────────────────────────────

    fn endpoint_locked(&self) -> bool {
        sys::http_get(
            &format!("http://127.0.0.1:{}/wda/locked", self.ctx.wda_port),
            Duration::from_secs(3),
        )
        .filter(|(status, _)| *status < 400)
        .and_then(|(_, body)| serde_json::from_slice::<serde_json::Value>(&body).ok())
        .is_some_and(|value| value.get("value") == Some(&serde_json::Value::Bool(true)))
    }

    /// The installed runner's profile is a free-account one due for renewal,
    /// no session holds the phone, and the phone is unlocked (a locked phone
    /// cannot launch the rebuilt runner, so it would only trade a working
    /// runner for a wait).
    fn free_profile_renewal_due(&self) -> bool {
        let app = self.ctx.runner_products_dir.join(RUNNER_APP_NAME);
        runner::free_profile_due(&app, retry::now())
            && owner::current(&self.ctx).is_none()
            && !self.endpoint_locked()
    }

    /// Stay alive while the runner and both relays do; launchd sees the exit
    /// and rebuilds. A single unanswered `/status` cannot tell a busy runner
    /// from a dead one, so three in a row (~32 s) count; a dead process or a
    /// vanished listener ends the hold at once.
    pub fn hold(&mut self) -> Step {
        if !self.ctx.keepalive {
            return Ok(());
        }
        info("KeepAlive mode: holding while the PID-verified runner and relays stay healthy");
        const MAX_FAILURES: u32 = ProbeCount::MAX;
        let wda_port = valid_port(&self.ctx.wda_port).unwrap_or(8100);
        let mjpeg_port = valid_port(&self.ctx.mjpeg_port).unwrap_or(9100);
        let status_url = format!("http://127.0.0.1:{wda_port}/status");
        let mut failures = 0;
        let mut probes = ProbeCount::default();
        let mut relay_restarts: Vec<Instant> = Vec::new();
        let mut warned_owner = false;
        // A free Apple ID's runner profile lasts 7 days. Replace the runner
        // while the phone is free and unlocked, before it lapses, instead of
        // leaving the next reconnect to find it expired.
        const RENEW_CHECK: Duration = Duration::from_secs(30 * 60);
        let mut renew_checked = Instant::now();
        let cause = loop {
            if renew_checked.elapsed() >= RENEW_CHECK {
                renew_checked = Instant::now();
                if self.free_profile_renewal_due() {
                    break "renew";
                }
            }
            if pid::validate(
                &self.ctx,
                &self.ctx.runner_pid_file,
                &self.legacy.runner,
                Role::Runner,
                false,
            )
            .is_none()
            {
                break "runner";
            }
            if !pid::verify_loopback_listener(
                &self.ctx,
                &self.ctx.relay_pid_file,
                &self.legacy.relay,
                Role::Relay,
                wda_port,
            ) || !pid::verify_loopback_listener(
                &self.ctx,
                &self.ctx.mjpeg_relay_pid_file,
                &self.legacy.mjpeg,
                Role::Mjpeg,
                mjpeg_port,
            ) {
                // The runner is alive: rebuild only the relays, so whoever is
                // driving the phone keeps its runner and test session. At most
                // three times in ten minutes; past that, a full rebuild.
                relay_restarts.retain(|at| at.elapsed() < Duration::from_secs(600));
                if relay_restarts.len() >= 3 || self.phone_url.is_empty() {
                    break "relay";
                }
                relay_restarts.push(Instant::now());
                warn("a runner relay stopped listening while the runner stayed alive — restarting the relays only");
                let url = self.phone_url.clone();
                self.relays(&url)?;
                ok("relays restored; the runner kept running");
                failures = 0;
                probes = ProbeCount::default();
                continue;
            }
            let answered = sys::http_ok(&status_url, Duration::from_secs(4));
            let rebuild = probes.observe(answered);
            failures = probes.failures;
            if !answered {
                if rebuild {
                    // A slow runner under another session's lease is that
                    // session's to wait for, not ours to replace.
                    if let Some(lease) =
                        owner::foreign(owner::current(&self.ctx), owner::caller().as_deref())
                    {
                        if !warned_owner {
                            warn(&format!(
                                "the device runner did not answer /status {failures} times in a row, but session \"{}\" holds the phone; not replacing the runner it is using",
                                lease.owner
                            ));
                            warned_owner = true;
                        }
                        failures = 0;
                        probes = ProbeCount::default();
                        proc::sleep(Duration::from_secs(10))?;
                        continue;
                    }
                    break "unreachable";
                }
                info(&format!(
                    "the device runner did not answer /status within 4s ({failures}/{MAX_FAILURES}); the runner and relays are alive, so holding"
                ));
            }
            proc::sleep(Duration::from_secs(10))?;
        };
        // An unplugged phone takes its runner down with it. Nothing can be
        // rebuilt until it is back, so say so and wait instead.
        if checks::presence(&self.ctx.udid) == checks::Presence::Absent {
            warn(&format!(
                "the device runner went away with the iPhone ({cause}); waiting for the phone to come back"
            ));
            self.phase("waiting", "not_connected", NOT_CONNECTED_MESSAGE);
            self.failure_kind = Kind::NotConnected;
            for (file, legacy, role) in [
                (&self.ctx.mjpeg_relay_pid_file, &self.legacy.mjpeg, Role::Mjpeg),
                (&self.ctx.relay_pid_file, &self.legacy.relay, Role::Relay),
                (&self.ctx.runner_pid_file, &self.legacy.runner, Role::Runner),
            ] {
                pid::stop(&self.ctx, file, legacy, role);
            }
            return Err(Exit(1));
        }
        if runner::log_shows_lock(&self.ctx.run_log) || self.endpoint_locked() {
            return self.locked_retry();
        }
        match cause {
            "unreachable" => {
                warn(&format!(
                    "the device runner did not answer /status {failures} times in a row while the runner and both relays stayed alive — rebuilding"
                ));
                self.phase(
                    "building",
                    "",
                    &format!("the device runner did not answer {failures} consecutive /status probes — rebuilding"),
                );
            }
            "relay" => warn(
                "the runner relay stopped listening — exiting so launchd KeepAlive rebuilds it",
            ),
            "renew" => {
                info(&format!(
                    "the runner's free Apple ID provisioning profile expires within {}h; the phone is free and unlocked, so rebuilding it with a fresh profile",
                    runner::PROFILE_RENEW_SECS / 3600
                ));
                self.phase(
                    "building",
                    "",
                    "renewing the runner's 7-day provisioning profile",
                );
            }
            _ => warn("the device runner exited — exiting so launchd KeepAlive rebuilds it"),
        }
        pid::stop(
            &self.ctx,
            &self.ctx.mjpeg_relay_pid_file,
            &self.legacy.mjpeg,
            Role::Mjpeg,
        );
        pid::stop(
            &self.ctx,
            &self.ctx.relay_pid_file,
            &self.legacy.relay,
            Role::Relay,
        );
        pid::stop(
            &self.ctx,
            &self.ctx.runner_pid_file,
            &self.legacy.runner,
            Role::Runner,
        );
        Err(Exit(1))
    }
}

pub enum RelayTool {
    Native(PathBuf),
    Iproxy(PathBuf),
    Socat(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The phone can act now.
    Drivable,
    /// The daemon reaches the runner; the phone cannot act yet.
    Reachable,
    Down,
}

impl Verdict {
    fn of(status: &serde_json::Value) -> Verdict {
        if status.get("drivable") == Some(&serde_json::Value::Bool(true)) {
            Verdict::Drivable
        } else if status.get("wda") == Some(&serde_json::Value::Bool(true)) {
            Verdict::Reachable
        } else {
            Verdict::Down
        }
    }
}

/// `http://host:port/` → (host, port text).
pub fn split_host_port(url: &str) -> (String, String) {
    let hostport = url.trim_start_matches("http://").trim_end_matches('/');
    let host = hostport.split(':').next().unwrap_or("").to_string();
    let port = hostport.rsplit(':').next().unwrap_or("").to_string();
    (host, port)
}

fn copy_preserving(from: &Path, to: &Path) -> bool {
    std::process::Command::new("cp")
        .arg("-p")
        .arg(from)
        .arg(to)
        .status()
        .is_ok_and(|status| status.success())
}

fn plist_buddy(plist: &Path, command: &str) -> bool {
    std::process::Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", command])
        .arg(plist)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn plist_set_env(plist: &Path, key: &str, value: &str) {
    if !plist_buddy(plist, &format!("Set :EnvironmentVariables:{key} {value}")) {
        plist_buddy(
            plist,
            &format!("Add :EnvironmentVariables:{key} string {value}"),
        );
    }
}

pub fn plist_lints(plist: &Path) -> bool {
    std::process::Command::new("plutil")
        .arg("-lint")
        .arg(plist)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Put a backup back over `target` (copy, mode, rename, compare).
pub fn restore_backup(backup: &Path, target: &Path, mode: u32) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    if !backup.is_file() {
        return false;
    }
    let temporary = PathBuf::from(format!(
        "{}.restore.{}",
        target.display(),
        std::process::id()
    ));
    let restored = copy_preserving(backup, &temporary)
        && std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode)).is_ok()
        && std::fs::rename(&temporary, target).is_ok()
        && std::fs::read(backup).ok() == std::fs::read(target).ok();
    if !restored {
        let _ = std::fs::remove_file(&temporary);
    }
    restored
}

// ── pure decisions (unit-tested below) ──────────────────────────────────────

/// `WDA_LOCK_WAIT_SECS`: whole seconds; anything else falls back to 300 with
/// a complaint instead of breaking the comparison.
pub fn lock_wait_limit(raw: Option<&str>) -> (u64, Option<String>) {
    match raw {
        None => (300, None),
        Some(value) if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
            (value.parse().unwrap_or(300), None)
        }
        Some(value) => (
            300,
            Some(format!(
                "WDA_LOCK_WAIT_SECS='{value}' is not a whole number of seconds; using 300"
            )),
        ),
    }
}

/// The pre-launch lock wait: unlocked only after two explicit "no passcode
/// required" readings in a row. A failed read proves neither lock nor unlock
/// (on hardware, failed reads during a lock launched into "Unlock iPhone to
/// Continue").
#[derive(Debug, Default)]
pub struct UnlockWait {
    unlocked_reads: u32,
}

impl UnlockWait {
    /// Feed one reading (`Some(true)` = passcode required); `true` = unlocked.
    pub fn observe(&mut self, passcode_required: Option<bool>) -> bool {
        match passcode_required {
            Some(false) => self.unlocked_reads += 1,
            Some(true) => self.unlocked_reads = 0,
            None => {}
        }
        self.unlocked_reads >= 2
    }
}

/// #126: only when the SDK's major.minor is older than the phone's does a
/// code-74 refusal mean the Xcode is too old.
/// Which blocker an IDE-channel refusal (code 74, supported iOS) is: over
/// Wi-Fi iOS never shows the authorization prompt, so it is not something a
/// person can allow on the phone. An unknown transport keeps the USB reading.
pub fn ide_refusal_blocker(transport: checks::Transport) -> &'static str {
    match transport {
        checks::Transport::Network => "wifi_automation_refused",
        checks::Transport::Usb | checks::Transport::Unknown => "automation_not_allowed",
    }
}

fn waited(wait: Option<u64>) -> String {
    wait.map(|secs| format!(" after waiting {secs} s"))
        .unwrap_or_default()
}

pub fn wifi_automation_message(wait: Option<u64>) -> String {
    format!(
        "over Wi-Fi the iPhone did not authorize the device runner's UI-automation session (runner exit code 74, IDE channel refused{}) — iOS cannot show the passcode prompt over Wi-Fi, and some iOS versions refuse it entirely. Connect the iPhone by USB and enter the passcode when it asks; if it still fails over Wi-Fi afterwards, keep this phone on USB",
        waited(wait)
    )
}

pub fn automation_message(wait: Option<u64>) -> String {
    format!(
        "the iPhone did not authorize the device runner's UI-automation session (runner exit code 74, IDE channel refused{}) although this Xcode supports its iOS — a passcode or Allow prompt appears on the iPhone while the runner starts and times out after about 30 s; unlock the iPhone, check Settings › Developer › Enable UI Automation, and answer that prompt during the next attempt",
        waited(wait)
    )
}

pub fn xcode_too_old_message(sdk: Option<&str>, device: Option<&str>) -> Option<String> {
    let sdk = checks::os_major_minor(sdk?);
    let device = checks::os_major_minor(device?);
    checks::version_lt(&sdk, &device).then(|| {
        format!(
            "iPhone runs iOS {device} but this Xcode's SDK is iOS {sdk} — install an Xcode that supports iOS {device} (a beta Xcode for a beta iOS)"
        )
    })
}

/// The holding loop's patience: three unanswered `/status` probes in a row
/// (~32 s) before a rebuild; one answer resets it.
#[derive(Debug, Default)]
pub struct ProbeCount {
    pub failures: u32,
}

impl ProbeCount {
    pub const MAX: u32 = 3;

    /// `true` once the runner has missed [`Self::MAX`] probes in a row.
    pub fn observe(&mut self, answered: bool) -> bool {
        if answered {
            self.failures = 0;
        } else {
            self.failures += 1;
        }
        self.failures >= Self::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_port_from_the_runner_url() {
        assert_eq!(
            split_host_port("http://192.168.0.51:8100"),
            ("192.168.0.51".into(), "8100".into())
        );
        assert_eq!(
            split_host_port("http://127.0.0.1:8100/"),
            ("127.0.0.1".into(), "8100".into())
        );
    }

    #[test]
    fn supervisor_plist_values_are_escaped() {
        assert_eq!(xml_escape("/a&b/<x>.p8"), "/a&amp;b/&lt;x&gt;.p8");
    }

    #[test]
    fn lock_wait_limits() {
        assert_eq!(lock_wait_limit(None), (300, None));
        assert_eq!(lock_wait_limit(Some("45")), (45, None));
        let (limit, complaint) = lock_wait_limit(Some("five"));
        assert_eq!(limit, 300);
        assert!(complaint.unwrap().contains("not a whole number"));
        assert_eq!(lock_wait_limit(Some("")).0, 300);
    }

    #[test]
    fn unlocking_needs_two_explicit_readings_in_a_row() {
        let mut wait = UnlockWait::default();
        assert!(!wait.observe(Some(false)));
        assert!(wait.observe(Some(false)), "two unlocked readings in a row");
        let mut wait = UnlockWait::default();
        assert!(!wait.observe(Some(false)));
        assert!(!wait.observe(Some(true)), "a locked reading starts over");
        assert!(!wait.observe(Some(false)));
        assert!(!wait.observe(None), "an unreadable state is not an unlock");
        assert!(!wait.observe(None));
        assert!(wait.observe(Some(false)));
        let mut wait = UnlockWait::default();
        for _ in 0..10 {
            assert!(!wait.observe(None), "unreadable never counts as unlocked");
        }
    }

    #[test]
    fn a_wifi_refusal_is_its_own_blocker_and_names_usb() {
        assert_eq!(
            ide_refusal_blocker(checks::Transport::Network),
            "wifi_automation_refused"
        );
        assert_eq!(
            ide_refusal_blocker(checks::Transport::Usb),
            "automation_not_allowed"
        );
        assert_eq!(
            ide_refusal_blocker(checks::Transport::Unknown),
            "automation_not_allowed",
            "without evidence of Wi-Fi, keep the on-phone reading"
        );
        let wifi = wifi_automation_message(Some(30));
        assert!(wifi.contains("over Wi-Fi"), "{wifi}");
        assert!(wifi.contains("by USB"), "{wifi}");
        assert!(wifi.contains("after waiting 30 s"), "{wifi}");
        let usb = automation_message(None);
        assert!(usb.contains("about 30 s"), "{usb}");
        assert!(!usb.contains("after waiting"), "{usb}");
        for text in [wifi, usb, automation_message(Some(2))] {
            assert!(!text.contains("--"), "never a bypass flag: {text}");
        }
    }

    #[test]
    fn xcode_is_too_old_only_when_the_phone_is_newer() {
        assert!(xcode_too_old_message(Some("27.0"), Some("27.2"))
            .unwrap()
            .contains("iOS 27.2"));
        assert!(xcode_too_old_message(Some("27.0"), Some("28.0")).is_some());
        assert!(
            xcode_too_old_message(Some("27.2"), Some("27.2")).is_none(),
            "matching: automation_not_allowed"
        );
        assert!(
            xcode_too_old_message(Some("27.0"), Some("27.0.1")).is_none(),
            "a patch gap is not too old"
        );
        assert!(xcode_too_old_message(Some("27.2"), Some("27.0")).is_none());
        assert!(
            xcode_too_old_message(None, Some("27.2")).is_none(),
            "unknown versions never fail"
        );
        assert!(xcode_too_old_message(Some("27.0"), None).is_none());
    }

    #[test]
    fn the_hold_rebuilds_after_three_missed_probes_in_a_row() {
        let mut probes = ProbeCount::default();
        assert!(!probes.observe(false));
        assert!(!probes.observe(false));
        assert!(!probes.observe(true), "an answer resets the count");
        assert!(!probes.observe(false));
        assert!(!probes.observe(false));
        assert!(probes.observe(false));
    }

    #[test]
    fn verdicts() {
        assert_eq!(
            Verdict::of(&serde_json::json!({"drivable": true, "wda": true})),
            Verdict::Drivable
        );
        assert_eq!(
            Verdict::of(&serde_json::json!({"drivable": false, "wda": true, "managed_wda": true})),
            Verdict::Reachable
        );
        assert_eq!(
            Verdict::of(&serde_json::json!({"managed_wda": true, "wda_actionable": true})),
            Verdict::Down
        );
        assert_eq!(Verdict::of(&serde_json::Value::Null), Verdict::Down);
    }
}

// ── interactive setup (a person ran `iphone-use setup`) ───────────────────────

/// The runner supervisor as it was before an interactive setup took the
/// lifecycle over, so a failed setup can put it back exactly.
#[derive(Default)]
pub struct SupervisorTx {
    active: bool,
    prev_loaded: bool,
    prev_present: bool,
    prev_disabled: bool,
    rollback: PathBuf,
    staged: Option<PathBuf>,
}

/// The fixed copy of the setup script the supervisor runs, replaced by a
/// setup started from somewhere else (a checkout, a fresh install).
#[derive(Default)]
pub struct SelfInstallTx {
    replaced: bool,
    had_previous: bool,
    rollback: PathBuf,
}

impl Setup {
    /// An interactive setup: build and prove the runner, then hand it to the
    /// dedicated launchd supervisor and verify the hand-off.
    pub fn run_interactive(mut self) -> i32 {
        proc::install_signal_handlers();
        if let Err(code) = self.install_self() {
            return code;
        }
        let code = match self.interactive() {
            Ok(()) => 0,
            Err(Exit(code)) => code,
        };
        self.cleanup(code)
    }

    fn interactive(&mut self) -> Step {
        // Another session's lease: this setup would replace its runner.
        if owner::check(&self.ctx) != 0 {
            return Err(Exit(1));
        }
        // Before anything is paused or built, so stopping here undoes nothing.
        if sys::stdout_is_tty() && !self.first_run_checklist() {
            return die(format!(
                "fix the ✗ items above, then run: {}",
                self.ctx.rerun_command()
            ));
        }
        if let Err(code) = self.begin_status() {
            return Err(Exit(code));
        }
        self.pause_supervisor()?;
        self.body()
    }

    /// `setup-wda.sh` keeps a copy at a fixed path so the daemon can start and
    /// stop the runner without knowing where the repo lives. Only a setup run
    /// from somewhere else replaces it; the prior copy is kept for rollback.
    fn install_self(&mut self) -> Result<(), i32> {
        let target = self.ctx.self_install.clone();
        self.self_install.rollback = self
            .ctx
            .state_dir()
            .join(format!("setup-wda.rollback.{}.sh", std::process::id()));
        let Some(script) = self.ctx.script.clone() else {
            return Ok(());
        };
        let same = script.canonicalize().ok() == target.canonicalize().ok() && target.exists();
        if same {
            return Ok(());
        }
        let _ = std::fs::create_dir_all(self.ctx.state_dir());
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(
                self.ctx.state_dir(),
                std::fs::Permissions::from_mode(0o700),
            );
        }
        if target.is_file() {
            if !copy_preserving(&target, &self.self_install.rollback) {
                eprintln!("could not back up existing setup-wda.sh");
                return Err(1);
            }
            self.self_install.had_previous = true;
        }
        match std::fs::read(&script) {
            Ok(bytes) if sys::write_atomic(&target, &bytes, 0o700).is_ok() => {
                self.self_install.replaced = true;
                Ok(())
            }
            _ => {
                eprintln!(
                    "could not atomically install setup-wda.sh at {}",
                    target.display()
                );
                Err(1)
            }
        }
    }

    /// What a person has to do by hand before the first build, one plain fix
    /// per missing item.
    fn first_run_checklist(&self) -> bool {
        use super::term::checklist_line;
        let ctx = &self.ctx;
        let mut missing = false;
        println!("\n{BOLD}Before the first build{RST}");
        if checks::xcode_version().is_empty() {
            checklist_line(
                false,
                "Xcode is installed",
                &format!("get it from the App Store ({XCODE_APP_STORE_URL}) and open it once"),
            );
            missing = true;
        } else {
            checklist_line(true, "Xcode is installed", "");
        }
        // Any source setup itself would sign with counts: an explicit or
        // persisted team, an App Store Connect key, the team last picked in
        // Xcode, or a signed-in account.
        let account = !ctx.team_id.is_empty()
            || ctx.asc_signing_enabled()
            || !sys::plist_env(&ctx.wda_agent_plist, "WDA_TEAM_ID").is_empty()
            || !sys::defaults_read(
                "com.apple.dt.Xcode",
                "IDEProvisioningTeamManagerLastSelectedTeamID",
            )
            .is_empty()
            || !checks::xcode_account_teams().is_empty();
        checklist_line(
            account,
            "Xcode is signed in to an Apple account",
            "Xcode → Settings → Accounts → + → Apple ID (a free one works)",
        );
        missing |= !account;
        if !ctx.lan() {
            let usb = checks::usb_udids();
            if usb.is_empty() {
                checklist_line(
                    false,
                    "iPhone connected over USB",
                    "plug it in with a cable, unlock it, and tap Trust",
                );
                missing = true;
            } else {
                checklist_line(true, "iPhone connected over USB", "");
                if usb.len() == 1 {
                    match checks::developer_mode_status(&usb[0]).as_str() {
                        "enabled" => checklist_line(true, "Developer Mode is on", ""),
                        "disabled" => {
                            checklist_line(
                                false,
                                "Developer Mode is on",
                                "on the iPhone: Settings → Privacy & Security → Developer Mode → On, then let it restart",
                            );
                            missing = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        println!("  Keep the iPhone unlocked and awake until setup finishes.\n");
        if missing && !account {
            checks::open_xcode_for_account(ctx);
        }
        !missing
    }

    /// A manual setup owns the lifecycle while it runs, so an already-running
    /// supervisor cannot race its build or relays. Its plist, loaded state and
    /// enable policy are saved first.
    fn pause_supervisor(&mut self) -> Step {
        let ctx = self.ctx.clone();
        let label = ctx.instance.wda_label.clone();
        self.sup.active = true;
        self.sup.rollback = ctx.state_dir().join(format!(
            "wda-supervisor.rollback.{}.plist",
            std::process::id()
        ));
        let Some(disabled) = launchd::disabled_state(&ctx, &label) else {
            return die("could not snapshot the runner supervisor's launchd disabled policy");
        };
        self.sup.prev_disabled = disabled;
        if ctx.wda_agent_plist.is_file() {
            self.sup.prev_present = true;
            if !copy_preserving(&ctx.wda_agent_plist, &self.sup.rollback) {
                return die("could not save the existing runner supervisor plist for rollback");
            }
        }
        if launchd::loaded(&ctx, &label) {
            if !self.sup.prev_present {
                return die("runner supervisor is loaded but its plist is missing; refusing an unrecoverable handoff");
            }
            info("Pausing the existing runner supervisor for interactive setup");
            self.sup.prev_loaded = true;
            launchd::bootout(&ctx, &label);
            if !launchd::wait_gone(&ctx, &label) {
                return die("existing runner supervisor did not stop; refusing to race it");
            }
        }
        Ok(())
    }

    /// Write and bootstrap the supervisor plist: launchd runs the fixed copy
    /// of the setup script with `WDA_KEEPALIVE=1` and this run's settings.
    fn install_supervisor(&mut self) -> bool {
        let ctx = self.ctx.clone();
        if !sys::is_executable(&ctx.self_install) {
            warn(&format!(
                "fixed setup script is missing or not executable: {}",
                ctx.self_install.display()
            ));
            return false;
        }
        let mut env: Vec<(&str, String)> = Vec::new();
        if !ctx.instance.is_default() {
            env.push(("PHONE_REMOTE_INSTANCE", ctx.instance.name.clone()));
        }
        // The key order is the script's, so a plist reads the same either way.
        let mut pairs: Vec<(&str, String)> = vec![
            ("WDA_KEEPALIVE", "1".into()),
            ("PATH", super::ctx::SUPERVISOR_PATH.into()),
            ("WDA_UDID", ctx.udid.clone()),
            ("WDA_TEAM_ID", ctx.team_id.clone()),
            ("WDA_BUNDLE_ID", ctx.bundle_id.clone()),
        ];
        if ctx.runner_src != ctx.runner_default_src {
            pairs.push((
                "IPU_RUNNER_SRC",
                ctx.runner_src.to_string_lossy().into_owned(),
            ));
        }
        pairs.push(("WDA_PORT", ctx.wda_port.clone()));
        pairs.push(("MJPEG_PORT", ctx.mjpeg_port.clone()));
        pairs.push(("WDA_ALLOW_LAN", ctx.allow_lan.clone()));
        if let Ok(icon) = std::env::var("WDA_RUNNER_ICON") {
            if !icon.is_empty() && icon != "auto" {
                pairs.push(("WDA_RUNNER_ICON", icon));
            }
        }
        if ctx.asc_signing_enabled() {
            pairs.push(("WDA_ASC_KEY_PATH", ctx.asc_key_path.clone()));
            pairs.push(("WDA_ASC_KEY_ID", ctx.asc_key_id.clone()));
            pairs.push(("WDA_ASC_ISSUER_ID", ctx.asc_issuer_id.clone()));
        }
        pairs.extend(env);
        if let Some(dir) = &ctx.developer_dir {
            pairs.push(("DEVELOPER_DIR", dir.to_string_lossy().into_owned()));
        }
        if let Ok(dir) = std::env::var("PHONE_REMOTE_STATE_DIR") {
            pairs.push(("PHONE_REMOTE_STATE_DIR", dir));
        }
        let env_block: String = pairs
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(key, value)| {
                format!(
                    "        <key>{key}</key><string>{}</string>\n",
                    xml_escape(value)
                )
            })
            .collect();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array><string>/bin/bash</string><string>{setup}</string></array>
    <key>EnvironmentVariables</key>
    <dict>
{env_block}    </dict>
    <key>KeepAlive</key><true/>
    <!-- The script persists a 5s→10s→…300s retry schedule. Keep launchd's
         own floor at 5s so it does not flatten the first retry steps. -->
    <key>ThrottleInterval</key><integer>5</integer>
    <key>RunAtLoad</key><true/>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#,
            label = ctx.instance.wda_label,
            setup = xml_escape(&ctx.self_install.to_string_lossy()),
            log = xml_escape(&ctx.wda_agent_log.to_string_lossy()),
        );
        let _ = std::fs::create_dir_all(ctx.home.join("Library/LaunchAgents"));
        let staged = PathBuf::from(format!(
            "{}.install.{}",
            ctx.wda_agent_plist.display(),
            std::process::id()
        ));
        self.sup.staged = Some(staged.clone());
        if sys::write_atomic(&staged, plist.as_bytes(), 0o600).is_err() {
            warn("could not stage the runner supervisor plist");
            return false;
        }
        if !plist_lints(&staged) {
            warn("generated runner supervisor plist is invalid");
            return false;
        }
        if std::fs::rename(&staged, &ctx.wda_agent_plist).is_err() {
            warn("could not atomically install the runner supervisor plist");
            return false;
        }
        self.sup.staged = None;
        let label = ctx.instance.wda_label.clone();
        launchd::bootout(&ctx, &label);
        if !launchd::wait_gone(&ctx, &label) {
            warn("old runner supervisor did not finish stopping");
            return false;
        }
        // A prior installer may have disabled the label persistently.
        launchd::enable(&ctx, &label);
        if !launchd::bootstrap(&ctx, &ctx.wda_agent_plist) {
            warn(&format!(
                "could not bootstrap the runner supervisor: {}",
                ctx.wda_agent_plist.display()
            ));
            return false;
        }
        if launchd::loaded(&ctx, &label) {
            ok(&format!(
                "runner supervisor job loaded: {}",
                launchd::service(&ctx, &label)
            ));
            true
        } else {
            warn("launchctl accepted the runner plist but the job is not visible");
            false
        }
    }

    /// Hand the proven runner and relays to the supervisor and verify that a
    /// fresh supervisor-owned runner replaced this run's, with both relays
    /// and `/status`, within two minutes.
    fn handoff(&mut self, target_url: &str) -> Step {
        let ctx = self.ctx.clone();
        let Some(record) = pid::validate(
            &ctx,
            &ctx.runner_pid_file,
            &self.legacy.runner,
            Role::Runner,
            false,
        )
        .and_then(|_| pid::parse(&ctx.runner_pid_file, &self.legacy.runner)) else {
            return die("interactive runner identity was lost before launchd handoff");
        };
        let old_id = (record.pid, record.lstart.clone());
        self.phase(
            "supervisor",
            "",
            "handing the device runner to its launchd supervisor",
        );
        info("Handing the verified runner setup to its dedicated launchd supervisor");
        if !self.install_supervisor() {
            return die("the device runner is reachable now, but its launchd supervisor could not be installed");
        }
        let wda_port = valid_port(&ctx.wda_port).unwrap_or(8100);
        let mjpeg_port = valid_port(&ctx.mjpeg_port).unwrap_or(9100);
        for _ in 0..60 {
            let new_id = pid::validate(
                &ctx,
                &ctx.runner_pid_file,
                &self.legacy.runner,
                Role::Runner,
                false,
            )
            .and_then(|_| pid::parse(&ctx.runner_pid_file, &self.legacy.runner))
            .map(|record| (record.pid, record.lstart));
            let replaced = new_id
                .as_ref()
                .is_some_and(|id| *id != old_id && !id.1.is_empty());
            if replaced
                && launchd::loaded(&ctx, &ctx.instance.wda_label)
                && pid::verify_loopback_listener(
                    &ctx,
                    &ctx.relay_pid_file,
                    &self.legacy.relay,
                    Role::Relay,
                    wda_port,
                )
                && pid::verify_loopback_listener(
                    &ctx,
                    &ctx.mjpeg_relay_pid_file,
                    &self.legacy.mjpeg,
                    Role::Mjpeg,
                    mjpeg_port,
                )
                && status::phase_is(&ctx.status_file, "ready")
                && sys::http_ok(&format!("{target_url}/status"), Duration::from_secs(4))
            {
                ok("launchd replacement verified: runner identity, both loopback relays, and runner /status");
                return Ok(());
            }
            proc::sleep(Duration::from_secs(2))?;
        }
        self.phase("supervisor-fail", "wda", "launchd handoff not verified");
        die(format!(
            "runner launchd job loaded, but its replacement runner was not verified within 120s.\n   Check: {}\n   Then:  {} status",
            ctx.wda_agent_log.display(),
            ctx.self_install.display()
        ))
    }

    /// The script's EXIT-trap rollbacks for the two things only an
    /// interactive setup changes: the fixed script copy and the supervisor.
    fn rollback_interactive(&mut self, status: i32) -> bool {
        let mut ok_all = true;
        let mut self_ok = true;
        if status != 0 && self.self_install.replaced {
            if self.self_install.had_previous {
                if restore_backup(&self.self_install.rollback, &self.ctx.self_install, 0o700) {
                    let _ = std::fs::remove_file(&self.self_install.rollback);
                } else {
                    self_ok = false;
                    ok_all = false;
                    warn(&format!(
                        "could not restore the prior setup script; rescue backup retained at:\n   {}",
                        self.self_install.rollback.display()
                    ));
                }
            } else {
                let _ = std::fs::remove_file(&self.ctx.self_install);
                if self.ctx.self_install.exists() {
                    self_ok = false;
                    ok_all = false;
                    warn(&format!(
                        "could not remove the newly installed setup script: {}",
                        self.ctx.self_install.display()
                    ));
                }
            }
        }
        if status != 0 && self.sup.active && !self.handoff_complete {
            let ctx = self.ctx.clone();
            let label = ctx.instance.wda_label.clone();
            let mut sup_ok = true;
            warn("setup failed — restoring the prior runner supervisor file and loaded state");
            launchd::bootout(&ctx, &label);
            if !launchd::wait_gone(&ctx, &label) {
                sup_ok = false;
                warn("new runner supervisor did not fully stop during rollback");
            }
            if self.sup.prev_present {
                if !restore_backup(&self.sup.rollback, &ctx.wda_agent_plist, 0o600) {
                    sup_ok = false;
                    warn(&format!(
                        "could not restore the prior supervisor plist; rescue backup retained at:\n   {}",
                        self.sup.rollback.display()
                    ));
                }
            } else {
                let _ = std::fs::remove_file(&ctx.wda_agent_plist);
                if ctx.wda_agent_plist.exists() {
                    sup_ok = false;
                    warn(&format!(
                        "could not remove the newly created supervisor plist: {}",
                        ctx.wda_agent_plist.display()
                    ));
                }
            }
            if self.sup.prev_loaded {
                if sup_ok && self_ok && plist_lints(&ctx.wda_agent_plist) {
                    launchd::enable(&ctx, &label);
                    if !launchd::bootstrap(&ctx, &ctx.wda_agent_plist)
                        || !launchd::loaded(&ctx, &label)
                    {
                        sup_ok = false;
                        warn("prior runner supervisor plist was restored, but its loaded state was not");
                    }
                } else {
                    sup_ok = false;
                    warn("prior runner supervisor was not restarted because its files were not fully restored");
                }
            }
            if !launchd::restore_policy(&ctx, &label, self.sup.prev_disabled) {
                sup_ok = false;
            }
            if sup_ok {
                let _ = std::fs::remove_file(&self.sup.rollback);
            } else {
                ok_all = false;
                if self.sup.rollback.is_file() {
                    warn(&format!(
                        "supervisor rescue backup retained at: {}",
                        self.sup.rollback.display()
                    ));
                }
            }
        }
        if let Some(staged) = self.sup.staged.take() {
            let _ = std::fs::remove_file(staged);
        }
        ok_all
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
