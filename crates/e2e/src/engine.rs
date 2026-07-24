//! Liveness of the conversion engine, for the scenarios that kill it or
//! flood it and then check it recovers.
//!
//! The engine's identity across a restart is its **pid**: `launcher.exe`
//! respawns `azookey-server.exe` after a crash, kill or watchdog hang, and a
//! changed pid is the proof the restart happened rather than the same process
//! having answered all along.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

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

/// The pid of the first process with this image name.
fn pid_of(image: &str) -> Option<u32> {
    pids_of(image).into_iter().next()
}

/// Every pid with this image name.
pub fn pids_of(image: &str) -> Vec<u32> {
    let wanted = image.to_ascii_lowercase();
    let mut pids = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pids;
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if name.eq_ignore_ascii_case(&wanted) {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
    }
    pids
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
             \x20   Start-Process C:\\e2e\\build\\launcher.exe"
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
