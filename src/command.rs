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
    DeleteBackward,   // backspace
    DeleteForward,    // delete key
    InsertNewline,
}
