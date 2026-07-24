//! The Tier 2 scenarios, and the shared gestures they are built from.
//!
//! Numbered against `docs/e2e-automation-plan.md`: the conversion basics
//! (1–3) and the fault-injection trio (5–7). The IME-cycle and log-scan
//! scenarios come later. Each is independent — its own fresh host, killed on
//! the way out — so a failure in one does not poison the next (the plan's
//! "失敗時は続行").

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
/// How long the engine gets to come back after a kill or a watchdog restart.
/// Generous: launcher restarts with backoff and the fresh server reloads its
/// dictionary before it can answer (its own startup grace is 120s).
const RESTART_TIMEOUT: Duration = Duration::from_secs(90);

/// One scenario: a name for the report and the work that either succeeds or
/// explains where it stopped. The returned string is a human-readable detail
/// for the summary (typically the text read back).
pub struct Scenario {
    pub name: &'static str,
    pub run: fn(&Uia) -> Result<String>,
}

/// The scenarios this build runs, chosen by whether the hang hook is armed.
///
/// `watchdog_restarts_hung_server` needs `azookey-server.exe` started with
/// `AZOOKEY_TEST_HANG_AFTER_SECS`, which launcher only passes if it inherited
/// it — so that scenario is run in a **separate** invocation where launcher
/// (and this harness) both see the variable. Mixing it with the others is
/// impossible anyway: an armed server hangs partway through the suite. So:
/// variable set → only the watchdog scenario; unset → everything else.
pub fn all() -> Vec<Scenario> {
    if hang_after_secs().is_some() {
        return vec![Scenario {
            name: "watchdog_restarts_hung_server",
            run: watchdog_restarts_hung_server,
        }];
    }

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
        Scenario {
            name: "server_kill_recovers",
            run: server_kill_recovers,
        },
        Scenario {
            name: "mid_composition_kill_resets",
            run: mid_composition_kill_resets,
        },
    ]
}

