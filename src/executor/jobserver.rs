//! A job's jobserver pool (design 10.5), held by its executor: a fifo in
//! the form of make 4.4 (`--jobserver-auth=fifo:PATH`), which make, ninja
//! 1.13+ and cargo use. Each client also runs one job without a token.
//! Lowering the target takes tokens back only as clients return them.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub struct Pool {
    pub path: PathBuf,
    fd: i32,
    target: Arc<AtomicU32>,
}

impl Pool {
    /// Creates the fifo with `tokens` tokens. The executor keeps it open
    /// for reading and writing, so clients never see it closed.
    pub fn create(dir: &std::path::Path, job: &str, tokens: u32) -> Result<Pool> {
        crate::channel::prepare(dir)?;
        let path = dir.join(format!("{job}.fifo"));
        let _ = std::fs::remove_file(&path);
        let c = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: creates a fifo at a path this process owns.
        if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("creating {}", path.display()));
        }
        // SAFETY: opens the fifo just created.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("opening {}", path.display()));
        }
        let pool = Pool {
            path,
            fd,
            target: Arc::new(AtomicU32::new(tokens)),
        };
        pool.put(tokens)?;
        Ok(pool)
    }

    /// `MAKEFLAGS` for clients: `-j` covers the tokens plus one implicit
    /// slot.
    pub fn makeflags(&self, tokens: u32) -> String {
        format!(
            " -j{} --jobserver-auth=fifo:{}",
            tokens + 1,
            self.path.display()
        )
    }

    fn put(&self, n: u32) -> Result<()> {
        let bytes = vec![b'+'; n as usize];
        // SAFETY: writes from a buffer of that length.
        let w = unsafe { libc::write(self.fd, bytes.as_ptr().cast(), bytes.len()) };
        if w != bytes.len() as isize {
            bail!(
                "writing jobserver tokens: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    /// Takes up to `n` tokens, waiting until clients return at least one.
    /// A blocked read gets returned tokens ahead of a client that would
    /// read them back in its own loop, which polling does not; reading all
    /// that are there at once keeps up with clients returning several.
    fn take(&self, n: u32) -> u32 {
        let mut buf = vec![0u8; n as usize];
        // SAFETY: reads at most `n` bytes into `buf`.
        let r = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
        r.max(0) as u32
    }

    /// The handle the executor uses to change the target.
    pub fn target(&self) -> Arc<AtomicU32> {
        self.target.clone()
    }

    /// Keeps the pool at the target until `done` is set: puts tokens back
    /// at once, takes them only as clients return them (design 10.5:
    /// lent tokens cannot be recalled). Call [`Pool::finish`] to end it.
    pub fn regulate(&self, initial: u32, done: &std::sync::atomic::AtomicBool) {
        let mut current = initial;
        while !done.load(Ordering::Relaxed) {
            let target = self.target.load(Ordering::Relaxed);
            if target < current {
                let took = self.take(current - target);
                // The target may have risen while waiting.
                let keep = took.min(current.saturating_sub(self.target.load(Ordering::Relaxed)));
                if took > keep {
                    let _ = self.put(took - keep);
                }
                current -= keep;
                continue;
            }
            if target > current && self.put(target - current).is_ok() {
                current = target;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Wakes a regulator waiting for a token after `done` is set.
    pub fn finish(&self) {
        let _ = self.put(1);
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: closes the descriptor this pool opened.
        unsafe { libc::close(self.fd) };
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Tokens a client can take now, read without blocking and returned.
    fn available(path: &std::path::Path) -> usize {
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: opens the test's fifo without blocking.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
        let mut buf = [0u8; 64];
        // SAFETY: reads into `buf`.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) }.max(0) as usize;
        // SAFETY: puts the tokens back and closes.
        unsafe {
            libc::write(fd, buf.as_ptr().cast(), n);
            libc::close(fd);
        }
        n
    }

    #[test]
    fn follows_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(Pool::create(dir.path(), "j", 5).unwrap());
        assert_eq!(available(&pool.path), 5);
        let done = Arc::new(AtomicBool::new(false));
        let regulator = {
            let (pool, done) = (pool.clone(), done.clone());
            std::thread::spawn(move || pool.regulate(5, &done))
        };
        pool.target().store(2, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(available(&pool.path), 2, "three tokens taken back");
        pool.target().store(4, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(available(&pool.path), 4, "two put back");
        // Waiting for a token that never comes back still ends.
        pool.target().store(0, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        done.store(true, Ordering::Relaxed);
        pool.finish();
        regulator.join().unwrap();
        assert!(
            pool.makeflags(4)
                .ends_with(&format!("fifo:{}", pool.path.display()))
        );
    }
}
