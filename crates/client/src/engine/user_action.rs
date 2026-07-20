use crate::extension::VKeyExt;
use anyhow::{Context, Result};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, ToUnicode, VK_KANA, VK_SHIFT};

#[derive(Debug)]
pub enum UserAction {
    Input(char),
    Backspace,
    Enter,
    Space,
    Tab,
    Escape,
    Unknown,
    Navigation(Navigation),
    Function(Function),
    Number(i8),
    ToggleInputMode,
    /// Host editing keys (Delete/Insert/Home/End/PageUp/PageDown). Left to
    /// the host while idle; during a composition they must be consumed as
    /// no-ops — passed through, the host edits the document underneath the
    /// open composition (Delete removes the character right after it).
    EditingKey,
}

#[derive(Debug)]
pub enum Navigation {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug)]
pub enum Function {
    Six,
    Seven,
    Eight,
    Nine,
    Ten,
}

impl TryFrom<usize> for UserAction {
    type Error = anyhow::Error;
    fn try_from(key_code: usize) -> Result<UserAction> {
        let action = match key_code {
            0x08 => UserAction::Backspace, // VK_BACK
            0x09 => UserAction::Tab,       // VK_TAB
            0x0D => UserAction::Enter,     // VK_RETURN
            0x20 => UserAction::Space,     // VK_SPACE
            0x1B => UserAction::Escape,    // VK_ESCAPE

            // VK_PRIOR, VK_NEXT, VK_END, VK_HOME, VK_INSERT, VK_DELETE
            0x21..=0x24 | 0x2D | 0x2E => UserAction::EditingKey,

            0x25 => UserAction::Navigation(Navigation::Left), // VK_LEFT
            0x26 => UserAction::Navigation(Navigation::Up),   // VK_UP
            0x27 => UserAction::Navigation(Navigation::Right), // VK_RIGHT
            0x28 => UserAction::Navigation(Navigation::Down), // VK_DOWN

            0x30..=0x39 | 0x60..=0x69 if !VK_SHIFT.is_pressed() => {
                match key_code {
                    0x30 | 0x60 => UserAction::Number(0), // VK_0, VK_NUMPAD0
                    0x31 | 0x61 => UserAction::Number(1), // VK_1, VK_NUMPAD1
                    0x32 | 0x62 => UserAction::Number(2), // VK_2, VK_NUMPAD2
                    0x33 | 0x63 => UserAction::Number(3), // VK_3, VK_NUMPAD3
                    0x34 | 0x64 => UserAction::Number(4), // VK_4, VK_NUMPAD4
                    0x35 | 0x65 => UserAction::Number(5), // VK_5, VK_NUMPAD5
                    0x36 | 0x66 => UserAction::Number(6), // VK_6, VK_NUMPAD6
                    0x37 | 0x67 => UserAction::Number(7), // VK_7, VK_NUMPAD7
                    0x38 | 0x68 => UserAction::Number(8), // VK_8, VK_NUMPAD8
                    0x39 | 0x69 => UserAction::Number(9), // VK_9, VK_NUMPAD9
                    _ => UserAction::Unknown,
                }
            }

            0x75 => UserAction::Function(Function::Six), // VK_F6
            0x76 => UserAction::Function(Function::Seven), // VK_F7
            0x77 => UserAction::Function(Function::Eight), // VK_F8
            0x78 => UserAction::Function(Function::Nine), // VK_F9
            0x79 => UserAction::Function(Function::Ten), // VK_F10

            0xF3 | 0xF4 => UserAction::ToggleInputMode, // Zenkaku/Hankaku

            _ => {
                let key_state = {
                    let mut key_state = [0u8; 256];
                    unsafe {
                        GetKeyboardState(&mut key_state)?;
                    }

                    // Ignore the kana lock. ToUnicode honours the VK_KANA
                    // toggle, so with the lock on it answers ち for the A
                    // key instead of 'a'. The engine takes romaji, so that
                    // reading is never what we want.
                    //
                    // This is not hypothetical: ATOK's かな入力 mode leaves
                    // the lock set, and switching back to azooKey then typed
                    // ﾁ for every A. Clearing both the toggle (bit 0) and the
                    // pressed (bit 7) flag keeps our decoding independent of
                    // whatever the previous IME left behind.
                    key_state[VK_KANA.0 as usize] = 0;

                    key_state
                };
                let unicode = {
                    // Bit 2 = "do not change keyboard state" (Win10 1607+;
                    // ignored and harmless on older builds). Without it,
                    // ToUnicode consumes kernel dead-key state — and this
                    // path runs TWICE per keystroke (OnTestKeyDown and
                    // OnKeyDown both call process_key), so on layouts with
                    // dead keys (e.g. US-International) the state was
                    // double-consumed and output garbled (B17). Known,
                    // pre-existing limitations left as-is: the return value
                    // is ignored (-1 dead key / 2+ chars are not handled)
                    // and the 1-unit buffer cannot represent non-BMP output.
                    const TOUNICODE_NO_KBD_STATE_CHANGE: u32 = 0x4;
                    let mut unicode = [0u16; 1];
                    unsafe {
                        ToUnicode(
                            key_code as u32,
                            0,
                            Some(&key_state),
                            &mut unicode,
                            TOUNICODE_NO_KBD_STATE_CHANGE,
                        )
                    };
                    unicode[0]
                };

                if unicode != 0 {
                    UserAction::Input(char::from_u32(unicode as u32).context("Invalid char")?)
                } else {
                    UserAction::Unknown
                }
            }
        };

        Ok(action)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The host editing keys must decode to EditingKey — falling through to
    /// the ToUnicode branch instead would make them Unknown and pass them
    /// through mid-composition (the issue-#5-sibling bug).
    #[test]
    fn host_editing_keys_decode_to_editing_key() {
        // VK_PRIOR, VK_NEXT, VK_END, VK_HOME, VK_INSERT, VK_DELETE
        for vk in [0x21usize, 0x22, 0x23, 0x24, 0x2D, 0x2E] {
            assert!(
                matches!(UserAction::try_from(vk).unwrap(), UserAction::EditingKey),
                "0x{vk:02X} must decode to EditingKey"
            );
        }
    }
}
