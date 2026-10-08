//! Size caps for the logs launchd and the supervisor point at our stdio.
//!
//! Nothing ever rotated `iphone-use.log/.err`, `wda-agent.log` or the relay
//! logs: a supervisor stuck in a crash loop grew them to gigabytes. Each
//! long-running process now caps the files behind its own stdout/stderr, found
//! with `F_GETPATH`, so it works for every path the installer or the supervisor
//! chose without either having to say. A file over the cap moves to `.1`
//! (older copies shift to `.2`, `.3`), and the live file keeps only its last
//! [`KEEP_TAIL`] bytes, cut at a line start: writers hold it open with
//! `O_APPEND`, so they keep appending, and readers of the latest round (the
//! daemon parses `wda-agent.log`) still find it.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A log may grow to this size before it is rotated.
pub const CAP_BYTES: u64 = 20 * 1024 * 1024;
/// How much of the newest output stays in the live file after a rotation.
pub const KEEP_TAIL: u64 = 512 * 1024;
/// Rotated copies kept next to the live file (`.1` newest).
pub const KEEP_ROTATED: u32 = 3;
/// How often a long-running process re-checks its logs.
pub const CHECK_EVERY: Duration = Duration::from_secs(300);

/// Rotate `path` when it is a regular file larger than `cap`. Returns whether
/// it rotated. Failures leave the file as it was (a log cap must never take a
/// process down).
pub fn cap_file(path: &Path, cap: u64, keep_tail: u64, keep_rotated: u32) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() <= cap {
        return false;
    }
    let tail = match read_tail(path, keep_tail) {
        Ok(tail) => tail,
        Err(_) => return false,
    };
    // Shift .{n-1} → .n … .1 → .2, then copy the whole live file to .1.
    for n in (1..keep_rotated).rev() {
        let from = rotated(path, n);
        if from.exists() {
            let _ = fs::rename(&from, rotated(path, n + 1));
        }
    }
    if keep_rotated > 0 && fs::copy(path, rotated(path, 1)).is_err() {
        return false;
    }
    // Truncate in place (never rename the live file: its writers would keep
    // writing to the renamed inode) and put the tail back.
    let Ok(mut file) = OpenOptions::new().write(true).open(path) else {
        return false;
    };
    if file.set_len(0).is_err() {
        return false;
    }
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.write_all(&tail);
    true
}

fn rotated(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// The last `keep` bytes of `path`, starting after a newline so the live file
/// never begins mid-line.
fn read_tail(path: &Path, keep: u64) -> std::io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(keep);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity(keep.min(len) as usize);
    file.read_to_end(&mut buf)?;
    if start > 0 {
        match buf.iter().position(|b| *b == b'\n') {
            Some(i) => {
                buf.drain(..=i);
            }
            None => buf.clear(),
        }
    }
    Ok(buf)
}

/// The path of the file behind `fd`, when it is a regular file.
#[cfg(target_os = "macos")]
fn fd_path(fd: i32) -> Option<PathBuf> {
    use std::ffi::CStr;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes a NUL-terminated path of at most PATH_MAX bytes.
    let rc = unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) };
    if rc == -1 {
        return None;
    }
    let path = CStr::from_bytes_until_nul(&buf).ok()?.to_str().ok()?;
    let path = PathBuf::from(path);
    fs::metadata(&path).ok().filter(|m| m.is_file())?;
    Some(path)
}

#[cfg(not(target_os = "macos"))]
fn fd_path(_fd: i32) -> Option<PathBuf> {
    None
}

/// The distinct regular files behind this process's stdout and stderr.
pub fn own_stdio_logs() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = [1, 2].into_iter().filter_map(fd_path).collect();
    paths.dedup();
    paths
}

/// Cap this process's own stdout/stderr log files now and then every
/// [`CHECK_EVERY`], on a background thread. A no-op when stdio is a terminal.
pub fn spawn_own_stdio_cap() {
    let paths = own_stdio_logs();
    if paths.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("logcap".into())
        .spawn(move || loop {
            for path in &paths {
                cap_file(path, CAP_BYTES, KEEP_TAIL, KEEP_ROTATED);
            }
            std::thread::sleep(CHECK_EVERY);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_small_log_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("a.log");
        write(&log, b"line 1\nline 2\n");
        assert!(!cap_file(&log, 1024, 64, 3));
        assert_eq!(fs::read(&log).unwrap(), b"line 1\nline 2\n");
        assert!(!rotated(&log, 1).exists());
    }

    #[test]
    fn an_oversized_log_rotates_and_keeps_its_last_whole_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("a.log");
        let body: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
        write(&log, body.as_bytes());
        assert!(cap_file(&log, 100, 40, 3));
        // The whole old content is in .1.
        assert_eq!(fs::read_to_string(rotated(&log, 1)).unwrap(), body);
        // The live file keeps only whole trailing lines within the tail budget.
        let live = fs::read_to_string(&log).unwrap();
        assert!(live.len() <= 40, "{live:?}");
        assert!(live.ends_with("line 199\n"), "{live:?}");
        assert!(live.starts_with("line "), "starts mid-line: {live:?}");
    }

    #[test]
    fn rotated_copies_shift_and_the_oldest_falls_off() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("a.log");
        for round in 0..5 {
            write(&log, format!("round {round}\n").repeat(50).as_bytes());
            assert!(cap_file(&log, 10, 0, 3));
        }
        assert!(fs::read_to_string(rotated(&log, 1))
            .unwrap()
            .starts_with("round 4"));
        assert!(fs::read_to_string(rotated(&log, 2))
            .unwrap()
            .starts_with("round 3"));
        assert!(fs::read_to_string(rotated(&log, 3))
            .unwrap()
            .starts_with("round 2"));
        assert!(!rotated(&log, 4).exists());
    }

    #[test]
    fn an_append_mode_writer_keeps_appending_after_a_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("a.log");
        let mut writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        writer.write_all("x\n".repeat(100).as_bytes()).unwrap();
        assert!(cap_file(&log, 50, 4, 1));
        writer.write_all(b"after\n").unwrap();
        let live = fs::read_to_string(&log).unwrap();
        assert!(live.ends_with("after\n"), "{live:?}");
        assert!(
            live.len() < 50,
            "the writer re-grew the old size: {}",
            live.len()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_file_behind_a_descriptor_is_found_and_a_pipe_is_not() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("own.log");
        let file = fs::File::create(&log).unwrap();
        // A spare descriptor number, so the test never touches real stdio.
        let spare = 200;
        // SAFETY: dup2 onto an unused descriptor; closed below.
        assert_ne!(unsafe { libc::dup2(file.as_raw_fd(), spare) }, -1);
        let found = fd_path(spare).map(|p| p.canonicalize().unwrap());
        assert_eq!(found, Some(log.canonicalize().unwrap()));
        let mut pipe = [0i32; 2];
        // SAFETY: pipe(2) fills two descriptors; dup2 the read end onto `spare`.
        unsafe {
            assert_eq!(libc::pipe(pipe.as_mut_ptr()), 0);
            libc::dup2(pipe[0], spare);
        }
        assert_eq!(fd_path(spare), None);
        // SAFETY: closing descriptors this test opened.
        unsafe {
            libc::close(spare);
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
    }

    #[test]
    fn a_missing_or_non_regular_path_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!cap_file(&dir.path().join("nope.log"), 1, 0, 3));
        assert!(!cap_file(dir.path(), 1, 0, 3));
    }
}
