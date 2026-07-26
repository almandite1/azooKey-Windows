//! Liveness of the conversion engine, for the scenarios that kill it or
//! flood it and then check it recovers.
//!
//! The engine's identity across a restart is its **pid**: `launcher.exe`
//! respawns `azookey-server.exe` after a crash, kill or watchdog hang, and a
//! changed pid is the proof the restart happened rather than the same process
//! having answered all along.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use test_support::process::pid_of;
pub use test_support::process::pids_of;

use crate::poll_until;

/// The engine process launcher supervises.
pub const SERVER_IMAGE: &str = "azookey-server.exe";

/// The supervisor. Exactly one per session — see [`preflight`].
pub const LAUNCHER_IMAGE: &str = "launcher.exe";

/// Whether a process with this image name is currently running.
pub fn is_running(image: &str) -> bool {
    pid_of(image).is_some()
}

/// The pid of the (single) engine process, or `None` if it is not running.
pub fn server_pid() -> Option<u32> {
    pid_of(SERVER_IMAGE)
}

/// Refuses to run against an engine that is missing or duplicated.
///
/// A second `launcher.exe` is the failure that cost a VM: the pipe is
/// first-instance, so the extra supervisor can never bring its server up, and
/// it keeps respawning one that keeps failing — which burns through the
/// session's resources until nothing new can start at all (a plain
/// `powershell.exe` stopped launching). Starting launcher once per boot is the
/// rule; this is what enforces it.
pub fn preflight() -> Result<()> {
    let launchers = pids_of(LAUNCHER_IMAGE);
    match launchers.len() {
        0 => bail!(
            "{LAUNCHER_IMAGE} が動いていません。先に管理者で起動してください:\n\
             \x20   Start-Process C:\\azookey-e2e\\payload-<timestamp>\\launcher.exe\n\
             (payload ディレクトリは実行ごとに作られます。scripts/e2e-vm-run.ps1 参照)"
        ),
        1 => {}
        n => bail!(
            "{LAUNCHER_IMAGE} が {n} 個動いています (pids {launchers:?})。\n\
             パイプは first-instance なので余分な supervisor は server を上げられず、\n\
             失敗し続ける server を再生成してセッション資源を食い潰します。\n\
             \"clean\" チェックポイントから復元してやり直してください。"
        ),
    }

    let servers = pids_of(SERVER_IMAGE);
    if servers.len() > 1 {
        bail!(
            "{SERVER_IMAGE} が {} 個動いています (pids {servers:?})。同上。",
            servers.len()
        );
    }
    if servers.is_empty() {
        bail!(
            "{SERVER_IMAGE} が動いていません。launcher が起動しきるまで待つか、\n\
             ログ (%LOCALAPPDATA%\\Azookey\\logs) を確認してください。"
        );
    }

    println!("engine ok: launcher {launchers:?}, server {servers:?}");
    Ok(())
}

/// Force-kills a process by pid via `taskkill`, the same tool the manual
/// checklist uses. By pid, not image, so a server that has *already* been
/// restarted (a new pid) is not caught in the crossfire.
pub fn kill_pid(pid: u32) -> Result<()> {
    let output = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output()
        .context("failed to run taskkill")?;
    if !output.status.success() {
        bail!(
            "taskkill /PID {pid} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Waits for the engine to come back under a pid different from `previous` —
/// launcher having respawned it. Returns the new pid.
pub fn wait_for_restart(previous: u32, timeout: Duration) -> Result<u32> {
    poll_until(timeout, || match server_pid() {
        Some(pid) if pid != previous => Some(pid),
        _ => None,
    })
    .with_context(|| {
        format!(
            "{SERVER_IMAGE} did not come back under a new pid within {timeout:?} (was {previous})"
        )
    })
}
