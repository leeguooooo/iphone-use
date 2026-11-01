//! Cross-process exclusion between interactive setup and daemon bootstrap.
//! Keep the inode: unlinking a locked file would let another caller lock a
//! replacement inode. Dropping the guard explicitly releases ownership even
//! while a forked helper still has a descriptor before exec.

use std::io;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::os::unix::io::AsRawFd as _;
use std::path::Path;

#[derive(Debug)]
pub struct Guard {
    _file: std::fs::File,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // CLOEXEC closes inherited descriptors at exec, not at fork. Release
        // logical ownership now even if a helper has not reached exec yet.
        loop {
            // SAFETY: the guard still owns this live descriptor.
            let result = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
            if result == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
    }
}

pub fn try_acquire(state_dir: &Path) -> io::Result<Option<Guard>> {
    if state_dir.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runner state directory is not configured",
        ));
    }
    std::fs::create_dir_all(state_dir)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(state_dir.join("runner-transition.lock"))?;
    let meta = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runner transition lock must be a private regular file owned by this user",
        ));
    }
    // SAFETY: this function owns the live descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(Guard { _file: file }));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_excludes_other_transitions_and_is_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let guard = try_acquire(dir.path()).unwrap().unwrap();
        assert!(try_acquire(dir.path()).unwrap().is_none());
        drop(guard);
        assert!(try_acquire(dir.path()).unwrap().is_some());
        assert!(
            dir.path().join("runner-transition.lock").is_file(),
            "never unlink the shared inode"
        );
    }

    #[test]
    fn dropping_owner_releases_lock_even_if_a_non_owner_descriptor_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let guard = try_acquire(dir.path()).unwrap().unwrap();
        // dup, like fork before exec, keeps the same open file description.
        let inherited = guard._file.try_clone().unwrap();
        drop(guard);
        let acquired = try_acquire(dir.path()).unwrap().is_some();
        drop(inherited);
        assert!(acquired, "a non-owning inherited descriptor retained the transition lock");
    }

    #[test]
    fn a_spawned_child_does_not_keep_the_parent_guard_alive() {
        let dir = tempfile::tempdir().unwrap();
        let guard = try_acquire(dir.path()).unwrap().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        assert!(child.try_wait().unwrap().is_none());
        drop(guard);
        let acquired = try_acquire(dir.path()).unwrap().is_some();
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            acquired,
            "launchd/helper children must not inherit the interactive lock"
        );
    }

    #[test]
    fn refuses_a_symlink_without_touching_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sentinel");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("runner-transition.lock")).unwrap();
        assert!(try_acquire(dir.path()).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"keep");
    }
}
