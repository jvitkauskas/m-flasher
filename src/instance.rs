//! Keep the device scanner and commands in separate processes from competing.
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub const ALREADY_OPEN: &str = "M-Flasher is already open. Close the other instance before starting another one.";

pub fn acquire() -> io::Result<Option<File>> {
    #[cfg(unix)]
    // SAFETY: getuid has no preconditions. Separate users have separate locks.
    let name = format!("m-flasher-{}.lock", unsafe { libc::getuid() });
    #[cfg(windows)]
    let name = "m-flasher.lock";
    lock(&std::env::temp_dir().join(name))
}

fn lock(path: &Path) -> io::Result<Option<File>> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
    // Leave the file in place: deleting it could let processes lock different
    // files at the same path. The OS releases the lock even after a crash.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_another_instance_and_releases_on_exit() {
        let path = std::env::temp_dir().join(format!("m-flasher-test-{}.lock", std::process::id()));
        let first = lock(&path).unwrap().unwrap();
        assert!(lock(&path).unwrap().is_none());
        drop(first);
        assert!(lock(&path).unwrap().is_some());
        std::fs::remove_file(path).unwrap();
    }
}
