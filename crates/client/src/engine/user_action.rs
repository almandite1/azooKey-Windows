use crate::engine::client_action::SetTextType;
use crate::extension::VKeyExt;
use crate::globals::VK_IME_TOGGLE;
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
/// `VK_KANJI` is one of them because Windows translates **Alt+`** on a
/// 101-key Japanese layout into it, with Alt still held. `process_key` has to
/// know that before it reaches its Ctrl/Alt chord branch, which would
/// otherwise discard the chord as a host shortcut (issue #19).
///
/// The keys themselves are [`VK_IME_TOGGLE`], shared with the reservations
/// `tsf::preserved_key` makes — the two must not drift apart.
pub fn is_ime_toggle_key(key_code: usize) -> bool {
    u32::try_from(key_code).is_ok_and(|vk| VK_IME_TOGGLE.contains(&vk))
}

/// What the `lparam` of a WM_KEYDOWN says about autorepeat.
///
/// Two independent facts, both needed: `count` is how many presses this one
/// message stands for, and `is_repeat` says whether the key was already down
/// — a held key rather than a fresh press. The distinction matters because a
/// fresh press is always the user's intent, while a repeat may have to be
/// discarded once the composition it was deleting is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyRepeat {
    pub count: u32,
    pub is_repeat: bool,
}

impl KeyRepeat {
    /// One deliberate press, no autorepeat.
    ///
    /// Test-only: in production every decode starts from a real `lparam`, and
    /// a constant standing in for one would be a way to lose the repeat
    /// information without noticing.
    #[cfg(test)]
    pub const SINGLE: Self = Self {
        count: 1,
        is_repeat: false,
    };
}

/// Decodes the autorepeat fields of a keydown `lparam`.
///
/// Bits 0–15 are the repeat count: the host's message pump coalesces presses
/// that arrived while it was busy, so one WM_KEYDOWN can stand for several.
/// Bit 30 is the previous key state — set when the key was already down, i.e.
/// this is autorepeat rather than a fresh press.
///
/// A count of zero is not something Windows sends, but it is what a synthetic
/// or forwarded event can carry, and zero presses would be a keystroke that
/// does nothing; it is raised to one. Whether the batch is worth batching is
/// the caller's decision, not this one's.
pub fn key_repeat(lparam: isize) -> KeyRepeat {
    KeyRepeat {
        count: ((lparam as u32) & 0xFFFF).max(1),
        // bit 30: 1 = the key was down before this message
        is_repeat: (lparam as u32) & (1 << 30) != 0,
    }
}

