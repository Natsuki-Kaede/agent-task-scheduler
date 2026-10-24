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
                let requests = std::env::temp_dir().join("ats").join("requests");
                Ok(Self::with(home, requests))
            }
        }
    }

    /// Builds paths under `home`, with the request channel at `requests`.
    pub fn with(home: PathBuf, requests: PathBuf) -> Self {
        Self {
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
