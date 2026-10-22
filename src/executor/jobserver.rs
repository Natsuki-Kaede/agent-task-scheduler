//! A job's jobserver pool (design 10.5), held by its executor: a fifo in
//! the form of make 4.4 (`--jobserver-auth=fifo:PATH`), which make, ninja
//! 1.13+ and cargo use. Each client also runs one job without a token.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub struct Pool {
    pub path: PathBuf,
    fd: i32,
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
        let pool = Pool { path, fd };
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
    fn holds_its_tokens_and_removes_the_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::create(dir.path(), "j", 5).unwrap();
        assert_eq!(available(&pool.path), 5);
        assert_eq!(
            pool.makeflags(5),
            format!(" -j6 --jobserver-auth=fifo:{}", pool.path.display())
        );
        let path = pool.path.clone();
        drop(pool);
        assert!(!path.exists());
    }
}
