//! Launch the iphone-use XCTest runner on an iOS 15/16 phone and hold it alive,
//! over USB (usbmuxd) or over Wi-Fi with no cable at all.
//!
//! On iOS < 17 the runner lives only as long as the testmanagerd connection
//! that launched it: drop that connection and the runner dies within seconds.
//! A runner launched over USB therefore dies when the cable is pulled. Over
//! Wi-Fi, the launcher talks to lockdown at `<phone-ip>:62078` directly with
//! the host's pair record, and every connection (lockdown, heartbeat,
//! installation_proxy, testmanagerd, DVT) goes over the LAN, so unplugging
//! changes nothing. The process that called [`launch`] must stay up for the
//! runner's lifetime.
//!
//! Two things are needed on the Wi-Fi path and neither is optional:
//! - TLS on every service port (`EnableServiceSSL`); the idevice crate does it.
//! - A live `com.apple.mobile.heartbeat` session, see [`start_heartbeat`].
//!
//! The XCTest launch itself also needs idevice to open the
//! `dtxproxy:XCTestManager_IDEInterface:…` channel on iOS < 17 (see the
//! `[patch.crates-io]` entry in the workspace Cargo.toml).
//!
//! Measured on 2026-10-09 over Wi-Fi only: iPhone 12 mini (iOS 15.4.1) runner
//! `/status` 200 at 7.99 s after the call, iPhone X (iOS 16.5) at 7.85 s. The
//! 12 mini's runner kept answering every poll with the cable pulled.

use std::{
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use idevice::{
    IdeviceError, IdeviceService,
    pairing_file::PairingFile,
    provider::{IdeviceProvider, TcpProvider},
    services::{
        dvt::xctest::{TestConfig, XCUITestService, listener::XCUITestListener},
        heartbeat::HeartbeatClient,
        installation_proxy::InstallationProxyClient,
        lockdown::LockdownClient,
        mobile_image_mounter::ImageMounter,
    },
    usbmuxd::{UsbmuxdAddr, UsbmuxdConnection},
};
use plist::{Dictionary, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
};

pub const DEFAULT_BUNDLE: &str = "com.leeguoo.iphone-use.wda.6zpxg4kvvs.xctrunner";
const LABEL: &str = "iphone-use-legacy-launch";
const WIRELESS_DOMAIN: &str = "com.apple.mobile.wireless_lockdown";

/// Where the phone is and how to reach it.
#[derive(Debug, Clone, Default)]
pub struct Target {
    pub udid: Option<String>,
    /// Phone's LAN address. Set = Wi-Fi path; unset = USB through usbmuxd.
    pub host: Option<IpAddr>,
    /// Pair record plist. Unset = ask usbmuxd for the UDID's record.
    pub pair_record: Option<String>,
}

impl Target {
    pub fn transport(&self) -> &'static str {
        if self.host.is_some() { "wifi" } else { "usb" }
    }
}

fn elapsed(t0: Instant) -> String {
    format!("+{:.2}s", t0.elapsed().as_secs_f64())
}

pub async fn pairing_file(t: &Target) -> Result<PairingFile, String> {
    if let Some(path) = &t.pair_record {
        return PairingFile::read_from_file(path).map_err(|e| format!("pair record {path}: {e}"));
    }
    let udid = t
        .udid
        .as_deref()
        .ok_or("--udid or --pair-record required")?;
    let mut mux = UsbmuxdConnection::default()
        .await
        .map_err(|e| format!("usbmuxd: {e}"))?;
    mux.get_pair_record(udid)
        .await
        .map_err(|e| format!("usbmuxd pair record for {udid}: {e}"))
}

pub async fn provider(t: &Target) -> Result<Box<dyn IdeviceProvider>, String> {
    if let Some(host) = t.host {
        return Ok(Box::new(TcpProvider {
            addr: host,
            scope_id: None,
            pairing_file: pairing_file(t).await?,
            label: LABEL.into(),
        }));
    }
    let udid = t.udid.as_deref().ok_or("--udid required for USB")?;
    let mut mux = UsbmuxdConnection::default()
        .await
        .map_err(|e| format!("usbmuxd: {e}"))?;
    let dev = mux
        .get_device(udid)
        .await
        .map_err(|e| format!("device {udid}: {e}"))?;
    let addr = UsbmuxdAddr::from_env_var().unwrap_or_default();
    Ok(Box::new(dev.to_provider(addr, LABEL)))
}

