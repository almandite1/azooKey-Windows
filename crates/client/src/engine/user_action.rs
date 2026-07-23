use crate::extension::VKeyExt;
use anyhow::Result;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardState, ToUnicode, VK_CAPITAL, VK_KANA, VK_SHIFT,
};

/// Interprets what `ToUnicode` wrote, given what it returned.
///
/// A negative count is a dead key. The buffer holds the accent on its own,
/// and treating that as typed text — which is what reading `units[0]`
/// unconditionally did — puts a stray `^` into the reading. A dead key has
/// no text of its own, so it decodes to nothing and reaches the host, whose
/// own dead-key handling is the only thing that can compose it: our call
/// passes TOUNICODE_NO_KBD_STATE_CHANGE and so never advances the kernel's
/// dead-key state machine (it cannot: this runs twice per keystroke).
///
/// Zero means the key produces no character. Otherwise the buffer holds
/// `count` UTF-16 units, which is more than one for a non-BMP character
/// (a surrogate pair) or for a key that produces several characters.
fn decoded_text(count: i32, units: &[u16]) -> Option<String> {
    let units = units.get(..usize::try_from(count).ok()?)?;
    let text = String::from_utf16(units).ok()?;

    (!text.is_empty()).then_some(text)
}

/// Clears the keyboard *locks* that must not colour the romaji decoding.
///
/// Both of these are toggles whose entire purpose is to change what a Latin
/// key produces, and `ToUnicode` honours them — but the engine takes plain
/// lowercase romaji, so neither answer is ever what we want:
///
/// * **Kana lock** (`VK_KANA`): with the lock on, the A key answers ち. Not
///   hypothetical — ATOK's かな入力 mode leaves the lock set, and switching
///   back to azooKey then typed ﾁ for every A.
/// * **Caps Lock** (`VK_CAPITAL`): with the lock on, the A key answers 'A',
///   and the engine cannot make kana of uppercase. Typing あいうえお
///   produced `AIUEO` instead, i.e. Japanese input stopped working entirely
///   (issue #80).
///
/// Safe to do unconditionally even though direct-input users legitimately
/// want Caps Lock: in `InputMode::Latin` the transition table hands the key
/// straight back to the host (`transition` answers `None` for `Input`), so
/// the host applies the lock to the key itself and this decoding is never
/// consulted.
///
/// Clearing the whole byte drops the toggle bit (0) and the pressed bit (7)
/// alike, which is what keeps the decoding independent of whatever another
/// IME left behind.
fn without_input_locks(mut key_state: [u8; 256]) -> [u8; 256] {
    key_state[VK_KANA.0 as usize] = 0;
    key_state[VK_CAPITAL.0 as usize] = 0;
    key_state
}

/// Whether a virtual key means "switch the IME on/off".
///
/// `VK_KANJI` (0x19) is one of them because Windows translates **Alt+`** on a
/// 101-key Japanese layout into it, with Alt still held. `process_key` has to
/// know that before it reaches its Ctrl/Alt chord branch, which would
/// otherwise discard the chord as a host shortcut (issue #19).
pub fn is_ime_toggle_key(key_code: usize) -> bool {
    matches!(key_code, 0xF3 | 0xF4 | 0x19)
}

#[derive(Debug)]
pub enum UserAction {
    /// The text this keystroke produced. A `String` rather than a `char`
    /// because one keystroke can yield several characters, and because a
    /// non-BMP character arrives as two UTF-16 units.
    Input(String),
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

            // Zenkaku/Hankaku, and VK_KANJI — which is what Windows
            // translates Alt+` into on a 101-key Japanese layout
            0xF3 | 0xF4 | 0x19 => UserAction::ToggleInputMode,

