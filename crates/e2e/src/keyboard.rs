//! Synthesised keystrokes.
//!
//! `SendInput` rather than `PostMessage`: the TIP sits on the TSF keystroke
//! path, which is fed from the system input queue. Messages posted straight
//! to a window bypass it entirely and would prove nothing.

use std::time::Duration;

use anyhow::{Result, bail};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW,
    SendInput, VIRTUAL_KEY, VK_BACK, VK_ESCAPE, VK_RETURN, VK_SPACE, VK_TAB,
};

/// `VK_DBE_DBCSCHAR` — one of the two virtual keys the 半角/全角 key produces.
/// The TIP reserves it (and its `VK_DBE_SBCSCHAR` sibling) with no modifiers
/// and routes both through `OnPreservedKey`, which flips the input mode
/// (`crates/client/src/tsf/preserved_key.rs`). A freshly activated azooKey
/// defaults to Latin (`InputMode`'s `#[default]`), so a scenario that expects
/// conversion has to send this once to reach Kana — the manual spike worked
/// only because a human had already switched it.
const VK_ZENKAKU_HANKAKU: u16 = 0xF4;
/// The physical scan code of the 半角/全角 key (shared with `` ` `` on JIS).
/// `MapVirtualKeyW` does not resolve the DBE virtual keys to a scan code, so
/// it is supplied explicitly.
const SCAN_ZENKAKU_HANKAKU: u16 = 0x29;

/// Toggles the input mode via the 半角/全角 key. From the default Latin this
/// reaches Kana; call it once before a scenario that expects conversion.
pub fn toggle_input_mode() -> Result<()> {
    send_with_scan(VIRTUAL_KEY(VK_ZENKAKU_HANKAKU), SCAN_ZENKAKU_HANKAKU, false)?;
    std::thread::sleep(Duration::from_millis(15));
    send_with_scan(VIRTUAL_KEY(VK_ZENKAKU_HANKAKU), SCAN_ZENKAKU_HANKAKU, true)?;
    std::thread::sleep(KEY_INTERVAL);
    Ok(())
}

/// Gap between keystrokes. Real typing is not instantaneous and neither is
/// the conversion round trip; a burst with no gap has repeatedly been the
/// difference between "the IME is broken" and "the test types faster than
/// anything can respond".
const KEY_INTERVAL: Duration = Duration::from_millis(40);

pub fn space() -> VIRTUAL_KEY {
    VK_SPACE
}

pub fn enter() -> VIRTUAL_KEY {
    VK_RETURN
}

pub fn escape() -> VIRTUAL_KEY {
    VK_ESCAPE
}

pub fn backspace() -> VIRTUAL_KEY {
    VK_BACK
}

/// Tab — how a person reaches the next field. Outside a composition the TIP
/// leaves it unhandled, so it reaches the host and moves focus.
pub fn tab() -> VIRTUAL_KEY {
    VK_TAB
}

/// Types an ASCII string one key at a time — the romaji a user would type.
///
/// Only the characters a romaji reading needs are supported; anything else is
/// an error rather than a silently dropped keystroke, which would show up as
/// an inexplicably wrong conversion much later.
pub fn type_ascii(text: &str) -> Result<()> {
    for ch in text.chars() {
        let vk = match ch {
            'a'..='z' => VIRTUAL_KEY((ch.to_ascii_uppercase() as u16) - b'A' as u16 + 0x41),
            'A'..='Z' => VIRTUAL_KEY(ch as u16),
            '0'..='9' => VIRTUAL_KEY(ch as u16),
            ' ' => VK_SPACE,
            _ => bail!("no virtual key mapping for {ch:?}"),
        };
        tap(vk)?;
    }
    Ok(())
}

/// Presses and releases one key.
pub fn tap(vk: VIRTUAL_KEY) -> Result<()> {
    send(vk, false)?;
    std::thread::sleep(Duration::from_millis(15));
    send(vk, true)?;
    std::thread::sleep(KEY_INTERVAL);
    Ok(())
}

fn send(vk: VIRTUAL_KEY, up: bool) -> Result<()> {
    // The scan code is filled in alongside the virtual key rather than used
    // instead of it: layers below TSF (and the TIP's own ToUnicode decoding)
    // read the scan code, while the keystroke sink matches on the virtual
    // key. Sending only one of the two leaves the other reading zero.
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    send_with_scan(vk, scan, up)
}

fn send_with_scan(vk: VIRTUAL_KEY, scan: u16, up: bool) -> Result<()> {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent != 1 {
        bail!(
            "SendInput was blocked (sent {sent} of 1) — UIPI, or a more privileged window has focus"
        );
    }
    Ok(())
}