/// Holds the Wi-Fi heartbeat (Marco/Polo) for as long as the task runs.
///
/// Over Wi-Fi, lockdownd hands each service socket to heartbeatd, which drops
/// the client unless the host holds a live heartbeat session. Without one,
/// every service closes right after its TLS handshake (the phone logs
/// "Could not receive message from client"), which is the `EOF` that go-ios
/// and libimobiledevice hit on network devices when Apple's usbmuxd is not the
/// one holding the device. Must be up before any other service is started.
pub async fn start_heartbeat(
    p: &dyn IdeviceProvider,
    t0: Instant,
) -> Result<JoinHandle<String>, String> {
    let mut hb = HeartbeatClient::connect(p)
        .await
        .map_err(|e| format!("heartbeat: {e}"))?;
    let first = hb
        .get_marco(15)
        .await
        .map_err(|e| format!("heartbeat marco: {e}"))?;
    hb.send_polo()
        .await
        .map_err(|e| format!("heartbeat polo: {e}"))?;
    println!("{} heartbeat up (interval {first}s)", elapsed(t0));
    Ok(tokio::spawn(async move {
        let mut interval = first;
        loop {
            match hb.get_marco(interval + 15).await {
                Ok(i) => interval = i,
                Err(e) => return format!("heartbeat lost: {e}"),
            }
            if let Err(e) = hb.send_polo().await {
                return format!("heartbeat polo failed: {e}");
            }
        }
    }))
}

async fn lockdown(p: &dyn IdeviceProvider) -> Result<LockdownClient, String> {
    let mut l = LockdownClient::connect(p)
        .await
        .map_err(|e| format!("lockdown: {e}"))?;
    let pf = p.get_pairing_file().await.map_err(|e| e.to_string())?;
    l.start_session(&pf)
        .await
        .map_err(|e| format!("lockdown session: {e}"))?;
    Ok(l)
}

async fn get_string(l: &mut LockdownClient, key: &str) -> String {
    match l.get_value(Some(key), None).await {
        Ok(v) => v.as_string().unwrap_or("?").to_owned(),
        Err(e) => format!("? ({e})"),
    }
}

/// Lockdown facts, plus whether Wi-Fi lockdown is on.
pub async fn info(t: &Target) -> Result<(), String> {
    let t0 = Instant::now();
    let p = provider(t).await?;
    let _hb = if t.host.is_some() {
        Some(start_heartbeat(p.as_ref(), t0).await?)
    } else {
        None
    };
    let mut l = lockdown(p.as_ref()).await?;
    for k in ["DeviceName", "ProductVersion", "UniqueDeviceID"] {
        println!("{k}: {}", get_string(&mut l, k).await);
    }
    let w = l
        .get_value(Some("EnableWifiConnections"), Some(WIRELESS_DOMAIN))
        .await
        .ok()
        .and_then(|v| v.as_boolean());
    println!("EnableWifiConnections: {w:?}");
    println!("took {}", elapsed(t0));
    Ok(())
}

/// Turns on Wi-Fi lockdown (what Finder's "Show this iPhone when on Wi-Fi"
/// sets). Needs one trusted connection, normally USB, and survives reboots.
pub async fn enable_wifi(t: &Target) -> Result<bool, String> {
    let p = provider(t).await?;
    let mut l = lockdown(p.as_ref()).await?;
    let domain = Some(WIRELESS_DOMAIN);
    let cur = l
        .get_value(Some("EnableWifiConnections"), domain)
        .await
        .ok();
    if cur.and_then(|v| v.as_boolean()) == Some(true) {
        return Ok(false);
    }
    l.set_value("EnableWifiConnections", Value::Boolean(true), domain)
        .await
        .map_err(|e| format!("set EnableWifiConnections: {e}"))?;
    Ok(true)
}

/// The testmanagerd service a usable Developer Disk Image provides.
pub const TESTMANAGERD: &str = "com.apple.testmanagerd.lockdown.secure";

/// What [`mount`] found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountOutcome {
    /// An image was mounted already and testmanagerd starts.
    AlreadyMounted,
    /// This call mounted the image and testmanagerd starts.
    Mounted,
    /// An image is mounted but lockdown answers testmanagerd with
    /// `InvalidService` (a stale or mismatched image; restart the phone).
    ServicesInvalid,
}

fn invalid_service(e: &IdeviceError) -> bool {
    matches!(e, IdeviceError::ServiceNotFound) || format!("{e:?} {e}").contains("InvalidService")
}

