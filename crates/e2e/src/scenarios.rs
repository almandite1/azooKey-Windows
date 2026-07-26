//! The Tier 2 scenarios, and the shared gestures they are built from.
//!
//! Numbered against `docs/e2e-automation-plan.md`. Each is independent — its
//! own fresh host, killed on the way out — so a failure in one does not poison
//! the next (the plan's "失敗時は続行").

use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use crate::host::HostApp;
use crate::profile::DefaultProfile;
use crate::winevent::ImeEvent;
use crate::{engine, keyboard, logs, overlay, poll_until, uia::Uia, winevent};

/// The reading and candidate the dictionary tests already pin down, so a
/// scenario failure is about the input path, not conversion quality.
const READING: &str = "mizu";
const EXPECTED: &str = "水";

/// How long conversion output has to appear once the keys are sent.
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a window transition (candidate list up or down) gets.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(8);
/// After a fresh host is focused, the moment the TIP needs to be activated
/// into it before the first keystroke lands.
const ACTIVATION_GRACE: Duration = Duration::from_millis(750);
/// How long the engine gets to come back after a kill or a watchdog restart.
/// Generous: launcher restarts with backoff and the fresh server reloads its
/// dictionary before it can answer (its own startup grace is 120s).
const RESTART_TIMEOUT: Duration = Duration::from_secs(90);

/// What a scenario is given: the accessibility client for reading hosts back,
/// and the profile handle (scenario 9 cycles the default input method through
/// it).
pub struct Ctx<'a> {
    pub uia: &'a Uia,
    pub profile: &'a DefaultProfile,
}

/// One scenario: a name for the report and the work that either succeeds or
/// explains where it stopped. The returned string is a human-readable detail
/// for the summary.
pub struct Scenario {
    pub name: &'static str,
    pub run: fn(&Ctx) -> Result<String>,
}

/// The scenarios this build runs, chosen by whether the hang hook is armed.
///
/// `watchdog_restarts_hung_server` needs `azookey-server.exe` started with
/// `AZOOKEY_TEST_HANG_AFTER_SECS`, which launcher only passes if it inherited
/// it — so that scenario is run in a **separate** invocation where launcher
/// (and this harness) both see the variable. Mixing it with the others is
/// impossible anyway: an armed server hangs partway through the suite. So:
/// variable set → only the watchdog scenario; unset → everything else.
/// Names a scenario after the function that runs it, so the two cannot drift
/// — every entry used to spell the name twice.
macro_rules! scenarios {
    ($($run:ident),* $(,)?) => {
        vec![$(Scenario {
            name: stringify!($run),
            run: $run,
        }),*]
    };
}

pub fn all() -> Vec<Scenario> {
    if hang_after_secs().is_some() {
        return scenarios![watchdog_restarts_hung_server];
    }

    scenarios![
        basic_conversion,
        long_input_survives,
        second_host_converts,
        password_field_disables_ime,
        server_kill_recovers,
        mid_composition_kill_resets,
        winevents_fire_once_per_transition,
        ime_cycle_keeps_working,
        // last: it reads the logs everything above produced
        logs_are_clean,
    ]
}

