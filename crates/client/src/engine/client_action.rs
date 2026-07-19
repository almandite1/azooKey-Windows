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

    MoveCursor(i32),
    SetSelection(SetSelectionType),

    SetIMEMode(InputMode),
}

#[derive(Debug, PartialEq)]
pub enum SetSelectionType {
    Up,
    Down,
}

#[derive(Debug, PartialEq)]
pub enum SetTextType {
    Hiragana,     // F6
    Katakana,     // F7
    HalfKatakana, // F8
    FullLatin,    // F9
    HalfLatin,    // F10
}
