//! The user configuration, `config.toml` in the data directory.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::paths::Paths;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub recognize: Recognize,
    pub hook: Hooks,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Recognize {
    /// Programs that run the rest of their arguments as a command, such as
    /// output filters, removed before recognition (design 8.1).
    pub wrappers: Vec<String>,
}

/// Other agents' command-rewriting hooks that ats runs first, in order,
/// before it wraps the result (design 6.1). `ats install` writes these.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hooks {
    pub claude: Chain,
    pub codex: Chain,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Chain {
    /// Shell commands, each given the hook input on stdin.
    pub chain: Vec<String>,
}

impl Config {
    /// Reads the configuration; a missing file means the defaults.
    pub fn load(paths: &Paths) -> Result<Self> {
        match std::fs::read_to_string(&paths.config) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("reading {}", paths.config.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", paths.config.display())),
        }
    }
}
