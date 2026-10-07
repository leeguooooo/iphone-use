//! The setup status protocol (v1): `wda-setup-status.json`, which the daemon
//! and the web client read to show what setup is doing and what blocks it.
//!
//! One owner per attempt. Every writer — the setup run itself, its heartbeat
//! watcher, and the final write — takes the same `flock` on `<file>.lock`,
//! compares `run_id`, and atomically replaces the JSON. A watcher process
//! (`iphone-use setup-native status-watch`) beats every 15 s while the run is
//! active and marks the attempt `interrupted` if its owner dies without a
//! final write. Byte-compatible with the `_status_publish` helper the shell
//! setup used, so either can write the file the other started.

use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use super::sys;

/// Blockers a new attempt keeps visible until its own checks decide.
const STICKY_ON_BEGIN: &[&str] = &[
    "warp",
    "proxy",
    "usb",
    "trust",
    "ddi",
    "automation_mode_disabled",
    "xcode_too_old",
    "wda",
];

#[derive(Debug, Clone)]
pub enum Op {
    Begin,
    Phase {
        phase: String,
        blocked: String,
        message: String,
    },
    Heartbeat,
    Finish(i32),
    Abandoned,
}

#[derive(Debug, Clone)]
pub struct Owner {
    pub path: PathBuf,
    pub run_id: String,
    pub pid: u32,
    pub start: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Apply `op` under the lock. `Ok(false)` when the attempt no longer owns the
/// file (a newer run began) or a heartbeat/abandon has nothing to do.
pub fn publish(owner: &Owner, op: &Op) -> std::io::Result<bool> {
    let lock_path = PathBuf::from(format!("{}.lock", owner.path.display()));
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)?;
    {
        use std::os::unix::fs::MetadataExt as _;
        let meta = lock.metadata()?;
        if !meta.file_type().is_file() || meta.uid() != sys::uid() {
            return Err(std::io::Error::other("unsafe setup status lock"));
        }
    }
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&lock);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::other("setup status lock timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // The lock releases when `lock` drops.
    if std::fs::symlink_metadata(&owner.path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("refusing symlinked setup status"));
    }
    let mut data = read_object(&owner.path);
    let Some(next) = apply(&mut data, owner, op, now()) else {
        return Ok(false);
    };
    let mut text = serde_json::to_string(&Value::Object(next)).map_err(std::io::Error::other)?;
    text.push('\n');
    sys::write_atomic(&owner.path, text.as_bytes(), 0o600)?;
    drop(lock);
    Ok(true)
}

fn read_object(path: &Path) -> Map<String, Value> {
    let mut text = String::new();
    if std::fs::File::open(path)
        .and_then(|mut file| file.read_to_string(&mut text))
        .is_err()
    {
        return Map::new();
    }
    match serde_json::from_str(&text) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// The pure state transition; `None` means "write nothing".
pub fn apply(
    data: &mut Map<String, Value>,
    owner: &Owner,
    op: &Op,
    now: u64,
) -> Option<Map<String, Value>> {
    let mut out = data.clone();
    match op {
        Op::Begin => {
            let blocker = data.get("blocked_on").and_then(Value::as_str).unwrap_or("");
            let kept = if STICKY_ON_BEGIN.contains(&blocker) {
                blocker
            } else {
                ""
            };
            out = json!({
                "schema_version": 1,
                "run_id": owner.run_id,
                "owner_pid": owner.pid,
                "owner_start": owner.start,
                "phase": "starting",
                "phase_started_at": now,
                "blocked_on": kept,
                "message": "starting setup",
                "active": true,
                "terminal": false,
            })
            .as_object()
            .cloned()
            .unwrap_or_default();
        }
        _ if data.get("run_id").and_then(Value::as_str) != Some(owner.run_id.as_str()) => {
            return None
        }
        Op::Phase {
            phase,
            blocked,
            message,
        } => {
            if data.get("phase").and_then(Value::as_str) != Some(phase.as_str()) {
                out.insert("phase_started_at".into(), json!(now));
            }
            let terminal = phase == "ready" || phase.ends_with("-fail") || phase == "lock-backoff";
            out.insert("phase".into(), json!(phase));
            out.insert("blocked_on".into(), json!(blocked));
            out.insert("message".into(), json!(message));
            out.insert("active".into(), json!(!terminal));
            out.insert("terminal".into(), json!(terminal));
        }
        Op::Heartbeat => {
            let active = data.get("active").and_then(Value::as_bool).unwrap_or(false);
            let terminal = data
                .get("terminal")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !active || terminal {
                return None;
            }
        }
        Op::Finish(_) | Op::Abandoned => {
            let terminal = data
                .get("terminal")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if matches!(op, Op::Abandoned) && terminal {
                return None;
            }
            let code = match op {
                Op::Finish(code) => *code,
                _ => 137,
            };
            let previous = data
                .get("phase")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| "starting".into());
            let phase = if matches!(op, Op::Abandoned) {
                "interrupted".to_string()
            } else if code == 130 {
                "stopped".to_string()
            } else if code != 0 {
                if previous.ends_with("-fail") {
                    previous.clone()
                } else {
                    format!("{previous}-fail")
                }
            } else if previous == "ready" {
                "ready".to_string()
            } else {
                "completed".to_string()
            };
            out.insert("phase".into(), json!(phase));
            out.insert("last_phase".into(), json!(previous));
            out.insert("active".into(), json!(false));
            out.insert("terminal".into(), json!(true));
            out.insert("exit_code".into(), json!(code));
            out.insert("ended_at".into(), json!(now));
        }
    }
    out.insert("ts".into(), json!(now));
    out.insert("heartbeat_ts".into(), json!(now));
    Some(out)
}