/// Starts testmanagerd once through a fresh lockdown session. A mounted
/// image that does not match the iOS still reads as mounted, but every
/// developer service then fails with `InvalidService`.
pub async fn developer_services(p: &dyn IdeviceProvider) -> Result<bool, String> {
    let mut l = lockdown(p).await?;
    match l.start_service(TESTMANAGERD).await {
        Ok(_) => Ok(true),
        Err(e) if invalid_service(&e) => Ok(false),
        Err(e) => Err(format!("start {TESTMANAGERD}: {e}")),
    }
}

/// Mounts the Developer Disk Image (`image`, `signature`) unless one is
/// mounted, then proves testmanagerd starts. With `remount`, an image that is
/// mounted but unusable is unmounted and mounted again once.
pub async fn mount(
    t: &Target,
    image: &std::path::Path,
    signature: &std::path::Path,
    remount: bool,
) -> Result<MountOutcome, String> {
    let t0 = Instant::now();
    let p = provider(t).await?;
    let _hb = if t.host.is_some() {
        Some(start_heartbeat(p.as_ref(), t0).await?)
    } else {
        None
    };
    let mut mounted_now = false;
    for attempt in 0..2 {
        let mut m = ImageMounter::connect(p.as_ref())
            .await
            .map_err(|e| format!("image mounter: {e}"))?;
        let present = m.lookup_image("Developer").await.is_ok();
        if !present {
            let image = tokio::fs::read(image)
                .await
                .map_err(|e| format!("read {}: {e}", image.display()))?;
            let signature = tokio::fs::read(signature)
                .await
                .map_err(|e| format!("read {}: {e}", signature.display()))?;
            m.mount_developer(&image, signature)
                .await
                .map_err(|e| format!("mount: {e}"))?;
            mounted_now = true;
            println!("{} mounted the Developer Disk Image", elapsed(t0));
        }
        drop(m);
        if developer_services(p.as_ref()).await? {
            println!("{} testmanagerd starts", elapsed(t0));
            return Ok(if mounted_now {
                MountOutcome::Mounted
            } else {
                MountOutcome::AlreadyMounted
            });
        }
        println!("{} testmanagerd: InvalidService", elapsed(t0));
        if !remount || attempt == 1 {
            break;
        }
        let mut m = ImageMounter::connect(p.as_ref())
            .await
            .map_err(|e| format!("image mounter: {e}"))?;
        m.unmount_image("/Developer")
            .await
            .map_err(|e| format!("unmount: {e}"))?;
        println!("{} unmounted the stale image; mounting again", elapsed(t0));
    }
    Ok(MountOutcome::ServicesInvalid)
}

/// Installs (or upgrades) an app bundle directory: AFC upload into
/// `PublicStaging`, then installation_proxy.
pub async fn install(t: &Target, app: &std::path::Path) -> Result<(), String> {
    let t0 = Instant::now();
    let p = provider(t).await?;
    let _hb = if t.host.is_some() {
        Some(start_heartbeat(p.as_ref(), t0).await?)
    } else {
        None
    };
    idevice::utils::installation::install_package(p.as_ref(), app, None)
        .await
        .map_err(|e| format!("install {}: {e}", app.display()))?;
    println!("{} installed {}", elapsed(t0), app.display());
    Ok(())
}

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub bundle_id: String,
    pub env: Dictionary,
    /// Runner HTTP port probed over the LAN to report readiness (Wi-Fi only).
    pub probe_port: Option<u16>,
    /// Stop after resolving the runner app; launches nothing.
    pub dry_run: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            bundle_id: DEFAULT_BUNDLE.into(),
            env: Dictionary::new(),
            probe_port: Some(8100),
            dry_run: false,
        }
    }
}

struct Events {
    t0: Instant,
}

