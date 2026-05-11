use crate::{COPY_ANIMATION_MAX_ID, PASTE_ANIMATION_MAX_ID, lexer::{LexState, Token, lex_from}};

use std::{io::{BufWriter, Write}, path::Path};

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
pub struct AnimatedRegion {
    pub byte_start: u32,
    pub byte_len:   u32,
    pub t:          f32,   // 0.0 = just inserted (bright), 1.0 = fully faded
    pub id:         u8,    // stable, never reused within a session (or wrap at 15)
}

// :FeelImprovement @Incomplete:
//
// For whatever reason if user pastes something into a freshly copied region, the paste
// doesn't get animated. Which isn't really good.
//
#[derive(Default)]
pub struct Buffer {
    pub text: Rope,

    pub path: Option<Box<Path>>,
    pub pretty_path: Box<str>,

    pub is_dirty: bool,

    pub last_save_generation: u64,
    pub last_edit_generation: u64,

    pub last_insert: Option<(usize, u32)>, // (CHAR_index, BYTE_len)
    pub last_delete: Option<(usize, u32)>, // (CHAR_index, BYTE_len)

    pub scratch_space_to_flatten_rope_into: String,

    pub visible_tokens: Vec<Token>,
    pub comment_cache:  Vec<(usize, LexState)>, // (byte_offset, state_at_that_offset)

    pub currently_animated_deletions:  SmallVec<[AnimatedDeletion;  2]>,

    pub next_copy_id:  u8, // Starts at 8, wraps at  COPY_ANIMATION_MAX_ID+1
    pub next_paste_id: u8, // Starts at 1, wraps at PASTE_ANIMATION_MAX_ID+1

    pub currently_animated_copies: SmallVec<[AnimatedRegion; 4]>, // copy,  ids 9..=15
    pub currently_animated_pastes: SmallVec<[AnimatedRegion; 4]>, // paste, ids 1..=8
}

impl Buffer {
    pub fn new() -> Self {
        Buffer {
            next_paste_id: 1,
            next_copy_id:  PASTE_ANIMATION_MAX_ID as u8 + 1, // starts at 8
            ..Default::default()
        }
    }

    pub fn from_file(path: impl Into<Box<Path>>) -> std::io::Result<Self> {
        let path = path.into();

        let text = Rope::from_reader(std::fs::File::open(&path)?)?;

        Ok(Self { text, path: Some(path), ..Self::new() })
    }

    pub fn append_last_insertion_to_currently_animated_pastes(&mut self) {
        let Some((char_start, char_len)) = self.last_insert else { return };
        let byte_start = self.text.char_to_byte(char_start);
        let byte_end   = (byte_start + char_len as usize).min(self.text.len_bytes());
        let byte_len   = byte_end - byte_start;

        self.animate_paste(byte_start as _, byte_len as _);
    }

    fn animate_region(
        vec:     &mut SmallVec<[AnimatedRegion; 4]>,
        next_id: &mut u8,

        id_min:  u8,
        id_max:  u8,

        byte_start: u32,
        byte_len:   u32,
    ) {
        if byte_len == 0 { return; }
        let byte_end = byte_start + byte_len;

        for existing in vec.iter_mut() {
            let ex_start = existing.byte_start;
            let ex_end   = ex_start + existing.byte_len;
            if byte_start < ex_end && byte_end > ex_start {
                existing.byte_start = ex_start.min(byte_start);
                existing.byte_len   = (ex_end.max(byte_end) - existing.byte_start) as u32;
                existing.t = 0.0;
                return;
            }
        }

        // @Robustness: No overlap - add new entry, evict oldest if full
        if vec.len() == (id_max - id_min + 1) as usize {
            vec.remove(0);
            let mut id = id_min;
            for a in vec.iter_mut() {
                a.id = id;
                id += 1;
            }
            *next_id = id;
        }

        let id = *next_id;
        *next_id = if *next_id >= id_max { id_min } else { *next_id + 1 };
        vec.push(AnimatedRegion { byte_start, byte_len, t: 0.0, id });
    }

    pub fn animate_paste(&mut self, byte_start: u32, byte_len: u32) {
        Self::animate_region(
            &mut self.currently_animated_pastes,
            &mut self.next_paste_id,
            1,
            PASTE_ANIMATION_MAX_ID as u8,
            byte_start, byte_len,
        );
        self.is_dirty = true;
    }

    pub fn animate_copy(&mut self, byte_start: u32, byte_len: u32) {
        Self::animate_region(
            &mut self.currently_animated_copies,
            &mut self.next_copy_id,
            PASTE_ANIMATION_MAX_ID as u8 + 1,
             COPY_ANIMATION_MAX_ID as u8,        // 15
            byte_start, byte_len,
        );
        self.is_dirty = true;
    }

    pub fn adjust_animated_regions_for_insert(&mut self, insert_byte: usize, insert_len: usize) {
        Self::adjust_animated_regions_for_insert_impl(&mut self.currently_animated_copies, insert_byte, insert_len);
        Self::adjust_animated_regions_for_insert_impl(&mut self.currently_animated_pastes, insert_byte, insert_len);
    }