/// The blocker the last attempt left, if it is one a new attempt keeps.
pub fn previous_blocker(path: &Path) -> String {
    let data = read_object(path);
    let blocker = data.get("blocked_on").and_then(Value::as_str).unwrap_or("");
    if STICKY_ON_BEGIN.contains(&blocker) {
        blocker.to_string()
    } else {
        String::new()
    }
}

pub fn phase_is(path: &Path, phase: &str) -> bool {
    read_object(path).get("phase").and_then(Value::as_str) == Some(phase)
}

/// One setup attempt's ownership of the status file, with its watcher.
pub struct Run {
    pub owner: Owner,
    watcher: Option<std::process::Child>,
}

impl Run {
    /// Become the owner: write `begin`, then start the heartbeat watcher.
    pub fn begin(path: &Path) -> std::io::Result<Run> {
        let pid = std::process::id();
        let start = sys::ps_lstart(pid);
        if start.is_empty() {
            return Err(std::io::Error::other(
                "could not read this process's start time",
            ));
        }
        let mut random = [0u8; 4];
        let _ = getrandom::fill(&mut random);
        let owner = Owner {
            path: path.to_path_buf(),
            run_id: format!("{pid}-{}-{}", now(), u32::from_le_bytes(random)),
            pid,
            start,
        };
        publish(&owner, &Op::Begin)?;
        let watcher = std::env::current_exe().ok().and_then(|exe| {
            std::process::Command::new(exe)
                .args(["setup-native", "status-watch"])
                .arg(&owner.path)
                .args([&owner.run_id, &owner.pid.to_string(), &owner.start])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()
        });
        Ok(Run { owner, watcher })
    }

    /// `_setstatus`: best effort, never fails the run.
    pub fn phase(&self, phase: &str, blocked: &str, message: &str) {
        let _ = publish(
            &self.owner,
            &Op::Phase {
                phase: phase.into(),
                blocked: blocked.into(),
                message: message.into(),
            },
        );
    }

