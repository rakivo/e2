use crate::command::EditorCommand;
use crate::lexer::{Token, lex};

use std::path::Path;

use ropey::Rope;

pub struct Buffer {
    pub text:   Rope,
    pub path:   Option<Box<Path>>,
    pub dirty:  bool,
    pub tokens: Vec<Token>,
}

#[derive(Clone, Debug)]
pub struct Cursor {
    pub char_idx:      usize,
    pub anchor_char_idx: Option<usize>,
    pub preferred_col: Option<usize>,
}

impl Cursor {
    pub fn new() -> Self {
        Self { char_idx: 0, preferred_col: None, anchor_char_idx: None, }
    }

    pub fn set_anchor(&mut self) {
        self.anchor_char_idx = Some(self.char_idx);
    }

    pub fn is_anchor_set(&self) -> bool {
        self.anchor_char_idx.is_some()
    }

    pub fn unset_anchor(&mut self) {
        self.anchor_char_idx = None;
    }
}

impl Buffer {
    pub fn empty() -> Self {
        let mut b = Self {
            text:   Rope::new(),
            path:   None,
            dirty:  false,
            tokens: Vec::new(),
        };
        b.relex();
        b
    }

    pub fn from_file(path: impl Into<Box<Path>>) -> std::io::Result<Self> {
        let path = path.into();

        let text = Rope::from_reader(std::fs::File::open(&path)?)?;
        let mut b = Self {
            text,
            dirty:  false,
            path:   Some(path),
            tokens: Vec::new(),
        };
        b.relex();
        Ok(b)
    }

    pub fn set_cursor_line_col(&self, line: usize, col: usize, cursor: &mut Cursor) {
        let line = line.min(self.text.len_lines().saturating_sub(1));
        let line_start = self.text.line_to_char(line);
        let line_len = self.text.line(line).len_chars()
            .saturating_sub(1); // don't land on the \n
        cursor.char_idx = line_start + col.min(line_len);
    }

    pub fn relex(&mut self) {
        let src = self.text.to_string();
        lex(&src, &mut self.tokens);
    }

    pub fn apply(&mut self, cmd: EditorCommand, cursor: &mut Cursor) {
        match cmd {
            EditorCommand::InsertChar(c)  => self.insert_char(c, cursor),
            EditorCommand::InsertNewline  => self.insert_char('\n', cursor),
            EditorCommand::DeleteBackward => self.delete_backward(cursor),
            EditorCommand::DeleteForward  => self.delete_forward(cursor),
            EditorCommand::MoveLeft       => self.move_left(cursor),
            EditorCommand::MoveRight      => self.move_right(cursor),
            EditorCommand::MoveUp         => self.move_vertical(-1, cursor),
            EditorCommand::MoveDown       => self.move_vertical(1, cursor),
            EditorCommand::MoveLineStart  => self.move_line_start(cursor),
            EditorCommand::MoveLineEnd    => self.move_line_end(cursor),
            EditorCommand::MoveFileStart  => { cursor.char_idx = 0; cursor.preferred_col = None; }
            EditorCommand::MoveFileEnd    => {
                cursor.char_idx = self.text.len_chars().saturating_sub(1);
                cursor.preferred_col = None;
            }
        }
    }

    pub fn cursor_line_col(&self, cursor: &Cursor) -> (usize, usize) {
        self.char_to_line_col(cursor.char_idx)
    }

    pub fn char_to_line_col(&self, idx: usize) -> (usize, usize) {
        let idx = idx.min(self.text.len_chars());
        let line = self.text.char_to_line(idx);
        let line_start = self.text.line_to_char(line);
        (line, idx - line_start)
    }

    fn insert_char(&mut self, c: char, cursor: &mut Cursor) {
        let idx = cursor.char_idx.min(self.text.len_chars());
        self.text.insert_char(idx, c);
        cursor.char_idx = idx + 1;
        cursor.preferred_col = None;
        self.dirty = true;
        self.relex();
    }

    fn delete_backward(&mut self, cursor: &mut Cursor) {
        if cursor.char_idx == 0 { return; }
        let idx = cursor.char_idx - 1;
        self.text.remove(idx..cursor.char_idx);
        cursor.char_idx = idx;
        cursor.preferred_col = None;
        self.dirty = true;
        self.relex();
    }

    fn delete_forward(&mut self, cursor: &mut Cursor) {
        let len = self.text.len_chars();
        if cursor.char_idx >= len { return; }
        self.text.remove(cursor.char_idx..cursor.char_idx + 1);
        cursor.char_idx = cursor.char_idx.min(self.text.len_chars());
        cursor.preferred_col = None;
        self.dirty = true;
        self.relex();
    }

    fn move_left(&self, cursor: &mut Cursor) {
        cursor.char_idx = cursor.char_idx.saturating_sub(1);
        cursor.preferred_col = None;
    }

    fn move_right(&self, cursor: &mut Cursor) {
        if cursor.char_idx < self.text.len_chars() { cursor.char_idx += 1; }
        cursor.preferred_col = None;
    }

    fn move_line_start(&self, cursor: &mut Cursor) {
        let (line, _) = self.cursor_line_col(cursor);
        cursor.char_idx = self.text.line_to_char(line);
        cursor.preferred_col = None;
    }

    fn move_line_end(&self, cursor: &mut Cursor) {
        let (line, _) = self.cursor_line_col(cursor);
        let line_start = self.text.line_to_char(line);
        let line_str   = self.text.line(line);
        let trailing   = if line_str.len_chars() > 0 && line_str.char(line_str.len_chars() - 1) == '\n' { 1 } else { 0 };
        cursor.char_idx = line_start + line_str.len_chars() - trailing;
        cursor.preferred_col = None;
    }

    fn move_vertical(&self, delta: i64, cursor: &mut Cursor) {
        let (line, col) = self.cursor_line_col(cursor);
        let target_col  = cursor.preferred_col.unwrap_or(col);
        if cursor.preferred_col.is_none() { cursor.preferred_col = Some(col); }

        let num_lines   = self.text.len_lines();
        let target_line = (line as i64 + delta).clamp(0, num_lines as i64 - 1) as usize;
        if target_line == line { return; }

        let line_start = self.text.line_to_char(target_line);
        let line_str   = self.text.line(target_line);
        let trailing   = if line_str.len_chars() > 0 && line_str.char(line_str.len_chars() - 1) == '\n' { 1 } else { 0 };
        let line_len   = line_str.len_chars() - trailing;

        cursor.char_idx = line_start + target_col.min(line_len);
    }
}
