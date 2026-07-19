//! The pure keystroke transition table of the IME state machine.
//!
//! `transition` maps (current state, decoded key, input mode) to the next
//! state and the actions to run — no COM, no IPC, no globals — so every
//! branch is unit-testable and future features (predictive conversion,
//! contextual conversion) add table cases plus a test line instead of
//! touching the effectful interpreter. The impure parts stay in
//! `process_key` (guards, key decoding) and `handle_action` (execution).

use super::{
    client_action::{ClientAction, SetSelectionType, SetTextType},
    composition::CompositionState,
    input_mode::InputMode,
    user_action::{Function, Navigation, UserAction},
};

/// The state read by the transition table — a snapshot of the fields the
/// table needs, cheap to build and to fabricate in tests.
#[derive(Debug, Clone)]
pub struct KeystrokeContext {
    pub state: CompositionState,
    pub mode: InputMode,
    /// `raw_hiragana.chars().count()`: the reading's kana count — the unit
    /// RemoveText actually deletes on the engine side. Backspace decisions
    /// must use this, never the converted preview's length: a multi-kana
    /// reading can have a single-char candidate (さい→際), and ending on
    /// "preview == 1 char" committed the leftover reading (issue #35).
    pub reading_chars: usize,
    /// `suffix.is_empty()`: Enter with nothing pending commits and ends;
    /// otherwise it commits the selected candidate and keeps composing.
    pub suffix_is_empty: bool,
}

