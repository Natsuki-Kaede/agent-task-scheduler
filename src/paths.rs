//! Locations of the daemon's files.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Environment variable that overrides the data directory. Tests and
/// side-by-side instances use it.
pub const HOME_ENV: &str = "ATS_HOME";

/// Paths shared by the daemon and its clients.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Data directory: database, socket, lock and logs.
    pub home: PathBuf,
    pub db: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub logs: PathBuf,
    pub config: PathBuf,
    /// Hooks earlier versions of `ats install` moved, which it puts back.
    pub installed: PathBuf,
    /// User tool profiles (design 8.2).
    pub profiles: PathBuf,
    /// Private temporary files shared by this instance's clients.
    pub temporary: PathBuf,
    /// Request-file channel. It lives under the temporary directory because
    /// sandboxed agents can usually write there but not to the data
    /// directory (design 4).
    pub requests: PathBuf,
}

impl Paths {
    /// Resolves paths from `ATS_HOME`, or the platform data directory.
    pub fn from_env() -> Result<Self> {
        match std::env::var_os(HOME_ENV) {
            Some(home) => {
                let home = PathBuf::from(home);
                let requests = home.join("requests");
                Ok(Self::with(home, requests))
            }
            None => {
                let home = dirs::data_dir()
                    .context("no platform data directory")?
                    .join("ats");
                // SAFETY: geteuid has no preconditions.
                let temporary = temporary_root(&std::env::temp_dir(), unsafe { libc::geteuid() });
                let requests = temporary.join("requests");
                Ok(Self::with(home, requests))
            }
        }
    }

    /// Builds paths under `home`, with the request channel at `requests`.
    pub fn with(home: PathBuf, requests: PathBuf) -> Self {
        Self {
            temporary: requests
                .parent()
                .expect("request directory has a parent")
                .to_path_buf(),
            db: home.join("ats.db"),
            socket: home.join("ats.sock"),
            lock: home.join("daemon.lock"),
            logs: home.join("logs"),
            config: home.join("config.toml"),
            installed: home.join("installed.json"),
            profiles: home.join("profiles.d"),
            home,
            requests,
        }
    }
}

fn temporary_root(base: &std::path::Path, uid: u32) -> PathBuf {
    base.join(format!("ats-{uid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_directories_are_separate_for_users_and_instances() {
        let base = PathBuf::from("/tmp");
        let a = temporary_root(&base, 1000);
        let b = temporary_root(&base, 1001);
        assert!(!a.starts_with(&b) && !b.starts_with(&a));
        for home in [base.join("one"), base.join("two")] {
            let paths = Paths::with(home.clone(), home.join("requests"));
            assert_eq!(paths.temporary, home);
            assert!(paths.requests.starts_with(&paths.temporary));
        }
    }
}