impl XCUITestListener for Events {
    async fn did_begin_executing_test_plan(&mut self) -> Result<(), IdeviceError> {
        println!("{} test plan started", elapsed(self.t0));
        Ok(())
    }
    async fn did_finish_executing_test_plan(&mut self) -> Result<(), IdeviceError> {
        println!("{} test plan finished", elapsed(self.t0));
        Ok(())
    }
    async fn test_case_did_start(&mut self, class: &str, method: &str) -> Result<(), IdeviceError> {
        println!("{} case started {class}/{method}", elapsed(self.t0));
        Ok(())
    }
    async fn test_case_did_fail(
        &mut self,
        class: &str,
        method: &str,
        message: &str,
        file: &str,
        line: u64,
    ) -> Result<(), IdeviceError> {
        eprintln!(
            "{} case failed {class}/{method}: {message} ({file}:{line})",
            elapsed(self.t0)
        );
        Ok(())
    }
    async fn log_message(&mut self, m: &str) -> Result<(), IdeviceError> {
        println!("{} runner: {}", elapsed(self.t0), m.trim_end());
        Ok(())
    }
    async fn initialization_for_ui_testing_did_fail(
        &mut self,
        d: &str,
    ) -> Result<(), IdeviceError> {
        eprintln!("{} ui testing init failed: {d}", elapsed(self.t0));
        Ok(())
    }
    async fn did_fail_to_bootstrap(&mut self, d: &str) -> Result<(), IdeviceError> {
        eprintln!("{} bootstrap failed: {d}", elapsed(self.t0));
        Ok(())
    }
}

/// True when `GET /status` on the runner answers 200 directly over the LAN.
pub async fn probe_status(ip: IpAddr, port: u16) -> bool {
    let fut = async {
        let mut s = tokio::net::TcpStream::connect((ip, port)).await.ok()?;
        s.write_all(b"GET /status HTTP/1.0\r\nHost: phone\r\n\r\n")
            .await
            .ok()?;
        let mut buf = [0u8; 16];
        let n = s.read(&mut buf).await.ok()?;
        Some(is_http_200(&buf[..n]))
    };
    matches!(
        tokio::time::timeout(Duration::from_millis(800), fut).await,
        Ok(Some(true))
    )
}

fn is_http_200(head: &[u8]) -> bool {
    head.starts_with(b"HTTP/1.1 200") || head.starts_with(b"HTTP/1.0 200")
}

/// Launches the runner and blocks until it exits, the heartbeat dies, or
/// Ctrl-C. The runner stops with this call.
pub async fn launch(t: &Target, o: &LaunchOptions) -> Result<(), String> {
    let t0 = Instant::now();
    let p = provider(t).await?;
    let heartbeat = if t.host.is_some() {
        Some(start_heartbeat(p.as_ref(), t0).await?)
    } else {
        None
    };
    {
        let mut l = lockdown(p.as_ref()).await?;
        let ver = get_string(&mut l, "ProductVersion").await;
        println!(
            "{} lockdown ok over {}, iOS {ver}",
            elapsed(t0),
            t.transport()
        );
    }
    let mut ip = InstallationProxyClient::connect(p.as_ref())
        .await
        .map_err(|e| format!("installation_proxy: {e}"))?;
    let mut cfg = TestConfig::from_installation_proxy(&mut ip, &o.bundle_id, None)
        .await
        .map_err(|e| format!("runner {}: {e}", o.bundle_id))?;
    drop(ip);
    if !o.env.is_empty() {
        cfg.runner_env = Some(o.env.clone());
    }
    println!("{} runner app {}", elapsed(t0), cfg.runner_app_path);
    if o.dry_run {
        return Ok(());
    }

    if let (Some(host), Some(port)) = (t.host, o.probe_port) {
        tokio::spawn(async move {
            loop {
                if probe_status(host, port).await {
                    println!(
                        "{} ready: http://{host}:{port}/status answered over Wi-Fi",
                        elapsed(t0)
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
    }

    let svc = XCUITestService::new(Arc::from(p));
    let mut ev = Events { t0 };
    let hb_lost = async {
        match heartbeat {
            Some(h) => h.await.unwrap_or_else(|e| format!("heartbeat task: {e}")),
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        r = svc.run(cfg, &mut ev, None) => {
            r.map_err(|e| format!("xctest: {e}"))?;
            println!("{} runner exited", elapsed(t0));
            Ok(())
        }
        why = hb_lost => Err(format!("{} {why}; the runner stops with this session", elapsed(t0))),
        _ = tokio::signal::ctrl_c() => {
            println!("{} interrupted; the runner stops with this session", elapsed(t0));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_200_detection() {
        assert!(is_http_200(b"HTTP/1.1 200 OK\r\n"));
        assert!(is_http_200(b"HTTP/1.0 200 OK"));
        assert!(!is_http_200(b"HTTP/1.1 503 Service"));
        assert!(!is_http_200(b""));
    }

    #[test]
    fn transport_follows_host() {
        let mut t = Target::default();
        assert_eq!(t.transport(), "usb");
        t.host = Some("192.168.0.2".parse().unwrap());
        assert_eq!(t.transport(), "wifi");
    }
}
