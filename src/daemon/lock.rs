//! Single-instance lock (design 7.3): an exclusive `flock` on the lock file,
//! held for the daemon's lifetime and released by the OS when it exits.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub struct InstanceLock {
    _file: File,
}

pub fn acquire(path: &Path) -> Result<InstanceLock> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: flock on a valid descriptor owned by `file`.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            let mut holder = String::new();
            let _ = file.read_to_string(&mut holder);
            bail!("another ats daemon is running (pid {})", holder.trim());
        }
        return Err(err).context("locking the daemon lock file");
    }
    file.set_len(0)?;
    file.rewind()?;
    write!(file, "{}", std::process::id())?;
    file.sync_all()?;
    Ok(InstanceLock { _file: file })
}

#[cfg(test)]
mod tests {
    #[test]
    fn second_acquire_fails_until_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        let first = super::acquire(&path).unwrap();
        let err = super::acquire(&path).err().expect("second lock must fail");
        assert!(err.to_string().contains(&std::process::id().to_string()));
        drop(first);
        super::acquire(&path).unwrap();
    }
}
