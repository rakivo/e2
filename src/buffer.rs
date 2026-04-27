use crate::{PASTE_ANIMATION_MAX_ID, lexer::{LexState, Token, lex_from}};

use std::path::Path;

use ropey::Rope;
use smallvec::SmallVec;

#[derive(Default, Copy, Clone, Debug)]
pub struct Cursor {
    pub char_index:        usize,
    pub anchor_char_index: Option<usize>,
    pub preferred_col:     Option<u32>,
}

impl Cursor {
    pub fn new() -> Self {
        Self::default()
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
pub struct AnimatedDeletion {
    pub start_line: u32,
    pub start_col:  u32,
    pub end_line:   u32,
    pub end_col:    u32,
    pub t:          f32,
}

#[derive(Default)]
pub struct AnimatedInsertion {
    pub byte_start: usize,
    pub byte_len:   u32,
    pub t:          f32,   // 0.0 = just inserted (bright), 1.0 = fully faded
    pub id:         u8,    // stable, never reused within a session (or wrap at 15)
}

#[derive(Default)]
pub struct Buffer {
    pub text: Rope,
    pub path: Option<Box<Path>>,

    pub is_dirty: bool,

    pub last_insert: Option<(usize, u32)>, // (CHAR_index, BYTE_len)
    pub last_delete: Option<(usize, u32)>, // (CHAR_index, BYTE_len)

    pub scratch_space_to_flatten_rope_into: String,

    pub visible_tokens: Vec<Token>,
    pub comment_cache:  Vec<(usize, LexState)>, // (byte_offset, state_at_that_offset)

    pub next_insertion_id: u8,  // Starts at 1, wraps at PASTE_ANIMATION_MAX_ID+1
    pub currently_animated_insertions: SmallVec<[AnimatedInsertion; 4]>, // @Memory: Make this a static array

    pub currently_animated_deletions:  SmallVec<[AnimatedDeletion;  4]>,
}

impl Buffer {
    pub fn new() -> Self {
        Buffer {
            next_insertion_id: 1,
            ..Default::default()
        }
    }

    pub fn from_file(path: impl Into<Box<Path>>) -> std::io::Result<Self> {
        let path = path.into();

        let text = Rope::from_reader(std::fs::File::open(&path)?)?;
        Ok(Self { next_insertion_id: 1, text, path: Some(path), ..Default::default() })
    }

    pub fn append_last_insertion_to_currently_animated_insertions(&mut self) {
        let Some((char_start, char_len)) = self.last_insert else { return };
        let byte_start = self.text.char_to_byte(char_start);
        let byte_end   = (byte_start + char_len as usize).min(self.text.len_bytes());
        let byte_len   = (byte_end - byte_start) as u32;

        //
        //
        // Shift existing animations for this insertion first
        self.adjust_animated_insertions_for_insert(byte_start, byte_len as usize);

        //
        // Check for overlap with existing animations and merge
        //
        for existing in &mut self.currently_animated_insertions {
            let ex_start = existing.byte_start;
            let ex_end   = ex_start + existing.byte_len as usize;
            let overlaps = byte_start < ex_end && byte_end > ex_start;
            if overlaps {
                let merged_start = ex_start.min(byte_start);
                let merged_end   = ex_end.max(byte_end);

                existing.byte_start = merged_start;
                existing.byte_len   = (merged_end - merged_start) as u32;

                //
                // Only restart if new paste is FULLY inside existing, meaning that user
                // pasted within already-highlighted region
                //
                let fully_inside = byte_start >= ex_start && byte_end <= ex_end;
                if fully_inside {
                    existing.t = 0.0;
                }

                return;
            }
        }

        // @Robustness: No overlap - add new entry, evict oldest if full
        if self.currently_animated_insertions.len() == PASTE_ANIMATION_MAX_ID {
            self.currently_animated_insertions.remove(0);
            for (i, a) in self.currently_animated_insertions.iter_mut().enumerate() {
                a.id = (i + 1) as u8;
            }
            self.next_insertion_id = self.currently_animated_insertions.len() as u8 + 1;
        }

        let id = self.next_insertion_id;
        self.next_insertion_id = (self.next_insertion_id % PASTE_ANIMATION_MAX_ID as u8) + 1;
        self.currently_animated_insertions.push(AnimatedInsertion {
            byte_start,
            byte_len,
            t: 0.0,
            id,
        });
    }

    pub fn adjust_animated_insertions_for_insert(&mut self, insert_byte: usize, insert_len: usize) {
        for a in &mut self.currently_animated_insertions {
            if insert_byte <= a.byte_start {
                a.byte_start += insert_len;
            } else if insert_byte < a.byte_start + a.byte_len as usize {
                a.byte_len += insert_len as u32;
            }
        }
    }

    #[allow(unused, reason = "@Incomplete")]
    pub fn adjust_animated_insertions_for_delete(&mut self, delete_byte: usize, delete_len: usize) {
        let delete_end = delete_byte + delete_len;
        self.currently_animated_insertions.retain_mut(|a| {
            let a_end = a.byte_start + a.byte_len as usize;
            if delete_end <= a.byte_start {
                a.byte_start -= delete_len;
                true
            } else if delete_byte >= a_end {
                true
            } else {
                // Deletion overlaps the animated range, just kill the animation
                false
            }
        });
    }

    pub fn set_cursor_line_col(&self, line: u32, col: u32, cursor: &mut Cursor) {
        let line = line.min(self.text.len_lines().saturating_sub(1) as u32);
        let line_start = self.text.line_to_char(line as usize);
        let line_len = self.text.line(line as usize).len_chars()
            .saturating_sub(1); // don't land on the \n

        cursor.char_index = line_start + col.min(line_len as u32) as usize;
    }

    pub fn flatten_rope_into_scratch(&mut self, start_byte: usize, end_byte: usize) { // :BufferScratch
        self.scratch_space_to_flatten_rope_into.clear();
        for chunk in self.text.slice(self.text.byte_to_char(start_byte)..self.text.byte_to_char(end_byte)).chunks() {
            self.scratch_space_to_flatten_rope_into.push_str(chunk);
        }
    }

    pub fn lex_visible(&mut self, start_line: usize, end_line: usize) { // :BufferScratch
        let start_byte = self.text.try_line_to_byte(start_line).unwrap_or(0);
        let end_byte   = self.text.try_line_to_byte(end_line).unwrap_or(self.text.len_bytes());

        // Determine block comment state at start_line
        let restart_state = self.state_at_byte(start_byte);

        self.flatten_rope_into_scratch(start_byte, end_byte);

        self.visible_tokens.clear();
        lex_from(
            &self.scratch_space_to_flatten_rope_into,
            start_byte,
            restart_state,
            &mut self.visible_tokens,
        );
    }

    fn invalidate_cache_from_char(&mut self, char_idx: usize) {
        let byte = self.text.char_to_byte(char_idx);
        let keep = self.comment_cache.partition_point(|(b, _)| *b < byte);
        self.comment_cache.truncate(keep);
    }

    fn extend_cache_to(&mut self, target_byte: usize) {
        let (resume_byte, resume_state) = self.comment_cache
            .last()
            .copied()
            .unwrap_or((0, LexState::Normal));

        if resume_byte > target_byte { return; }

        let char_start = self.text.byte_to_char(resume_byte);
        let char_end   = self.text.byte_to_char(target_byte);
        let mut state    = resume_state;
        let mut byte_pos = resume_byte;
        let mut tmp      = Vec::new();

        for chunk in self.text.slice(char_start..char_end).chunks() {
            self.comment_cache.push((byte_pos, state));
            tmp.clear();
            state = lex_from(chunk, byte_pos, state, &mut tmp);
            byte_pos += chunk.len();
        }
        self.comment_cache.push((byte_pos, state));
    }

    fn state_at_byte(&mut self, target_byte: usize) -> LexState {
        self.extend_cache_to(target_byte);
        let idx = self.comment_cache
            .partition_point(|(b, _)| *b <= target_byte)
            .saturating_sub(1);
        self.comment_cache[idx].1
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
        self.invalidate_cache_from_char(idx);

        cursor.char_index = idx + 1;
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_insert = Some((idx, 1));
    }

    pub fn insert_char_after(&mut self, c: char, cursor: &mut Cursor) {
        let idx = cursor.char_index.min(self.text.len_chars());

        self.text.insert_char(idx, c);
        self.invalidate_cache_from_char(idx);

        self.is_dirty = true;
        self.last_insert = Some((idx, 1));
    }

    pub fn insert_literal(&mut self, l: &str, cursor: &mut Cursor) {
        let idx = cursor.char_index.min(self.text.len_chars());
        self.invalidate_cache_from_char(idx);

        for c in l.chars() {
            let idx = cursor.char_index.min(self.text.len_chars());
            self.text.insert_char(idx, c);
            cursor.char_index = idx + 1;
            cursor.preferred_col = None;
        }

        self.is_dirty = true;
        let len: u32 = l.chars().map(|c| c.len_utf8() as u32).sum();
        self.last_insert = Some((idx, len as u32));
    }

    pub fn delete_backward(&mut self, cursor: &mut Cursor) {
        if cursor.char_index == 0 { return; }

        let idx = cursor.char_index - 1;

        self.text.remove(idx..cursor.char_index);
        self.invalidate_cache_from_char(idx);

        cursor.char_index = idx;
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_delete = Some((idx, 1));
    }

    pub fn delete_forward(&mut self, cursor: &mut Cursor) {
        let len = self.text.len_chars();
        if cursor.char_index >= len { return; }

        self.text.remove(cursor.char_index..cursor.char_index + 1);
        self.invalidate_cache_from_char(cursor.char_index);

        cursor.char_index = cursor.char_index.min(self.text.len_chars());
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_delete = Some((cursor.char_index, 1));
    }

    pub fn delete_forward_until_newline(&mut self, cursor: &mut Cursor) {
        let len = self.text.len_chars();
        if cursor.char_index >= len { return; }

        let line_slice = self.text.slice(cursor.char_index..);
        let chars_to_delete = line_slice
            .chars()
            .position(|c| c == '\n')
            .map(|p| p.max(1))
            .unwrap_or(len - cursor.char_index);

        if chars_to_delete == 0 { return; }

        cursor.anchor_char_index = Some(cursor.char_index + chars_to_delete);
        self.delete_selection_with_animation(cursor);
    }

    pub fn delete_word_forward(&mut self, cursor: &mut Cursor) {
        let start = cursor.char_index;
        let len   = self.text.len_chars();
        let mut i = start;

        // Skip non-word chars
        while i < len && !is_word_char(self.text.char(i)) { i += 1; }
        // Skip word chars
        while i < len && is_word_char(self.text.char(i)) { i += 1; }

        if i == start { return; }

        self.text.remove(start..i);
        self.invalidate_cache_from_char(start);

        cursor.char_index    = start.min(self.text.len_chars());
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_delete = Some((start, (i-start) as u32));
    }

    pub fn delete_word_backward(&mut self, cursor: &mut Cursor) {
        let end = cursor.char_index;
        if end == 0 { return; }
        let mut i = end;

        // Skip non-word chars going left
        while i > 0 && !is_word_char(self.text.char(i - 1)) { i -= 1; }
        // Skip word chars going left
        while i > 0 && is_word_char(self.text.char(i - 1)) { i -= 1; }

        if i == end { return; }

        self.text.remove(i..end);
        self.invalidate_cache_from_char(i);

        cursor.char_index    = i.min(self.text.len_chars());
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_delete = Some((i, (end-i) as u32));
    }

    pub fn delete_selection_without_animation(&mut self, cursor: &mut Cursor) {
        let anchor = match cursor.anchor_char_index {
            Some(a) => a,
            None => return,
        };

        let start = anchor.min(cursor.char_index);
        let end = anchor.max(cursor.char_index);

        if start != end {
            self.text.remove(start..end);
            self.invalidate_cache_from_char(start);

            // Move cursor to the start of the deleted range
            cursor.char_index = start;
            self.is_dirty = true;
            self.last_delete = Some((start, (end-start) as u32));
        }

        // Always clear selection state
        cursor.anchor_char_index = None;
        cursor.preferred_col = None;
    }

    pub fn delete_selection_with_animation(&mut self, cursor: &mut Cursor) {
        let anchor = cursor.anchor_char_index.unwrap_or(cursor.char_index);
        let start  = anchor.min(cursor.char_index);
        let end    = anchor.max(cursor.char_index);

        if start != end {
            let (start_line, start_col) = self.char_to_line_col(start);
            let (end_line,   end_col)   = self.char_to_line_col(end);
            self.currently_animated_deletions.push(AnimatedDeletion {
                start_line, start_col,
                end_line,   end_col,
                t: 0.0,
            });
        }

        self.delete_selection_without_animation(cursor);
    }

    pub fn clear(&mut self) {
        self.is_dirty = true;
        self.text = Rope::new();
        self.comment_cache.clear();
        self.last_delete = None;
        self.last_insert = None;
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

    pub fn move_word_forward(&self, cursor: &mut Cursor) {
        let len = self.text.len_chars();
        let mut i = cursor.char_index;

        // Skip non-word chars
        while i < len && !is_word_char(self.text.char(i)) { i += 1; }
        // Skip word chars
        while i < len && is_word_char(self.text.char(i)) { i += 1; }

        cursor.char_index    = i;
        cursor.preferred_col = None;
    }

    pub fn move_word_backward(&self, cursor: &mut Cursor) {
        let mut i = cursor.char_index;

        // Skip non-word chars going left
        while i > 0 && !is_word_char(self.text.char(i - 1)) { i -= 1; }
        // Skip word chars going left
        while i > 0 && is_word_char(self.text.char(i - 1)) { i -= 1; }

        cursor.char_index    = i;
        cursor.preferred_col = None;
    }
}

#[inline]
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