/// The armed hang delay, if `AZOOKEY_TEST_HANG_AFTER_SECS` is set to a number.
pub fn hang_after_secs() -> Option<u64> {
    std::env::var("AZOOKEY_TEST_HANG_AFTER_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
}

/// Scenario 1: `mizu` + Space + Enter in Notepad commits 水.
fn basic_conversion(ctx: &Ctx) -> Result<String> {
    let host = notepad_in_kana()?;

    convert_and_expect(ctx.uia, &host)
}

/// Scenario 2: a long burst does not kill the engine, and it still converts
/// afterwards. 65×"ka" = 130 keystrokes past the `as i8` truncation point the
/// server used to trap on (see `ipc_smoke`), driven this time through a real
/// host rather than a raw gRPC call.
fn long_input_survives(ctx: &Ctx) -> Result<String> {
    let host = notepad_in_kana()?;

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
    let text = convert_and_expect(ctx.uia, &host)
        .context("server は生存しているが変換が返らない（ハング疑い）")?;
    Ok(format!("engine alive; {text:?}"))
}

/// Scenario 3: conversion works in a second host too, so scenario 1 was not a
/// Notepad-specific fluke. The host is our own tiny Win32 window
/// (`bin/azookey-e2e-host.rs`), which pins the representativeness to a control
/// we own rather than to whatever Notepad ships this month.
fn second_host_converts(ctx: &Ctx) -> Result<String> {
    let host = HostApp::launch(&custom_host_path()?, CUSTOM_HOST_IMAGE)?;
    enter_kana(&host)?;

    // this host starts genuinely empty (no session restore), but count anyway
    // so every scenario asserts the same way
    convert_and_expect(ctx.uia, &host)
}

/// Scenario 4: the IME disengages in a password field.
///
/// A TSF host sets the disable-IME compartment on a password control, and the
/// TIP must fall back to direct input there. Reading that back took three
/// tries, because the obvious observables are all blind here:
///
/// * the password field's own value — UI Automation refuses to hand out a
///   password's text, by design;
/// * the preedit — a plain EDIT is bridged to TSF by CUAS, which keeps an
///   in-flight composition out of the control's buffer until it is committed,
///   so nothing is readable mid-composition (Notepad, natively TSF-aware,
///   shows its preedit and misled the first attempt);
/// * the candidate window — it never came up for this host at all, which is
///   its own open question and not this scenario's to answer.
///
/// So the host publishes it: `bin/azookey-e2e-host.rs` echoes the password
/// field into a read-only mirror the test can read (see `State::mirror`). Raw
/// `mizu` in the mirror means direct input; kana or kanji, or nothing at all
/// (a composition still pending), means the IME did not disengage.
fn password_field_disables_ime(ctx: &Ctx) -> Result<String> {
    /// AutomationIds from the host's `ID_PASSWORD` / `ID_MIRROR`.
    const PASSWORD_ID: &str = "2";
    const MIRROR_ID: &str = "3";

    let host = HostApp::launch(&custom_host_path()?, CUSTOM_HOST_IMAGE)?;
    enter_kana(&host)?;

    // the ordinary field, as the positive control: the IME must be genuinely
    // active in this host before "it did not engage" means anything
    convert_and_expect(ctx.uia, &host)
        .context("通常欄で変換できていないので、パスワード欄の判定材料がありません")?;

    // into the password field, and confirm the caret really went there —
    // "focus never moved" and "the IME misbehaved" look identical downstream
    ctx.uia.focus_by_automation_id(host.window, PASSWORD_ID)?;
    std::thread::sleep(Duration::from_millis(500));
    let focused = ctx.uia.focused_automation_id();
    println!("   focus: {focused:?} (expecting {PASSWORD_ID:?})");
    if focused.as_deref() != Some(PASSWORD_ID) {
        for line in ctx.uia.describe(host.window) {
            println!("     {line}");
        }
        bail!("パスワード欄へフォーカスが移りませんでした（focus={focused:?}）");
    }

    // the same keys again — this time they must land as plain latin
    keyboard::type_ascii(READING)?;
    let mirrored = poll_until(SETTLE_TIMEOUT, || {
        let value = ctx.uia.value_by_automation_id(host.window, MIRROR_ID)?;
        (!value.is_empty()).then_some(value)
    });

    keyboard::tap(keyboard::escape())?;

    let Some(mirrored) = mirrored else {
        // whatever went wrong, the tree says which: an empty mirror with an
        // empty password field means nothing was committed there (a pending
        // composition); an empty mirror with no mirror control at all means
        // the readback is broken, not the IME
        for line in ctx.uia.describe(host.window) {
            println!("     {line}");
        }
        bail!(
            "パスワード欄のミラーが空のままでした。上のツリー出力を確認してください\
             （id={MIRROR_ID:?} が存在しないなら読み取り側の問題、存在して空なら\
             IME が合成を抱えたまま確定していない）。"
        );
    };
    if mirrored != READING {
        bail!(
            "パスワード欄に {mirrored:?} が入りました（生の {READING:?} であるべき）\
             = IME が無効化されていない"
        );
    }

    Ok(format!("password field received raw {mirrored:?}"))
}

/// Scenario 5: kill the engine, and conversion comes back once launcher has
/// respawned it. The automatable half of "the IME recovers when the engine
/// dies" — a kill this test can cause, unlike a real crash.
fn server_kill_recovers(ctx: &Ctx) -> Result<String> {
    let host = notepad_in_kana()?;

    let pid = engine::server_pid().context("engine を起動してから実行してください")?;
    println!("   killing {} (pid {pid})", engine::SERVER_IMAGE);
    engine::kill_pid(pid)?;

    let new_pid = engine::wait_for_restart(pid, RESTART_TIMEOUT)?;
    println!("   respawned as pid {new_pid}");

    let text = convert_with_recovery(ctx.uia, &host)?;
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
fn mid_composition_kill_resets(ctx: &Ctx) -> Result<String> {
    let host = notepad_in_kana()?;

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

    let text = convert_with_recovery(ctx.uia, &host)?;
    Ok(format!("clean conversion after reset; {text:?}"))
}

/// Scenario 7: an engine that hangs (stops answering without dying) is
/// detected by launcher's watchdog, killed and restarted, and conversion
/// recovers. Requires the server to have been started with
/// `AZOOKEY_TEST_HANG_AFTER_SECS` (see [`all`]).
fn watchdog_restarts_hung_server(ctx: &Ctx) -> Result<String> {
    let secs = hang_after_secs().expect("only selected when the hang hook is armed");
    println!("   hang hook armed for {secs}s after each server start");

    let host = notepad_in_kana()?;

    let pid = engine::server_pid().context("engine を起動してから実行してください")?;
    println!("   waiting for the watchdog to catch the hang and restart pid {pid}");

    // the hang fires `secs` after the server started; the watchdog then needs
    // a few ping cycles (10s each, 3 failures) to declare it hung
    let new_pid = engine::wait_for_restart(pid, RESTART_TIMEOUT + Duration::from_secs(secs))?;
    println!("   watchdog restarted the engine as pid {new_pid}");

    let text = convert_with_recovery(ctx.uia, &host)?;
    Ok(format!(
        "recovered after watchdog restart under pid {new_pid}; {text:?}"
    ))
}

/// Scenario 8: the IME UI announces each transition exactly once.
///
/// `ui.exe` reports `EVENT_OBJECT_IME_SHOW`/`_HIDE`/`_CHANGE` so the shell and
/// assistive technology can follow a window they otherwise cannot see. The
/// contract is one announcement per real transition — announcing every request
/// buried listeners in duplicates — and CHANGE only while the window is up.
fn winevents_fire_once_per_transition(_ctx: &Ctx) -> Result<String> {
    // held for the whole scenario: dropping it kills the host, and the
    // candidate window under observation belongs to its composition
    let _host = notepad_in_kana()?;

    let candidate = overlay::candidate_window()
        .context("ui.exe の候補ウィンドウが見つかりません（ui.exe は動いていますか）")?;
    let events = winevent::log();

    keyboard::tap(keyboard::escape())?;
    wait_for_candidates_hidden("開始時に候補ウィンドウが閉じている")?;

    // composing brings it up: exactly one SHOW
    let mark = events.mark();
    keyboard::type_ascii(READING)?;
    let seen = events.wait_for_event(mark, candidate, ImeEvent::Show, SETTLE_TIMEOUT);
    let shows = seen.iter().filter(|e| **e == ImeEvent::Show).count();
    if shows != 1 {
        bail!("SHOW が {shows} 回（1 回であるべき）: {seen:?}");
    }

    // cancelling takes it down: exactly one HIDE, and nothing after it
    let mark = events.mark();
    keyboard::tap(keyboard::escape())?;
    let seen = events.wait_for_event(mark, candidate, ImeEvent::Hide, SETTLE_TIMEOUT);
    let hides = seen.iter().filter(|e| **e == ImeEvent::Hide).count();
    if hides != 1 {
        bail!("HIDE が {hides} 回（1 回であるべき）: {seen:?}");
    }

    // ending a composition sends Hide and then an empty SetCandidate; the
    // latter must not announce a CHANGE on a window that just went away
    std::thread::sleep(Duration::from_millis(500));
    let after_hide: Vec<_> = events
        .since(mark, candidate)
        .into_iter()
        .skip_while(|e| *e != ImeEvent::Hide)
        .skip(1)
        .collect();
    if after_hide.contains(&ImeEvent::Change) {
        bail!("HIDE の後に CHANGE が出ました（非表示中の通知）: {after_hide:?}");
    }

    Ok(format!(
        "SHOW×1 / HIDE×1, nothing after HIDE ({after_hide:?})"
    ))
}

/// Scenario 9: switching the default input method away and back repeatedly
/// leaves azooKey working. Guards the activation path — a TIP that did not
/// survive being deactivated, or that double-advised its sinks on the way
/// back, would fail one of the rounds.
fn ime_cycle_keeps_working(ctx: &Ctx) -> Result<String> {
    if !ctx.profile.has_previous() {
        bail!("切り替え先の入力方式が無いため往復できません");
    }

    const ROUNDS: usize = 3;
    for round in 1..=ROUNDS {
        ctx.profile.cycle()?;
        println!("   round {round}: switched away and back");

        // a fresh host, because only processes started after the change pick
        // the default up
        let host = notepad()?;
        enter_kana(&host)?;
        convert_and_expect(ctx.uia, &host)
            .with_context(|| format!("{round} 周目で変換できなくなりました"))?;
    }

    Ok(format!("{ROUNDS} 周とも変換成立"))
}

/// Scenario 10: nothing panicked along the way.
///
/// Reduced in scope on purpose: the TIP writes no log FILES in release builds
/// (`crates/client/src/trace.rs` forwards to `OutputDebugStringW`), so the
/// checklist's "client ログに panic が無い" cannot be answered from disk at
/// all. What is left — server, ui and launcher — is still where an engine-side
/// panic would land.
fn logs_are_clean(_ctx: &Ctx) -> Result<String> {
    let (hits, files) = logs::scan()?;
    if !hits.is_empty() {
        let detail = hits
            .iter()
            .map(|hit| format!("  {}: {}", hit.file, hit.line))
            .collect::<Vec<_>>()
            .join("\n");
        bail!("ログに問題のマーカーが {} 件:\n{detail}", hits.len());
    }
    Ok(format!(
        "{files} 個のログにマーカー無し（TIP はリリースでファイルログを書かないため対象外）"
    ))
}

// --- shared gestures ---

/// The second host's image name; the file sits beside this binary.
const CUSTOM_HOST_IMAGE: &str = "azookey-e2e-host.exe";

/// A fresh Notepad. Both arguments are the same string, and every scenario
/// that wanted one repeated it: the command and the image name coincide here,
/// but `HostApp::launch` takes them apart because for the custom host they
/// differ (a full path vs a bare image name).
fn notepad() -> Result<HostApp> {
    HostApp::launch("notepad.exe", "notepad.exe")
}

/// The gesture six scenarios open with: a fresh Notepad, focused and in Kana.
fn notepad_in_kana() -> Result<HostApp> {
    let host = notepad()?;
    enter_kana(&host)?;
    Ok(host)
}

/// Convert `mizu` in `host` and wait for one MORE 水 than was there before —
/// the count-before/convert/count-after gesture five scenarios share.
fn convert_and_expect(uia: &Uia, host: &HostApp) -> Result<String> {
    let before = expected_count(uia, host);
    convert_reading(READING)?;
    wait_for_new_expected(uia, host, before)
}

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

/// Waits for the candidate window to be down.
///
/// Only this direction: the scenarios establish a clean starting point with
/// it, and "the window came up" is asserted through the WinEvent log, which
/// says *how many times* rather than merely *whether*.
fn wait_for_candidates_hidden(what: &str) -> Result<()> {
    if poll_until(SETTLE_TIMEOUT, || {
        (!overlay::candidates_visible()).then_some(())
    })
    .is_none()
    {
        bail!("{what} を {SETTLE_TIMEOUT:?} 待っても成立しませんでした");
    }
    Ok(())
}

/// How many times [`EXPECTED`] already appears in the host's text.
///
/// **Never assert `contains`.** Windows 11's Notepad restores its previous
/// session, so a "fresh" one opens holding whatever the last scenario left —
/// including its 水. Three scenarios once passed against that leftover without
/// their own conversion ever landing. The honest question is not whether 水 is
/// present but whether one MORE appeared.
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

/// The custom host executable, next to this binary in `build/`.
fn custom_host_path() -> Result<String> {
    let exe = std::env::current_exe().context("current_exe failed")?;
    let host = exe
        .parent()
        .context("harness exe has no parent directory")?
        .join(CUSTOM_HOST_IMAGE);
    if !host.is_file() {
        bail!("custom host not found at {host:?} — is it in the payload?");
    }
    Ok(host.to_string_lossy().into_owned())
}
