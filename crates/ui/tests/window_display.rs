//! Tier 1 of `docs/e2e-automation-plan.md`: what the candidate window
//! actually puts on screen, driven through the real `WindowService` pipe.
//!
//! `utils.rs` unit-tests every *decision* the window makes (where to clamp,
//! how wide to be, how to escape a mode string). None of that proves the
//! decision reaches a window — that the candidates are rendered, that the
//! highlight moves, that the window appears where it was told to, that the
//! shell is told the IME UI came and went. That is what these do, and they
//! replace the manual checks in `docs/azookey-windows-manual-checklist.md`
//! that used to need a person looking at a screen.
//!
//! They start a real ui.exe (WebView2 and all), so they are `#[ignore]`d like
//! the server's `ipc_smoke`. Run them with:
//!
//!     cargo make test_ui_display
//!
//! or directly:
//!
//!     cargo test -p ui --test window_display -- --ignored --test-threads=1
//!
//! Unlike Tier 2 they need no TIP registration, no keystroke injection and no
//! conversion engine, so they are safe on a working machine and can run in CI.

mod support;

use support::{HwndExt as _, ImeEvent, SETTLE_TIMEOUT, Ui, poll_until, wait_for};

use std::time::Duration;
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

/// A caret rect well inside the primary work area, so the window lands where
/// the placement rules put it with no clamping and the assertions stay exact.
fn caret_with_room() -> RECT {
    let mut work = RECT::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .expect("SPI_GETWORKAREA failed");

    RECT {
        left: work.left + 200,
        top: work.top + 100,
        right: work.left + 260,
        bottom: work.top + 140,
    }
}

/// The window goes up where it was told, comes down when asked, and the shell
/// is told about each transition exactly once — the checklist's "候補窓が出る"
/// and "WinEvent が遷移時のみ 1 回ずつ" in one pass.
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn showing_and_hiding_announces_one_transition_each() {
    let mut ui = Ui::start().await;
    let candidate = ui.candidate();

    assert!(
        !candidate.is_visible(),
        "the candidate window must start hidden"
    );

    let mark = ui.events.mark();
    ui.show_at(caret_with_room()).await;
    ui.events
        .expect_exactly(mark, candidate, &[ImeEvent::Show], SETTLE_TIMEOUT);

    let mark = ui.events.mark();
    ui.hide().await;
    wait_for(SETTLE_TIMEOUT, "the candidate window to disappear", || {
        !candidate.is_visible()
    });
    ui.events
        .expect_exactly(mark, candidate, &[ImeEvent::Hide], SETTLE_TIMEOUT);

    // a second Hide is not a transition and must announce nothing
    let mark = ui.events.mark();
    ui.hide().await;
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        ui.events.since(mark, candidate),
        &[],
        "hiding an already-hidden window must not announce anything"
    );

    ui.assert_no_errors();
}

/// A `Show` for a composition whose position never arrives still puts the
/// candidates on screen once the grace period ends (issue #59): no candidates
/// at all is worse than candidates in a stale spot.
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn a_show_without_a_position_still_appears() {
    let mut ui = Ui::start().await;

    ui.show().await;

    wait_for(
        SETTLE_TIMEOUT,
        "the deferred show to be honoured by the deadline",
        || ui.candidate().is_visible(),
    );
    ui.assert_no_errors();
}

/// The candidates reach the webview and arrive as a list a screen reader can
/// read — the automatable half of the Narrator checklist item.
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn the_candidates_reach_the_window_as_an_accessible_list() {
    let mut ui = Ui::start().await;
    let candidate = ui.candidate();
    ui.show_at(caret_with_room()).await;

    ui.set_candidates(&["水", "みず", "ミズ", "瑞"]).await;

    let rendered = ui.uia.wait_for(
        "the candidates to be rendered",
        |uia| uia.candidates(candidate),
        |candidates| candidates.len() == 4,
    );
    assert_eq!(rendered, ["水", "みず", "ミズ", "瑞"]);

    assert_eq!(
        ui.uia.list_label(candidate).as_deref(),
        Some("変換候補"),
        "the list must carry the label Narrator announces"
    );

    ui.assert_no_errors();
}

