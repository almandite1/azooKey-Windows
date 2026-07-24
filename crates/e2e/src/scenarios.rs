//! The Tier 2 scenarios, and the shared gestures they are built from.
//!
//! Numbered against `docs/e2e-automation-plan.md`. This first cut covers the
//! conversion basics (1–3); the fault-injection and IME-cycle scenarios come
//! later. Each is independent — its own fresh host, killed on the way out —
//! so a failure in one does not poison the next (the plan's "失敗時は続行").

use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use crate::host::HostApp;
use crate::{engine, keyboard, poll_until, uia::Uia};

/// The reading and candidate the dictionary tests already pin down, so a
/// scenario failure is about the input path, not conversion quality.
const READING: &str = "mizu";
const EXPECTED: &str = "水";

/// How long conversion output has to appear once the keys are sent.
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(15);
/// After a fresh host is focused, the moment the TIP needs to be activated
/// into it before the first keystroke lands.
const ACTIVATION_GRACE: Duration = Duration::from_millis(750);

/// One scenario: a name for the report and the work that either succeeds or
/// explains where it stopped. The returned string is a human-readable detail
/// for the summary (typically the text read back).
pub struct Scenario {
    pub name: &'static str,
    pub run: fn(&Uia) -> Result<String>,
}

/// The scenarios this build knows how to run, in order.
pub fn all() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "basic_conversion",
            run: basic_conversion,
        },
        Scenario {
            name: "long_input_survives",
            run: long_input_survives,
        },
        Scenario {
            name: "second_host_converts",
            run: second_host_converts,
        },
    ]
}

/// Scenario 1: `mizu` + Space + Enter in Notepad commits 水.
fn basic_conversion(uia: &Uia) -> Result<String> {
    let host = HostApp::launch("notepad.exe", "notepad.exe")?;
    enter_kana(&host)?;
    convert_reading(READING)?;
    let text = wait_for_expected(uia, &host)?;
    Ok(text)
}

/// Scenario 2: a long burst does not kill the engine, and it still converts
/// afterwards. 65×"ka" = 130 keystrokes past the `as i8` truncation point the
/// server used to trap on (see `ipc_smoke`), driven this time through a real
/// host rather than a raw gRPC call.
fn long_input_survives(uia: &Uia) -> Result<String> {
    let host = HostApp::launch("notepad.exe", "notepad.exe")?;
    enter_kana(&host)?;

    for _ in 0..65 {
        keyboard::type_ascii("ka")?;
    }
    keyboard::tap(keyboard::space())?;
    keyboard::tap(keyboard::enter())?;

    // the point of the scenario: the engine is still there
    if !engine::is_running(engine::SERVER_IMAGE) {
        bail!(
            "{} が長文入力後に消えました（server クラッシュ）",
            engine::SERVER_IMAGE
        );
    }

    // and still converts — a live process that no longer answers would pass
    // the check above but fail here
    convert_reading(READING)?;
    let text = wait_for_expected(uia, &host)
        .context("server は生存しているが変換が返らない（ハング疑い）")?;
    Ok(format!("engine alive; {text:?}"))
}

/// Scenario 3: conversion works in a second host too, so scenario 1 was not a
/// Notepad-specific fluke. The host is our own tiny Win32 EDIT window
/// (`bin/host.rs`), which pins the representativeness to a control we own
/// rather than to whatever Notepad ships this month.
fn second_host_converts(uia: &Uia) -> Result<String> {
    let exe = custom_host_path()?;
    let host = HostApp::launch(&exe, "azookey-e2e-host.exe")?;
    enter_kana(&host)?;
    convert_reading(READING)?;
    let text = wait_for_expected(uia, &host)?;
    Ok(text)
}

// --- shared gestures ---

/// Focuses a freshly launched host and switches it to Kana. A new azooKey
/// starts in Latin (`InputMode`'s `#[default]`), so every scenario that
/// expects conversion has to toggle first — Phase 0 proved the manual spike
/// only worked because a human had already done it.
fn enter_kana(host: &HostApp) -> Result<()> {
    host.focus()?;
    std::thread::sleep(ACTIVATION_GRACE);
    keyboard::toggle_input_mode()?;
    std::thread::sleep(Duration::from_millis(250));
    Ok(())
}

/// Types a romaji reading and commits it: reading, Space to convert, Enter to
/// accept the first candidate.
fn convert_reading(reading: &str) -> Result<()> {
    keyboard::type_ascii(reading)?;
    keyboard::tap(keyboard::space())?;
    keyboard::tap(keyboard::enter())?;
    Ok(())
}

/// Waits for [`EXPECTED`] to appear in the host's text.
///
/// `contains`, never equality: the VM's Notepad keeps a fixed `1234567890`
/// prefix (Phase 0), and in general the harness does not control every byte a
/// real application already holds.
fn wait_for_expected(uia: &Uia, host: &HostApp) -> Result<String> {
    let found = poll_until(CONVERSION_TIMEOUT, || {
        let text = uia.text_of(host.window)?;
        text.contains(EXPECTED).then_some(text)
    });

    match found {
        Some(text) => Ok(text),
        None => {
            let seen = uia.text_of(host.window);
            bail!(
                "{EXPECTED} は現れませんでした（{CONVERSION_TIMEOUT:?}）。読み取れた本文: {seen:?}\n\
                 空なら打鍵が届いていない、ローマ字のままなら Kana 切替が効いていない、\n\
                 「みず」で止まりなら変換要求がエンジンに届いていない。"
            )
        }
    }
}

/// The custom host executable, next to this binary in `build/`.
fn custom_host_path() -> Result<String> {
    let exe = std::env::current_exe().context("current_exe failed")?;
    let host = exe
        .parent()
        .context("harness exe has no parent directory")?
        .join("azookey-e2e-host.exe");
    if !host.is_file() {
        bail!("custom host not found at {host:?} — is it in the payload?");
    }
    Ok(host.to_string_lossy().into_owned())
}