    pub fn adjust_animated_regions_for_insert_impl(regions: &mut SmallVec<[AnimatedRegion; 4]>, insert_byte: usize, insert_len: usize) {
        for a in regions {
            if insert_byte < a.byte_start as usize {
                // Insert strictly before             -> shift right
                a.byte_start += insert_len as u32;
            } else if insert_byte < a.byte_start as usize + a.byte_len as usize {
                // Insert at start, inside, or at end -> grow
                a.byte_len += insert_len as u32;
            }

            // else: strictly after                   -> no-op
        }
    }

    pub fn adjust_animated_regions_for_delete(&mut self, delete_byte: usize, delete_len: usize) {
        Self::adjust_animated_regions_for_delete_impl(&mut self.currently_animated_copies, delete_byte, delete_len);
        Self::adjust_animated_regions_for_delete_impl(&mut self.currently_animated_pastes, delete_byte, delete_len);
    }

    pub fn adjust_animated_regions_for_delete_impl(regions: &mut SmallVec<[AnimatedRegion; 4]>, delete_byte: usize, delete_len: usize) {
        let delete_end = delete_byte + delete_len;
        regions.retain_mut(|a| {
            let a_end = (a.byte_start + a.byte_len) as usize;

            if delete_end <= a.byte_start as usize {
                // Deletion entirely before        -> shift left
                a.byte_start -= delete_len as u32;
                true
            } else if delete_byte >= a_end {
                // Deletion entirely after         -> no-op
                true
            } else if delete_byte <= a.byte_start as usize && delete_end >= a_end {
                // Deletion swallows entire region -> kill
                false
            } else if delete_byte <= a.byte_start as usize {
                // Clips the start                 -> delete_end is inside the region
                let clipped = delete_end - a.byte_start as usize;
                a.byte_start = delete_byte as u32;
                a.byte_len  -= clipped as u32;
                true
            } else if delete_end >= a_end {
                // Clips the end                   -> delete_byte is inside the region
                a.byte_len = (delete_byte - a.byte_start as usize) as u32;
                true
            } else {
                // Fully interior                  -> just shrink
                a.byte_len -= delete_len as u32;
                true
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

    pub fn write_onto_disk(&mut self) -> std::io::Result<()> {
        let Some(path) = self.path.as_ref() else { return Ok(()) };

        let tmp_path = path.with_extension("tmp");
        let mut f = BufWriter::new(std::fs::File::create(&tmp_path)?);
        for chunk in self.text.chunks() {
            f.write(chunk.as_bytes())?;
        }

        f.flush()?;
        drop(f);

        std::fs::rename(&tmp_path, path)?;

        self.last_save_generation = self.last_edit_generation;

        Ok(())
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.last_save_generation != self.last_edit_generation
    }

    pub fn lex_visible(&mut self, start_line: usize, end_line: usize) { // :BufferScratch
        let start_byte = self.text.try_line_to_byte(start_line).unwrap_or(0);
        let end_byte   = self.text.try_line_to_byte(end_line).unwrap_or(self.text.len_bytes());

        self.extend_cache_to(end_byte);

        let restart_state = self.state_at_byte(start_byte);

        // :LexerDebug
        // eprintln!("lex_visible: lines {}..{} bytes {}..{} state={:?}", start_line, end_line, start_byte, end_byte, restart_state);

        self.flatten_rope_into_scratch(start_byte, end_byte);

        self.visible_tokens.clear();
        lex_from(
            &self.scratch_space_to_flatten_rope_into,
            start_byte,
            restart_state,
            Some(&mut self.visible_tokens),
        );
    }

    fn invalidate_cache_from_char(&mut self, char_index: usize) {
        let byte = self.text.char_to_byte(char_index);
        let keep = self.comment_cache.partition_point(|(b, _)| *b < byte);
        self.comment_cache.truncate(keep);
    }

    fn extend_cache_to(&mut self, target_byte: usize) {
        let (resume_byte, resume_state) = self.comment_cache
            .last()
            .copied()
            .unwrap_or((0, LexState::Normal));

        if resume_byte > target_byte { return; }

        // Flatten to contiguous buffer to avoid chunk-boundary mid-token splits
        self.flatten_rope_into_scratch(resume_byte, target_byte);

        let end_state = lex_from(&self.scratch_space_to_flatten_rope_into, resume_byte, resume_state, None);
        self.comment_cache.push((target_byte, end_state));
    }

    fn state_at_byte(&mut self, target_byte: usize) -> LexState {
        self.extend_cache_to(target_byte);
        let index = self.comment_cache
            .partition_point(|(b, _)| *b <= target_byte)
            .saturating_sub(1);

        self.comment_cache[index].1
    }

    pub fn cursor_line_col(&self, cursor: &Cursor) -> (u32, u32) {
        self.char_to_line_col(cursor.char_index)
    }

    pub fn char_to_line_col(&self, index: usize) -> (u32, u32) {
        let index = index.min(self.text.len_chars());
        let line = self.text.char_to_line(index);
        let line_start = self.text.line_to_char(line);
        (line as u32, (index - line_start) as u32)
    }

    pub fn reset_buffer_to(&mut self, text: Rope, cursor: &mut Cursor) {
        let cursor_char = cursor.char_index;

        let cursor_line = self.text.char_to_line(cursor_char);
        let cursor_col  = cursor_char - self.text.line_to_char(cursor_line);

        self.text     = text.into();
        self.is_dirty = true;
        self.last_edit_generation += 1;

        let cursor_line = cursor_line.min(self.text.len_lines().saturating_sub(1));
        let line_start  = self.text.line_to_char(cursor_line);
        let line_len    = self.text.line(cursor_line).len_chars();
        cursor.char_index = (line_start + cursor_col).min(line_start + line_len.saturating_sub(1));
    }

    pub fn insert_char(&mut self, c: char, cursor: &mut Cursor) {
        let index = cursor.char_index.min(self.text.len_chars());
        let byte  = self.text.char_to_byte(index);

        self.text.insert_char(index, c);
        self.invalidate_cache_from_char(index);
        self.adjust_animated_regions_for_insert(byte, c.len_utf8());

        cursor.char_index = index + 1;
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_edit_generation += 1;
        self.last_insert = Some((index, 1));
    }

    pub fn insert_char_after(&mut self, c: char, cursor: &mut Cursor) {
        let index = cursor.char_index.min(self.text.len_chars());
        let byte  = self.text.char_to_byte(index);

        self.text.insert_char(index, c);
        self.invalidate_cache_from_char(index);
        self.adjust_animated_regions_for_insert(byte, c.len_utf8());

        self.is_dirty = true;
        self.last_edit_generation += 1;
        self.last_insert = Some((index, 1));
    }

    pub fn insert_literal(&mut self, l: &str, cursor: &mut Cursor) {
        let index = cursor.char_index.min(self.text.len_chars());

        let byte     = self.text.char_to_byte(index);
        let byte_len = l.len();

        self.invalidate_cache_from_char(index);

        for c in l.chars() {
            let index = cursor.char_index.min(self.text.len_chars());
            self.text.insert_char(index, c);
            cursor.char_index = index + 1;
            cursor.preferred_col = None;
        }

        self.adjust_animated_regions_for_insert(byte, byte_len);

        self.is_dirty = true;
        self.last_edit_generation += 1;

        let len: u32 = l.chars().map(|c| c.len_utf8() as u32).sum();
        self.last_insert = Some((index, len as u32));
    }

    pub fn delete_backward(&mut self, cursor: &mut Cursor) {
        if cursor.char_index == 0 { return; }

        let index = cursor.char_index - 1;

        let byte_start = self.text.char_to_byte(index);
        let byte_len   = self.text.char(index).len_utf8();

        self.text.remove(index..cursor.char_index);
        self.invalidate_cache_from_char(index);
        self.adjust_animated_regions_for_delete(byte_start, byte_len);

        cursor.char_index = index;
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_edit_generation += 1;
        self.last_delete = Some((index, 1));
    }

    pub fn delete_forward(&mut self, cursor: &mut Cursor) {
        let len = self.text.len_chars();
        if cursor.char_index >= len { return; }

        let byte_start = self.text.char_to_byte(cursor.char_index);
        let byte_len   = self.text.char(cursor.char_index).len_utf8();

        self.text.remove(cursor.char_index..cursor.char_index + 1);
        self.invalidate_cache_from_char(cursor.char_index);
        self.adjust_animated_regions_for_delete(byte_start, byte_len);

        cursor.char_index = cursor.char_index.min(self.text.len_chars());
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_edit_generation += 1;
        self.last_delete = Some((cursor.char_index, 1));
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

        let byte_start = self.text.char_to_byte(start);
        let byte_end   = self.text.char_to_byte(i);

        self.text.remove(start..i);
        self.invalidate_cache_from_char(start);
        self.adjust_animated_regions_for_delete(byte_start, byte_end - byte_start);

        cursor.char_index    = start.min(self.text.len_chars());
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_edit_generation += 1;
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

        let byte_start = self.text.char_to_byte(i);
        let byte_end   = self.text.char_to_byte(end);

        self.text.remove(i..end);
        self.invalidate_cache_from_char(i);
        self.adjust_animated_regions_for_delete(byte_start, byte_end - byte_start);

        cursor.char_index    = i.min(self.text.len_chars());
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_edit_generation += 1;
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
            let byte_start = self.text.char_to_byte(start);
            let byte_end   = self.text.char_to_byte(end);

            self.text.remove(start..end);
            self.invalidate_cache_from_char(start);
            self.adjust_animated_regions_for_delete(byte_start, byte_end - byte_start);

            // Move cursor to the start of the deleted range
            cursor.char_index = start;

            self.is_dirty = true;
            self.last_edit_generation += 1;
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
        self.last_edit_generation = Default::default();
        self.last_save_generation = Default::default();
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
