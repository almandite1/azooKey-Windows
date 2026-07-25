use super::input_mode::InputMode;

#[derive(Debug, PartialEq)]
pub enum ClientAction {
    StartComposition,
    /// Ends the composition, COMMITTING whatever is in the range.
    EndComposition,
    /// Ends the composition WITHOUT committing: the range is emptied
    /// first, so the discarded text never reaches the document (Escape).
    CancelComposition,

    AppendText(String),
    RemoveText,
    ShrinkText(String),

    SetTextWithType(SetTextType),

    /// Relative move of the engine-side cursor, in kana.
    ///
    /// **The client-side wiring is deliberately deferred**, and this is the
    /// one place that says so; the notes elsewhere point here. The arrow keys
    /// emit it (`transition`), the interpreter arm in `actions` drops it, and
    /// the RPC and Swift engine below it are live and kept green by
    /// `move_cursor_round_trips_without_breaking_the_composition`
    /// (crates/server/tests/ipc_smoke.rs). Predictive conversion is the
    /// feature that needs cursor movement, and it is what will connect the
    /// two ends — the payload is carried, unread, until then.
    MoveCursor(i32),
    SetSelection(SetSelectionType),

    SetIMEMode(InputMode),
}

#[derive(Debug, PartialEq)]
pub enum SetSelectionType {
    Up,
    Down,
}

/// The forms F6–F10 rewrite the whole reading into.
///
/// THE list of the five. `UserAction::Function` carries this type directly,
/// so the key-to-form mapping is stated once (in `user_action`'s VK decode)
/// and the transition table just passes it through; `act_set_text_with_type`
/// is where each form becomes text.
#[derive(Debug, Clone, PartialEq)]
pub enum SetTextType {
    Hiragana,     // F6
    Katakana,     // F7
    HalfKatakana, // F8
    FullLatin,    // F9
    HalfLatin,    // F10
}