    pub fn finish(mut self, code: i32) {
        let _ = publish(&self.owner, &Op::Finish(code));
        if let Some(mut watcher) = self.watcher.take() {
            let _ = watcher.kill();
            let _ = watcher.wait();
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(mut watcher) = self.watcher.take() {
            let _ = watcher.kill();
            let _ = watcher.wait();
        }
    }
}

/// `setup-native status-watch <file> <run_id> <owner_pid> <owner_start>`:
/// beat every 15 s while the owner lives; mark the attempt interrupted when
/// the owner (our parent) goes away without a final write.
pub fn watch(args: &[String]) -> i32 {
    let [path, run_id, pid, start] = args else {
        return 2;
    };
    let Ok(pid) = pid.parse::<u32>() else {
        return 2;
    };
    let owner = Owner {
        path: PathBuf::from(path),
        run_id: run_id.clone(),
        pid,
        start: start.clone(),
    };
    let mut next_heartbeat = Instant::now() + Duration::from_secs(15);
    loop {
        // SAFETY: getppid has no preconditions.
        if unsafe { libc::getppid() } as u32 != owner.pid {
            let _ = publish(&owner, &Op::Abandoned);
            return 0;
        }
        if Instant::now() >= next_heartbeat {
            if sys::ps_lstart(owner.pid) != owner.start {
                let _ = publish(&owner, &Op::Abandoned);
                return 0;
            }
            if !publish(&owner, &Op::Heartbeat).unwrap_or(false) {
                return 0;
            }
            next_heartbeat = Instant::now() + Duration::from_secs(15);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(path: &Path, run: &str) -> Owner {
        Owner {
            path: path.to_path_buf(),
            run_id: run.into(),
            pid: 4242,
            start: "Wed Oct  7 01:02:03 2026".into(),
        }
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn begin_keeps_only_sticky_blockers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wda-setup-status.json");
        std::fs::write(&path, r#"{"blocked_on":"trust","run_id":"old"}"#).unwrap();
        assert!(publish(&owner(&path, "a"), &Op::Begin).unwrap());
        let value = read(&path);
        assert_eq!(value["blocked_on"], "trust");
        assert_eq!(value["phase"], "starting");
        assert_eq!(value["active"], true);
        assert_eq!(value["schema_version"], 1);
        std::fs::write(&path, r#"{"blocked_on":"locked"}"#).unwrap();
        publish(&owner(&path, "b"), &Op::Begin).unwrap();
        assert_eq!(read(&path)["blocked_on"], "");
    }

    #[test]
    fn a_newer_run_owns_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        publish(&owner(&path, "a"), &Op::Begin).unwrap();
        publish(&owner(&path, "b"), &Op::Begin).unwrap();
        let phase = Op::Phase {
            phase: "prereq".into(),
            blocked: "".into(),
            message: "x".into(),
        };
        assert!(!publish(&owner(&path, "a"), &phase).unwrap());
        assert!(publish(&owner(&path, "b"), &phase).unwrap());
        assert_eq!(read(&path)["phase"], "prereq");
    }

    #[test]
    fn terminal_phases_and_finish_codes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let o = owner(&path, "a");
        publish(&o, &Op::Begin).unwrap();
        publish(
            &o,
            &Op::Phase {
                phase: "building".into(),
                blocked: "".into(),
                message: "m".into(),
            },
        )
        .unwrap();
        assert_eq!(read(&path)["terminal"], false);
        publish(&o, &Op::Finish(1)).unwrap();
        let value = read(&path);
        assert_eq!(value["phase"], "building-fail");
        assert_eq!(value["last_phase"], "building");
        assert_eq!(value["exit_code"], 1);
        assert!(
            !publish(&o, &Op::Heartbeat).unwrap(),
            "no heartbeat after terminal"
        );
        assert!(
            !publish(&o, &Op::Abandoned).unwrap(),
            "abandon keeps a terminal record"
        );

        publish(&o, &Op::Begin).unwrap();
        publish(
            &o,
            &Op::Phase {
                phase: "ready".into(),
                blocked: "".into(),
                message: "ok".into(),
            },
        )
        .unwrap();
        assert_eq!(read(&path)["terminal"], true);
        publish(&o, &Op::Finish(0)).unwrap();
        assert_eq!(read(&path)["phase"], "ready");

        publish(&o, &Op::Begin).unwrap();
        publish(&o, &Op::Finish(130)).unwrap();
        assert_eq!(read(&path)["phase"], "stopped");

        publish(&o, &Op::Begin).unwrap();
        publish(
            &o,
            &Op::Phase {
                phase: "lock-backoff".into(),
                blocked: "locked".into(),
                message: "m".into(),
            },
        )
        .unwrap();
        assert_eq!(read(&path)["terminal"], true);
        assert_eq!(read(&path)["active"], false);

        publish(&o, &Op::Begin).unwrap();
        publish(&o, &Op::Abandoned).unwrap();
        assert_eq!(read(&path)["phase"], "interrupted");
        assert_eq!(read(&path)["exit_code"], 137);
    }

    #[test]
    fn output_is_compact_json_the_shell_greps_can_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let o = owner(&path, "a");
        publish(&o, &Op::Begin).unwrap();
        publish(
            &o,
            &Op::Phase {
                phase: "ready".into(),
                blocked: "".into(),
                message: "ok".into(),
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""phase":"ready""#), "{text}");
        assert!(text.contains(r#""blocked_on":"""#), "{text}");
        assert!(text.ends_with('\n'));
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn refuses_a_symlinked_status_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.json");
        std::fs::write(&target, "{}").unwrap();
        let path = dir.path().join("s.json");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(publish(&owner(&path, "a"), &Op::Begin).is_err());
    }
}