#[derive(Debug)]
pub enum UserAction {
    /// The text this keystroke produced. A `String` rather than a `char`
    /// because one keystroke can yield several characters, and because a
    /// non-BMP character arrives as two UTF-16 units.
    Input(String),
    /// `count` presses' worth of Backspace in one keystroke — see
    /// [`key_repeat`]. Only Backspace carries it: it is the only key whose
    /// autorepeat costs a full reconversion per press, and the only one where
    /// N presses provably mean the same thing as one press repeated N times
    /// (a held あ key still has to go through the engine one keystroke at a
    /// time, because each one can change the reading's romaji state).
    Backspace {
        count: u32,
    },
    Enter,
    Space,
    Tab,
    Escape,
    Unknown,
    Navigation(Navigation),
    /// F6–F10: rewrite the whole reading as the named form. Carries the
    /// [`SetTextType`] directly rather than an F-key-shaped enum of its own,
    /// so the five forms are listed once (there) instead of being restated
    /// here and re-matched one-for-one in the transition table.
    Function(SetTextType),
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

impl UserAction {
    /// Decodes a virtual key into the action it means.
    ///
    /// `repeat` is what the keydown's lparam said (see [`key_repeat`]); it
    /// only reaches Backspace, and callers that have no lparam — or no
    /// interest in one — pass [`KeyRepeat::SINGLE`].
    pub fn decode(key_code: usize, repeat: KeyRepeat) -> Result<UserAction> {
        let action = match key_code {
            0x08 => UserAction::Backspace {
                count: repeat.count,
            }, // VK_BACK
            0x09 => UserAction::Tab,    // VK_TAB
            0x0D => UserAction::Enter,  // VK_RETURN
            0x20 => UserAction::Space,  // VK_SPACE
            0x1B => UserAction::Escape, // VK_ESCAPE

            // VK_PRIOR, VK_NEXT, VK_END, VK_HOME, VK_INSERT, VK_DELETE
            0x21..=0x24 | 0x2D | 0x2E => UserAction::EditingKey,

            0x25 => UserAction::Navigation(Navigation::Left), // VK_LEFT
            0x26 => UserAction::Navigation(Navigation::Up),   // VK_UP
            0x27 => UserAction::Navigation(Navigation::Right), // VK_RIGHT
            0x28 => UserAction::Navigation(Navigation::Down), // VK_DOWN

            // VK_0..VK_9 and VK_NUMPAD0..VK_NUMPAD9. Both runs are laid out
            // so the low nibble IS the digit, which is what the ten-arm table
            // this replaces spelled out one line at a time.
            0x30..=0x39 | 0x60..=0x69 if !VK_SHIFT.is_pressed() => {
                UserAction::Number((key_code & 0x0F) as i8)
            }

            // VK_F6..VK_F10
            0x75 => UserAction::Function(SetTextType::Hiragana),
            0x76 => UserAction::Function(SetTextType::Katakana),
            0x77 => UserAction::Function(SetTextType::HalfKatakana),
            0x78 => UserAction::Function(SetTextType::FullLatin),
            0x79 => UserAction::Function(SetTextType::HalfLatin),

            // Zenkaku/Hankaku, and VK_KANJI — which is what Windows
            // translates Alt+` into on a 101-key Japanese layout
            _ if is_ime_toggle_key(key_code) => UserAction::ToggleInputMode,

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

    /// One lparam's worth of bits as the platform's `isize`.
    ///
    /// The cast chain matters: `lparam` is 32 bits wide in the x86 TIP, where
    /// a value with bit 31 set is a NEGATIVE isize. Writing the literal
    /// directly is a compile error there, and dropping the top bit instead
    /// would test something other than what Windows sends.
    fn lparam(bits: u32) -> isize {
        bits as i32 as isize
    }

    /// The two fields live in one 32-bit word and are read from opposite
    /// ends of it, so a mask that was one bit off would still look plausible
    /// on a fresh press.
    #[test]
    fn a_fresh_press_carries_one_press_and_no_repeat_flag() {
        let repeat = key_repeat(lparam(0x0001_0001));
        assert_eq!(repeat.count, 1);
        assert!(!repeat.is_repeat, "bit 30 is clear on the first press");
    }

    /// What a held key looks like: bit 30 set, and a count the host's message
    /// pump inflated while it was busy. Both halves matter — the count is the
    /// batching, the flag is the discard guard.
    #[test]
    fn a_held_key_carries_the_coalesced_count_and_the_repeat_flag() {
        // bit 30 (previous state) + bit 29..24 scan-code noise + count 5
        let repeat = key_repeat(lparam(0x4001_0005));
        assert_eq!(repeat.count, 5, "only the low word is the count");
        assert!(repeat.is_repeat);
    }

    /// Bit 31 is the transition state (set on key-UP) and bit 29 is the
    /// context code (Alt). Neither says anything about autorepeat, and reading
    /// one of them as bit 30 would discard keystrokes the user meant.
    #[test]
    fn the_neighbouring_lparam_bits_are_not_the_repeat_flag() {
        assert!(
            !key_repeat(lparam(0x8000_0001)).is_repeat,
            "bit 31 is key-up"
        );
        assert!(
            !key_repeat(lparam(0x2000_0001)).is_repeat,
            "bit 29 is the Alt context"
        );
    }

    /// Zero presses would be a keystroke that does nothing at all. Windows
    /// does not send it, but a synthetic or forwarded event can.
    #[test]
    fn a_zero_count_is_raised_to_one() {
        assert_eq!(key_repeat(lparam(0)).count, 1);
        assert_eq!(key_repeat(lparam(0x4000_0000)).count, 1);
    }

    /// Only Backspace carries the count; every other key would be wrong to
    /// collapse presses (each あ keystroke can change the reading's romaji
    /// state on its own).
    #[test]
    fn only_backspace_carries_the_repeat_count() {
        let held = KeyRepeat {
            count: 7,
            is_repeat: true,
        };

        assert!(matches!(
            UserAction::decode(0x08, held).unwrap(),
            UserAction::Backspace { count: 7 }
        ));
        assert!(matches!(
            UserAction::decode(0x0D, held).unwrap(),
            UserAction::Enter
        ));
    }

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
                    UserAction::decode(vk, KeyRepeat::SINGLE).unwrap(),
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
                matches!(
                    UserAction::decode(vk, KeyRepeat::SINGLE).unwrap(),
                    UserAction::EditingKey
                ),
                "0x{vk:02X} must decode to EditingKey"
            );
        }
    }
}
