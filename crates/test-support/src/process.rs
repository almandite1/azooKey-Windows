//! Asking the process table which processes exist and how they are related.
//!
//! Both harnesses need this and both had their own Toolhelp walk: the display
//! tests to restrict window lookup to the ui.exe they spawned (UIAccess makes
//! it re-exec, so the windows can belong to a grandchild), and the E2E suite
//! to tell an engine restart from the same process having answered all along.

use std::collections::HashSet;

use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

/// Every (pid, parent pid) the process table currently reports. Empty when
/// the snapshot fails, which the callers treat as "nothing known" rather than
/// as an error — the process table is not something a test can repair.
fn snapshot() -> Vec<(u32, u32, String)> {
    let mut entries = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return entries;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                entries.push((
                    entry.th32ProcessID,
                    entry.th32ParentProcessID,
                    String::from_utf16_lossy(&entry.szExeFile[..end]),
                ));
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    entries
}

/// Every pid whose image name is `image` (case-insensitive).
pub fn pids_of(image: &str) -> Vec<u32> {
    snapshot()
        .into_iter()
        .filter(|(_, _, name)| name.eq_ignore_ascii_case(image))
        .map(|(pid, _, _)| pid)
        .collect()
}

/// The pid of the first process with this image name.
pub fn pid_of(image: &str) -> Option<u32> {
    pids_of(image).into_iter().next()
}

/// `root` and every process descended from it.
///
/// Iterated per generation rather than recursively: the trees this is asked
/// about are two deep at most (the UIAccess shim and the ui.exe it re-execs),
/// and a bounded loop cannot be led in circles by a recycled pid.
pub fn process_tree(root: u32) -> HashSet<u32> {
    let parents = snapshot();
    let mut tree = HashSet::from([root]);
    for _ in 0..4 {
        let before = tree.len();
        for (pid, parent, _) in &parents {
            if tree.contains(parent) {
                tree.insert(*pid);
            }
        }
        if tree.len() == before {
            break;
        }
    }
    tree
}

/// The lowercased executable name of a process, for telling one owner's
/// windows from another's.
///
/// `QueryFullProcessImageNameW` rather than the Toolhelp name because the
/// caller already has a pid in hand (from a window) and a targeted open is
/// cheaper than a whole snapshot.
pub fn image_name_of(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        let mut buffer = [0u16; MAX_PATH as usize];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        ok.ok()?;

        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        Some(path.rsplit(['\\', '/']).next()?.to_ascii_lowercase())
    }
}
