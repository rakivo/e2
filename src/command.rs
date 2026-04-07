/// Every editing action is expressed as a command.
#[derive(Debug, Clone)]
pub enum EditorCommand {
    // Movement
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveLineStart,
    MoveLineEnd,
    MoveFileStart,
    MoveFileEnd,

    // Editing
    InsertChar(char),
    InsertLiteral(&'static str),
    DeleteBackward,   // backspace
    DeleteForward,    // delete key
    InsertNewline,
    InsertNewlineAfter,
    DeleteForwardUntilNewline
}

impl EditorCommand {
    #[inline]
    pub const fn is_insert(&self) -> bool {
        matches!(self, Self::InsertChar(_) | Self::InsertLiteral(_))
    }

    #[inline]
    pub const fn is_big_scroll(&self) -> bool {
        matches!(self, Self::MoveFileStart | Self::MoveFileEnd)
    }
}
