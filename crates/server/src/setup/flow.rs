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
use super::legacy_ios;
use super::owner;
use super::pid::{self, Legacy, Role};
use super::proc::{self, XcconfigEnv};
use super::retry::{self, Kind};
use super::runner;
use super::status;
use super::sys;
use super::term::{die, info, ok, warn, Exit, Step, BOLD, RST};
use super::usbdiag;

const RUNNER_DEVICE_PORT: u16 = 8100;
const MJPEG_DEVICE_PORT: u16 = 9100;
/// Published as `not_connected`; the daemon's hint says the same.
const NOT_CONNECTED_MESSAGE: &str = "the iPhone isn't connected to this Mac — plug it in over USB (or join the same Wi-Fi) and unlock it";
const AUTOMATION_MODE_HINT: &str = "enable UI automation on the iPhone: Settings › Developer › Enable UI Automation, then accept any passcode or Allow automation prompt while the phone is unlocked";

/// How often [`Setup::wait_for_developer_services`] re-checks the mount.
const DDI_POLL: Duration = Duration::from_secs(1);
/// … whether the phone is still attached at all.
const DDI_PRESENCE_EVERY: Duration = Duration::from_secs(20);
/// … repeats its interactive "still waiting" prompt.
const DDI_REMIND_EVERY: Duration = Duration::from_secs(32);
/// … gives up on a phone that never mounts the disk image.
const DDI_GIVE_UP: Duration = Duration::from_secs(180);

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
    /// The target is off USB and this run reaches it through CoreDevice's
    /// encrypted Wi-Fi tunnel (`WDA_TRANSPORT=auto`).
    over_tunnel: bool,
    interactive_lock: Option<InteractiveLock>,
    /// Set when the phone takes the legacy (iOS 15/16) path.
    legacy_ios: Option<LegacyRun>,
}