/// A shorter list must not leave the previous composition's leftovers behind.
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn a_shorter_list_drops_the_leftover_candidates() {
    let mut ui = Ui::start().await;
    let candidate = ui.candidate();
    ui.show_at(caret_with_room()).await;

    ui.set_candidates(&["一", "二", "三", "四", "五"]).await;
    ui.uia.wait_for(
        "the first list to be rendered",
        |uia| uia.candidates(candidate),
        |candidates| candidates.len() == 5,
    );

    ui.set_candidates(&["壱", "弐"]).await;
    let rendered = ui.uia.wait_for(
        "the shorter list to replace it",
        |uia| uia.candidates(candidate),
        |candidates| candidates.len() == 2,
    );
    assert_eq!(rendered, ["壱", "弐"]);

    ui.assert_no_errors();
}

/// The highlight follows `SetSelection`, and an index past the end clears it
/// instead of throwing (which used to abort the script and freeze the
/// highlight where it was).
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn the_highlight_follows_the_selection() {
    let mut ui = Ui::start().await;
    let candidate = ui.candidate();
    ui.show_at(caret_with_room()).await;

    ui.set_candidates(&["水", "みず", "ミズ"]).await;
    ui.uia.wait_for(
        "the candidates to be rendered",
        |uia| uia.candidates(candidate),
        |candidates| candidates.len() == 3,
    );

    for index in [0, 2, 1] {
        ui.select(index).await;
        ui.uia.wait_for(
            "the highlight to move",
            |uia| uia.selected_index(candidate),
            |selected| *selected == Some(index as usize),
        );
    }

    // past the end of the list: nothing selected, and the window survives
    ui.select(99).await;
    ui.uia.wait_for(
        "the highlight to be cleared",
        |uia| uia.selected_index(candidate),
        |selected| selected.is_none(),
    );
    ui.select(1).await;
    ui.uia.wait_for(
        "the highlight to come back afterwards",
        |uia| uia.selected_index(candidate),
        |selected| *selected == Some(1),
    );

    ui.assert_no_errors();
}

/// What a keystroke actually sends: the fresh list and the highlight that goes
/// with it, in ONE update. Both halves have to land — and land as one content
/// change, not two.
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn a_combined_update_applies_the_list_and_the_highlight_together() {
    let mut ui = Ui::start().await;
    let candidate = ui.candidate();
    ui.show_at(caret_with_room()).await;

    let mark = ui.events.mark();
    ui.update_view(Some(&["水", "みず", "ミズ"]), Some(2), None)
        .await;

    let rendered = ui.uia.wait_for(
        "the candidates to be rendered",
        |uia| uia.candidates(candidate),
        |candidates| candidates.len() == 3,
    );
    assert_eq!(rendered, ["水", "みず", "ミズ"]);
    ui.uia.wait_for(
        "the highlight that came with them",
        |uia| uia.selected_index(candidate),
        |selected| *selected == Some(2),
    );

    ui.events
        .expect_exactly(mark, candidate, &[ImeEvent::Change], SETTLE_TIMEOUT);

    // ...and the highlight alone moves through the list already on screen,
    // without the list being resent (the traffic this replaced) and without a
    // content change being announced for it
    let mark = ui.events.mark();
    ui.update_view(None, Some(0), None).await;
    ui.uia.wait_for(
        "the highlight to move on its own",
        |uia| uia.selected_index(candidate),
        |selected| *selected == Some(0),
    );
    assert_eq!(
        ui.uia.candidates(candidate),
        ["水", "みず", "ミズ"],
        "a selection-only update must leave the list exactly as it was"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        ui.events.since(mark, candidate),
        &[],
        "moving the highlight is not a content change"
    );

    ui.assert_no_errors();
}