/// Returns `None` when the keystroke is not handled (the host application
/// should process it).
pub fn transition(
    ctx: &KeystrokeContext,
    action: UserAction,
) -> Option<(CompositionState, Vec<ClientAction>)> {
    let result = match ctx.state {
        CompositionState::None => match action {
            UserAction::Input(char) if ctx.mode == InputMode::Kana => (
                CompositionState::Composing,
                vec![
                    ClientAction::StartComposition,
                    ClientAction::AppendText(char.to_string()),
                ],
            ),
            UserAction::Number(number) if ctx.mode == InputMode::Kana => (
                CompositionState::Composing,
                vec![
                    ClientAction::StartComposition,
                    ClientAction::AppendText(number.to_string()),
                ],
            ),
            UserAction::ToggleInputMode => (
                CompositionState::None,
                vec![match ctx.mode {
                    InputMode::Kana => ClientAction::SetIMEMode(InputMode::Latin),
                    InputMode::Latin => ClientAction::SetIMEMode(InputMode::Kana),
                }],
            ),
            _ => return None,
        },
        // Composing and Previewing share every binding except how new
        // input is applied: while Previewing, the selected candidate is
        // committed first (ShrinkText) instead of appending
        ref state @ (CompositionState::Composing | CompositionState::Previewing) => {
            let input_action = |text: String| {
                if *state == CompositionState::Previewing {
                    ClientAction::ShrinkText(text)
                } else {
                    ClientAction::AppendText(text)
                }
            };

            match action {
                UserAction::Input(char) => (
                    CompositionState::Composing,
                    vec![input_action(char.to_string())],
                ),
                UserAction::Number(number) => (
                    CompositionState::Composing,
                    vec![input_action(number.to_string())],
                ),
                UserAction::Backspace => {
                    // <=: an empty reading (nothing left to remove) must
                    // also end rather than loop in Composing forever
                    if ctx.reading_chars <= 1 {
                        (
                            CompositionState::None,
                            vec![ClientAction::RemoveText, ClientAction::EndComposition],
                        )
                    } else {
                        (CompositionState::Composing, vec![ClientAction::RemoveText])
                    }
                }
                UserAction::Enter => {
                    if ctx.suffix_is_empty {
                        (CompositionState::None, vec![ClientAction::EndComposition])
                    } else {
                        (
                            CompositionState::Composing,
                            vec![ClientAction::ShrinkText("".to_string())],
                        )
                    }
                }
                UserAction::Escape => {
                    (CompositionState::None, vec![ClientAction::CancelComposition])
                }
                UserAction::Navigation(direction) => match direction {
                    Navigation::Right => {
                        (CompositionState::Composing, vec![ClientAction::MoveCursor(1)])
                    }
                    Navigation::Left => (
                        CompositionState::Composing,
                        vec![ClientAction::MoveCursor(-1)],
                    ),
                    Navigation::Up => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetSelection(SetSelectionType::Up)],
                    ),
                    Navigation::Down => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetSelection(SetSelectionType::Down)],
                    ),
                },
                UserAction::ToggleInputMode => (
                    CompositionState::None,
                    vec![
                        ClientAction::EndComposition,
                        ClientAction::SetIMEMode(InputMode::Latin),
                    ],
                ),
                UserAction::Space | UserAction::Tab => (
                    CompositionState::Previewing,
                    vec![ClientAction::SetSelection(SetSelectionType::Down)],
                ),
                UserAction::Function(key) => match key {
                    Function::Six => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::Hiragana)],
                    ),
                    Function::Seven => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::Katakana)],
                    ),
                    Function::Eight => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::HalfKatakana)],
                    ),
                    Function::Nine => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::FullLatin)],
                    ),
                    Function::Ten => (
                        CompositionState::Previewing,
                        vec![ClientAction::SetTextWithType(SetTextType::HalfLatin)],
                    ),
                },
                _ => return None,
            }
        }
    };
    Some(result)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ctx(state: CompositionState, mode: InputMode) -> KeystrokeContext {
        KeystrokeContext {
            state,
            mode,
            reading_chars: 2,
            suffix_is_empty: true,
        }
    }

    fn kana(state: CompositionState) -> KeystrokeContext {
        ctx(state, InputMode::Kana)
    }

    #[test]
    fn idle_kana_input_starts_a_composition() {
        let (next, actions) =
            transition(&kana(CompositionState::None), UserAction::Input('a')).unwrap();
        assert_eq!(next, CompositionState::Composing);
        assert_eq!(
            actions,
            vec![
                ClientAction::StartComposition,
                ClientAction::AppendText("a".to_string()),
            ]
        );
    }

    #[test]
    fn idle_kana_number_starts_a_composition() {
        let (next, actions) =
            transition(&kana(CompositionState::None), UserAction::Number(5)).unwrap();
        assert_eq!(next, CompositionState::Composing);
        assert_eq!(
            actions,
            vec![
                ClientAction::StartComposition,
                ClientAction::AppendText("5".to_string()),
            ]
        );
    }

    #[test]
    fn idle_latin_input_is_passed_through_to_the_host() {
        let latin = ctx(CompositionState::None, InputMode::Latin);
        assert!(transition(&latin, UserAction::Input('a')).is_none());
        assert!(transition(&latin, UserAction::Number(5)).is_none());
    }

    #[test]
    fn idle_toggle_flips_the_input_mode_both_ways() {
        let (next, actions) =
            transition(&kana(CompositionState::None), UserAction::ToggleInputMode).unwrap();
        assert_eq!(next, CompositionState::None);
        assert_eq!(actions, vec![ClientAction::SetIMEMode(InputMode::Latin)]);

        let latin = ctx(CompositionState::None, InputMode::Latin);
        let (_, actions) = transition(&latin, UserAction::ToggleInputMode).unwrap();
        assert_eq!(actions, vec![ClientAction::SetIMEMode(InputMode::Kana)]);
    }

    #[test]
    fn idle_ignores_editing_keys() {
        for action in [
            UserAction::Backspace,
            UserAction::Enter,
            UserAction::Escape,
            UserAction::Space,
            UserAction::Tab,
            UserAction::Unknown,
            UserAction::Navigation(Navigation::Up),
            UserAction::Function(Function::Six),
        ] {
            assert!(
                transition(&kana(CompositionState::None), action).is_none(),
                "idle state must not consume editing keys"
            );
        }
    }

    #[test]
    fn composing_input_appends_but_previewing_input_commits_first() {
        let (next, actions) =
            transition(&kana(CompositionState::Composing), UserAction::Input('k')).unwrap();
        assert_eq!(next, CompositionState::Composing);
        assert_eq!(actions, vec![ClientAction::AppendText("k".to_string())]);

        let (next, actions) =
            transition(&kana(CompositionState::Previewing), UserAction::Input('k')).unwrap();
        assert_eq!(next, CompositionState::Composing);
        assert_eq!(actions, vec![ClientAction::ShrinkText("k".to_string())]);
    }

    #[test]
    fn backspace_on_the_last_kana_ends_the_composition() {
        let mut context = kana(CompositionState::Composing);
        context.reading_chars = 1;
        let (next, actions) = transition(&context, UserAction::Backspace).unwrap();
        assert_eq!(next, CompositionState::None);
        assert_eq!(
            actions,
            vec![ClientAction::RemoveText, ClientAction::EndComposition]
        );
    }

    #[test]
    fn backspace_with_more_text_just_removes() {
        let (next, actions) =
            transition(&kana(CompositionState::Composing), UserAction::Backspace).unwrap();
        assert_eq!(next, CompositionState::Composing);
        assert_eq!(actions, vec![ClientAction::RemoveText]);
    }

    /// Upstream issue #35: さい converts to the single-char candidate 際,
    /// so the converted preview hits 1 char while the reading still has 2
    /// kana. RemoveText deletes ONE KANA — judging "last one" by the
    /// preview ended (= committed!) the composition a keystroke early,
    /// leaving 「さ」 behind. The reading is the only valid measure.
    #[test]
    fn backspace_judges_by_the_reading_not_the_converted_preview() {
        let mut context = kana(CompositionState::Previewing);
        context.reading_chars = 2; // さい — even though the preview 際 is 1 char
        let (next, actions) = transition(&context, UserAction::Backspace).unwrap();
        assert_eq!(next, CompositionState::Composing);
        assert_eq!(
            actions,
            vec![ClientAction::RemoveText],
            "the composition must keep going while the reading has kana left"
        );
    }

    #[test]
    fn enter_commits_and_ends_when_nothing_is_pending() {
        let (next, actions) =
            transition(&kana(CompositionState::Previewing), UserAction::Enter).unwrap();
        assert_eq!(next, CompositionState::None);
        assert_eq!(actions, vec![ClientAction::EndComposition]);
    }

    #[test]
    fn enter_with_a_pending_suffix_commits_the_candidate_and_keeps_composing() {
        let mut context = kana(CompositionState::Previewing);
        context.suffix_is_empty = false;
        let (next, actions) = transition(&context, UserAction::Enter).unwrap();
        assert_eq!(next, CompositionState::Composing);
        assert_eq!(actions, vec![ClientAction::ShrinkText("".to_string())]);
    }

    /// Escape must discard the whole composition without committing
    /// anything. The old [RemoveText, EndComposition] pair deleted one
    /// kana and then COMMITTED the remainder (confirmed on hardware
    /// alongside issue #35).
    #[test]
    fn escape_discards_the_composition_without_committing() {
        for state in [CompositionState::Composing, CompositionState::Previewing] {
            let (next, actions) = transition(&kana(state), UserAction::Escape).unwrap();
            assert_eq!(next, CompositionState::None);
            assert_eq!(actions, vec![ClientAction::CancelComposition]);
        }
    }

    #[test]
    fn horizontal_navigation_moves_the_cursor() {
        let (next, actions) = transition(
            &kana(CompositionState::Composing),
            UserAction::Navigation(Navigation::Right),
        )
        .unwrap();
        assert_eq!(next, CompositionState::Composing);
        assert_eq!(actions, vec![ClientAction::MoveCursor(1)]);

        let (_, actions) = transition(
            &kana(CompositionState::Composing),
            UserAction::Navigation(Navigation::Left),
        )
        .unwrap();
        assert_eq!(actions, vec![ClientAction::MoveCursor(-1)]);
    }

    #[test]
    fn vertical_navigation_selects_candidates() {
        let (next, actions) = transition(
            &kana(CompositionState::Composing),
            UserAction::Navigation(Navigation::Up),
        )
        .unwrap();
        assert_eq!(next, CompositionState::Previewing);
        assert_eq!(
            actions,
            vec![ClientAction::SetSelection(SetSelectionType::Up)]
        );

        let (next, actions) = transition(
            &kana(CompositionState::Composing),
            UserAction::Navigation(Navigation::Down),
        )
        .unwrap();
        assert_eq!(next, CompositionState::Previewing);
        assert_eq!(
            actions,
            vec![ClientAction::SetSelection(SetSelectionType::Down)]
        );
    }

    #[test]
    fn space_and_tab_open_the_candidate_selection() {
        for action in [UserAction::Space, UserAction::Tab] {
            let (next, actions) =
                transition(&kana(CompositionState::Composing), action).unwrap();
            assert_eq!(next, CompositionState::Previewing);
            assert_eq!(
                actions,
                vec![ClientAction::SetSelection(SetSelectionType::Down)]
            );
        }
    }

    #[test]
    fn toggle_while_composing_ends_and_switches_to_latin() {
        let (next, actions) =
            transition(&kana(CompositionState::Composing), UserAction::ToggleInputMode).unwrap();
        assert_eq!(next, CompositionState::None);
        assert_eq!(
            actions,
            vec![
                ClientAction::EndComposition,
                ClientAction::SetIMEMode(InputMode::Latin),
            ]
        );
    }

    #[test]
    fn function_keys_convert_the_reading() {
        let cases = [
            (Function::Six, SetTextType::Hiragana),
            (Function::Seven, SetTextType::Katakana),
            (Function::Eight, SetTextType::HalfKatakana),
            (Function::Nine, SetTextType::FullLatin),
            (Function::Ten, SetTextType::HalfLatin),
        ];
        for (key, expected) in cases {
            let (next, actions) =
                transition(&kana(CompositionState::Composing), UserAction::Function(key)).unwrap();
            assert_eq!(next, CompositionState::Previewing);
            assert_eq!(actions, vec![ClientAction::SetTextWithType(expected)]);
        }
    }

    #[test]
    fn unknown_keys_are_passed_through_while_composing() {
        assert!(transition(&kana(CompositionState::Composing), UserAction::Unknown).is_none());
    }
}
