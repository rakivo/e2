use crate::command::EditorCommand;
use crate::lexer::{Token, lex};

use std::path::Path;

use ropey::Rope;

pub struct Buffer {
    pub text:   Rope,
    pub path:   Option<Box<Path>>,
    pub dirty:  bool,
    pub tokens: Vec<Token>,
    pub lex_scratch: String
}

#[derive(Clone, Debug)]
pub struct Cursor {
    pub char_index:        usize,
    pub anchor_char_index: Option<usize>,
    pub preferred_col:     Option<u32>,
}

impl Cursor {
    pub fn new() -> Self {
        Self { char_index: 0, preferred_col: None, anchor_char_index: None, }
    }

    pub fn set_anchor(&mut self) {
        self.anchor_char_index = Some(self.char_index);
    }

    pub fn is_anchor_set(&self) -> bool {
        self.anchor_char_index.is_some()
    }

    pub fn unset_anchor(&mut self) {
        self.anchor_char_index = None;
    }
}

impl Buffer {
    pub fn empty() -> Self {
        let mut b = Self {
            text:   Rope::new(),
            path:   None,
            dirty:  false,
            tokens: Vec::new(),
            lex_scratch: String::new()
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
            lex_scratch: String::new()
        };

        b.relex();
        Ok(b)
    }

    pub fn set_cursor_line_col(&self, line: u32, col: u32, cursor: &mut Cursor) {
        let line = line.min(self.text.len_lines().saturating_sub(1) as u32);
        let line_start = self.text.line_to_char(line as usize);
        let line_len = self.text.line(line as usize).len_chars()
            .saturating_sub(1); // don't land on the \n
        cursor.char_index = line_start + col.min(line_len as u32) as usize;
    }

    pub fn relex(&mut self) {
        let t0 = std::time::Instant::now();
        self.lex_scratch.clear();
        for chunk in self.text.chunks() {
            self.lex_scratch.push_str(chunk);
        }
        lex(&self.lex_scratch, &mut self.tokens);
        eprintln!("relex {}us", t0.elapsed().as_micros());
    }

    pub fn relex_from(&mut self, _edit_byte: usize) {
        self.relex();
    }

    pub fn apply(&mut self, cmd: EditorCommand, cursor: &mut Cursor) {
        match cmd {
            EditorCommand::InsertChar(c)  => self.insert_char(c, cursor),
            EditorCommand::InsertNewline  => self.insert_char('\n', cursor),
            EditorCommand::InsertNewlineAfter => self.insert_char_after('\n', cursor),
            EditorCommand::InsertLiteral(l) => self.insert_literal(l, cursor),
            EditorCommand::DeleteBackward => self.delete_backward(cursor),
            EditorCommand::DeleteForward  => self.delete_forward(cursor),
            EditorCommand::DeleteForwardUntilNewline => self.delete_forward_until_newline(cursor),
            EditorCommand::MoveLeft       => self.move_left(cursor),
            EditorCommand::MoveRight      => self.move_right(cursor),
            EditorCommand::MoveUp         => self.move_vertical(-1, cursor),
            EditorCommand::MoveDown       => self.move_vertical(1, cursor),
            EditorCommand::MoveLineStart  => self.move_line_start(cursor),
            EditorCommand::MoveLineEnd    => self.move_line_end(cursor),
            EditorCommand::MoveFileStart  => { cursor.char_index = 0; cursor.preferred_col = None; }
            EditorCommand::MoveFileEnd    => {
                cursor.char_index = self.text.len_chars();
                cursor.preferred_col = None;
            }
        }
    }

    pub fn cursor_line_col(&self, cursor: &Cursor) -> (u32, u32) {
        self.char_to_line_col(cursor.char_index)
    }

    pub fn char_to_line_col(&self, idx: usize) -> (u32, u32) {
        let idx = idx.min(self.text.len_chars());
        let line = self.text.char_to_line(idx);
        let line_start = self.text.line_to_char(line);
        (line as u32, (idx - line_start) as u32)
    }

    fn insert_char(&mut self, c: char, cursor: &mut Cursor) {
        let idx = cursor.char_index.min(self.text.len_chars());
        let byte = self.text.char_to_byte(idx);
        self.text.insert_char(idx, c);
        cursor.char_index = idx + 1;
        cursor.preferred_col = None;
        self.dirty = true;
        self.relex_from(byte);
    }