/// The window follows the reported caret: 15px left of it, directly below.
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn the_window_moves_to_the_reported_caret() {
    let mut ui = Ui::start().await;
    let candidate = ui.candidate();
    let caret = caret_with_room();
    ui.sized().await;
    ui.show_at(caret).await;

    let expected = (caret.left - 15, caret.bottom);
    wait_for(SETTLE_TIMEOUT, "the window to reach the caret", || {
        let rect = candidate.rect();
        (rect.left, rect.top) == expected
    });

    // and it follows a second caret (the composition moved along the line)
    let moved = RECT {
        left: caret.left + 300,
        top: caret.top + 60,
        right: caret.right + 300,
        bottom: caret.bottom + 60,
    };
    ui.set_position(moved).await;
    let expected = (moved.left - 15, moved.bottom);
    wait_for(SETTLE_TIMEOUT, "the window to follow the caret", || {
        let rect = candidate.rect();
        (rect.left, rect.top) == expected
    });

    ui.assert_no_errors();
}

/// A visible window announces content changes; a hidden one must not — ending
/// a composition sends `Hide` and then an empty `SetCandidate`, which used to
/// announce a CHANGE on a window that had just gone away.
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn only_a_visible_window_announces_content_changes() {
    let mut ui = Ui::start().await;
    let candidate = ui.candidate();
    ui.show_at(caret_with_room()).await;

    let mark = ui.events.mark();
    ui.set_candidates(&["水", "みず"]).await;
    ui.events
        .expect_exactly(mark, candidate, &[ImeEvent::Change], SETTLE_TIMEOUT);

    ui.hide_and_wait().await;

    let mark = ui.events.mark();
    ui.set_candidates(&[]).await;
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        ui.events.since(mark, candidate),
        &[],
        "a hidden window must not announce content changes"
    );

    ui.assert_no_errors();
}

/// A longer candidate widens the window, in the proportion
/// `candidate_window_logical_width` decides. Asserted as a ratio so the test
/// does not have to know the monitor's scale factor.
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn a_long_candidate_widens_the_window() {
    let mut ui = Ui::start().await;
    let candidate = ui.candidate();
    ui.show_at(caret_with_room()).await;

    // The three widths below are the constants in src/geometry.rs:
    // MIN_CANDIDATE_WINDOW_WIDTH (225), MAX_CANDIDATE_WINDOW_WIDTH (640),
    // and CANDIDATE_ROW_CHROME_WIDTH + n * CANDIDATE_PX_PER_CHAR in between.
    // Spelled out rather than imported: `ui` is a bin crate, so an
    // integration test cannot reach into it.

    // one short candidate: below the floor, so the window takes its minimum
    ui.set_candidates(&["水"]).await;
    let narrow = poll_until(SETTLE_TIMEOUT, || {
        let width = candidate.logical_width();
        ((width - 225.0).abs() < 2.0).then_some(width)
    })
    .unwrap_or_else(|| {
        panic!(
            "expected the 225px floor, got {:.0}px (at {} dpi)",
            candidate.logical_width(),
            candidate.dpi()
        )
    });

    // 20 chars -> 120 + 20*18 = 480 logical px, well past the floor
    ui.set_candidates(&["あいうえおかきくけこさしすせそたちつてと"])
        .await;
    poll_until(SETTLE_TIMEOUT, || {
        let width = candidate.logical_width();
        ((width - 480.0).abs() < 2.0).then_some(width)
    })
    .unwrap_or_else(|| {
        panic!(
            "expected the window to widen from {narrow:.0}px to 480px, got {:.0}px \
             (at {} dpi)",
            candidate.logical_width(),
            candidate.dpi()
        )
    });

    // ...but only up to the cap. Candidate length is unbounded, and an
    // uncapped window stretched until it had to be slid away from the caret
    // to fit the work area, taking the whole list away from the text.
    ui.set_candidates(&["あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほまみむめも"])
        .await;
    poll_until(SETTLE_TIMEOUT, || {
        let width = candidate.logical_width();
        ((width - 640.0).abs() < 2.0).then_some(width)
    })
    .unwrap_or_else(|| {
        panic!(
            "a 35-char candidate must stop at the 640px cap, got {:.0}px (at {} dpi)",
            candidate.logical_width(),
            candidate.dpi()
        )
    });

    ui.assert_no_errors();
}

