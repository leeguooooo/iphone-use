//! `iphone-use doctor`: a read-only preflight that reports every blocker as a
//! checklist instead of a blind wait. `X` lines fail it, `~` lines inform.

use std::time::Duration;

use super::checks;
use super::ctx::Ctx;
use super::sys;
use super::term::{info, ok, warn, BOLD, RST};
use super::usbdiag;

/// Exit 0 when nothing blocks setup, 1 otherwise.
pub fn run(ctx: &Ctx) -> i32 {
    info("Device runner preflight");
    let mut fail = false;
    if ctx.state_dir().is_dir() {
        ok(&format!(
            "setup state directory present: {}",
            ctx.state_dir().display()
        ));
    } else {
        warn("~ setup state is not initialized; doctor will not create it");
    }
    let xcode = checks::xcode_version();
    if xcode.is_empty() {
        warn(&checks::xcode_missing_message());
        fail = true;
    } else {
        ok(&format!("Full Xcode: {xcode}"));
        match &ctx.developer_dir {
            Some(dir) => ok(&format!("This phone uses its own Xcode: {}", dir.display())),
            None => ok(&format!(
                "This phone uses the Mac's selected Xcode: {}",
                sys::stdout_of("xcode-select", &["-p"])
            )),
        }
        if !checks::doctor_xcode_compat(ctx) {
            fail = true;
        }
    }
    match checks::resolve_signing(ctx) {
        Ok(signing) => {
            ok(&format!("Dev team: {}", signing.team));
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
        }
        Err(error) => {
            warn(&format!("X {error}"));
            checks::open_xcode_for_account(ctx);
            fail = true;
        }
    }
    match checks::runner_source_valid(ctx) {
        Ok(()) => match checks::runner_source_hash(&ctx.runner_src) {
            Some(hash) => ok(&format!(
                "Device runner source: {} (sha256 {})",
                ctx.runner_src.display(),
                &hash[..12]
            )),
            None => {
                warn(&format!(
                    "X device runner sources could not be read: {}",
                    ctx.runner_src.display()
                ));
                fail = true;
            }
        },
        Err(error) => {
            warn(&format!("X {error}"));
            fail = true;
        }
    }
    if ctx.wda_dir.join(".git").is_dir() {
        warn(&format!(
            "~ a WebDriverAgent checkout from an earlier release is still at {}; nothing uses it any more (uninstall.sh removes it when it can prove ownership)",
            ctx.wda_dir.display()
        ));
    }
    let ports_ok =
        matches!((ctx.wda_port_number(), ctx.mjpeg_port_number()), (Some(a), Some(b)) if a != b);
    if ports_ok {
        ok(&format!(
            "Loopback ports: control {}, video {}",
            ctx.wda_port, ctx.mjpeg_port
        ));
    } else {
        warn("X WDA_PORT and MJPEG_PORT must be distinct TCP ports from 1 to 65535");
        fail = true;
    }
    match checks::warp_preflight() {
        Ok(()) => {
            if checks::warp_on() {
                ok(checks::warp_ready_summary());
            } else {
                ok("WARP: off / not present");
            }
        }
        Err(error) => {
            warn(&format!("X {error}"));
            fail = true;
        }
    }
    if let Err(error) = checks::system_proxy_check() {
        warn(&format!("X {error}"));
        fail = true;
    }
    let usb = checks::usb_udids();
    let usb_list = usb.join(" ");
    let absent = !ctx.udid.is_empty()
        && !checks::on_usb(&ctx.udid, &usb)
        && checks::presence(&ctx.udid) == checks::Presence::Absent;
    // What the USB layer says when usbmuxd cannot give us a usable phone.
    let diagnosis = usbdiag::probe(&usb);
    if absent {
        warn(&format!(
            "X configured target {} is not connected to this Mac (usbmuxd does not list it; CoreDevice reports it unavailable) — plug it in over USB (or join the same Wi-Fi) and unlock it",
            ctx.udid
        ));
        if let Some(diagnosis) = &diagnosis {
            warn(&format!("  USB: {}", diagnosis.message()));
        }
        fail = true;
    } else if !ctx.lan() && usb.is_empty() {
        warn("X the default device layer requires an iPhone connected over USB");
        if let Some(diagnosis) = &diagnosis {
            warn(&format!("  USB: {}", diagnosis.message()));
        }
        fail = true;
    } else if let Some(diagnosis @ usbdiag::Diagnosis::NotTrusted { .. }) = &diagnosis {
        warn(&format!("X {}", diagnosis.message()));
        fail = true;
    } else if !ctx.lan() && usb.len() > 1 && ctx.udid.is_empty() {
        warn(&format!(
            "X multiple USB iPhones found ({usb_list}); set WDA_UDID=<one exact UDID>"
        ));
        fail = true;
    } else if !ctx.lan() && !ctx.udid.is_empty() && !checks::on_usb(&ctx.udid, &usb) {
        warn(&format!(
            "X configured target {} is not connected over USB",
            ctx.udid
        ));
        fail = true;
    } else if !usb.is_empty() {
        ok(&format!("iPhone on USB: {usb_list}"));
    } else {
        warn("~ WDA_ALLOW_LAN=1: no USB iPhone; setup will require one unambiguous paired destination");
    }
    // Informational, never a failure: Xcode usually drives an iOS one minor
    // release ahead of its SDK, so only the runner's own refusal is decisive.
    let target = if !ctx.udid.is_empty() {
        ctx.udid.clone()
    } else if usb.len() == 1 {
        usb[0].clone()
    } else {
        String::new()
    };
    let sdk = checks::ios_sdk_version();
    let device = (!target.is_empty())
        .then(|| checks::device_ios_version(&target))
        .flatten();
    match (&device, &sdk) {
        (Some(device), Some(sdk)) => {
            if checks::version_lt(
                &checks::os_major_minor(sdk),
                &checks::os_major_minor(device),
            ) {
                warn(&format!(
                    "~ iPhone runs iOS {device} but the Xcode SDK is iOS {sdk}. A one-minor bump often still works; a beta iOS or a newer major makes the runner exit with code 74. If setup then reports xcode_too_old, install an Xcode that supports iOS {}",
                    checks::os_major_minor(device)
                ));
            } else {
                ok(&format!(
                    "iPhone iOS {device} is covered by the Xcode SDK (iOS {sdk})"
                ));
            }
        }
        _ if !target.is_empty() => warn(&format!(
            "~ could not read the iPhone iOS version to compare with the Xcode SDK{}",
            sdk.as_ref()
                .map(|sdk| format!(" (iOS {sdk})"))
                .unwrap_or_default()
        )),
        _ => {}
    }
    // Decisive, unlike the SDK comparison: below the CoreDevice floor the
    // selected Xcode never lists the phone, so setup stops as ios_too_old.
    if let Some(device) = device.as_deref() {
        let xcode = checks::xcode_version();
        let legacy = checks::min_device_ios(&xcode)
            .is_some_and(|floor| checks::version_lt(device, floor))
            && checks::legacy_device_support(device);
        if let Some(message) = super::flow::ios_too_old_message(&xcode, Some(device), legacy) {
            warn(&format!("X {message}"));
            fail = true;
        }
    }
    if sys::which("lsof").is_some() {
        ok("lsof present for listener ownership checks");
    } else {
        warn("X lsof is required");
        fail = true;
    }
    let relay = checks::relay_binary(ctx);
    let iproxy = sys::which("iproxy");
    if let Some(relay) = &relay {
        ok(&format!(
            "USB relay: {} relay (macOS usbmuxd)",
            relay.display()
        ));
    } else if iproxy.is_some() {
        ok("USB relay: iproxy (this iphone-use app predates the built-in relay; upgrade with: iphone-use upgrade)");
    } else if !ctx.lan() {
        warn("X no USB relay: the iPhoneUse app is missing or too old — reinstall or run: iphone-use upgrade");
        fail = true;
    }
    if ctx.lan() && sys::which("socat").is_none() && relay.is_none() && iproxy.is_none() {
        warn("X WDA_ALLOW_LAN=1 needs a USB relay or socat");
        fail = true;
    }
    if relay.is_some()
        && !ctx.udid.is_empty()
        && checks::transport(&ctx.udid) != checks::Transport::Usb
    {
        if checks::wifi_tunnel(&ctx.udid) {
            ok("Wi-Fi: the relay reaches this iPhone through its CoreDevice tunnel (encrypted, this Mac only)");
        } else {
            println!("  ~ Wi-Fi: no CoreDevice tunnel to this iPhone right now; over Wi-Fi the relay needs one (Xcode or devicectl opens it while the phone is on the same network)");
        }
    }
    if let Some(port) = ctx.wda_port_number() {
        // Any complete answer counts here (`curl -s`, not `-f`).
        if sys::http_get(
            &format!("http://127.0.0.1:{port}/status"),
            Duration::from_secs(4),
        )
        .is_some()
        {
            ok(&format!(
                "device runner already serving on 127.0.0.1:{port}"
            ));
        }
    }
    // Caveats that matter only when something above goes wrong.
    println!("{BOLD}Notes{RST}");
    println!("  • The device runner on the iPhone has no password of its own. The Mac relays it on 127.0.0.1 only,");
    println!("    over USB or, off the cable, through CoreDevice's encrypted Wi-Fi tunnel (launch over USB once: iOS asks");
    println!(
        "    for the passcode to allow UI automation, and cannot show that prompt over Wi-Fi)."
    );
    println!("    WDA_ALLOW_LAN=1 (a socat relay to the phone's LAN address) is an explicit, unsafe last resort.");
    println!("  • Cloudflare WARP or another tunnel VPN can break Xcode's connection to the phone; disconnect it during setup if setup stalls.");
    if fail {
        warn("fix the X items above, then re-run");
        1
    } else {
        ok("preflight checks passed; build, signing, device trust, and launch still require setup verification");
        0
    }
}
