//! Keep ordinary GUI/CLI processes from competing, across users and launch environments.
use std::io;

pub const ALREADY_OPEN: &str = "M-Flasher is already open. Close the other instance before starting another one.";

pub struct Instance {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(unix)]
    _file: std::fs::File,
}

pub fn acquire() -> io::Result<Option<Instance>> {
    lock("M-Flasher.Instance")
}

#[cfg(windows)]
fn lock(name: &str) -> io::Result<Option<Instance>> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    // Global also covers other Windows login sessions. Neither the name nor
    // its lifetime depends on TMP/TEMP or on a file left behind after a crash.
    let name: Vec<u16> = format!("Global\\{name}").encode_utf16().chain(Some(0)).collect();
    // SAFETY: valid terminated name, default security, no initial ownership.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    let error = unsafe { GetLastError() };
    if handle.is_null() {
        // An instance owned by another user may deny access to its mutex.
        // It still excludes this launch; never proceed without the guard.
        return if error == ERROR_ACCESS_DENIED { Ok(None) } else { Err(io::Error::from_raw_os_error(error as i32)) };
    }
    if error == ERROR_ALREADY_EXISTS {
        // SAFETY: this call returned a valid handle, which we no longer need.
        unsafe { CloseHandle(handle) };
        return Ok(None);
    }
    Ok(Some(Instance { handle }))
}

#[cfg(windows)]
impl Drop for Instance {
    fn drop(&mut self) {
        // Existence, not mutex ownership, excludes other processes. The last
        // handle closes automatically on process termination, including crashes.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

#[cfg(unix)]
fn lock(name: &str) -> io::Result<Option<Instance>> {
    use std::fs::{OpenOptions, Permissions};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    // Fixed system namespace: root CLI, desktop users, and launches with a
    // different TMPDIR must all contend for the same inode.
    let path = std::path::Path::new("/tmp").join(format!("{name}.lock"));
    let file = match OpenOptions::new().write(true).read(true).create_new(true).mode(0o644).open(&path) {
        Ok(file) => {
            // Override a restrictive umask: other users need read access to
            // flock this file, but must not be able to change its contents.
            file.set_permissions(Permissions::from_mode(0o644))?;
            file
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // An exclusive flock works on a read-only descriptor on Unix.
            // Reject symlinks and avoid blocking if the path is a FIFO.
            OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path)?
        }
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the instance lock is not a regular file"));
    }
    match file.try_lock() {
        Ok(()) => Ok(Some(Instance { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
    // Never delete this file: unlinking it could let processes lock different
    // inodes at the same path. The OS releases the lock even after a crash.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_another_instance_and_releases_on_exit() {
        let name = format!("m-flasher-test-{}", std::process::id());
        let first = lock(&name).unwrap().unwrap();
        assert!(lock(&name).unwrap().is_none());
        drop(first);
        assert!(lock(&name).unwrap().is_some());
    }

    #[test]
    fn probe_child() {
        if let Ok(name) = std::env::var("MFLASHER_TEST_LOCK") {
            assert!(lock(&name).unwrap().is_none(), "child bypassed the instance lock");
        }
    }

    #[test]
    fn another_process_with_different_temp_settings_is_excluded() {
        let name = format!("m-flasher-child-test-{}", std::process::id());
        let _first = lock(&name).unwrap().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "instance::tests::probe_child", "--nocapture"])
            .env("MFLASHER_TEST_LOCK", &name)
            .env("TMPDIR", "/a-different-temp-directory")
            .env("TMP", "C:\\a-different-temp-directory")
            .env("TEMP", "C:\\another-temp-directory")
            .status().unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn other_users_can_open_the_shared_lock_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let name = format!("m-flasher-permission-test-{}", std::process::id());
        let first = lock(&name).unwrap().unwrap();
        assert_eq!(first._file.metadata().unwrap().permissions().mode() & 0o777, 0o644);
        assert!(lock(&name).unwrap().is_none());
        drop(first);
        assert!(lock(&name).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn root_and_an_unprivileged_user_contend_for_the_same_lock() {
        use std::os::unix::process::CommandExt;
        // This additional integration check runs when invoked as root (e.g.
        // the isolated Linux lock tests); ordinary test runs need no elevation.
        if unsafe { libc::geteuid() } != 0 { return }
        let name = format!("m-flasher-cross-user-test-{}", std::process::id());
        let _first = lock(&name).unwrap().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "instance::tests::probe_child", "--nocapture"])
            .env("MFLASHER_TEST_LOCK", &name)
            .env("TMPDIR", "/a-different-temp-directory")
            .uid(65534).gid(65534).status().unwrap();
        assert!(status.success());
    }
}