/// The window is as tall as the list it is showing, not a fixed five rows
/// (issue #81).
///
/// The wasted space was the lesser half of that bug. `clamp_candidate_position`
/// flips the window above the caret whenever it would overrun the work area,
/// and it flips by the window's HEIGHT — so near the bottom of the screen a
/// two-item list jumped five rows' worth upward and landed over the host
/// application's title bar, far from the text it belonged to.
///
/// The assertions are relative: row height comes from the CSS and changes with
/// the theme, but "five rows is taller than two" and "eight is not taller than
/// five" hold whatever it is.
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn the_window_is_as_tall_as_the_list() {
    const FIVE: [&str; 5] = ["水", "見ず", "みず", "ミズ", "瑞"];
    const EIGHT: [&str; 8] = ["水", "見ず", "みず", "ミズ", "瑞", "水面", "水位", "淡水"];

    let mut ui = Ui::start().await;
    let candidate = ui.candidate();
    ui.show_at(caret_with_room()).await;

    // the startup measurement sizes for a full list, so a short one must
    // SHRINK the window
    let full = candidate.logical_height();
    ui.set_candidates(&["水", "見ず"]).await;
    let two = poll_until(SETTLE_TIMEOUT, || {
        let height = candidate.logical_height();
        (height < full - 1.0).then_some(height)
    })
    .unwrap_or_else(|| {
        panic!(
            "a two-item list must not keep the full-height window: still {:.0}px \
             (at {} dpi)",
            candidate.logical_height(),
            candidate.dpi()
        )
    });

    ui.set_candidates(&FIVE).await;
    let five = poll_until(SETTLE_TIMEOUT, || {
        let height = candidate.logical_height();
        (height > two + 1.0).then_some(height)
    })
    .unwrap_or_else(|| panic!("a five-item list must grow the window back from {two:.0}px"));
    assert!(
        (five - full).abs() < 2.0,
        "five candidates is what the startup measurement sized for, so the two \
         must agree: {five:.0}px vs {full:.0}px"
    );

    // past five the list scrolls instead of growing — updateSelection pages
    // through it in groups of five, which only works if five is what fits
    ui.set_candidates(&EIGHT).await;
    if let Some(grown) = poll_until(Duration::from_millis(750), || {
        let height = candidate.logical_height();
        ((height - five).abs() > 2.0).then_some(height)
    }) {
        panic!("eight candidates must scroll, not grow: {five:.0}px -> {grown:.0}px");
    }

    ui.assert_no_errors();
}

/// The mode indicator flashes on a mode change and takes itself back down,
/// showing the mode it was given — the checklist's あ/A item, minus the
/// looking.
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn the_mode_indicator_flashes_and_shows_the_mode() {
    let mut ui = Ui::start().await;
    let indicator = ui.indicator();

    ui.set_position(caret_with_room()).await;
    ui.set_input_mode("あ").await;

    wait_for(SETTLE_TIMEOUT, "the mode indicator to appear", || {
        indicator.is_visible()
    });
    ui.indicator_shows("あ").await;

    // and it switches, rather than only ever showing the first mode it got
    ui.indicator_shows("A").await;

    // the flash is half a second: once nothing refreshes it, it goes away on
    // its own — an indicator that stayed up would sit over the application
    wait_for(SETTLE_TIMEOUT, "the mode indicator to hide itself", || {
        !indicator.is_visible()
    });

    ui.assert_no_errors();
}

/// Both overlays must stay non-activating tool windows in the topmost band:
/// an IME window that can take focus steals it from the application being
/// typed into, and one that is not topmost hides behind it.
#[tokio::test]
#[ignore = "spawns ui.exe (needs a desktop and the WebView2 runtime)"]
async fn the_overlays_are_non_activating_topmost_tool_windows() {
    let mut ui = Ui::start().await;
    ui.show_at(caret_with_room()).await;

    for (name, window) in [("candidate", ui.candidate()), ("indicator", ui.indicator())] {
        let style = window.extended_style();
        for (flag, label) in [
            (WS_EX_NOACTIVATE.0, "WS_EX_NOACTIVATE"),
            (WS_EX_TOOLWINDOW.0, "WS_EX_TOOLWINDOW"),
            (WS_EX_TOPMOST.0, "WS_EX_TOPMOST"),
        ] {
            assert_ne!(
                style & flag,
                0,
                "the {name} window lost {label} (style {style:#x})"
            );
        }
    }

    ui.assert_no_errors();
}