/// A phone on the legacy (iOS 15/16) path: see `legacy_ios`.
#[derive(Debug, Clone, Default)]
struct LegacyRun {
    ios: String,
    /// The `iphone-use-legacy-launch` binary.
    launcher: PathBuf,
    /// The LAN address this run starts the runner over; `None` = USB.
    host: Option<String>,
    lan_ip: Option<String>,
    wifi_mac: Option<String>,
    wifi_ready: bool,
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
            over_tunnel: false,
            interactive_lock: None,
            legacy_ios: None,
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
            if state.kind == Kind::LegacyUnreachable {
                while retry::now() < state.next_at {
                    if checks::transport(&self.ctx.udid) == checks::Transport::Usb
                        || legacy_reachable_over_wifi(&self.ctx)
                    {
                        info("the iOS 15/16 iPhone is reachable again; retrying");
                        break;
                    }
                    let left = state.next_at.saturating_sub(retry::now()).clamp(1, 5);
                    proc::sleep(Duration::from_secs(left))?;
                }
                return Ok(());
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
                } else if self.failure_kind == Kind::IosTooOld {
                    if previous != Some(Kind::IosTooOld) {
                        warn("the iPhone's iOS is too old for the selected Xcode; checking again every 15 min (update the iPhone or select an older Xcode with --xcode)");
                    }
                } else if self.failure_kind == Kind::LegacyUnreachable {
                    if previous != Some(Kind::LegacyUnreachable) {
                        warn("the iOS 15/16 iPhone is neither on USB nor reachable over Wi-Fi; retrying as soon as it is");
                    }
                } else if self.failure_kind == Kind::NeedsReboot {
                    if previous != Some(Kind::NeedsReboot) {
                        warn("the iPhone's developer services need a restart of the iPhone; checking again every few minutes");
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
        self.ios_supported()?;
        if self.legacy_ios.is_some() {
            self.legacy_prepare()?;
        } else {
            legacy_ios::clear_record(self.ctx.state_dir());
            self.wait_for_developer_services()?;
            self.wait_for_unlock()?;
        }
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
        let (products, xctestrun, from_cache) = self.product(&xcodebuild, &key)?;
        let url = if self.legacy_ios.is_some() {
            self.legacy_launch(&products, &key)?
        } else {
            self.launch(&xcodebuild, &xctestrun, from_cache)?
        };
        self.phone_url = url.clone();
        let mut target_url = self.relays(&url)?;
        if self.legacy_ios.is_some() {
            target_url = self.legacy_wifi(&url, &target_url)?;
        }
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
        self.phase("ready", "", &self.ready_message());
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
            } else if !self.ctx.lan() && self.ctx.wifi_tunnel_allowed() {
                // No cable: the one phone with a live encrypted Wi-Fi tunnel.
                // Guessing among several could drive the wrong phone.
                let tunneled = checks::wifi_tunnel_udids();
                if tunneled.len() == 1 {
                    self.ctx.udid = tunneled[0].clone();
                    ok(&format!(
                        "using the iPhone on its CoreDevice Wi-Fi tunnel: {}",
                        self.ctx.udid
                    ));
                } else if tunneled.len() > 1 {
                    return die(format!(
                        "no iPhone is on USB and several have a Wi-Fi tunnel ({}). Set WDA_UDID=<one>; refusing to guess.",
                        tunneled.join(" ")
                    ));
                }
            }
        }
        if !self.ctx.udid.is_empty() {
            self.wait_until_connected()?;
        }
        if !self.ctx.lan() {
            if self.ctx.udid.is_empty() {
                // usbmuxd lists nothing: ask the USB plane why (a cable that
                // only charges, or an iPhone this Mac's device support cannot
                // claim yet, both look like "no device" from here).
                if let Some(diagnosis) = usbdiag::probe(&[]) {
                    self.phase("prereq", diagnosis.blocker(), &diagnosis.message());
                    return die(format!("{}; no build was started.", diagnosis.message()));
                }
                self.phase("prereq", "usb", "no USB iPhone is connected");
                if self.ctx.wifi_tunnel_allowed() {
                    return die("no iPhone was found over USB or on a CoreDevice Wi-Fi tunnel.\n   Plug in and unlock one iPhone (a Wi-Fi phone needs a tunnel: pair it with this Mac over USB once,\n   keep it on the same network), or set WDA_UDID=<UDID>; no build was started.");
                }
                return die("WDA_TRANSPORT=usb requires USB, but no USB iPhone was found.\n   Plug in and unlock one iPhone, or set WDA_UDID=<USB UDID>; no build was started.");
            }
            let usb = checks::usb_udids();
            if !checks::on_usb(&self.ctx.udid, &usb)
                && self.ctx.wifi_tunnel_allowed()
                && legacy_reachable_over_wifi(&self.ctx)
            {
                // iOS 15/16 has no CoreDevice tunnel; its auto transport is
                // the legacy launcher's lockdown TLS session on the LAN.
                ok("the iOS 15/16 iPhone is off USB but its lockdown answers over Wi-Fi; starting it there");
                self.over_tunnel = true;
            } else if !checks::on_usb(&self.ctx.udid, &usb) {
                // Off the cable, CoreDevice's encrypted Wi-Fi tunnel reaches
                // the phone from this Mac only, so it needs no opt-in; the
                // plain LAN relay stays behind WDA_ALLOW_LAN=1.
                if self.ctx.wifi_tunnel_allowed() && checks::wifi_tunnel_or_wake(&self.ctx.udid) {
                    ok(&format!(
                        "{} is off USB; setting up through its encrypted CoreDevice Wi-Fi tunnel (USB is used whenever it is plugged in)",
                        self.ctx.udid
                    ));
                    self.over_tunnel = true;
                } else {
                    self.phase(
                        "prereq",
                        "usb",
                        if self.ctx.wifi_tunnel_allowed() {
                            "the configured iPhone is neither on USB nor on a CoreDevice Wi-Fi tunnel"
                        } else {
                            "the configured iPhone is not connected over USB (WDA_TRANSPORT=usb)"
                        },
                    );
                    return die(not_reachable_message(
                        &self.ctx.udid,
                        self.ctx.wifi_tunnel_allowed(),
                    ));
                }
            }
            // usbmuxd keys pairing records by its own spelling of the serial.
            // A tunnel target is not on the bus, so there is nothing to
            // diagnose there.
            if !self.over_tunnel {
                let serial: Vec<String> = usb
                    .into_iter()
                    .filter(|serial| checks::on_usb(&self.ctx.udid, std::slice::from_ref(serial)))
                    .collect();
                let untrusted = usbdiag::untrusted(&serial);
                if let Some(diagnosis) = usbdiag::diagnose(&[], &serial, &untrusted) {
                    self.phase("prereq", diagnosis.blocker(), &diagnosis.message());
                    return die(format!("{}; no build was started.", diagnosis.message()));
                }
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
            // Under KeepAlive nobody reads the name, and each devicectl call
            // here costs 1–8 s of every reconnect.
            _ if self.ctx.keepalive => (String::new(), false),
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
        if !over_usb && !self.ctx.keepalive {
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

    /// Fails fast when the phone's iOS is below what the selected Xcode can
    /// run the device runner on. Such a phone is on USB and paired, but
    /// CoreDevice never lists it, so waiting for developer services would
    /// only time out minutes later with cable and WARP advice that does not
    /// apply. Reads the version from lockdownd over usbmuxd; when it cannot be
    /// read, nothing is decided here.
    fn ios_supported(&mut self) -> Step {
        // Off the cable usbmuxd may not list an iOS 15/16 phone; its last
        // legacy run recorded the version.
        let Some(device) = checks::lockdown_ios_version(&self.ctx.udid)
            .or_else(|| legacy_record_for(&self.ctx).map(|r| r.ios))
        else {
            return Ok(());
        };
        let Some(xcodebuild) = sys::which("xcodebuild") else {
            return Ok(());
        };
        let xcode =
            checks::xcode_version_cached(self.ctx.state_dir(), &xcodebuild.to_string_lossy());
        let below =
            checks::min_device_ios(&xcode).is_some_and(|floor| checks::version_lt(&device, floor));
        let legacy = below && checks::legacy_device_support(&device);
        if below && !legacy && legacy_ios::applies(&device) {
            if self.ctx.asc_signing_enabled() {
                ok(&format!(
                    "iOS {device}: using the legacy device path ({xcode} cannot drive iOS below 17 itself)"
                ));
                let record = legacy_record_for(&self.ctx);
                self.legacy_ios = Some(LegacyRun {
                    ios: device,
                    lan_ip: record.as_ref().and_then(|r| r.lan_ip.clone()),
                    wifi_mac: record.as_ref().and_then(|r| r.wifi_mac.clone()),
                    wifi_ready: record.as_ref().is_some_and(|r| r.wifi_ready),
                    ..LegacyRun::default()
                });
                return Ok(());
            }
            let message = legacy_needs_asc_message(&device);
            self.phase("prereq", "ios_too_old", &message);
            self.failure_kind = Kind::IosTooOld;
            return die(format!("{message}.\n   No build was started."));
        }
        let Some(message) = ios_too_old_message(&xcode, Some(&device), legacy) else {
            return Ok(());
        };
        self.phase("prereq", "ios_too_old", &message);
        self.failure_kind = Kind::IosTooOld;
        die(format!(
            "{message}.\n   Retrying, reconnecting, or a different cable cannot fix this; no build was started."
        ))
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
            let present = if tick.is_multiple_of(5) {
                checks::presence(&udid) != checks::Presence::Absent
                    // An iOS 15/16 phone off the cable that starts over Wi-Fi.
                    || legacy_reachable_over_wifi(&self.ctx)
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
                // An iPhone on the bus that no driver claimed is not "not
                // connected": it needs newer device support, not a cable.
                match usbdiag::probe(&[]) {
                    Some(diagnosis @ usbdiag::Diagnosis::NotClaimed { .. }) => {
                        self.phase("prereq", diagnosis.blocker(), &diagnosis.message());
                        warn(&format!(
                            "{}. Waiting for {udid}; nothing is built or launched until usbmuxd lists it.",
                            diagnosis.message()
                        ));
                    }
                    _ => {
                        self.phase("prereq", "not_connected", NOT_CONNECTED_MESSAGE);
                        warn(&format!(
                            "{NOT_CONNECTED_MESSAGE}. Waiting for {udid}; nothing is built or launched until it is back."
                        ));
                    }
                }
            } else if !self.ctx.keepalive && tick.is_multiple_of(30) {
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
        // Off the cable by choice (the Wi-Fi tunnel, or the LAN opt-in): no
        // USB reminder.
        let off_cable = self.ctx.lan() || self.over_tunnel;
        if off_cable {
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
        // Polled every second (it was every 4 s, so a mount was noticed up to
        // 4 s late on every reconnect). The give-up, presence and reminder
        // intervals are wall-clock, so a slow `ddi_ready` cannot stretch them.
        let mut tries = 0;
        let mut waiting_since = Instant::now();
        let mut presence_checked = Instant::now();
        let mut reminded: Option<Instant> = None;
        while !self.ddi_ready() {
            proc::check()?;
            tries += 1;
            // Developer services never come up for a phone that left: wait
            // for it to come back instead of counting toward a DDI failure.
            if presence_checked.elapsed() >= DDI_PRESENCE_EVERY {
                presence_checked = Instant::now();
                if checks::presence(&self.ctx.udid) == checks::Presence::Absent {
                    self.wait_until_connected()?;
                    tries = 0;
                    waiting_since = Instant::now();
                }
            }
            self.phase(
                "ddi-wait",
                &blocker,
                &format!("waiting for developer services (attempt {tries})"),
            );
            if waiting_since.elapsed() > DDI_GIVE_UP {
                warn(&format!(
                    "developer services never became available for {}.",
                    self.ctx.udid
                ));
                // On the cable but invisible to CoreDevice: the selected
                // Xcode cannot drive this phone, so cable and WARP advice
                // would send the person the wrong way.
                if checks::usbmux_lists(&self.ctx.udid)
                    && checks::coredevice_lists(&self.ctx.udid) == Some(false)
                {
                    let version = checks::lockdown_ios_version(&self.ctx.udid)
                        .map(|v| format!(" (iOS {v})"))
                        .unwrap_or_default();
                    let message = format!(
                        "the iPhone{version} is attached to this Mac (usbmuxd lists it), but the selected Xcode's CoreDevice does not list it at all — this Xcode most likely cannot run tests on that iOS; update the iPhone (Settings → General → Software Update), or select an Xcode that supports it with --xcode"
                    );
                    self.phase("ddi-fail", "ddi", &message);
                    return die(message);
                }
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
            } else if reminded.is_none_or(|at| at.elapsed() >= DDI_REMIND_EVERY) {
                reminded = Some(Instant::now());
                if off_cable {
                    warn("still waiting — UNLOCK the phone and keep the screen on ...");
                } else {
                    warn(
                        "still waiting — UNLOCK the phone, keep the screen on, and plug in USB ...",
                    );
                }
            }
            proc::sleep(DDI_POLL)?;
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
        // Xcode cannot name an iOS 15/16 phone; the generic product runs there.
        let destination = if self.legacy_ios.is_some() {
            "generic/platform=iOS".to_string()
        } else {
            format!("platform=iOS,id={}", self.ctx.udid)
        };
        let extra = [
            "-destination".to_string(),
            destination,
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
                // Remembered past this run: the daemon keeps a runner that is
                // up over Wi-Fi instead of idle-releasing it (it could not be
                // started again without the cable).
                let _ = std::fs::write(
                    self.rel(WIFI_START_REFUSED_FILE),
                    b"wifi_automation_refused\n",
                );
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
        // A runner that started over Wi-Fi clears an earlier refusal: this
        // phone can be idle-released and brought back without the cable.
        let refused = self.rel(WIFI_START_REFUSED_FILE);
        if refused.exists() && checks::transport(&self.ctx.udid) == checks::Transport::Network {
            let _ = std::fs::remove_file(&refused);
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
            RelayTool::Native(bin) => {
                let mut args = vec![
                    "relay".into(),
                    "--udid".into(),
                    udid.clone(),
                    "--listen".into(),
                    format!("127.0.0.1:{local}"),
                    "--device-port".into(),
                    device_port.to_string(),
                ];
                let mut desc = format!("USB relay (usbmuxd) on 127.0.0.1:{local}");
                if let Some(ip) = self.legacy_lan_host() {
                    desc.push_str(&format!(", LAN fallback {ip}:{device_port} (WDA_ALLOW_LAN=1)"));
                    args.extend(["--lan-host".into(), ip]);
                }
                (bin.clone(), args, desc)
            }
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
        // Off the cable, CoreDevice's encrypted Wi-Fi tunnel reaches the
        // runner from this Mac only; it beats the LAN socat relay, which needs
        // the phone's LAN address and leaves the runner open to the network.
        // An iOS 15/16 phone has no CoreDevice tunnel; its relay follows it
        // from usbmuxd's USB attachment to its Wi-Fi one (or to its LAN
        // address behind WDA_ALLOW_LAN=1).
        let legacy = self.legacy_ios.is_some();
        let wifi_tunnel = !legacy && !target_is_usb && checks::wifi_tunnel(&self.ctx.udid);
        if wifi_tunnel {
            ok("the iPhone is off USB; relaying through its CoreDevice Wi-Fi tunnel");
        }
        // Readiness seen over USB carries no LAN address; only the socat
        // fallback needs one, from the log line once xcodebuild flushes it.
        if self.from_probe && !target_is_usb && !wifi_tunnel && !legacy {
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
        if legacy {
            tool = checks::relay_binary(&self.ctx).map(RelayTool::Native);
        } else if target_is_usb {
            tool = checks::relay_binary(&self.ctx)
                .map(RelayTool::Native)
                .or_else(|| sys::which("iproxy").map(RelayTool::Iproxy));
        } else if wifi_tunnel {
            // Only the native relay knows the tunnel; iproxy does not.
            tool = checks::relay_binary(&self.ctx).map(RelayTool::Native);
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
            return die("the device layer relays over USB, or off the cable through the phone's encrypted CoreDevice Wi-Fi tunnel.\n   Neither is available: plug the iPhone in over USB (or keep it on the same network as this Mac so its tunnel\n   comes up); if it is connected, the iPhoneUse app is missing or too old to relay — reinstall it or run:\n   iphone-use upgrade. The on-phone runner has no HTTP authentication, so a plain LAN relay to the phone's\n   address stays disabled unless WDA_ALLOW_LAN=1 is explicitly set for a trusted, isolated network.");
        };
        // Both relays start before either is checked: the control check and
        // the video's first frame used to run one after the other.
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
        // Live video: the runner's MJPEG stream on the device's :9100, in the
        // same XCUITest session as control.
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
        ok(&format!(
            "PID-verified video relay {mjpeg_pid}: {mjpeg_desc}"
        ));
        // The first video frame is checked off the critical path: control
        // works without video, and the daemon now prefers the runner's H.264
        // stream anyway, so a slow first MJPEG frame is a warning, not a
        // failed round that relaunches the runner.
        spawn_video_first_frame_check(mjpeg_port, mjpeg_log.clone());
        let target_url = format!("http://127.0.0.1:{wda_port}");
        if !sys::http_ok(&format!("{target_url}/status"), Duration::from_secs(5)) {
            return die(format!(
                "relay up but the device runner is not answering through it — check {}",
                relay_log.display()
            ));
        }
        ok(&format!("device runner reachable at {target_url}"));
        warn("The Mac relay is loopback-only, but the runner on the iPhone has no HTTP authentication.\n   Keep the iPhone on a trusted, isolated network even when the Mac relay uses USB.");
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
        if current("WDA_TRANSPORT") != ctx.transport {
            changed.push("WDA_TRANSPORT");
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
                ("WDA_TRANSPORT", ctx.transport.as_str()),
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
    /// from a dead one, so three failures among the last five probes count
    /// (a window, so a runner answering every other time is still caught). A
    /// probe also reads the screen lightly every third cycle, and every cycle
    /// once something failed: a runner that answers `/status` but cannot read
    /// is replaced even under another session's lease. A dead process or a
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
        let read_url = format!(
            "http://127.0.0.1:{wda_port}/source?format=json&excluded_attributes=visible,accessible"
        );
        let mut cycle: u64 = 0;
        let mut failures = 0;
        let mut probes = ProbeCount::default();
        let mut away = AwayGate::default();
        let mut relay_restarts: Vec<Instant> = Vec::new();
        let mut warned_owner = false;
        // A free Apple ID's runner profile lasts 7 days. Replace the runner
        // while the phone is free and unlocked, before it lapses, instead of
        // leaving the next reconnect to find it expired.
        const RENEW_CHECK: Duration = Duration::from_secs(30 * 60);
        let mut renew_checked = Instant::now();
        let cause = loop {
            // xcodebuild appends to the runner log for as long as the runner
            // lives; it is only truncated at the next launch.
            crate::logcap::cap_file(
                &self.ctx.run_log,
                crate::logcap::CAP_BYTES,
                crate::logcap::KEEP_TAIL,
                crate::logcap::KEEP_ROTATED,
            );
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
                // An iOS 15/16 runner ends with its launcher (and, started over USB, with
                // the cable): hardware, iPhone 12 mini on iOS 15.4.1.
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
            // A pulled cable looks like a dead runner from here. Ask whether
            // the phone is still attached before counting a miss: an absent
            // phone is `not_connected`, never a runner failure to rebuild.
            // An iOS 15/16 phone is only "here" on the cable: CoreDevice never
            // lists it, and the runner cannot be started again over Wi-Fi.
            let legacy = self.legacy_ios.is_some();
            let presence = || {
                if legacy {
                    if self.legacy_on_usb() {
                        checks::Presence::Present
                    } else {
                        checks::Presence::Absent
                    }
                } else {
                    checks::presence(&self.ctx.udid)
                }
            };
            match away.on_probe(answered, presence) {
                AwayStep::Away { first } => {
                    if first {
                        warn("the iPhone left this Mac while its runner was held; waiting for it to come back (nothing is rebuilt)");
                        self.phase("waiting", "not_connected", NOT_CONNECTED_MESSAGE);
                    }
                    failures = 0;
                    probes = ProbeCount::default();
                    proc::sleep(Duration::from_secs(2))?;
                    continue;
                }
                AwayStep::Count { cleared } => {
                    if cleared {
                        ok("the iPhone is connected to this Mac again; holding its runner");
                        self.phase("ready", "", "device runner and launchd supervisor verified");
                        probes = ProbeCount::default();
                    }
                }
            }
            // `/status` alone cannot see a runner that answers it but can no
            // longer read the screen. Read lightly every third cycle (~30 s),
            // and every cycle while anything in the window looks wrong.
            cycle += 1;
            let probe = if !answered {
                Probe::StatusMiss
            } else if (cycle.is_multiple_of(3) || probes.suspicious()) && !runner_reads(&read_url) {
                Probe::ReadFail
            } else {
                Probe::Ok
            };
            let rebuild = probes.record(probe);
            failures = probes.failures;
            if rebuild && probes.half_dead() {
                // A runner that cannot read the screen is of no use to whoever
                // holds the phone either, so this replaces it even under
                // another session's lease (unlike a slow /status below).
                if let Some(lease) =
                    owner::foreign(owner::current(&self.ctx), owner::caller().as_deref())
                {
                    warn(&format!(
                        "session \"{}\" holds the phone, but its runner cannot read the screen; replacing it",
                        lease.owner
                    ));
                }
                break "half_dead";
            }
            if probe == Probe::ReadFail {
                info(&format!(
                    "the device runner answered /status but a screen read failed ({failures}/{MAX_FAILURES} of the last {} probes); holding",
                    ProbeCount::WINDOW
                ));
            }
            if !answered {
                if rebuild {
                    // A slow runner under another session's lease is that
                    // session's to wait for, not ours to replace.
                    if let Some(lease) =
                        owner::foreign(owner::current(&self.ctx), owner::caller().as_deref())
                    {
                        if !warned_owner {
                            warn(&format!(
                                "the device runner missed /status {failures} of the last {} probes, but session \"{}\" holds the phone; not replacing the runner it is using",
                                ProbeCount::WINDOW,
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
                    "the device runner did not answer /status within 4s ({failures}/{MAX_FAILURES} of the last {} probes); the runner and relays are alive, so holding",
                    ProbeCount::WINDOW
                ));
            }
            proc::sleep(Duration::from_secs(10))?;
        };
        // An unplugged phone takes its runner down with it. Nothing can be
        // rebuilt until it is back, so say so and wait instead.
        let absent = if self.legacy_ios.is_some() {
            !self.legacy_on_usb() && !legacy_reachable_over_wifi(&self.ctx)
        } else {
            checks::presence(&self.ctx.udid) == checks::Presence::Absent
        };
        if absent {
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
                    "the device runner missed /status {failures} of the last {} probes while the runner and both relays stayed alive — rebuilding",
                    ProbeCount::WINDOW
                ));
                self.phase(
                    "building",
                    "",
                    &format!(
                        "the device runner missed {failures} of the last {} /status probes — rebuilding",
                        ProbeCount::WINDOW
                    ),
                );
            }
            "half_dead" => {
                warn(&format!(
                    "the device runner answers /status but {failures} of the last {} probes failed to read the screen — restarting it",
                    ProbeCount::WINDOW
                ));
                self.phase(
                    "building",
                    "",
                    "the device runner answers /status but cannot read the screen — restarting it",
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

/// A light screen read through the runner's relay: 2xx with a tree, not an
/// error envelope. Heavy screens can take several seconds, hence the budget.
fn runner_reads(url: &str) -> bool {
    let Some((status, body)) = sys::http_get(url, Duration::from_secs(15)) else {
        return false;
    };
    status < 400 && read_body_ok(&body)
}

/// The body of a successful `/source` read: a JSON value that is not WDA's
/// `{"value":{"error":…}}` envelope.
fn read_body_ok(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body).is_ok_and(|root| {
        root.get("value")
            .is_some_and(|value| value.get("error").is_none() && !value.is_null())
    })
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
/// Wi-Fi some iOS versions never finish the test session (hardware: iPhone 14
/// on iOS 27.2 beta, Enable UI Automation on, no prompt, Xcode 27.0 and 27.2
/// alike), so it is not something a person can allow on the phone. A runner
/// started over USB keeps working after the unplug. An unknown transport
/// keeps the USB reading.
/// State-dir marker: this phone refused to start the device runner over
/// Wi-Fi (`wifi_automation_refused`). Cleared when a runner next starts over
/// Wi-Fi. While it exists the daemon does not idle-release a runner that is
/// up over the Wi-Fi tunnel.
pub const WIFI_START_REFUSED_FILE: &str = "wifi-start-refused";

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

/// Why setup stops for a configured phone that is not on USB: with the tunnel
/// allowed, it has no live CoreDevice Wi-Fi tunnel either.
pub fn not_reachable_message(udid: &str, tunnel_allowed: bool) -> String {
    if tunnel_allowed {
        format!(
            "target {udid} is not on USB and has no CoreDevice Wi-Fi tunnel to this Mac.\n   Plug that iPhone in, or keep it unlocked on the same network as this Mac (it must have been paired over USB once);\n   a plain LAN relay to the phone's address is never used unless WDA_ALLOW_LAN=1."
        )
    } else {
        format!(
            "target {udid} is not currently connected over USB, and WDA_TRANSPORT=usb rules out its Wi-Fi tunnel.\n   Plug in that iPhone, or unset WDA_TRANSPORT to allow the encrypted CoreDevice Wi-Fi tunnel."
        )
    }
}

pub fn wifi_automation_message(wait: Option<u64>) -> String {
    format!(
        "over Wi-Fi the iPhone would not start the device runner's UI-automation session (runner exit code 74: testmanagerd took the Mac's test session but never gave the runner its IDE channel{}). This iPhone's iOS refuses to start UI automation over the network; it is not a passcode prompt or a setting, so nothing on the phone and no Wi-Fi retry fixes it. Plug the iPhone in by USB once, unlocked: the runner starts in about 20 s, and after you unplug it keeps working over Wi-Fi until it has to start again (phone restart, runner crash), which needs the cable once more",
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

/// The phone's iOS is below the floor of the Xcode named by `xcode` (the
/// first line of `xcodebuild -version`) and that Xcode has no legacy
/// DeviceSupport for it. `None` when the versions are unknown or fine.
pub fn ios_too_old_message(
    xcode: &str,
    device: Option<&str>,
    legacy_support: bool,
) -> Option<String> {
    let device = device.filter(|version| checks::valid_os_version(version))?;
    let floor = checks::min_device_ios(xcode)?;
    if legacy_support || !checks::version_lt(device, floor) {
        return None;
    }
    if checks::version_lt(device, legacy_ios::MIN_IOS) {
        return Some(format!(
            "This iPhone runs iOS {device}; the device runner needs iOS 15 or later (iOS 15 and 16 work without an update). Update the iPhone (Settings → General → Software Update)"
        ));
    }
    let floor_major = floor.split('.').next().unwrap_or(floor);
    Some(format!(
        "This iPhone runs iOS {device}; {xcode} can only run the device runner on iOS {floor_major} or later. Update the iPhone (Settings → General → Software Update), or select an older Xcode with --xcode"
    ))
}

/// The holding loop's patience: three unanswered `/status` probes in a row
/// (~32 s) before a rebuild; one answer resets it.
#[derive(Debug, Default)]
pub struct ProbeCount {
    pub failures: u32,
    recent: std::collections::VecDeque<Probe>,
}

/// One KeepAlive health observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// `/status` answered and, when a read was tried, the read worked.
    Ok,
    /// `/status` did not answer in time.
    StatusMiss,
    /// `/status` answered but a light screen read failed: the "half dead"
    /// runner that answers status forever while every agent read errors.
    ReadFail,
}

/// Whether the held phone is still attached, consulted only when `/status`
/// misses. A cable pull stops `/status` as surely as a dead runner does, but
/// the runner and relays come back on their own when the phone does, so an
/// absent phone is published as `not_connected` and kept out of the probe
/// window. Unknown presence counts as attached.
#[derive(Debug, Default)]
pub struct AwayGate {
    away: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AwayStep {
    /// Count this probe as usual; `cleared` when the phone just came back.
    Count { cleared: bool },
    /// The phone is not attached: skip the probe; `first` on the first miss.
    Away { first: bool },
}

impl AwayGate {
    pub fn on_probe(
        &mut self,
        answered: bool,
        presence: impl FnOnce() -> checks::Presence,
    ) -> AwayStep {
        if !answered && presence() == checks::Presence::Absent {
            let first = !self.away;
            self.away = true;
            return AwayStep::Away { first };
        }
        let cleared = std::mem::take(&mut self.away);
        AwayStep::Count { cleared }
    }
}

impl ProbeCount {
    /// Failures among the last [`Self::WINDOW`] probes that trigger a rebuild.
    pub const MAX: u32 = 3;
    pub const WINDOW: usize = 5;

    /// Record one probe; `true` once [`Self::MAX`] of the last
    /// [`Self::WINDOW`] failed. A window, not a streak: a runner whose
    /// `/status` answers every other time used to reset a consecutive count
    /// forever and was never replaced.
    pub fn record(&mut self, probe: Probe) -> bool {
        if self.recent.len() == Self::WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(probe);
        self.failures = self.recent.iter().filter(|p| **p != Probe::Ok).count() as u32;
        self.failures >= Self::MAX
    }

    /// `/status`-only form of [`Self::record`].
    pub fn observe(&mut self, answered: bool) -> bool {
        self.record(if answered {
            Probe::Ok
        } else {
            Probe::StatusMiss
        })
    }

    /// Any failure in the window (so reads are checked every cycle).
    pub fn suspicious(&self) -> bool {
        self.failures > 0
    }

    /// The window holds a failed read while `/status` answered.
    pub fn half_dead(&self) -> bool {
        self.recent.contains(&Probe::ReadFail)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_ios_below_the_coredevice_floor_is_ios_too_old() {
        // Hardware: iPhone 12 mini, lockdownd ProductVersion 15.4.1, Xcode
        // 27.0 (devicectl shows only a bare ECID row, xctrace omits it).
        let message = super::ios_too_old_message("Xcode 27.0", Some("15.4.1"), false).unwrap();
        assert_eq!(
            message,
            "This iPhone runs iOS 15.4.1; Xcode 27.0 can only run the device runner on iOS 17 or later. Update the iPhone (Settings → General → Software Update), or select an older Xcode with --xcode"
        );
        assert!(!message.contains("USB") && !message.contains("WARP"));
        assert!(super::ios_too_old_message("Xcode 26.1", Some("16.7"), false).is_some());
        // Below the runner's own floor no path helps but an update.
        let ancient = super::ios_too_old_message("Xcode 27.0", Some("14.8"), false).unwrap();
        assert!(ancient.contains("iOS 15 or later"), "{ancient}");
        assert!(super::legacy_needs_asc_message("15.4.1").contains("WDA_ASC_KEY_PATH"));
        // At or above the floor, an Xcode without one, a legacy DeviceSupport
        // image, or an unreadable version never blocks.
        assert!(super::ios_too_old_message("Xcode 27.0", Some("17.0"), false).is_none());
        assert!(super::ios_too_old_message("Xcode 27.0", Some("27.2"), false).is_none());
        assert!(super::ios_too_old_message("Xcode 16.4", Some("15.4.1"), false).is_none());
        assert!(super::ios_too_old_message("Xcode 27.0", Some("15.4.1"), true).is_none());
        assert!(super::ios_too_old_message("Xcode 27.0", None, false).is_none());
        assert!(super::ios_too_old_message("Xcode 27.0", Some("garbage"), false).is_none());
        assert!(super::ios_too_old_message("", Some("15.4.1"), false).is_none());
    }

    use super::*;

    #[test]
    fn an_unreachable_target_names_the_tunnel_and_never_offers_the_lan_relay_as_the_fix() {
        let auto = not_reachable_message("00008110-001C18203AD2401E", true);
        assert!(auto.contains("no CoreDevice Wi-Fi tunnel"), "{auto}");
        assert!(auto.contains("unless WDA_ALLOW_LAN=1"), "{auto}");
        assert!(!auto.contains("slow Wi-Fi"), "{auto}");
        let usb = not_reachable_message("00008110-001C18203AD2401E", false);
        assert!(usb.contains("not currently connected over USB"), "{usb}");
        assert!(usb.contains("WDA_TRANSPORT=usb"), "{usb}");
    }

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
        assert!(wifi.contains("by USB once"), "{wifi}");
        assert!(wifi.contains("after you unplug"), "{wifi}");
        assert!(
            !wifi.contains("passcode when"),
            "no prompt to answer: {wifi}"
        );
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
    fn the_hold_rebuilds_after_three_failures_in_the_last_five_probes() {
        let mut probes = ProbeCount::default();
        assert!(!probes.observe(false));
        assert!(!probes.observe(false));
        assert!(!probes.observe(true), "two failures are not enough");
        assert!(
            probes.observe(false),
            "an answer no longer resets the count"
        );
        let mut healthy = ProbeCount::default();
        for _ in 0..10 {
            assert!(!healthy.observe(true), "a healthy runner is never rebuilt");
        }
        assert!(!healthy.suspicious());
    }

    #[test]
    fn a_flapping_status_is_still_caught() {
        // The field failure: answers every other probe. A streak counter never
        // got past 2; three of the last five fail by the fifth probe.
        let mut probes = ProbeCount::default();
        let pattern = [false, true, false, true, false];
        let verdicts: Vec<bool> = pattern.iter().map(|&a| probes.observe(a)).collect();
        assert_eq!(verdicts, [false, false, false, false, true]);
    }

    #[test]
    fn a_runner_that_answers_status_but_cannot_read_is_half_dead() {
        let mut probes = ProbeCount::default();
        assert!(!probes.record(Probe::ReadFail));
        assert!(
            probes.suspicious(),
            "a failed read makes the next cycles read too"
        );
        assert!(!probes.record(Probe::ReadFail));
        assert!(probes.record(Probe::ReadFail));
        assert!(probes.half_dead());
        let mut only_status = ProbeCount::default();
        for _ in 0..3 {
            only_status.observe(false);
        }
        assert!(
            !only_status.half_dead(),
            "status misses alone are not half dead"
        );
    }

    /// Drive the hold's per-probe decision the way `hold` does: an away probe
    /// skips the window, a counted one is recorded. Returns (step, rebuild).
    fn hold_step(
        gate: &mut AwayGate,
        probes: &mut ProbeCount,
        answered: bool,
        presence: checks::Presence,
    ) -> (AwayStep, bool) {
        let step = gate.on_probe(answered, || presence);
        let rebuild = match step {
            AwayStep::Away { .. } => {
                *probes = ProbeCount::default();
                false
            }
            AwayStep::Count { cleared } => {
                if cleared {
                    *probes = ProbeCount::default();
                }
                probes.observe(answered)
            }
        };
        (step, rebuild)
    }

    #[test]
    fn a_pulled_cable_during_the_hold_is_not_connected_and_never_rebuilds() {
        use checks::Presence::{Absent, Present};
        let mut gate = AwayGate::default();
        let mut probes = ProbeCount::default();
        // The field run: ~54 s unplugged, a miss every probe.
        let mut steps = Vec::new();
        for _ in 0..10 {
            let (step, rebuild) = hold_step(&mut gate, &mut probes, false, Absent);
            assert!(!rebuild, "an absent phone is never a runner failure");
            steps.push(step);
        }
        assert_eq!(steps[0], AwayStep::Away { first: true }, "published once");
        assert!(steps[1..]
            .iter()
            .all(|s| *s == AwayStep::Away { first: false }));
        assert!(!probes.suspicious(), "absence leaves the window empty");
        // Plugged back in: the runner answers again and the blocker clears.
        let (step, rebuild) = hold_step(&mut gate, &mut probes, true, Present);
        assert_eq!(step, AwayStep::Count { cleared: true });
        assert!(!rebuild);
        let (step, _) = hold_step(&mut gate, &mut probes, true, Present);
        assert_eq!(
            step,
            AwayStep::Count { cleared: false },
            "cleared only once"
        );
    }

    #[test]
    fn a_runner_failure_with_the_phone_attached_still_counts() {
        use checks::Presence::{Present, Unknown};
        let mut gate = AwayGate::default();
        let mut probes = ProbeCount::default();
        assert!(!hold_step(&mut gate, &mut probes, false, Present).1);
        assert!(
            !hold_step(&mut gate, &mut probes, false, Unknown).1,
            "unknown presence counts as attached"
        );
        let (step, rebuild) = hold_step(&mut gate, &mut probes, false, Present);
        assert_eq!(step, AwayStep::Count { cleared: false });
        assert!(rebuild, "three misses with the phone present still rebuild");
    }

    #[test]
    fn an_answered_probe_never_asks_for_presence() {
        let mut gate = AwayGate::default();
        let step = gate.on_probe(true, || panic!("presence is only checked on a miss"));
        assert_eq!(step, AwayStep::Count { cleared: false });
    }

    /// Serve one canned answer per connection on a free loopback port.
    fn canned_runner(status: u16, body: &'static str) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        port
    }

    #[test]
    fn a_read_probe_sees_a_runner_that_cannot_read() {
        // The half-dead runner: it answers, but every tree read is an error.
        let broken = canned_runner(
            500,
            r#"{"value":{"error":"unknown error","message":"snapshot failed"}}"#,
        );
        assert!(!runner_reads(&format!(
            "http://127.0.0.1:{broken}/source?format=json"
        )));
        let healthy = canned_runner(200, r#"{"value":{"type":"XCUIElementTypeApplication"}}"#);
        assert!(runner_reads(&format!(
            "http://127.0.0.1:{healthy}/source?format=json"
        )));
    }

    #[test]
    fn read_bodies() {
        assert!(read_body_ok(br#"{"value":{"type":"XCUIElementTypeApplication"}}"#));
        assert!(!read_body_ok(br#"{"value":{"error":"unknown error","message":"x"}}"#));
        assert!(!read_body_ok(br#"{"value":null}"#));
        assert!(!read_body_ok(b"<html>"));
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
            let tunneled = usb.is_empty()
                && ctx.wifi_tunnel_allowed()
                && if ctx.udid.is_empty() {
                    checks::wifi_tunnel_udids().len() == 1
                } else {
                    checks::wifi_tunnel(&ctx.udid)
                };
            if tunneled {
                checklist_line(
                    true,
                    "iPhone reachable over its encrypted Wi-Fi tunnel (a USB cable is faster)",
                    "",
                );
            } else if usb.is_empty() {
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
        pairs.push(("WDA_TRANSPORT", ctx.transport.clone()));
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

/// Warn, on a background thread, when the video relay has not delivered its
/// first byte within 8 s. Never fails setup: control is what agents need.
fn spawn_video_first_frame_check(mjpeg_port: u16, mjpeg_log: PathBuf) {
    let _ = std::thread::Builder::new()
        .name("video-first-frame".into())
        .spawn(move || {
            let first_byte = sys::http_get_prefix(
                &format!("http://127.0.0.1:{mjpeg_port}"),
                Duration::from_secs(8),
                1,
            )
            .is_some_and(|(status, body)| status < 400 && !body.is_empty());
            if !first_byte {
                warn(&format!(
                    "video relay owns 127.0.0.1:{mjpeg_port} but no video arrived within 8s; control works without it. Inspect {}",
                    mjpeg_log.display()
                ));
            }
        });
}

// ── the legacy (iOS 15/16) path ─────────────────────────────────────────────

/// iOS 15/16 needs App Store Connect API-key signing (`WDA_ASC_*`): Xcode
/// cannot register such a phone or provision for it, and only the API can.
pub fn legacy_needs_asc_message(device: &str) -> String {
    format!(
        "This iPhone runs iOS {device}. iphone-use drives iOS 15 and 16 without updating the phone, but only with a paid Apple Developer account's App Store Connect API key (WDA_ASC_KEY_PATH, WDA_ASC_KEY_ID, WDA_ASC_ISSUER_ID): this Xcode cannot see the phone, so the key registers it and signs the runner. Configure the key and rerun setup, or update the iPhone to iOS 17 or later"
    )
}

/// Published as `legacy_needs_usb`.
pub fn legacy_needs_usb_message(ios: &str) -> String {
    format!(
        "this iPhone runs iOS {ios} and is neither on USB nor reachable over Wi-Fi from this Mac — plug it in once (unlocked); after that it also starts over Wi-Fi"
    )
}

/// Published as `ddi_needs_reboot`.
pub const DDI_NEEDS_REBOOT_MESSAGE: &str = "the iPhone's developer services do not start even with the right Developer Disk Image mounted (an older image is stuck on the phone) — restart the iPhone once, unlock it and keep it plugged in; setup continues on its own";

/// The phone's legacy record for this run's UDID.
fn legacy_record_for(ctx: &Ctx) -> Option<legacy_ios::Record> {
    legacy_ios::read_record(ctx.state_dir()).filter(|r| {
        crate::usbmux::normalize_udid(&r.udid) == crate::usbmux::normalize_udid(&ctx.udid)
    })
}

/// This phone has run on the legacy path and its lockdownd answers on its
/// recorded LAN address: setup can start it over Wi-Fi with no cable.
pub fn legacy_reachable_over_wifi(ctx: &Ctx) -> bool {
    // WDA_TRANSPORT=usb: the cable only, for this path too.
    ctx.wifi_tunnel_allowed()
        && legacy_record_for(ctx)
        .and_then(|r| r.lan_ip)
        .is_some_and(|ip| legacy_ios::lockdown_reachable(&ip))
}

impl Setup {
    /// The LAN address the relays may fall back to off the cable: only behind
    /// WDA_ALLOW_LAN=1 (the runner has no authentication). Without it, off
    /// the cable the relays use usbmuxd's own Wi-Fi attachment.
    fn legacy_lan_host(&self) -> Option<String> {
        if !self.ctx.lan() {
            return None;
        }
        let legacy = self.legacy_ios.as_ref()?;
        legacy.wifi_ready.then(|| legacy.lan_ip.clone()).flatten()
    }

    fn legacy_on_usb(&self) -> bool {
        checks::on_usb(&self.ctx.udid, &checks::usb_udids())
    }

    fn ready_message(&self) -> String {
        match &self.legacy_ios {
            Some(legacy) if legacy.host.is_some() => format!(
                "device runner ready (iOS {}, started over Wi-Fi at {}): you can unplug the cable — it keeps running over Wi-Fi, and restarts over Wi-Fi too while the iPhone stays on this network",
                legacy.ios,
                legacy.host.as_deref().unwrap_or("?"),
            ),
            Some(legacy) => format!(
                "device runner ready (iOS {}, started over USB because this Mac cannot reach the iPhone's lockdown over Wi-Fi): keep the cable in — the runner stops when it is pulled",
                legacy.ios
            ),
            None => "device runner and launchd supervisor verified".to_string(),
        }
    }

    fn write_legacy_record(&self) {
        let Some(legacy) = &self.legacy_ios else {
            return;
        };
        legacy_ios::write_record(
            self.ctx.state_dir(),
            &legacy_ios::Record {
                ios: legacy.ios.clone(),
                udid: self.ctx.udid.clone(),
                bundle: self.ctx.bundle_id.clone(),
                launcher: legacy.launcher.to_string_lossy().into_owned(),
                launch_transport: if legacy.host.is_some() { "wifi" } else { "usb" }.into(),
                lan_ip: legacy.lan_ip.clone(),
                wifi_ready: legacy.wifi_ready,
                wifi_mac: legacy.wifi_mac.clone(),
            },
        );
    }

    /// Where to start the runner from: its LAN address when lockdownd answers
    /// there (a runner started over Wi-Fi survives the cable being pulled),
    /// else USB. The address comes from the last run's record, else Bonjour.
    fn legacy_host(&mut self) -> Option<String> {
        if !self.ctx.wifi_tunnel_allowed() {
            return None;
        }
        let record = legacy_record_for(&self.ctx);
        let mut candidates: Vec<String> = Vec::new();
        if let Some(ip) = self.legacy_ios.as_ref().and_then(|l| l.lan_ip.clone()) {
            candidates.push(ip);
        }
        if let Some(ip) = record.as_ref().and_then(|r| r.lan_ip.clone()) {
            candidates.push(ip);
        }
        for ip in &candidates {
            if legacy_ios::lockdown_reachable(ip) {
                return Some(ip.clone());
            }
        }
        let mac = self
            .legacy_ios
            .as_ref()
            .and_then(|l| l.wifi_mac.clone())
            .or_else(|| record.and_then(|r| r.wifi_mac));
        let ip = legacy_ios::bonjour_ipv4(&mac?)?;
        legacy_ios::lockdown_reachable(&ip).then(|| {
            if let Some(legacy) = self.legacy_ios.as_mut() {
                legacy.lan_ip = Some(ip.clone());
            }
            ip
        })
    }

    /// Launcher, Wi-Fi or USB, Developer Disk Image, mount, and proof the
    /// developer services answer.
    fn legacy_prepare(&mut self) -> Step {
        let ios = self.legacy_ios.as_ref().map(|l| l.ios.clone()).unwrap_or_default();
        let Some(launcher) = legacy_ios::launcher(&self.ctx) else {
            let message = format!(
                "the iOS 15/16 launcher ({}) is missing next to iphone-use; reinstall or upgrade iphone-use",
                legacy_ios::LAUNCHER_NAME
            );
            self.phase("prereq", "wda", &message);
            return die(message);
        };
        let on_usb = self.legacy_on_usb();
        if let Some(legacy) = self.legacy_ios.as_mut() {
            legacy.launcher = launcher.clone();
        }
        if on_usb {
            // Wi-Fi lockdown (Finder's "show this iPhone when on Wi-Fi") is
            // what lets the next start go over the network; it needs one
            // trusted connection and survives reboots.
            let args = ["enable-wifi".to_string(), "--udid".into(), self.ctx.udid.clone()];
            let (out, code) = legacy_ios::run_launcher(&launcher, &args, Duration::from_secs(20));
            if code != Some(0) {
                warn(&format!(
                    "could not turn on Wi-Fi lockdown for this iPhone ({}); it starts over USB",
                    legacy_ios::last_error(&out)
                ));
            }
            let udid = self.ctx.udid.clone();
            let mac = sys::block_on(async {
                tokio::time::timeout(Duration::from_secs(5), crate::lockdown::device_info(&udid))
                    .await
                    .ok()
                    .and_then(Result::ok)
            })
            .and_then(|info| info.wifi_address);
            if let (Some(legacy), Some(mac)) = (self.legacy_ios.as_mut(), mac) {
                legacy.wifi_mac = Some(mac);
            }
        }
        let host = self.legacy_host();
        if host.is_none() && !on_usb {
            let mut message = legacy_needs_usb_message(&ios);
            if !self.ctx.wifi_tunnel_allowed() {
                message.push_str(" (WDA_TRANSPORT=usb: Wi-Fi starts are turned off for this phone)");
            }
            self.phase("prereq", "legacy_needs_usb", &message);
            // Retried at once when the cable comes or the phone answers on
            // its LAN address, else every 5 minutes.
            self.failure_kind = Kind::LegacyUnreachable;
            return die(message);
        }
        if let Some(legacy) = self.legacy_ios.as_mut() {
            legacy.host = host.clone();
        }
        match &host {
            Some(ip) => ok(&format!("iOS {ios}: starting over Wi-Fi ({ip}); the runner keeps running with the cable pulled")),
            None if !self.ctx.wifi_tunnel_allowed() => {
                ok(&format!("iOS {ios}: starting over USB (WDA_TRANSPORT=usb)"))
            }
            None => ok(&format!(
                "iOS {ios}: starting over USB (lockdownd does not answer over Wi-Fi yet)"
            )),
        }
        self.phase(
            "ddi-wait",
            &self.build_blocker.clone(),
            &format!("preparing the iOS {ios} Developer Disk Image"),
        );
        let (dmg, signature) = match legacy_ios::ensure_ddi(&ios) {
            Ok(files) => files,
            Err(error) => {
                self.phase("ddi-fail", "ddi", &error);
                return die(format!(
                    "could not get the Developer Disk Image for iOS {ios}: {error}"
                ));
            }
        };
        let started = Instant::now();
        match legacy_ios::mount(&launcher, &self.ctx.udid, host.as_deref(), &dmg, &signature) {
            legacy_ios::Mount::Ready => {
                ok(&format!(
                    "Developer Disk Image mounted; testmanagerd answers ({:.1}s)",
                    started.elapsed().as_secs_f64()
                ));
                Ok(())
            }
            legacy_ios::Mount::NeedsReboot => {
                self.phase("ddi-fail", "ddi_needs_reboot", DDI_NEEDS_REBOOT_MESSAGE);
                self.failure_kind = Kind::NeedsReboot;
                die(DDI_NEEDS_REBOOT_MESSAGE)
            }
            legacy_ios::Mount::Failed(error) if legacy_ios::log_shows_locked(&error) => {
                self.phase(
                    "lock-wait",
                    "locked",
                    "the iPhone is locked — unlock it and connecting continues on its own",
                );
                if self.ctx.keepalive {
                    return self.locked_retry();
                }
                die("the iPhone is locked, so the Developer Disk Image cannot be mounted. Unlock it, then rerun setup.")
            }
            legacy_ios::Mount::Failed(error) => {
                self.phase("ddi-fail", "ddi", &format!("mounting the Developer Disk Image failed: {error}"));
                die(format!("mounting the Developer Disk Image failed: {error}"))
            }
        }
    }

    /// The development profile for this phone and the identity Xcode signed
    /// the build with, from the cache or, when the cache lacks this phone or
    /// that certificate or is close to expiring, from App Store Connect.
    fn legacy_signing(&mut self, built: &Path) -> Step<(Vec<u8>, String)> {
        let Some(cert) = legacy_ios::signing_certificate(&built.join("PlugIns/iPhoneUse.xctest"))
        else {
            self.phase(
                "signing-fail",
                "account",
                "could not read the runner's signing certificate",
            );
            return die("could not read the certificate Xcode signed the runner with (codesign -d --extract-certificates)");
        };
        let identity = legacy_ios::sha1_hex(&cert);
        let team = self.ctx.team_id.clone();
        let bundle = self.ctx.bundle_id.clone();
        let dir = sys::home().join(".iphone-use/legacy-profiles");
        let _ = std::fs::create_dir_all(&dir);
        let cached = dir.join(format!("{team}.{bundle}.mobileprovision"));
        let facts = legacy_ios::profile_facts(&cached, &self.ctx.udid, &cert);
        let fresh = facts
            .expiry
            .is_some_and(|e| e > retry::now() + legacy_ios::PROFILE_MIN_LEFT_SECS);
        if facts.has_device && facts.has_certificate && fresh {
            if let Ok(bytes) = std::fs::read(&cached) {
                ok("Reusing the iOS 15/16 runner profile (it covers this iPhone)");
                return Ok((bytes, identity));
            }
        }
        info("Registering this iPhone and creating the runner's development profile with the App Store Connect API");
        self.phase(
            "building",
            &self.build_blocker.clone(),
            "registering the iPhone and creating the runner profile (App Store Connect)",
        );
        let ctx = self.ctx.clone();
        let name = sys::block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                crate::lockdown::device_info(&ctx.udid),
            )
            .await
            .ok()
            .and_then(Result::ok)
        })
        .and_then(|info| info.name)
        .unwrap_or_else(|| "iPhone".into());
        let ios = self
            .legacy_ios
            .as_ref()
            .map(|l| l.ios.clone())
            .unwrap_or_default();
        let device_name: String = format!("iphone-use {name} (iOS {ios})")
            .chars()
            .filter(|c| !c.is_control())
            .take(50)
            .collect();
        let runner_bundle = legacy_ios::runner_bundle(&bundle);
        let profile_name = format!("iphone-use legacy {bundle}");
        let result: anyhow::Result<(Vec<u8>, bool)> = sys::block_on(async {
            let asc = super::asc::Asc::new(
                Path::new(&ctx.asc_key_path),
                &ctx.asc_key_id,
                &ctx.asc_issuer_id,
            )?;
            let (device_id, registered) = asc.ensure_device(&ctx.udid, &device_name).await?;
            let certificates = asc.development_certificates().await?;
            anyhow::ensure!(
                certificates.iter().any(|(_, der)| *der == cert),
                "the certificate Xcode signed the runner with is not one of team {}'s development certificates",
                ctx.team_id
            );
            let bundle_id = asc.bundle_id_for(&runner_bundle).await?;
            let mut devices = asc.ios_devices().await?;
            if !devices.contains(&device_id) {
                devices.push(device_id);
            }
            let ids: Vec<String> = certificates.into_iter().map(|(id, _)| id).collect();
            let profile = asc
                .recreate_profile(&profile_name, &bundle_id, &ids, &devices)
                .await?;
            Ok((profile, registered))
        });
        match result {
            Ok((profile, registered)) => {
                if registered {
                    ok(&format!("Registered {} with the team", self.ctx.udid));
                }
                if sys::write_atomic(&cached, &profile, 0o600).is_err() {
                    warn("could not cache the runner profile; the next setup creates it again");
                }
                ok("Created the iOS 15/16 runner development profile");
                Ok((profile, identity))
            }
            Err(error) => {
                let message =
                    format!("App Store Connect signing for the iOS {ios} runner failed: {error:#}");
                self.phase("signing-fail", "account", &message);
                die(message)
            }
        }
    }

    /// The signed legacy app, reused while nothing that goes into it changed.
    fn legacy_app(&mut self, built: &Path, key: &str) -> Step<(PathBuf, String)> {
        let (profile, identity) = self.legacy_signing(built)?;
        let assembled_key = legacy_ios::sha256_hex(
            format!(
                "{key}|{identity}|{}|{}",
                legacy_ios::sha256_hex(&profile),
                legacy_ios::sha256_hex(legacy_ios::host_source(&self.ctx).as_bytes())
            )
            .as_bytes(),
        );
        let app = legacy_ios::legacy_app(&self.ctx);
        let key_file = legacy_ios::legacy_dir(&self.ctx).join("assembled.key");
        if app.is_dir()
            && std::fs::read_to_string(&key_file).is_ok_and(|k| k.trim() == assembled_key)
        {
            ok("Reusing the assembled iOS 15/16 runner");
            return Ok((app, assembled_key));
        }
        let (team, bundle) = (self.ctx.team_id.clone(), self.ctx.bundle_id.clone());
        match legacy_ios::assemble(&self.ctx, built, &profile, &identity, &team, &bundle) {
            Ok(app) => {
                let _ = std::fs::write(&key_file, format!("{assembled_key}\n"));
                ok("Assembled the iOS 15/16 runner (legacy host, Info.plist, profile, signature)");
                Ok((app, assembled_key))
            }
            Err(error) => {
                self.phase("building-fail", "wda", &error);
                die(format!("could not assemble the iOS 15/16 runner: {error}"))
            }
        }
    }

    /// Install (when the phone lacks this build), start with the launcher
    /// over Wi-Fi or USB, and wait until the runner answers.
    fn legacy_launch(&mut self, products: &Path, key: &str) -> Step<String> {
        let built = products.join(RUNNER_APP_NAME);
        let (app, assembled_key) = self.legacy_app(&built, key)?;
        let legacy = self.legacy_ios.clone().unwrap_or_default();
        let launcher = legacy.launcher.clone();
        let host = legacy.host.clone();
        let udid = self.ctx.udid.clone();
        let bundle = self.ctx.bundle_id.clone();
        let installed_file = legacy_ios::legacy_dir(&self.ctx).join("installed");
        let installed_key = format!("{udid}|{assembled_key}");
        if std::fs::read_to_string(&installed_file)
            .map(|k| k.trim().to_string())
            .ok()
            .as_deref()
            != Some(installed_key.as_str())
        {
            info("Installing the runner on the iPhone");
            let started = Instant::now();
            if let Err(error) = legacy_ios::install(&launcher, &udid, host.as_deref(), &app) {
                self.phase(
                    "building-fail",
                    "wda",
                    &format!("installing the runner failed: {error}"),
                );
                return die(format!("installing the runner on the iPhone failed: {error}"));
            }
            let _ = std::fs::write(&installed_file, format!("{installed_key}\n"));
            ok(&format!(
                "Installed the runner ({:.1}s)",
                started.elapsed().as_secs_f64()
            ));
        }
        let argv = legacy_ios::launch_argv(&udid, host.as_deref(), &bundle);
        let expected = format!("runner:{} {}", launcher.display(), argv.join(" "));
        let lan_status = host
            .as_deref()
            .map(|ip| format!("http://{ip}:{RUNNER_DEVICE_PORT}/status"));
        let on_usb = self.legacy_on_usb();
        let started = 'launch: {
            'attempts: for attempt in 0..2 {
                let _ = std::fs::write(&self.ctx.run_log, b"");
                let Ok(spawned) = proc::spawn_detached(
                    &launcher,
                    &argv,
                    Some(self.ctx.state_dir()),
                    &self.ctx.run_log,
                    None,
                ) else {
                    return die("could not start the iOS 15/16 launcher");
                };
                let Some(runner_pid) = pid::write(
                    &self.ctx,
                    &self.ctx.runner_pid_file,
                    spawned,
                    &expected,
                    Role::Runner,
                ) else {
                    return die(format!(
                        "the launcher did not become the exact expected runner process; inspect {}",
                        self.ctx.run_log.display()
                    ));
                };
                self.started_runner = true;
                ok(&format!(
                    "PID-verified runner launcher {runner_pid} (over {}; log: {})",
                    if host.is_some() { "Wi-Fi" } else { "USB" },
                    self.ctx.run_log.display()
                ));
                let started = Instant::now();
                loop {
                    proc::check()?;
                    if (on_usb && self.runner_session().is_some())
                        || lan_status
                            .as_deref()
                            .is_some_and(|url| sys::http_ok(url, Duration::from_millis(800)))
                    {
                        break 'launch started;
                    }
                    let log = std::fs::read_to_string(&self.ctx.run_log).unwrap_or_default();
                    let alive = pid::validate(
                        &self.ctx,
                        &self.ctx.runner_pid_file,
                        &self.legacy.runner,
                        Role::Runner,
                        false,
                    )
                    .is_some();
                    if legacy_ios::log_shows_invalid_service(&log) {
                        self.phase("ddi-fail", "ddi_needs_reboot", DDI_NEEDS_REBOOT_MESSAGE);
                        self.failure_kind = Kind::NeedsReboot;
                        return die(DDI_NEEDS_REBOOT_MESSAGE);
                    }
                    if !alive || started.elapsed() > Duration::from_secs(60) {
                        if legacy_ios::log_shows_locked(&log) {
                            if self.ctx.keepalive {
                                return self.locked_retry();
                            }
                            return die("the iPhone is locked, so the runner cannot start. Unlock it, then rerun setup.");
                        }
                        if legacy_ios::log_shows_not_installed(&log) {
                            let _ = std::fs::remove_file(&installed_file);
                        }
                        if attempt == 0 && !alive {
                            warn(&format!(
                                "the runner did not start ({}); launching once more",
                                legacy_ios::last_error(&log)
                            ));
                            continue 'attempts;
                        }
                        let reason = legacy_ios::last_error(&log);
                        self.phase(
                            "building-fail",
                            "wda",
                            &format!("the iOS 15/16 runner did not start: {reason}"),
                        );
                        return die(format!(
                            "the runner did not start on the iPhone ({reason}); log: {}",
                            self.ctx.run_log.display()
                        ));
                    }
                    proc::sleep(Duration::from_millis(250))?;
                }
            }
            return die("the runner did not start on the iPhone");
        };
        ok(&format!(
            "device runner serving on device port {RUNNER_DEVICE_PORT} ({:.1}s after launch, started over {})",
            started.elapsed().as_secs_f64(),
            if host.is_some() { "Wi-Fi" } else { "USB" }
        ));
        self.phase("serving", "", "device runner serving — starting relay");
        self.from_probe = true;
        if host.is_some() {
            // Readiness seen on the LAN address: iOS already lets it there.
            if let Some(legacy) = self.legacy_ios.as_mut() {
                legacy.wifi_ready = lan_status
                    .as_deref()
                    .is_some_and(|url| sys::http_ok(url, Duration::from_secs(2)));
            }
        }
        self.write_legacy_record();
        Ok(format!("http://127.0.0.1:{RUNNER_DEVICE_PORT}"))
    }

    /// Let the runner onto the phone's network: iOS drops inbound LAN
    /// connections to an app it has not allowed (China's "wireless data"
    /// setting, or local network access) and asks only while the app is in
    /// front. Off the cable every path to the runner (usbmuxd's Wi-Fi
    /// attachment, or the LAN address behind WDA_ALLOW_LAN=1) needs this.
    /// Setup brings the runner's app to the front, answers the prompt and
    /// goes back to the Home Screen. Never fails setup.
    fn legacy_wifi(&mut self, url: &str, target_url: &str) -> Step<String> {
        let ip = sys::http_get(&format!("{target_url}/status"), Duration::from_secs(4))
            .and_then(|(_, body)| legacy_ios::status_ip(&body));
        let Some(ip) = ip else {
            warn("the runner reported no Wi-Fi address (is the iPhone on Wi-Fi?); it works over USB only");
            self.write_legacy_record();
            return Ok(target_url.to_string());
        };
        if let Some(legacy) = self.legacy_ios.as_mut() {
            legacy.lan_ip = Some(ip.clone());
        }
        // The relays already fall back to the LAN address when it was known
        // to answer before they started.
        let relays_have_lan = self.legacy_lan_host().is_some();
        let lan_status = format!("http://{ip}:{RUNNER_DEVICE_PORT}/status");
        let mut ready = sys::http_ok(&lan_status, Duration::from_secs(3));
        if !ready {
            info("Allowing the runner onto the iPhone's network (answering the iOS prompt)");
            ready = self.grant_network(target_url, &lan_status);
        }
        if let Some(legacy) = self.legacy_ios.as_mut() {
            legacy.wifi_ready = ready;
        }
        self.write_legacy_record();
        if !ready {
            warn(&format!(
                "the runner does not answer at {ip} over Wi-Fi; on the iPhone allow iPhoneUse-Runner in Settings › Privacy › Local Network (China models: Settings › Cellular › Apps Using WLAN & Cellular › WLAN & Cellular). USB control works meanwhile"
            ));
            return Ok(target_url.to_string());
        }
        ok(&format!("the runner also answers over Wi-Fi at {ip}:{RUNNER_DEVICE_PORT}"));
        if !relays_have_lan && self.legacy_lan_host().is_some() {
            // Restart the relays with the LAN address as their fallback.
            return self.relays(url);
        }
        Ok(target_url.to_string())
    }

    fn grant_network(&self, target_url: &str, lan_status: &str) -> bool {
        if self.endpoint_locked() {
            warn("the iPhone is locked, so the network prompt cannot be answered; unlock it and rerun setup to allow Wi-Fi");
            return false;
        }
        let runner = legacy_ios::runner_bundle(&self.ctx.bundle_id);
        let _ = legacy_ios::http_post_json(
            &format!("{target_url}/wda/apps/launch"),
            &serde_json::json!({ "bundleId": runner }),
            Duration::from_secs(20),
        );
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut answered = false;
        while Instant::now() < deadline && !answered {
            if sys::http_ok(lan_status, Duration::from_secs(1)) {
                break;
            }
            let text = sys::http_get(&format!("{target_url}/alert/text"), Duration::from_secs(5))
                .and_then(|(_, body)| serde_json::from_slice::<serde_json::Value>(&body).ok())
                .and_then(|v| v.get("value").and_then(|v| v.as_str()).map(str::to_string));
            if text.is_some_and(|t| legacy_ios::is_network_prompt(&t)) {
                let buttons: Vec<String> = sys::http_get(
                    &format!("{target_url}/wda/alert/buttons"),
                    Duration::from_secs(5),
                )
                .and_then(|(_, body)| serde_json::from_slice::<serde_json::Value>(&body).ok())
                .and_then(|v| v.get("value").cloned())
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
                if let Some(button) = legacy_ios::grant_button(&buttons) {
                    let _ = legacy_ios::http_post_json(
                        &format!("{target_url}/alert"),
                        &serde_json::json!({ "name": button }),
                        Duration::from_secs(10),
                    );
                    ok(&format!("answered the iPhone's network prompt: {button}"));
                    answered = true;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let _ = legacy_ios::http_post_json(
            &format!("{target_url}/wda/homescreen"),
            &serde_json::json!({}),
            Duration::from_secs(10),
        );
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if sys::http_ok(lan_status, Duration::from_secs(2)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        false
    }
}
