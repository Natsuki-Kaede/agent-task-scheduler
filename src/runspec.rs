//! What to run: the two entry forms of `ats run` (design 5.3).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "entry", rename_all = "lowercase")]
pub enum RunSpec {
    /// `ats run -- program args...`: executed directly, without a shell.
    Argv { argv: Vec<String> },
    /// `ats run --sh 'source'`: executed by the submitter's shell.
    Sh { source: String, shell: String },
}

impl RunSpec {
    /// The command for `std::process::Command`.
    pub fn command(&self) -> std::process::Command {
        match self {
            RunSpec::Argv { argv } => {
                let mut c = std::process::Command::new(&argv[0]);
                c.args(&argv[1..]);
                c
            }
            RunSpec::Sh { source, shell } => {
                let mut c = std::process::Command::new(shell);
                c.arg("-c").arg(source);
                c
            }
        }
    }

    /// The submitter's shell, from `$SHELL`, or `/bin/sh`.
    pub fn default_shell() -> String {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into())
    }
}
