//! Synthesised keystrokes.
//!
//! `SendInput` rather than `PostMessage`: the TIP sits on the TSF keystroke
//! path, which is fed from the system input queue. Messages posted straight
//! to a window bypass it entirely and would prove nothing.

use std::time::Duration;

use anyhow::{Result, bail};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW,
    SendInput, VIRTUAL_KEY, VK_BACK, VK_ESCAPE, VK_RETURN, VK_SPACE,
};

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
