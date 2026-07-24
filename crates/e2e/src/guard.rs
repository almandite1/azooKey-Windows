//! Refuses to run outside a virtual machine.
//!
//! This is not a nicety. The harness calls
//! `ITfInputProcessorProfiles::SetDefaultLanguageProfile` — it changes the
//! default input method of the account running it — and then synthesises
//! keystrokes into whatever window holds focus. On a development machine that
//! means typing romaji into someone's editor and leaving their IME switched.
//!
//! During Phase 0 a guest-configuration script was run against the host by
//! mistake (a window that looked the same). The cost there was a handful of
//! settings; here it would be keystrokes into live work. So the check is in
//! the binary rather than in the runbook.

use anyhow::{Result, bail};

/// Fails unless this process is running inside a Hyper-V guest.
///
/// The signal is the system model reported by the firmware: a Hyper-V guest
/// reports `Microsoft Corporation` / `Virtual Machine`, a physical machine
/// reports its motherboard. `HypervisorPresent` is deliberately NOT used —
/// it is true on the *host* too as soon as Hyper-V is enabled, which is
/// exactly the machine this must refuse to run on.
pub fn require_vm() -> Result<()> {
    let manufacturer = registry_string("SystemManufacturer").unwrap_or_default();
    let model = registry_string("SystemProductName").unwrap_or_default();

    if model == "Virtual Machine" && manufacturer.starts_with("Microsoft") {
        println!("guest: {manufacturer} / {model}");
        return Ok(());
    }

    bail!(
        "この機械は Hyper-V ゲストではありません ({manufacturer} / {model})。\n\
         このハーネスは既定の IME を切り替え、フォーカスのあるウィンドウへ\n\
         打鍵を送り込みます。テスト VM の中でだけ実行してください。"
    )
}

/// Reads one of the BIOS strings Windows caches under `HARDWARE\DESCRIPTION`.
/// Registry rather than WMI: no COM apartment is set up yet when the guard
/// runs, and this must work before anything else does.
fn registry_string(value: &str) -> Option<String> {
    use std::process::Command;

    // `reg query` rather than a registry crate: the guard has to hold even if
    // the process is starved of everything else, and this has no dependencies.
    let output = Command::new("reg")
        .args([
            "query",
            r"HKLM\HARDWARE\DESCRIPTION\System\BIOS",
            "/v",
            value,
        ])
        .output()
        .ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|line| line.contains(value))?;
    // "    SystemProductName    REG_SZ    Virtual Machine"
    let rest = line.split("REG_SZ").nth(1)?;
    Some(rest.trim().to_string())
}
