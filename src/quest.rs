//! Glue for viewing exports on a Meta Quest.
//!
//! Stereowright itself never uses the network. The Quest share helper
//! (`quest/stereowright_share.py`) is a separate program that serves Stereowright's 3D outputs
//! to the headset over the USB cable or the home Wi-Fi. This module only:
//!   * records each output folder in `~/.local/share/stereowright/share/folders.txt`
//!     so the helper knows what to show;
//!   * asks the user's systemd (local D-Bus socket) to start/stop the helper as its own
//!     service, outside Stereowright's network block, when the user clicks the button;
//!   * reads the helper's `status.json`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::models::data_dir;

/// The systemd user unit the helper runs as. Tests must set `STEREOWRIGHT_QUEST_UNIT` so they
/// never start/stop the user's real sharing.
fn unit() -> String {
    std::env::var("STEREOWRIGHT_QUEST_UNIT").unwrap_or_else(|_| "stereowright-quest-share".into())
}

pub fn share_dir() -> PathBuf {
    data_dir().join("share")
}

/// After an export: remember the folder if it got any 3D (side-by-side / top-bottom) file.
pub fn record_outputs(files: &[PathBuf]) {
    let stereo = |f: &&PathBuf| {
        f.file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with("_LR") || s.ends_with("_3dh") || s.ends_with("_TB"))
    };
    if let Some(dir) = files.iter().find(stereo).and_then(|f| f.parent()) {
        record_output_dir(dir);
    }
}

/// Remember a folder that now holds 3D outputs (ignored if already listed).
pub fn record_output_dir(dir: &Path) {
    if let Err(e) = try_record(dir) {
        log::warn!("could not record output folder for Quest sharing: {e:#}");
    }
}

fn try_record(dir: &Path) -> Result<()> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let line = dir.to_string_lossy().into_owned();
    anyhow::ensure!(!line.contains('\n'), "folder name contains a newline");
    let share = share_dir();
    std::fs::create_dir_all(&share)?;
    let file = share.join("folders.txt");
    let existing = std::fs::read_to_string(&file).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&line);
    text.push('\n');
    let tmp = share.join("folders.txt.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct UsbStatus {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub url: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct WifiStatus {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub message: String,
    /// Command that opens the firewall to the home network, if it is needed.
    #[serde(default)]
    pub firewall: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct ShareStatus {
    pub pid: u32,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub photos: usize,
    #[serde(default)]
    pub usb: Option<UsbStatus>,
    #[serde(default)]
    pub wifi: Option<WifiStatus>,
}

/// The running helper's status, or None if it isn't running.
pub fn status() -> Option<ShareStatus> {
    let text = std::fs::read_to_string(share_dir().join("status.json")).ok()?;
    let st: ShareStatus = serde_json::from_str(&text).ok()?;
    // A crashed helper leaves its file behind: check the process is really it.
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", st.pid)).ok()?;
    String::from_utf8_lossy(&cmdline)
        .contains("stereowright_share")
        .then_some(st)
}

/// Where the helper lives: `quest/` next to the app (installed) or in the source tree.
pub fn helper_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("STEREOWRIGHT_QUEST_HELPER") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    exe.ancestors()
        .skip(1)
        .take(4)
        .map(|d| d.join("quest/stereowright_share.py"))
        .chain(std::iter::once(data_dir().join("quest/stereowright_share.py")))
        .find(|p| p.is_file())
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

/// Start the helper as a user service. `wifi` = also serve over the home network.
pub fn start(wifi: bool) -> Result<()> {
    let helper = helper_path().context("Quest share helper not found (quest/stereowright_share.py)")?;
    let python = which("python3").context("python3 is needed for Quest sharing")?;
    let systemd_run = which("systemd-run").context("systemd-run not found")?;
    let mut cmd = Command::new(systemd_run);
    cmd.args(["--user", "--collect", "--quiet", "--unit", &unit()]);
    // Same data folder as this app (matters only if XDG_DATA_HOME is customised).
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        let mut arg = std::ffi::OsString::from("--setenv=XDG_DATA_HOME=");
        arg.push(x);
        cmd.arg(arg);
    }
    cmd.arg("--").arg(python).arg("-I").arg(helper);
    cmd.args(["--mode", if wifi { "both" } else { "usb" }]);
    let out = cmd.output().context("running systemd-run")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("already") {
            return Ok(()); // it's running
        }
        anyhow::bail!("could not start Quest sharing: {}", err.trim());
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    let out = Command::new("systemctl")
        .args(["--user", "stop", &format!("{}.service", unit())])
        .output()
        .context("running systemctl")?;
    anyhow::ensure!(
        out.status.success(),
        "could not stop Quest sharing: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_helper_status() {
        let st: ShareStatus = serde_json::from_str(
            r#"{"pid": 42, "mode": "both", "photos": 3, "updated": 1,
                "usb": {"state": "waiting", "message": "USB: waiting", "url": "http://localhost:8780/t/x/"},
                "wifi": {"url": "https://10.0.0.2:8781/t/x/", "message": "m", "firewall": ""}}"#,
        )
        .unwrap();
        assert_eq!(st.pid, 42);
        assert_eq!(st.usb.unwrap().state, "waiting");
        assert!(st.wifi.unwrap().firewall.is_empty());
        let minimal: ShareStatus = serde_json::from_str(r#"{"pid": 1}"#).unwrap();
        assert!(minimal.usb.is_none());
    }
}
