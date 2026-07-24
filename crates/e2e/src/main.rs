//! Tier 2 scenario runner: drives the IME through real host applications with
//! synthesised keystrokes and checks what they received.
//!
//! Phase 0 proved the vertical slice — set the default profile, launch a fresh
//! host, type, convert, read back — works from code. This runs the plan's
//! scenarios on top of that slice, each independent so one failure does not
//! stop the rest, and reports a summary at the end.
//!
//! Run it INSIDE THE TEST VM, in the interactive session (a scheduled task or
//! a console on the VM's own desktop). Started from `Invoke-Command` over
//! PowerShell Direct it lands in session 0, where synthesised keystrokes reach
//! nothing — measured during Phase 0.
//!
//! Prerequisites in the VM: the TIP registered (`regsvr32`), `launcher.exe`
//! running, and the VC++ redistributable installed (`ggml-cpu.dll` needs
//! `vcomp140.dll`, which the installer supplies in a real deployment).

use anyhow::Result;
use azookey_e2e::{Stage, guard, profile, scenarios, stage, uia};

fn main() -> std::process::ExitCode {
    match run() {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(e) => {
            eprintln!("\nセットアップ段階で失敗: {e:?}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Returns whether every scenario passed. Setup failures (guard, profile) are
/// `Err` — nothing can run — while a scenario failure is recorded and the run
/// continues.
fn run() -> Result<bool> {
    stage(Stage::Guard);
    guard::require_vm()?;

    // STA: the TSF profile manager is an apartment-threaded in-proc server.
    unsafe {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    stage(Stage::Profile);
    // held to the end of the run: dropping it restores the previous default
    let _profile = profile::DefaultProfile::set_to_azookey()?;

    let uia = uia::Uia::new()?;
    let scenarios = scenarios::all();
    println!("\n{} 個のシナリオを実行します\n", scenarios.len());

    let mut results = Vec::new();
    for scenario in &scenarios {
        println!("── {} ──", scenario.name);
        let result = (scenario.run)(&uia);
        match &result {
            Ok(detail) => println!("   pass: {detail}"),
            Err(e) => println!("   FAIL: {e:#}"),
        }
        results.push((scenario.name, result));
    }

    println!("\n===== 結果 =====");
    let mut all_passed = true;
    for (name, result) in &results {
        let mark = if result.is_ok() { "pass" } else { "FAIL" };
        println!("  [{mark}] {name}");
        all_passed &= result.is_ok();
    }
    let passed = results.iter().filter(|(_, r)| r.is_ok()).count();
    println!("\n{passed}/{} passed", results.len());
    println!("RESULT: {}", if all_passed { "pass" } else { "fail" });

    Ok(all_passed)
}