/// The armed hang delay, if `AZOOKEY_TEST_HANG_AFTER_SECS` is set to a number.
pub fn hang_after_secs() -> Option<u64> {
    std::env::var("AZOOKEY_TEST_HANG_AFTER_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
}

/// Scenario 1: `mizu` + Space + Enter in Notepad commits 水.
fn basic_conversion(uia: &Uia) -> Result<String> {
    let host = HostApp::launch("notepad.exe", "notepad.exe")?;
    enter_kana(&host)?;

    let before = expected_count(uia, &host);
    convert_reading(READING)?;
    let text = wait_for_new_expected(uia, &host, before)?;
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
    let before = expected_count(uia, &host);
    convert_reading(READING)?;
    let text = wait_for_new_expected(uia, &host, before)
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

    // this host starts genuinely empty (no session restore), but count anyway
    // so every scenario asserts the same way
    let before = expected_count(uia, &host);
    convert_reading(READING)?;
    let text = wait_for_new_expected(uia, &host, before)?;
    Ok(text)
}

/// Scenario 5: kill the engine, and conversion comes back once launcher has
/// respawned it. The automatable half of "the IME recovers when the engine
/// dies" — a kill this test can cause, unlike a real crash.
fn server_kill_recovers(uia: &Uia) -> Result<String> {
    let host = HostApp::launch("notepad.exe", "notepad.exe")?;
    enter_kana(&host)?;

    let pid = engine::server_pid().context("engine を起動してから実行してください")?;
    println!("   killing {} (pid {pid})", engine::SERVER_IMAGE);
    engine::kill_pid(pid)?;

    let new_pid = engine::wait_for_restart(pid, RESTART_TIMEOUT)?;
    println!("   respawned as pid {new_pid}");

    // the first keystrokes after a restart can be spent reconnecting (the
    // client resets its composition on the stale connection and swallows the
    // key), so drive the conversion with retries
    let text = convert_with_recovery(uia, &host)?;
    Ok(format!("recovered under pid {new_pid}; {text:?}"))
}

/// Scenario 6: killing the engine WHILE a reading is composing must not splice
/// the next keystroke onto a reading the fresh server never had.
///
/// The client detects the dead connection on the next key, tears the
/// composition down (`reset_composition_after_server_loss`: it commits what
/// was there and swallows the key), and opens a clean one against the new
/// server. The proof it did not splice is that a fresh `mizu` afterwards still
/// converts to 水 — a spliced reading (にほん + みず) would not.
fn mid_composition_kill_resets(uia: &Uia) -> Result<String> {
    let host = HostApp::launch("notepad.exe", "notepad.exe")?;
    enter_kana(&host)?;

    // a reading left composing, uncommitted
    keyboard::type_ascii("nihon")?;
    std::thread::sleep(Duration::from_millis(500));

    let pid = engine::server_pid().context("engine を起動してから実行してください")?;
    println!(
        "   killing {} mid-composition (pid {pid})",
        engine::SERVER_IMAGE
    );
    engine::kill_pid(pid)?;
    std::thread::sleep(Duration::from_millis(500));

    // the keystroke that meets the dead server and triggers the reset
    keyboard::type_ascii("k")?;

    let new_pid = engine::wait_for_restart(pid, RESTART_TIMEOUT)?;
    println!("   respawned as pid {new_pid}");

    // a clean composition against the fresh server still converts — proof the
    // reading was reset, not carried over
    let text = convert_with_recovery(uia, &host)?;
    Ok(format!("clean conversion after reset; {text:?}"))
}

/// Scenario 7: an engine that hangs (stops answering without dying) is
/// detected by launcher's watchdog, killed and restarted, and conversion
/// recovers. Requires the server to have been started with
/// `AZOOKEY_TEST_HANG_AFTER_SECS` (see `all`).
fn watchdog_restarts_hung_server(uia: &Uia) -> Result<String> {
    let secs = hang_after_secs().expect("only selected when the hang hook is armed");
    println!("   hang hook armed for {secs}s after each server start");

    let host = HostApp::launch("notepad.exe", "notepad.exe")?;
    enter_kana(&host)?;

    let pid = engine::server_pid().context("engine を起動してから実行してください")?;
    println!("   waiting for the watchdog to catch the hang and restart pid {pid}");

    // the hang fires `secs` after the server started; the watchdog then needs
    // a few ping cycles (10s each, 3 failures) to declare it hung. RESTART_
    // TIMEOUT covers the hang plus that detection.
    let new_pid = engine::wait_for_restart(pid, RESTART_TIMEOUT + Duration::from_secs(secs))?;
    println!("   watchdog restarted the engine as pid {new_pid}");

    let text = convert_with_recovery(uia, &host)?;
    Ok(format!(
        "recovered after watchdog restart under pid {new_pid}; {text:?}"
    ))
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

/// Converts `mizu` and waits for 水, retrying the whole gesture until it lands.
///
/// After a restart the client's channel is stale, so the first key (sometimes
/// the first few) is spent reconnecting: the client sees the dead connection,
/// resets, and swallows that key, so a single attempt can compose the wrong
/// reading. Retrying absorbs that — and the fresh server may still be loading
/// its dictionary, which the overall [`RESTART_TIMEOUT`] budget covers.
fn convert_with_recovery(uia: &Uia, host: &HostApp) -> Result<String> {
    // counted once, before any attempt: every retry must still be judged
    // against the text as it stood BEFORE recovery was attempted
    let before = expected_count(uia, host);
    let deadline = std::time::Instant::now() + RESTART_TIMEOUT;
    let mut attempt = 0;
    loop {
        attempt += 1;
        // drop anything a previous attempt left composing
        keyboard::tap(keyboard::escape())?;
        convert_reading(READING)?;

        let found = poll_until(CONVERSION_TIMEOUT, || {
            let text = uia.text_of(host.window)?;
            (text.matches(EXPECTED).count() > before).then_some(text)
        });
        if let Some(text) = found {
            println!("   converted on attempt {attempt}");
            return Ok(text);
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "{EXPECTED} が {RESTART_TIMEOUT:?} 以内に増えませんでした（{attempt} 回試行、\
                 開始時 {before} 個）。最後の本文: {:?}",
                uia.text_of(host.window)
            );
        }
        println!("   attempt {attempt} did not convert yet; retrying");
    }
}

/// How many times [`EXPECTED`] already appears in the host's text.
///
/// **Never assert `contains`.** Windows 11's Notepad restores its previous
/// session, so a "fresh" one opens holding whatever the last scenario left —
/// including its 水. Scenarios 2, 5 and 6 all passed against that leftover
/// without their own conversion ever landing. The honest question is not
/// whether 水 is present but whether one MORE appeared.
fn expected_count(uia: &Uia, host: &HostApp) -> usize {
    uia.text_of(host.window)
        .map(|text| text.matches(EXPECTED).count())
        .unwrap_or(0)
}

/// Waits for another [`EXPECTED`] to appear beyond the `before` count.
fn wait_for_new_expected(uia: &Uia, host: &HostApp, before: usize) -> Result<String> {
    let found = poll_until(CONVERSION_TIMEOUT, || {
        let text = uia.text_of(host.window)?;
        (text.matches(EXPECTED).count() > before).then_some(text)
    });

    match found {
        Some(text) => Ok(text),
        None => {
            let seen = uia.text_of(host.window);
            bail!(
                "{EXPECTED} が増えませんでした（{CONVERSION_TIMEOUT:?}、開始時 {before} 個）。\
                 読み取れた本文: {seen:?}\n\
                 変化なしなら打鍵が届いていない、ローマ字が増えているなら Kana 切替が\n\
                 効いていない、「みず」で止まりなら変換要求がエンジンに届いていない。"
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
