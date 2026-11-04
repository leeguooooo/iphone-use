//! Internal bounded USB reader used by the packaged Files receiver.
use anyhow::{bail, Context, Result};
use clap::Parser;
use serde_json::Value;
use std::{ffi::OsString, path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(name = "iphone-use files __native-read")]
struct ReadArgs {
    #[arg(long)]
    device: String,
    #[arg(long)]
    bundle: String,
    #[arg(long)]
    source: String,
    #[arg(long)]
    destination: PathBuf,
    #[arg(long)]
    max_bytes: usize,
    #[arg(long)]
    owner: String,
    #[arg(long)]
    instance: String,
    #[arg(long)]
    lease: String,
}

fn validate(args: &ReadArgs) -> Result<Value> {
    let parts: Vec<_> = args.source.split('/').collect();
    if parts.len() < 2
        || parts[0] != "Documents"
        || parts
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
        || args.source.contains(['\\', '\0'])
        || args.max_bytes > 64 * 1024 * 1024
        || args.owner.is_empty()
        || !args.destination.is_absolute()
        || !crate::app_lookup::valid_bundle(&args.bundle)
        || !args.bundle.ends_with(".xctrunner")
    {
        bail!("Invalid bounded Runner Documents request");
    }
    let lease: Value = serde_json::from_str(&args.lease).context("Invalid lease JSON")?;
    let nonce = lease["daemon_instance"].as_str().unwrap_or_default();
    if nonce.len() != 64
        || !nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || ["owner_epoch", "privacy_epoch", "lifecycle_stamp"]
            .iter()
            .any(|key| {
                let Some(value) = lease[*key].as_str() else {
                    return true;
                };
                value
                    .parse::<u64>()
                    .ok()
                    .is_none_or(|number| number.to_string() != value)
            })
    {
        bail!("Invalid control lease identity");
    }
    Ok(lease)
}

fn check_status(args: &ReadArgs, expected: &Value, status: &Value) -> Result<()> {
    let lease = &status["control_lease"];
    if status["drivable"] != true
        || status["udid"] != args.device
        || status["owner"] != args.owner
        || lease["owner"] != args.owner
        || lease["private"] != false
        || [
            "daemon_instance",
            "owner_epoch",
            "privacy_epoch",
            "lifecycle_stamp",
        ]
        .iter()
        .any(|key| lease[*key] != expected[*key])
    {
        bail!("Control lease changed or unavailable; native read stopped");
    }
    Ok(())
}

async fn guard(args: &ReadArgs, expected: &Value) -> Result<()> {
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command
        .args(["status", "--instance", &args.instance, "--json"])
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .context("Control lease query timed out")??;
    if !result.status.success() {
        bail!("Selected daemon control lease unavailable");
    }
    let status: Value = serde_json::from_slice(&result.stdout).context("Invalid daemon status")?;
    check_status(args, expected, &status)
}

async fn monitor(args: &ReadArgs, expected: &Value) -> Result<()> {
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        guard(args, expected).await?;
    }
}

async fn receive(args: &ReadArgs, expected: &Value) -> Result<usize> {
    use std::os::unix::fs::MetadataExt;
    guard(args, expected).await?;
    let mut device = crate::lockdown::open_runner_documents(&args.device, &args.bundle).await?;
    guard(args, expected).await?;
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&args.destination)
        .await
        .context("Create private native staging file")?;
    let identity = output.metadata().await?;
    let copied = async {
        let info = tokio::select! {
            copied = device.copy_regular_file(&args.source, args.max_bytes, &mut output) => copied?,
            changed = monitor(args, expected) => { changed?; bail!("Native lease monitor stopped"); }
        };
        output.sync_all().await?;
        guard(args, expected).await?;
        Ok::<_, anyhow::Error>(info.size as usize)
    }.await;
    drop(output);
    if copied.is_err() {
        // Never delete a different file if this private staging path was rebound.
        if let Ok(current) = tokio::fs::symlink_metadata(&args.destination).await {
            if current.dev() == identity.dev() && current.ino() == identity.ino() {
                let _ = tokio::fs::remove_file(&args.destination).await;
            }
        }
    }
    copied
}

pub fn main(args: &[OsString]) -> i32 {
    let args = match ReadArgs::try_parse_from(args) {
        Ok(args) => args,
        Err(error) => {
            let _ = error.print();
            return 2;
        }
    };
    let result = (|| {
        let expected = validate(&args)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(60), receive(&args, &expected))
                .await
                .context("Native read timed out; discard staging")?
        })
    })();
    match result {
        Ok(bytes) => {
            println!(
                "{}",
                serde_json::json!({"bytes":bytes,"provider":"native-usb-afc"})
            );
            0
        }
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({"event":"error","detail":format!("{error:#}")})
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args() -> ReadArgs {
        ReadArgs {
            device: "phone".into(),
            bundle: "owned.Runner.xctrunner".into(),
            source: "Documents/Send to Mac/saved.pdf".into(),
            destination: "/private/staging".into(),
            max_bytes: 100,
            owner: "me".into(),
            instance: "i13".into(),
            lease: serde_json::json!({"daemon_instance":"a".repeat(64),"owner_epoch":"1",
                "privacy_epoch":"2","lifecycle_stamp":"3"})
            .to_string(),
        }
    }
    #[test]
    fn rejects_invalid_native_read_before_device_or_output_access() {
        for source in [
            "/Documents/a",
            "Documents/../a",
            "Documents//a",
            "Library/a",
            "Documents/a\\b",
        ] {
            let mut args = args();
            args.source = source.into();
            assert!(validate(&args).is_err());
        }
        let mut value = args();
        value.max_bytes = 64 * 1024 * 1024 + 1;
        assert!(validate(&value).is_err());
        let mut value = args();
        value.bundle = "other.app".into();
        assert!(validate(&value).is_err());
        let mut value = args();
        value.destination = "relative".into();
        assert!(validate(&value).is_err());
        let mut value = args();
        value.owner.clear();
        assert!(validate(&value).is_err());
        let mut value = args();
        value.lease = "{}".into();
        assert!(validate(&value).is_err());
        for generation in ["01", "-1", "18446744073709551616"] {
            let mut value = args();
            let mut lease: Value = serde_json::from_str(&value.lease).unwrap();
            lease["owner_epoch"] = generation.into();
            value.lease = lease.to_string();
            assert!(validate(&value).is_err());
        }
    }
    #[test]
    fn native_scope_rejects_reacquired_owner_privacy_and_daemon_changes() {
        let args = args();
        let expected = validate(&args).unwrap();
        let mut status = serde_json::json!({"drivable":true,"owner":"me","udid":"phone","control_lease":expected});
        status["control_lease"]["owner"] = "me".into();
        status["control_lease"]["private"] = false.into();
        assert!(check_status(&args, &expected, &status).is_ok());
        for (key, value) in [
            ("owner_epoch", Value::from("2")),
            ("privacy_epoch", Value::from("3")),
            ("lifecycle_stamp", Value::from("4")),
            ("daemon_instance", Value::from("b".repeat(64))),
            ("private", Value::Bool(true)),
            ("owner", Value::from("other")),
        ] {
            let mut changed = status.clone();
            changed["control_lease"][key] = value;
            assert!(check_status(&args, &expected, &changed).is_err());
        }
        for (key, value) in [
            ("owner", Value::Null),
            ("udid", Value::from("other")),
            ("drivable", Value::Bool(false)),
        ] {
            let mut changed = status.clone();
            changed[key] = value;
            assert!(check_status(&args, &expected, &changed).is_err());
        }
    }
}
