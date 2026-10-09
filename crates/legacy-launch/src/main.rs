//! iphone-use-legacy-launch: start and hold the XCTest runner on iOS 15/16.
//!
//!   iphone-use-legacy-launch launch --udid U [--host IP] [--pair-record F]
//!       [--bundle-id B] [--env K=V]... [--probe-port 8100] [--dry-run]
//!   iphone-use-legacy-launch enable-wifi --udid U
//!   iphone-use-legacy-launch mount --udid U [--host IP] --image DMG --signature SIG [--remount]
//!       (exit 3: the image is mounted but developer services are InvalidService)
//!   iphone-use-legacy-launch install --udid U [--host IP] --app DIR
//!   iphone-use-legacy-launch info --udid U [--host IP] [--pair-record F]
//!
//! `--host` selects Wi-Fi: lockdown at <IP>:62078, nothing through usbmuxd.
//! Without `--pair-record`, the pair record is read from usbmuxd by UDID.
//! `RUST_LOG=debug` prints the protocol traffic.

use legacy_launch::{LaunchOptions, Target};
use plist::Value;

#[derive(Debug)]
struct Args {
    cmd: String,
    target: Target,
    opts: LaunchOptions,
    image: Option<String>,
    signature: Option<String>,
    app: Option<String>,
    remount: bool,
}

fn parse_args(argv: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut it = argv.into_iter();
    let cmd = it
        .next()
        .ok_or("missing command: launch | enable-wifi | info")?;
    let mut a = Args {
        cmd,
        target: Target::default(),
        opts: LaunchOptions::default(),
        image: None,
        signature: None,
        app: None,
        remount: false,
    };
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--udid" => a.target.udid = Some(val()?),
            "--host" => a.target.host = Some(val()?.parse().map_err(|e| format!("--host: {e}"))?),
            "--pair-record" => a.target.pair_record = Some(val()?),
            "--bundle-id" => a.opts.bundle_id = val()?,
            "--probe-port" => {
                a.opts.probe_port = Some(val()?.parse().map_err(|e| format!("--probe-port: {e}"))?)
            }
            "--no-probe" => a.opts.probe_port = None,
            "--dry-run" => a.opts.dry_run = true,
            "--image" => a.image = Some(val()?),
            "--signature" => a.signature = Some(val()?),
            "--app" => a.app = Some(val()?),
            "--remount" => a.remount = true,
            "--env" => {
                let kv = val()?;
                let (k, v) = kv
                    .split_once('=')
                    .filter(|(k, _)| !k.is_empty())
                    .ok_or(format!("--env wants K=V, got {kv}"))?;
                a.opts.env.insert(k.into(), Value::String(v.into()));
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(a)
}

#[tokio::main]
async fn main() {
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .init();
    }
    let a = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let r = match a.cmd.as_str() {
        "launch" => legacy_launch::launch(&a.target, &a.opts).await,
        "info" => legacy_launch::info(&a.target).await,
        "enable-wifi" => legacy_launch::enable_wifi(&a.target).await.map(|changed| {
            println!(
                "EnableWifiConnections {}",
                if changed {
                    "set to true"
                } else {
                    "already true"
                }
            )
        }),
        "mount" => match (&a.image, &a.signature) {
            (Some(image), Some(signature)) => legacy_launch::mount(
                &a.target,
                std::path::Path::new(image),
                std::path::Path::new(signature),
                a.remount,
            )
            .await
            .map(|outcome| {
                println!("outcome {outcome:?}");
                if outcome == legacy_launch::MountOutcome::ServicesInvalid {
                    std::process::exit(3);
                }
            }),
            _ => Err("mount needs --image and --signature".into()),
        },
        "install" => match &a.app {
            Some(app) => legacy_launch::install(&a.target, std::path::Path::new(app)).await,
            None => Err("install needs --app".into()),
        },
        c => Err(format!("unknown command {c}")),
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Result<Args, String> {
        parse_args(s.split_whitespace().map(String::from))
    }

    #[test]
    fn wifi_launch_args() {
        let a =
            args("launch --udid U --host 192.168.0.59 --env A=1 --env B=x=y --dry-run").unwrap();
        assert_eq!(a.cmd, "launch");
        assert_eq!(a.target.transport(), "wifi");
        assert_eq!(a.opts.env.get("B").and_then(|v| v.as_string()), Some("x=y"));
        assert!(a.opts.dry_run);
        assert_eq!(a.opts.probe_port, Some(8100));
    }

    #[test]
    fn mount_and_install_args() {
        let a = args("mount --udid U --image /d/i.dmg --signature /d/i.sig --remount").unwrap();
        assert_eq!(a.cmd, "mount");
        assert_eq!(a.image.as_deref(), Some("/d/i.dmg"));
        assert!(a.remount);
        let a = args("install --udid U --host 192.168.0.59 --app /x/R.app").unwrap();
        assert_eq!(a.app.as_deref(), Some("/x/R.app"));
        assert_eq!(a.target.transport(), "wifi");
    }

    #[test]
    fn rejects_bad_args() {
        assert!(args("launch --host nope").is_err());
        assert!(args("launch --env =1").is_err());
        assert!(args("launch --udid").is_err());
        assert!(args("launch --bogus").is_err());
        assert!(args("").is_err());
    }
}
