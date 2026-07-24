//! Phase 0 spike: `mizu` → 水 in a real application, driven entirely from
//! code.
//!
//! The manual pass established that a profile set as the ja-JP default is
//! picked up by applications started after the change — the plan's biggest
//! technical risk. This proves the same sequence can be driven programmatically,
//! which is the last thing standing between here and the Tier 2 scenarios.
//!
//! Run it INSIDE THE TEST VM, in the interactive session (a scheduled task, or
//! a console on the VM's own desktop). Started from `Invoke-Command` over
//! PowerShell Direct it would land in session 0, where synthesised keystrokes
//! reach nothing — measured during Phase 0.
//!
//! Prerequisites in the VM: the TIP registered (`regsvr32`), `launcher.exe`
//! running, and the VC++ redistributable installed (`ggml-cpu.dll` needs
//! `vcomp140.dll`, which `build/` does not carry — the installer supplies it
//! in a real deployment).

use std::time::Duration;

use anyhow::{Result, bail};
use azookey_e2e::{Stage, guard, host::HostApp, keyboard, poll_until, profile, stage, uia};

/// The reading and the candidate the dictionary tests already pin down, so a
/// failure here is about the input path rather than about conversion quality.
const ROMAJI: &str = "mizu";
const EXPECTED: &str = "水";

const CONVERSION_TIMEOUT: Duration = Duration::from_secs(15);

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => {
            println!("\nRESULT: pass");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nRESULT: fail\n{e:?}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    stage(Stage::Guard);
    guard::require_vm()?;

    // STA: the TSF profile manager is an apartment-threaded in-proc server.
    // UI Automation is happy either way for the synchronous reads below.
    unsafe {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    stage(Stage::Profile);
    // held to the end of the function: dropping it restores the previous
    // default input method
    let _profile = profile::DefaultProfile::set_to_azookey()?;

    stage(Stage::Launch);
    // a fresh process, so it loads the default that was just set
    let notepad = HostApp::launch("notepad.exe", "notepad.exe")?;

    stage(Stage::Focus);
    notepad.focus()?;
    // the window is in front; give the TIP the moment it needs to be
    // activated into the new host before the first keystroke arrives
    std::thread::sleep(Duration::from_millis(750));

    stage(Stage::Inject);
    // A freshly activated azooKey defaults to Latin (A) mode — its InputMode
    // #[default]. The manual spike only converted because a human had already
    // switched to Kana; the harness has to do it itself. The 半角/全角 key
    // toggles it (preserved_key.rs).
    println!("switching to Kana with the 半角/全角 key");
    keyboard::toggle_input_mode()?;
    std::thread::sleep(Duration::from_millis(250));

    println!("typing {ROMAJI:?} + Space + Enter");
    keyboard::type_ascii(ROMAJI)?;
    keyboard::tap(keyboard::space())?;
    keyboard::tap(keyboard::enter())?;

    stage(Stage::Read);
    let uia = uia::Uia::new()?;
    let text = poll_until(CONVERSION_TIMEOUT, || {
        let text = uia.text_of(notepad.window)?;
        text.contains(EXPECTED).then_some(text)
    });

    let Some(text) = text else {
        let seen = uia.text_of(notepad.window);
        bail!(
            "{EXPECTED} は現れませんでした（{CONVERSION_TIMEOUT:?} 待機）。\n\
             読み取れた本文: {seen:?}\n\
             本文が空なら打鍵がホストに届いていない（対話セッションか、フォーカス）、\n\
             ローマ字のまま残っているなら TIP が載っていないか既定 IME の切替が\n\
             効いていない、読みが「みず」で止まっているなら変換要求が\n\
             エンジンに届いていない、という切り分けになります。"
        );
    };

    println!("本文: {text:?}");
    Ok(())
}
