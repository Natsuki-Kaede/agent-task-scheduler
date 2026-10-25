//! The login service that starts `ats daemon` and starts it again if it
//! fails (design 12): a LaunchAgent on macOS, a systemd user unit on Linux.
//! A data directory chosen with `ATS_HOME` is passed on.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::paths::{HOME_ENV, Paths};

#[cfg(target_os = "macos")]
pub use launchd::*;
#[cfg(target_os = "linux")]
pub use systemd::*;

// Tests build both backends; the other platform's one is only partly used.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(test, allow(dead_code))]
mod launchd {
    use super::*;

    const LABEL: &str = "io.github.natsuki-kaede.ats";

    pub fn file(home: &Path) -> PathBuf {
        home.join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    fn xml(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    pub fn text(paths: &Paths, ats: &Path) -> String {
        let env = match std::env::var_os(HOME_ENV) {
            Some(_) => format!(
                "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>{HOME_ENV}</key>\n    <string>{}</string>\n  </dict>\n",
                xml(&paths.home.to_string_lossy())
            ),
            None => String::new(),
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>daemon</string>
  </array>
{env}  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>StandardErrorPath</key>
  <string>{}</string>
</dict>
</plist>
"#,
            xml(&ats.to_string_lossy()),
            xml(&paths.home.join("daemon.log").to_string_lossy())
        )
    }

    fn target() -> (String, String) {
        // SAFETY: getuid cannot fail.
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let target = format!("{domain}/{LABEL}");
        (domain, target)
    }

    fn loaded(target: &str) -> bool {
        std::process::Command::new("launchctl")
            .args(["print", target])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Unloads the agent and waits until launchd lets it go: launchd can
    /// still hold it when bootout returns, and a bootstrap then fails.
    pub fn unload(_home: &Path) -> Result<()> {
        let (_, target) = target();
        if loaded(&target) {
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &target])
                .status();
            for _ in 0..50 {
                if !loaded(&target) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        Ok(())
    }

    /// Loads the agent in this user's session. A loaded agent is reloaded
    /// only when `changed`: reloading restarts the daemon.
    pub fn load(home: &Path, changed: bool, out: &mut impl Write) -> Result<()> {
        let (domain, target) = target();
        if !changed && loaded(&target) {
            return Ok(());
        }
        unload(home)?;
        let status = std::process::Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(file(home))
            .status()
            .context("running launchctl")?;
        if !status.success() {
            bail!("launchctl bootstrap failed: {status}");
        }
        writeln!(out, "The daemon now runs at login: {LABEL}")?;
        Ok(())
    }
}

#[cfg(any(target_os = "linux", test))]
#[cfg_attr(test, allow(dead_code))]
mod systemd {
    use super::*;

    const UNIT: &str = "ats.service";

    pub fn file(home: &Path) -> PathBuf {
        home.join(".config/systemd/user").join(UNIT)
    }

    /// systemd unit files take C-style escapes in quoted values; `%` starts
    /// a specifier.
    fn quoted(s: &str) -> String {
        let s = s
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%");
        format!("\"{s}\"")
    }

    pub fn text(paths: &Paths, ats: &Path) -> String {
        let env = match std::env::var_os(HOME_ENV) {
            Some(_) => format!(
                "Environment={}\n",
                quoted(&format!("{HOME_ENV}={}", paths.home.to_string_lossy()))
            ),
            None => String::new(),
        };
        format!(
            "[Unit]\nDescription=Agent Task Scheduler\n\n[Service]\nExecStart={} daemon\n{env}Restart=on-failure\n\n[Install]\nWantedBy=default.target\n",
            quoted(&ats.to_string_lossy())
        )
    }

    fn systemctl(args: &[&str]) -> Result<()> {
        let status = std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .status()
            .context("running systemctl; without systemd, start `ats daemon` at login in a different way")?;
        if !status.success() {
            bail!("systemctl --user {} failed: {status}", args.join(" "));
        }
        Ok(())
    }

    /// Stops and disables the unit while its file still exists.
    pub fn unload(home: &Path) -> Result<()> {
        if file(home).exists() {
            systemctl(&["disable", "--now", UNIT])?;
        }
        Ok(())
    }

    /// Enables and starts the unit. A running unit is restarted only when
    /// `changed`: a restart restarts the daemon.
    pub fn load(_home: &Path, changed: bool, out: &mut impl Write) -> Result<()> {
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", UNIT])?;
        systemctl(&[if changed { "restart" } else { "start" }, UNIT])?;
        writeln!(out, "The daemon now runs at login: {UNIT}")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths::with(PathBuf::from("/data/ats"), PathBuf::from("/tmp/requests"))
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn writes_a_valid_property_list() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("a.plist");
        std::fs::write(&plist, launchd::text(&paths(), Path::new("/opt/a&b/ats"))).unwrap();
        let lint = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(&plist)
            .output()
            .unwrap();
        assert!(
            lint.status.success(),
            "{}",
            String::from_utf8_lossy(&lint.stdout)
        );
    }

    #[test]
    fn quotes_the_unit_command() {
        let unit = systemd::text(&paths(), Path::new("/opt/my ats/50%/ats"));
        assert!(
            unit.contains("ExecStart=\"/opt/my ats/50%%/ats\" daemon\n"),
            "{unit}"
        );
        assert!(unit.contains("WantedBy=default.target"));
    }
}