    fn insert_char_after(&mut self, c: char, cursor: &mut Cursor) {
        let idx = cursor.char_index.min(self.text.len_chars());
        let byte = self.text.char_to_byte(idx);
        self.text.insert_char(idx, c);
        self.dirty = true;
        self.relex_from(byte);
    }

    fn insert_literal(&mut self, l: &str, cursor: &mut Cursor) {
        let idx = cursor.char_index.min(self.text.len_chars());
        let byte = self.text.char_to_byte(idx);

        for c in l.chars() {
            let idx = cursor.char_index.min(self.text.len_chars());
            self.text.insert_char(idx, c);
            cursor.char_index = idx + 1;
            cursor.preferred_col = None;
        }

        self.dirty = true;
        self.relex_from(byte);
    }

    fn delete_backward(&mut self, cursor: &mut Cursor) {
        if cursor.char_index == 0 { return; }

        let idx = cursor.char_index - 1;
        let byte = self.text.char_to_byte(idx);

        self.text.remove(idx..cursor.char_index);
        cursor.char_index = idx;
        cursor.preferred_col = None;
        self.dirty = true;
        self.relex_from(byte);
    }

    fn delete_forward(&mut self, cursor: &mut Cursor) {
        let len = self.text.len_chars();
        if cursor.char_index >= len { return; }

        let byte = self.text.char_to_byte(cursor.char_index);

        self.text.remove(cursor.char_index..cursor.char_index + 1);
        cursor.char_index = cursor.char_index.min(self.text.len_chars());
        cursor.preferred_col = None;
        self.dirty = true;
        self.relex_from(byte);
    }

    fn delete_forward_until_newline(&mut self, cursor: &mut Cursor) {
        let len = self.text.len_chars();
        if cursor.char_index >= len { return; }
        let byte = self.text.char_to_byte(cursor.char_index);

        // Find newline by walking chars from cursor position
        let line_slice = self.text.slice(cursor.char_index..);
        let chars_to_delete = line_slice
            .chars()
            .position(|c| c == '\n')
            .map(|p| p.max(1))
            .unwrap_or(len - cursor.char_index);

        self.text.remove(cursor.char_index..cursor.char_index + chars_to_delete);
        cursor.char_index = cursor.char_index.min(self.text.len_chars());
        cursor.preferred_col = None;
        self.dirty = true;
        self.relex_from(byte);
    }

    fn move_left(&self, cursor: &mut Cursor) {
        cursor.char_index = cursor.char_index.saturating_sub(1);
        cursor.preferred_col = None;
    }

    fn move_right(&self, cursor: &mut Cursor) {
        if cursor.char_index < self.text.len_chars() { cursor.char_index += 1; }
        cursor.preferred_col = None;
    }

    fn move_line_start(&self, cursor: &mut Cursor) {
        let (line, _) = self.cursor_line_col(cursor);
        cursor.char_index = self.text.line_to_char(line as usize);
        cursor.preferred_col = None;
    }

    fn move_line_end(&self, cursor: &mut Cursor) {
        let (line, _) = self.cursor_line_col(cursor);
        let line_start = self.text.line_to_char(line as usize);
        let line_str   = self.text.line(line as usize);
        let trailing   = if line_str.len_chars() > 0 && line_str.char(line_str.len_chars() - 1) == '\n' { 1 } else { 0 };
        cursor.char_index = line_start + line_str.len_chars() - trailing;
        cursor.preferred_col = None;
    }

    fn move_vertical(&self, delta: i64, cursor: &mut Cursor) {
        let (line, col) = self.cursor_line_col(cursor);
        let target_col  = cursor.preferred_col.unwrap_or(col);
        if cursor.preferred_col.is_none() { cursor.preferred_col = Some(col); }

        let num_lines   = self.text.len_lines();
        let target_line = (line as i64 + delta).clamp(0, num_lines as i64 - 1) as u32;
        if target_line == line { return; }

        let line_start = self.text.line_to_char(target_line as usize);
        let line_str   = self.text.line(target_line as usize);
        let trailing   = if line_str.len_chars() > 0 && line_str.char(line_str.len_chars() - 1) == '\n' { 1 } else { 0 };
        let line_len   = line_str.len_chars() - trailing;

        cursor.char_index = line_start + target_col.min(line_len as u32) as usize;
    }
}