            _ => {
                let key_state = {
                    let mut key_state = [0u8; 256];
                    unsafe {
                        GetKeyboardState(&mut key_state)?;
                    }

                    without_input_locks(key_state)
                };
                let text = {
                    // Bit 2 = "do not change keyboard state" (Win10 1607+;
                    // ignored and harmless on older builds). Without it,
                    // ToUnicode consumes kernel dead-key state — and this
                    // path runs TWICE per keystroke (OnTestKeyDown and
                    // OnKeyDown both call process_key), so on layouts with
                    // dead keys (e.g. US-International) the state was
                    // double-consumed and output garbled (B17).
                    const TOUNICODE_NO_KBD_STATE_CHANGE: u32 = 0x4;
                    // Room for a surrogate pair and then some: ToUnicode can
                    // answer with several UTF-16 units, and a buffer of one
                    // silently truncated everything above the BMP.
                    let mut units = [0u16; 8];
                    let count = unsafe {
                        ToUnicode(
                            key_code as u32,
                            0,
                            Some(&key_state),
                            &mut units,
                            TOUNICODE_NO_KBD_STATE_CHANGE,
                        )
                    };
                    decoded_text(count, &units)
                };

                match text {
                    Some(text) => UserAction::Input(text),
                    None => UserAction::Unknown,
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

    /// A dead key (US-International `^`, `¨`, …) leaves the bare accent in
    /// the buffer and reports it with a negative count. Reading the buffer
    /// regardless — what the one-unit version did — typed that accent into
    /// the reading (issue #28).
    #[test]
    fn a_dead_key_produces_no_text() {
        assert_eq!(decoded_text(-1, &[0x005E, 0, 0, 0]), None);
    }

    #[test]
    fn a_key_with_no_character_produces_no_text() {
        assert_eq!(decoded_text(0, &[0, 0, 0, 0]), None);
    }

    #[test]
    fn a_single_unit_is_the_character_itself() {
        assert_eq!(decoded_text(1, &[0x0061, 0, 0, 0]), Some("a".to_string()));
    }

    /// A non-BMP character arrives as a surrogate pair. The one-unit buffer
    /// could only ever hold the high surrogate, so these were unproducible.
    #[test]
    fn a_surrogate_pair_becomes_one_character() {
        // U+1F600 GRINNING FACE
        let text = decoded_text(2, &[0xD83D, 0xDE00, 0, 0]).expect("a valid surrogate pair");

        assert_eq!(text, "\u{1F600}");
        assert_eq!(text.chars().count(), 1);
    }

    /// Some keys and layouts answer with several characters at once; the
    /// count says how many units are meant, and the rest of the buffer is
    /// not ours to read.
    #[test]
    fn several_units_become_several_characters() {
        assert_eq!(
            decoded_text(2, &[0x0061, 0x0062, 0x0063, 0]),
            Some("ab".to_string())
        );
    }

    /// A lone surrogate is not text; refusing it keeps the caller on the
    /// "no character" path instead of panicking or inventing a replacement.
    #[test]
    fn an_unpaired_surrogate_is_refused() {
        assert_eq!(decoded_text(1, &[0xD83D, 0, 0, 0]), None);
    }

    /// ToUnicode never writes past the buffer it was given, but the count
    /// drives a slice index and must not be trusted blindly.
    #[test]
    fn a_count_past_the_buffer_is_refused() {
        assert_eq!(decoded_text(9, &[0x0061; 4]), None);
    }

    /// Issue #80: with Caps Lock on, `ToUnicode` answers 'A' for the A key
    /// and the engine — which takes lowercase romaji — cannot make kana of
    /// it. Japanese input stopped working entirely: あいうえお came out as
    /// `AIUEO`. The kana lock was already cleared here for exactly the same
    /// reason; Caps Lock was not.
    #[test]
    fn caps_lock_is_cleared_before_decoding() {
        let mut key_state = [0u8; 256];
        // a toggled lock: bit 0 set (and bit 7 while the key is held)
        key_state[VK_CAPITAL.0 as usize] = 0x81;

        assert_eq!(
            without_input_locks(key_state)[VK_CAPITAL.0 as usize],
            0,
            "the toggle AND the pressed bit must both go, or ToUnicode still \
             sees the lock"
        );
    }

    /// The original lock this guard existed for (ATOK's かな入力 leaves it
    /// set). Guarding it here so a future edit cannot drop one while adding
    /// the other.
    #[test]
    fn the_kana_lock_is_cleared_before_decoding() {
        let mut key_state = [0u8; 256];
        key_state[VK_KANA.0 as usize] = 0x01;

        assert_eq!(without_input_locks(key_state)[VK_KANA.0 as usize], 0);
    }

    /// Only the locks. Shift is a per-keystroke modifier the user is holding
    /// on purpose, and the decoding of every other key must be untouched.
    #[test]
    fn no_other_key_state_is_disturbed() {
        let mut key_state = [0u8; 256];
        key_state[VK_SHIFT.0 as usize] = 0x80;
        key_state[0x41] = 0x80; // A held
        key_state[VK_CAPITAL.0 as usize] = 0x01;

        let cleaned = without_input_locks(key_state);

        assert_eq!(cleaned[VK_SHIFT.0 as usize], 0x80, "Shift is deliberate");
        assert_eq!(cleaned[0x41], 0x80);
        assert_eq!(
            cleaned.iter().filter(|b| **b != 0).count(),
            2,
            "exactly the two keys set above survive"
        );
    }

    /// VK_KANJI is an IME on/off key, not a character key: Windows
    /// translates Alt+` on a 101-key Japanese layout into it. Decoding it
    /// through the ToUnicode branch instead made it Unknown, and the chord
    /// branch in process_key then discarded it (issue #19).
    #[test]
    fn the_ime_toggle_keys_include_vk_kanji() {
        for vk in [0xF3usize, 0xF4, 0x19] {
            assert!(is_ime_toggle_key(vk), "0x{vk:02X} switches the IME");
            assert!(
                matches!(
                    UserAction::try_from(vk).unwrap(),
                    UserAction::ToggleInputMode
                ),
                "0x{vk:02X} must decode to ToggleInputMode"
            );
        }
    }

    /// …and ordinary keys must not be mistaken for one, or the chord branch
    /// would be bypassed for every host shortcut.
    #[test]
    fn ordinary_keys_are_not_ime_toggle_keys() {
        for vk in [0x41usize, 0x20, 0x0D, 0xC0, 0x1B] {
            assert!(!is_ime_toggle_key(vk), "0x{vk:02X} is not an on/off key");
        }
    }

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
