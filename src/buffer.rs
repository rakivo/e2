use crate::lexer::{LexState, Token, lex_from};

use std::path::Path;

use ropey::Rope;
use smallstr::SmallString;

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

#[derive(Default)]
pub struct Buffer {
    pub text:   Rope,
    pub path:   Option<Box<Path>>,
    pub dirty:  bool,
    pub lex_scratch: String,
    pub visible_tokens: Vec<Token>,
}

impl Buffer {
    pub fn from_file(path: impl Into<Box<Path>>) -> std::io::Result<Self> {
        let path = path.into();

        let text = Rope::from_reader(std::fs::File::open(&path)?)?;
        Ok(Self { text, path: Some(path), ..Default::default() })
    }

    pub fn set_cursor_line_col(&self, line: u32, col: u32, cursor: &mut Cursor) {
        let line = line.min(self.text.len_lines().saturating_sub(1) as u32);
        let line_start = self.text.line_to_char(line as usize);
        let line_len = self.text.line(line as usize).len_chars()
            .saturating_sub(1); // don't land on the \n

        cursor.char_index = line_start + col.min(line_len as u32) as usize;
    }

    pub fn lex_visible(&mut self, top_line: usize, bottom_line: usize) {
        let start_line = top_line.saturating_sub(10);
        let end_line   = (bottom_line + 10).min(self.text.len_lines());

        let start_byte = self.text.line_to_byte(start_line);
        let end_byte   = self.text.line_to_byte(end_line);

        // Determine block comment state at start_line
        let restart_state = self.state_at_byte(start_byte);

        self.lex_scratch.clear();
        for chunk in self.text.slice(self.text.byte_to_char(start_byte)..self.text.byte_to_char(end_byte)).chunks() {
            self.lex_scratch.push_str(chunk);
        }

        self.visible_tokens.clear();
        lex_from(
            &self.lex_scratch,
            start_byte,
            restart_state,
            &mut self.visible_tokens,
        );
    }

    fn state_at_byte(&self, end_byte: usize) -> LexState {
        let char_end = self.text.byte_to_char(end_byte);
        let mut s = SmallString::<[u8; 128]>::new();
        for chunk in self.text.slice(..char_end).chunks() {
            s.push_str(chunk);
        }

        let b = s.as_bytes();
        let mut i = b.len().saturating_sub(1);
        while i > 0 {
            // Scanning backwards, so the "first" char of a pair is at i-1
            // /* in source = b[i-1]=='/' && b[i]=='*'  -> we're inside a comment
            // */ in source = b[i-1]=='*' && b[i]=='/'  -> we closed a comment
            if b[i-1] == b'/' && b[i] == b'*' { return LexState::InBlockComment; }
            if b[i-1] == b'*' && b[i] == b'/' { return LexState::Normal; }
            i -= 1;
        }

        LexState::Normal
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

    pub fn insert_char(&mut self, c: char, cursor: &mut Cursor) {
        let idx = cursor.char_index.min(self.text.len_chars());

        self.text.insert_char(idx, c);

        cursor.char_index = idx + 1;
        cursor.preferred_col = None;

        self.dirty = true;
    }

    pub fn insert_char_after(&mut self, c: char, cursor: &mut Cursor) {
        let idx = cursor.char_index.min(self.text.len_chars());

        self.text.insert_char(idx, c);

        self.dirty = true;
    }

    pub fn insert_literal(&mut self, l: &str, cursor: &mut Cursor) {
        for c in l.chars() {
            let idx = cursor.char_index.min(self.text.len_chars());
            self.text.insert_char(idx, c);
            cursor.char_index = idx + 1;
            cursor.preferred_col = None;
        }

        self.dirty = true;
    }

    pub fn delete_backward(&mut self, cursor: &mut Cursor) {
        if cursor.char_index == 0 { return; }

        let idx = cursor.char_index - 1;

        self.text.remove(idx..cursor.char_index);

        cursor.char_index = idx;
        cursor.preferred_col = None;

        self.dirty = true;
    }

    pub fn delete_forward(&mut self, cursor: &mut Cursor) {
        let len = self.text.len_chars();
        if cursor.char_index >= len { return; }

        self.text.remove(cursor.char_index..cursor.char_index + 1);

        cursor.char_index = cursor.char_index.min(self.text.len_chars());
        cursor.preferred_col = None;

        self.dirty = true;
    }

    pub fn delete_forward_until_newline(&mut self, cursor: &mut Cursor) {
        let len = self.text.len_chars();
        if cursor.char_index >= len { return; }

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
    }

    pub fn move_left(&self, cursor: &mut Cursor) {
        cursor.char_index = cursor.char_index.saturating_sub(1);
        cursor.preferred_col = None;
    }

    pub fn move_right(&self, cursor: &mut Cursor) {
        if cursor.char_index < self.text.len_chars() { cursor.char_index += 1; }
        cursor.preferred_col = None;
    }

    pub fn move_line_start(&self, cursor: &mut Cursor) {
        let (line, _) = self.cursor_line_col(cursor);
        cursor.char_index = self.text.line_to_char(line as usize);
        cursor.preferred_col = None;
    }

    pub fn move_line_end(&self, cursor: &mut Cursor) {
        let (line, _) = self.cursor_line_col(cursor);
        let line_start = self.text.line_to_char(line as usize);
        let line_str   = self.text.line(line as usize);
        let trailing   = if line_str.len_chars() > 0 && line_str.char(line_str.len_chars() - 1) == '\n' { 1 } else { 0 };
        cursor.char_index = line_start + line_str.len_chars() - trailing;
        cursor.preferred_col = None;
    }

    pub fn move_file_start(&self, cursor: &mut Cursor) {
        cursor.char_index = 0;
        cursor.preferred_col = None;
    }

    pub fn move_file_end(&self, cursor: &mut Cursor) {
        cursor.char_index = self.text.len_chars();
        cursor.preferred_col = None;
    }

    pub fn move_vertical(&self, delta: i64, cursor: &mut Cursor) {
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
