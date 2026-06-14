#![allow(non_camel_case_types, non_snake_case)]

use editor_lexer::{LexState, Token, lex_from};

use std::hash::Hash;
use std::fmt::Write as _;
use std::{io::{BufWriter, Write}, path::Path};

use piece_tree::{HistoryEntry, NodeRef, PieceTree};
use smallstr::SmallString;
use smallvec::SmallVec;
use cranelift_entity::{EntityList, ListPool, PrimaryMap, packed_option::PackedOption};

pub const PASTE_ANIMATION_MAX_ID: usize = 7;  // pastes: 1..=7
pub const  COPY_ANIMATION_MAX_ID: usize = 15; // copies: 8..=15

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug, Default)]
pub struct e2_Point {
    pub row:    u32,
    pub column: u32,
}

impl e2_Point {
    pub const fn new(row: usize, column: usize) -> Self {
        Self { row: row as _, column: column as _ }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug, Default)]
pub struct e2_InputEdit {
    pub start_byte:       u32,
    pub old_end_byte:     u32,
    pub new_end_byte:     u32,
    pub start_position:   e2_Point,
    pub old_end_position: e2_Point,
    pub new_end_position: e2_Point,
    pub node_after_edit:  NodeRef,
}

pub enum ByteOp {
    Insert { at: usize, len: u32 },
    Delete { at: usize, len: u32 },
}

impl e2_InputEdit {
    pub fn as_byte_op(&self) -> ByteOp {
        let at = self.start_byte as usize;

        let del = self.old_end_byte.saturating_sub(self.start_byte);

        if del == 0 {
            let ins = self.new_end_byte.saturating_sub(self.start_byte);
            ByteOp::Insert { at, len: ins }
        } else {
            ByteOp::Delete { at, len: del }
        }
    }
}

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

#[repr(u32)]
#[derive(Clone, Debug, Copy, Eq, PartialEq)]
pub enum EditKind {
    Insert = 0,
    DeleteBackward = 1,
    DeleteForward = 2,
    /// Any operation that should explicitly break the merge chain
    /// (e.g., paste, delete word, reset buffer, or inserting a space).
    Boundary = 3,
}

impl EditKind {
    #[inline]
    pub fn from_u32(val: u32) -> Self {
        // Mask the bottom 2 bits (0b11)
        match val & 0b11 {
            0 => EditKind::Insert,
            1 => EditKind::DeleteBackward,
            2 => EditKind::DeleteForward,
            _ => EditKind::Boundary,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EditMeta {
    pub kind: EditKind,

    pub merge_count:   u32,  // Track how many times this node has merged
    pub cursor_before: u32,
    pub cursor_after:  u32,  // The position of the cursor right after the edit finished
}

// :FeelImprovement @Incomplete:
//
// For whatever reason if user pastes something into a freshly copied region, the paste
// doesn't get animated. Which isn't really good.
//
#[derive(Default)]
pub struct Buffer {
    pub text: PieceTree,
    pub undo: UndoTree,
    pub pending_edit_kind: Option<EditKind>,

    /// Remembers the cursor position when `begin_transaction` was called.
    pub transaction_cursor_before: u32,

    pub path: Option<Box<Path>>,
    pub pretty_path: Box<str>,

    pub is_dirty: bool,

    pub last_save_generation: u64,
    pub last_edit_generation: u64,

    // @Memory: Delete these fields
    pub last_insert: Option<(usize, u32)>, // (CHAR_index, BYTE_len)
    pub last_delete: Option<(usize, u32)>, // (CHAR_index, BYTE_len)

    pub lex_dirty_from: usize,

    // Flag to indicate that the buffer underwent a massive state jump
    // (e.g., undo, redo, jump_to) and secondary cursors must be clamped.
    pub did_snap_state: bool,

    // Same as did_snap_state but only set to false once we sent Reparse to the tree sitter thread after a did_snap_state.
    pub did_snap_state_tree_sitter: bool,

    pub ts_edits_in_this_frame: Vec<e2_InputEdit>,

    pub scratch_space_to_flatten_tree_into: String,

    pub visible_tokens: Vec<Token>,
    pub comment_cache:  Vec<(u32, LexState)>, // (byte_offset, state_at_that_offset)
    pub scratch_uncached_entries: Vec<(u32, LexState)>,

    pub currently_animated_deletions: SmallVec<[AnimatedDeletion;  2]>,

    pub next_copy_id:  u8, // Starts at 8, wraps at  COPY_ANIMATION_MAX_ID+1
    pub next_paste_id: u8, // Starts at 1, wraps at PASTE_ANIMATION_MAX_ID+1

    pub filestem_atom: editor_helpers::atum::Atom,

    pub currently_animated_copies: SmallVec<[AnimatedRegion; 4]>, // copy,  ids 9..=15
    pub currently_animated_pastes: SmallVec<[AnimatedRegion; 4]>, // paste, ids 1..=8
}

impl Buffer {
    #[inline]
    pub fn set_undo_merge(&mut self, kind: EditKind) {
        self.pending_edit_kind = Some(kind);
    }

    #[inline]
    pub fn set_undo_separate(&mut self) {
        self.pending_edit_kind = Some(EditKind::Boundary);
    }
}

impl Buffer {
    pub fn new() -> Self {
        Buffer {
            next_paste_id: 1,
            next_copy_id:  PASTE_ANIMATION_MAX_ID as u8 + 1, // starts at 8

            ts_edits_in_this_frame: Vec::with_capacity(8),

            undo: UndoTree::new(HistoryEntry { root: NodeRef(0), cursor_offset: 0 }),

            comment_cache: vec![(0, LexState::Normal)],

            lex_dirty_from: usize::MAX,

            ..Default::default()
        }
    }

    pub fn from_file(path: impl Into<Box<Path>>) -> std::io::Result<Self> {
        let path = path.into();

        let text = PieceTree::from_reader(std::fs::File::open(&path)?)?;
        let mut buffer = Self { text, path: Some(path), ..Self::new() };

        buffer.undo.nodes.last_mut().unwrap().1.snapshot = buffer.text.take_snapshot(0);

        if !buffer.is_file_too_huge_so_we_might_as_well_give_up_on_bookkeeping_lex_state_for_now() {
            buffer.extend_cache_to(buffer.text.total_length() as _);
        }

        Ok(buffer)
    }

    pub fn is_file_too_huge_so_we_might_as_well_give_up_on_bookkeeping_lex_state_for_now(&self) -> bool {
        const HUGE: usize = 3 * 1024 * 1024;
        self.text.len_bytes() as usize > HUGE
    }

    /// Starts a transaction (e.g., when the user starts typing)
    #[inline]
    pub fn begin_transaction(&mut self, cursor_index: usize) {
        if self.text.transaction_depth == 0 {
            self.transaction_cursor_before = cursor_index as u32;
        }

        // Tell the piece-tree to start grouping edits
        self.text.begin_undo_group(cursor_index as u32);
    }

    /// Commits the transaction and pushes a node to the visual graph
    #[inline]
    pub fn commit_transaction(&mut self, cursor_after: u32, kind: EditKind) -> Option<UndoNodeRef> {
        self.text.end_undo_group();

        if self.text.transaction_depth == 0 {
            let snapshot = self.text.take_snapshot(cursor_after);
            let head_node = &mut self.undo.nodes[self.undo.head];

            if snapshot.root == head_node.snapshot.root {
                return None;  // Bail on empty nodes
            }

            // Validate the merge using the cursor position from WHEN THE TRANSACTION BEGAN
            let can_merge = if self.undo.break_merge {
                self.undo.break_merge = false; // Consume the flag
                false                          // Force NO MERGE
            } else {
                head_node.is_mergeable_with(kind, self.transaction_cursor_before)
            };

            if can_merge && self.undo.head != self.undo.root {
                // MERGE
                head_node.snapshot = snapshot;
                head_node.cursor_after = cursor_after;
                head_node.increment_merge_count();
            } else {
                // NO MERGE
                let new_meta = EditMeta {
                    kind,
                    cursor_after,
                    cursor_before: self.transaction_cursor_before,
                    merge_count: 0
                };
                return Some(self.undo.commit(snapshot, new_meta));
            }
        }

        None
    }

    pub fn append_last_insertion_to_currently_animated_pastes(&mut self) {
        let Some((char_start, char_len)) = self.last_insert else { return };
        let byte_start = self.text.char_to_byte(char_start as _);
        let byte_end   = (byte_start + char_len).min(self.text.len_bytes());
        let byte_len   = byte_end - byte_start;

        self.animate_paste(byte_start as u32, byte_len as _);
    }

    pub fn make_delete_edit(&self, byte_start: usize, byte_end: usize, node_AFTER_edit: NodeRef) -> e2_InputEdit {
        let start_point   = self.byte_to_point(byte_start);
        let old_end_point = self.byte_to_point(byte_end);
        e2_InputEdit {
            start_byte:       byte_start as u32,
            start_position:   start_point,
            old_end_byte:     byte_end as u32,
            old_end_position: old_end_point,
            new_end_byte:     byte_start as u32,
            new_end_position: start_point,
            node_after_edit:  node_AFTER_edit
        }
    }

    pub fn make_insert_edit(&self, byte_start: usize, byte_end_after: usize, node_AFTER_edit: NodeRef) -> e2_InputEdit {
        let start_point   = self.byte_to_point(byte_start);
        let new_end_point = self.byte_to_point(byte_end_after);
        e2_InputEdit {
            start_byte:       byte_start as u32,
            start_position:   start_point,
            old_end_byte:     byte_start as u32,
            old_end_position: start_point,
            new_end_byte:     byte_end_after as _,
            new_end_position: new_end_point,
            node_after_edit:  node_AFTER_edit
        }
    }

    pub fn last_edit(&self) -> Option<&e2_InputEdit> {
        self.ts_edits_in_this_frame.last()
    }

    pub fn byte_to_point(&self, byte: usize) -> e2_Point {
        let line = self.text.byte_to_line(byte as _);
        let col  = byte - self.text.line_to_byte(line) as usize;
        e2_Point::new(line as _, col as _)
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
        let line_start = self.text.line_to_char(line);
        let line_len = self.text.line(line)
            .len_chars()
            .saturating_sub(1); // don't land on the \n

        cursor.char_index = (line_start + col.min(line_len as u32)) as usize;
    }

    pub fn flatten_tree_into_scratch(&mut self, start_byte: usize, end_byte: usize) { // :BufferScratch
        self.scratch_space_to_flatten_tree_into.clear();
        let slice = self.text.slice(start_byte as u32..end_byte as u32);
        _ = writeln!(&mut self.scratch_space_to_flatten_tree_into, "{slice}");
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

    pub fn apply_edits_to_lex_cache(&mut self) {
        if self.ts_edits_in_this_frame.is_empty() { return; }

        if self.is_file_too_huge_so_we_might_as_well_give_up_on_bookkeeping_lex_state_for_now() { return }

        for edit in &self.ts_edits_in_this_frame {
            self.lex_dirty_from = self.lex_dirty_from.min(edit.start_byte as usize);

            let delta = edit.new_end_byte as isize - edit.old_end_byte as isize;

            let start_idx = self.comment_cache.partition_point(|(b, _)| *b < edit.start_byte);
            let end_idx   = self.comment_cache.partition_point(|(b, _)| *b <= edit.old_end_byte);

            if start_idx < end_idx {
                self.comment_cache.drain(start_idx..end_idx);
            }

            for i in start_idx..self.comment_cache.len() {
                let old_byte = self.comment_cache[i].0 as isize;
                self.comment_cache[i].0 = (old_byte + delta).max(0) as u32;
            }
        }

        // If the user deleted the start of the file, the 0-byte checkpoint got drained.
        // We must enforce that byte 0 always exists and is Normal.
        if self.comment_cache.is_empty() || self.comment_cache[0].0 != 0 {
            self.comment_cache.insert(0, (0, LexState::Normal));
        }
    }

    pub fn lex_visible(&mut self, start_line: usize, end_line: usize) {  // :BufferScratch
        let _tracy = tracy::span!("lex_visible");

        let file_is_huge = self.is_file_too_huge_so_we_might_as_well_give_up_on_bookkeeping_lex_state_for_now();

        let start_byte = self.text.try_line_to_byte(start_line as _).unwrap_or(0);
        let end_byte   = self.text.try_line_to_byte(end_line as _).unwrap_or(self.text.len_bytes());

        let restart_state = if file_is_huge {
            LexState::Normal
        } else {
            self.extend_cache_to(end_byte as _);
            self.state_at_byte(start_byte as _)
        };

        // :LexerDebug
        // eprintln!("lex_visible: lines {}..{} bytes {}..{} state={:?}", start_line, end_line, start_byte, end_byte, restart_state);

        self.visible_tokens.clear();
        self.flatten_tree_into_scratch(start_byte as _, end_byte as _);  // :BufferScratch
        lex_from(
            &self.scratch_space_to_flatten_tree_into,
            start_byte as _,
            restart_state,
            Some(&mut self.visible_tokens),
        );
    }

    pub fn invalidate_cache_from_char(&mut self, char_index: usize) {
        let byte = self.text.char_to_byte(char_index as _);
        let keep = self.comment_cache.partition_point(|(b, _)| *b < byte as u32);
        self.comment_cache.truncate(keep);
    }

    fn extend_cache_to(&mut self, target_byte: usize) {      // @Horrible
        // Chunking by lines ensures we never split a `//` comment in half
        const CHECKPOINT_INTERVAL_LINES: u32 = 64;

        let target_line = self.text.try_byte_to_line(target_byte as _).unwrap_or(0);
        let target_checkpoint_line = (target_line / CHECKPOINT_INTERVAL_LINES) * CHECKPOINT_INTERVAL_LINES;
        let target_checkpoint_byte = self.text.try_line_to_byte(target_checkpoint_line).unwrap_or(0) as u32;

        let last_valid_index = self.comment_cache
            .partition_point(|(b, _)| *b <= target_checkpoint_byte)
            .saturating_sub(1);

        let (mut resume_byte, mut resume_state) = self.comment_cache
            .get(last_valid_index)
            .copied()
            .unwrap_or((0, LexState::Normal));

        if resume_byte >= target_checkpoint_byte { return }

        while resume_byte < target_checkpoint_byte {
            let resume_line = self.text.try_byte_to_line(resume_byte).unwrap_or(0);
            let next_checkpoint_line = resume_line + CHECKPOINT_INTERVAL_LINES;

            // Advance to the exact byte where the next checkpoint line starts
            let chunk_end = self.text.try_line_to_byte(next_checkpoint_line)
                .unwrap_or_else(|| self.text.len_bytes()) as u32;

            if chunk_end <= resume_byte {
                break;
            }

            self.flatten_tree_into_scratch(resume_byte as _, chunk_end as _);

            resume_state = lex_from(
                &self.scratch_space_to_flatten_tree_into,
                resume_byte as _,
                resume_state,
                None,
            );

            resume_byte = chunk_end;

            let index = self.comment_cache.partition_point(|(b, _)| *b < resume_byte);
            if index < self.comment_cache.len() && self.comment_cache[index].0 == resume_byte {
                self.comment_cache[index] = (resume_byte, resume_state);
            } else {
                self.comment_cache.insert(index, (resume_byte, resume_state));
            }
        }
    }

    fn state_at_byte(&mut self, target_byte: usize) -> LexState {
        self.extend_cache_to(target_byte);

        let target_byte = target_byte as u32;

        let index = self.comment_cache
            .partition_point(|(b, _)| *b < target_byte)
            .saturating_sub(1);

        let (checkpoint_byte, checkpoint_state) = self.comment_cache
            .get(index)
            .copied()
            .unwrap_or((0, LexState::Normal));

        if checkpoint_byte > target_byte {
            // extend_cache_to should have prevented this, but be safe
            return LexState::Normal;
        }

        if checkpoint_byte == target_byte {
            return checkpoint_state;
        }

        // Lex forward from checkpoint to exact target byte
        self.flatten_tree_into_scratch(checkpoint_byte as _, target_byte as _);
        let final_state = lex_from(
            &self.scratch_space_to_flatten_tree_into,
            checkpoint_byte as _,
            checkpoint_state,
            None,
        );

        final_state
    }

    pub fn cursor_line_col(&self, cursor: &Cursor) -> (u32, u32) {
        self.char_to_line_col(cursor.char_index)
    }

    pub fn char_to_line_col(&self, index: usize) -> (u32, u32) {
        let index = index.min(self.text.len_chars() as usize);
        let line = self.text.char_to_line(index as _);
        let line_start = self.text.line_to_char(line as _);
        (line, index as u32 - line_start)
    }

    pub fn reset_buffer_to(&mut self, text: PieceTree, cursor: &mut Cursor) {
        let cursor_char = cursor.char_index;

        let cursor_line = self.text.char_to_line(cursor_char as _);
        let cursor_col  = cursor_char - self.text.line_to_char(cursor_line) as usize;

        self.text     = text.into();
        self.is_dirty = true;
        self.last_edit_generation += 1;

        let cursor_line = cursor_line.min(self.text.len_lines().saturating_sub(1));
        let line_start  = self.text.line_to_char(cursor_line);
        let line_len    = self.text.line(cursor_line).len_chars();
        cursor.char_index = (line_start + cursor_col as u32).min(line_start + line_len.saturating_sub(1)) as usize;

        self.invalidate_cache_from_char(0); // @Speed
        self.ts_edits_in_this_frame.clear();

        self.set_undo_separate();
    }

    pub fn insert_char(&mut self, c: char, cursor: &mut Cursor) {
        let index = cursor.char_index.min(self.text.len_chars() as _);
        let byte  = self.text.char_to_byte(index as _) as usize;

        self.text.insert_char(byte as _, c);
        self.invalidate_cache_from_char(index);
        self.adjust_animated_regions_for_insert(byte, c.len_utf8());

        let byte_end = byte + c.len_utf8();
        self.ts_edits_in_this_frame.push(self.make_insert_edit(byte, byte_end, self.text.root));

        cursor.char_index = index + 1;
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_edit_generation += 1;
        self.last_insert = Some((index, c.len_utf8() as _));

        // let kind = if c.is_whitespace() { EditKind::Boundary } else { EditKind::Insert };
        let kind = EditKind::Insert;
        self.pending_edit_kind = Some(kind);
    }

    pub fn insert_char_after(&mut self, c: char, cursor: &mut Cursor) {
        let index = cursor.char_index.min(self.text.len_chars() as _);
        let byte  = self.text.char_to_byte(index as _) as usize;

        self.text.insert_char(byte as _, c);
        self.invalidate_cache_from_char(index);
        self.adjust_animated_regions_for_insert(byte, c.len_utf8());

        let byte_end = byte + c.len_utf8();
        self.ts_edits_in_this_frame.push(self.make_insert_edit(byte, byte_end, self.text.root));

        self.is_dirty = true;
        self.last_edit_generation += 1;
        self.last_insert = Some((index, c.len_utf8() as _));

        // let kind = if c.is_whitespace() { EditKind::Boundary } else { EditKind::Insert };
        let kind = EditKind::Insert;
        self.pending_edit_kind = Some(kind);
    }

    pub fn insert_literal(&mut self, l: &str, cursor: &mut Cursor) {
        let index = cursor.char_index.min(self.text.len_chars() as usize);

        let byte     = self.text.char_to_byte(index as _) as usize;
        let byte_len = l.len();

        self.invalidate_cache_from_char(index);

        self.text.insert(byte as _, l);

        let char_count = self.text.slice(byte as u32..byte as u32+byte_len as u32).len_chars();
        cursor.char_index = (cursor.char_index + char_count as usize).min(self.text.len_chars() as usize);
        cursor.preferred_col = None;

        self.adjust_animated_regions_for_insert(byte, byte_len);

        let byte_end = byte + byte_len;
        self.ts_edits_in_this_frame.push(self.make_insert_edit(byte, byte_end, self.text.root));

        self.is_dirty = true;
        self.last_edit_generation += 1;

        let len: u32 = l.chars().map(|c| c.len_utf8() as u32).sum();
        self.last_insert = Some((index, len as u32));
        self.set_undo_merge(EditKind::Insert);
    }

    pub fn delete_backward(&mut self, cursor: &mut Cursor) {
        if cursor.char_index == 0 { return; }

        let index = cursor.char_index - 1;

        let byte_start = self.text.char_to_byte(index as _) as usize;
        let byte_len   = self.text.char(index as _).len_utf8();
        let byte_end   = self.text.char_to_byte(cursor.char_index as _) as usize;

        let mut edit = self.make_delete_edit(byte_start, byte_end, NodeRef(0));

        self.text.remove(byte_start as u32..byte_end as u32);
        self.invalidate_cache_from_char(index);
        self.adjust_animated_regions_for_delete(byte_start, byte_len);

        edit.node_after_edit = self.text.root;
        self.ts_edits_in_this_frame.push(edit);

        cursor.char_index = index;
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_edit_generation += 1;
        self.last_delete = Some((index, byte_len as _));
        self.set_undo_merge(EditKind::DeleteBackward);
    }

    pub fn delete_forward(&mut self, cursor: &mut Cursor) {
        let len = self.text.len_chars() as usize;
        if cursor.char_index >= len { return; }

        let index = cursor.char_index;
        let end_index = index + 1;

        let byte_start = self.text.char_to_byte(index as _) as usize;
        let byte_len   = self.text.char(index as _).len_utf8();
        let byte_end   = self.text.char_to_byte(end_index as _) as usize;

        let mut edit = self.make_delete_edit(byte_start, byte_end, NodeRef(0));

        self.text.remove(byte_start as u32..byte_end as u32);
        self.invalidate_cache_from_char(index);
        self.adjust_animated_regions_for_delete(byte_start, byte_len);

        edit.node_after_edit = self.text.root;
        self.ts_edits_in_this_frame.push(edit);

        cursor.char_index = cursor.char_index.min(self.text.len_chars() as usize);
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_edit_generation += 1;
        self.last_delete = Some((cursor.char_index, byte_len as _));
        self.set_undo_merge(EditKind::DeleteForward);
    }

    pub fn delete_word_forward(&mut self, cursor: &mut Cursor) {
        let start = cursor.char_index;
        let len   = self.text.len_chars() as usize;
        let mut i = start;

        // Skip non-word chars
        while i < len && !is_word_char(self.text.char(i as _)) { i += 1; }
        // Skip word chars
        while i < len && is_word_char(self.text.char(i as _)) { i += 1; }

        if i == start { return; }

        let byte_start = self.text.char_to_byte(start as _) as usize;
        let byte_end   = self.text.char_to_byte(i as _) as usize;

        let mut edit = self.make_delete_edit(byte_start, byte_end, NodeRef(0));

        self.text.remove(byte_start as u32..byte_end as u32);
        self.invalidate_cache_from_char(start);
        self.adjust_animated_regions_for_delete(byte_start, byte_end - byte_start);

        edit.node_after_edit = self.text.root;
        self.ts_edits_in_this_frame.push(edit);

        cursor.char_index    = start.min(self.text.len_chars() as usize);
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_edit_generation += 1;
        self.set_undo_separate();
        self.last_delete = Some((start, (i-start) as u32));
    }

    pub fn delete_word_backward(&mut self, cursor: &mut Cursor) {
        let end = cursor.char_index;
        if end == 0 { return; }
        let mut i = end;

        // Skip non-word chars going left
        while i > 0 && !is_word_char(self.text.char(i as u32 - 1)) { i -= 1; }
        // Skip word chars going left
        while i > 0 && is_word_char(self.text.char(i as u32 - 1)) { i -= 1; }

        if i == end { return; }

        let byte_start = self.text.char_to_byte(i as _) as usize;
        let byte_end   = self.text.char_to_byte(end as _) as usize;

        let mut edit = self.make_delete_edit(byte_start, byte_end, NodeRef(0));

        self.text.remove(byte_start as u32..byte_end as u32);
        self.invalidate_cache_from_char(i);
        self.adjust_animated_regions_for_delete(byte_start, byte_end - byte_start);

        edit.node_after_edit = self.text.root;
        self.ts_edits_in_this_frame.push(edit);

        cursor.char_index    = i.min(self.text.len_chars() as usize);
        cursor.preferred_col = None;

        self.is_dirty = true;
        self.last_edit_generation += 1;
        self.last_delete = Some((i, (end-i) as u32));
        self.set_undo_separate();
    }

    pub fn delete_selection_without_animation(&mut self, cursor: &mut Cursor) {
        let anchor = match cursor.anchor_char_index {
            Some(a) => a,
            None => return,
        };

        let start = anchor.min(cursor.char_index);
        let end = anchor.max(cursor.char_index);

        if start != end {
            let byte_start = self.text.char_to_byte(start as _) as usize;
            let byte_end   = self.text.char_to_byte(end as _) as usize;

            let mut edit = self.make_delete_edit(byte_start, byte_end, NodeRef(0));

            self.text.remove(byte_start as u32..byte_end as u32);
            self.invalidate_cache_from_char(start);
            self.adjust_animated_regions_for_delete(byte_start, byte_end - byte_start);

            edit.node_after_edit = self.text.root;
            self.ts_edits_in_this_frame.push(edit);

            // Move cursor to the start of the deleted range
            cursor.char_index = start;

            self.is_dirty = true;
            self.last_edit_generation += 1;
            self.last_delete = Some((start, (byte_end-byte_start) as u32));
            self.set_undo_separate();
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
        self.text.remove(..);
        self.comment_cache.clear();
        self.ts_edits_in_this_frame.clear();
        self.last_delete = None;
        self.last_insert = None;
    }

    pub fn move_left(&self, cursor: &mut Cursor) {
        cursor.char_index = cursor.char_index.saturating_sub(1);
        cursor.preferred_col = None;
    }

    pub fn move_right(&self, cursor: &mut Cursor) {
        if cursor.char_index < self.text.len_chars() as usize { cursor.char_index += 1; }
        cursor.preferred_col = None;
    }

    pub fn move_line_start(&self, cursor: &mut Cursor) {
        let (line, _) = self.cursor_line_col(cursor);
        cursor.char_index = self.text.line_to_char(line) as usize;
        cursor.preferred_col = None;
    }

    pub fn move_line_end(&self, cursor: &mut Cursor) {
        let (line, _) = self.cursor_line_col(cursor);
        let line_start = self.text.line_to_char(line);
        let line_str   = self.text.line(line);
        let trailing   = if line_str.len_chars() > 0 &&
            line_str.char(line_str.len_chars() - 1) == '\n' { 1 } else { 0 };

        cursor.char_index = (line_start + line_str.len_chars() - trailing) as _;
        cursor.preferred_col = None;
    }

    pub fn move_file_start(&self, cursor: &mut Cursor) {
        cursor.char_index = 0;
        cursor.preferred_col = None;
    }

    pub fn move_file_end(&self, cursor: &mut Cursor) {
        cursor.char_index = self.text.len_chars() as _;
        cursor.preferred_col = None;
    }

    pub fn move_vertical(&self, delta: i64, cursor: &mut Cursor) {
        let (line, col) = self.cursor_line_col(cursor);
        let target_col  = cursor.preferred_col.unwrap_or(col);
        if cursor.preferred_col.is_none() { cursor.preferred_col = Some(col); }

        let num_lines   = self.text.len_lines();
        let target_line = (line as i64 + delta).clamp(0, num_lines as i64 - 1) as u32;
        if target_line == line { return; }

        let line_start = self.text.line_to_char(target_line);
        let line_str   = self.text.line(target_line);
        let trailing   = if line_str.len_chars() > 0 && line_str.char(line_str.len_chars() - 1) == '\n' { 1 } else { 0 };
        let line_len   = line_str.len_chars() - trailing;

        cursor.char_index = (line_start + target_col.min(line_len as u32)) as usize;
    }

    pub fn move_word_forward(&self, cursor: &mut Cursor) {
        let len = self.text.len_chars() as usize;
        let mut i = cursor.char_index;

        // Skip non-word chars
        while i < len && !is_word_char(self.text.char(i as _)) { i += 1; }
        // Skip word chars
        while i < len && is_word_char(self.text.char(i as _)) { i += 1; }

        cursor.char_index    = i;
        cursor.preferred_col = None;
    }

    pub fn move_word_backward(&self, cursor: &mut Cursor) {
        let mut i = cursor.char_index;

        // Skip non-word chars going left
        while i > 0 && !is_word_char(self.text.char(i as u32 - 1)) { i -= 1; }
        // Skip word chars going left
        while i > 0 && is_word_char(self.text.char(i as u32 - 1)) { i -= 1; }

        cursor.char_index    = i;
        cursor.preferred_col = None;
    }

    pub fn move_lines(&mut self, cursor: &mut Cursor, first_line: usize, last_line: usize, up: bool) {
        let total_lines = self.text.len_lines() as usize;

        if up  && first_line == 0                  { return }
        if !up && last_line + 1 >= total_lines     { return }

        //
        // Save columns before any mutation
        //
        let cursor_line = self.text.char_to_line(cursor.char_index as _);
        let cursor_col  = cursor.char_index - self.text.line_to_char(cursor_line as _) as usize;

        let (anchor_line, anchor_col) = match cursor.anchor_char_index {
            Some(anchor) => {
                let al = self.text.char_to_line(anchor as _);
                let ac = anchor as u32 - self.text.line_to_byte(al as _);
                (Some(al), Some(ac))
            }
            None => (None, None),
        };

        if up {
            //
            // Extract the line above the block
            //
            let above_start = self.text.line_to_byte(first_line as u32 - 1);
            let above_end   = self.text.line_to_byte(first_line as u32);
            let above_text: String = self.text.slice(above_start..above_end).to_string();

            //
            // Remove it
            //
            self.text.remove(above_start..above_end);

            //
            // insertion point is now after the block (which shifted up by one line)
            // last_line - 1 because everything shifted up
            //
            let insert_after_line = last_line - 1;
            let insert_byte = if insert_after_line + 1 < self.text.len_lines() as usize {
                self.text.line_to_byte(insert_after_line as u32 + 1)
            } else {
                //
                // Block is at end of file, need to append with a leading newline
                //
                let end = self.text.len_bytes();
                self.text.insert_char(end, '\n');
                self.text.len_bytes()
            };

            self.text.insert(insert_byte, &above_text);

            //
            // Cursor moved up one line, same column
            //
            let new_cursor_line = cursor_line.saturating_sub(1);
            let new_line_len    = self.text.line(new_cursor_line).len_chars().saturating_sub(1);
            cursor.char_index   =
                self.text.line_to_char(new_cursor_line) as usize + cursor_col.min(new_line_len as usize);

            if let (Some(al), Some(ac)) = (anchor_line, anchor_col) {
                let nal          = al.saturating_sub(1);
                let nal_len      = self.text.line(nal as _).len_chars().saturating_sub(1);
                cursor.anchor_char_index = Some((self.text.line_to_char(nal) + ac.min(nal_len)) as _);
            }
        } else {
            //
            // Extract the line below the block
            //
            let below_start = self.text.line_to_byte(last_line as u32 + 1);
            let below_end   = if last_line + 2 < self.text.len_lines() as usize {
                self.text.line_to_byte(last_line as u32 + 2)
            } else {
                self.text.len_bytes()
            };
            let below_text: String = self.text.slice(below_start..below_end).to_string();

            //
            // Ensure it ends with newline
            //
            let below_text = if below_text.ends_with('\n') {
                below_text
            } else {
                format!("{}\n", below_text)
            };

            //
            // Remove the line below
            //
            self.text.remove(below_start..below_end);

            //
            // Insert above first_line
            //
            let insert_byte = self.text.line_to_byte(first_line as _);
            self.text.insert(insert_byte, &below_text);

            //
            // Cursor moved down one line, same column
            //
            let new_cursor_line = (cursor_line + 1).min(self.text.len_lines().saturating_sub(1));
            let new_line_len    = self.text.line(new_cursor_line).len_chars().saturating_sub(1);
            cursor.char_index   = self.text.line_to_char(new_cursor_line) as usize + cursor_col.min(new_line_len as usize);

            if let (Some(anchor_line), Some(anchor_col)) = (anchor_line, anchor_col) {
                let new_al     = (anchor_line + 1).min(self.text.len_lines().saturating_sub(1));
                let new_al_len = self.text.line(new_al).len_chars().saturating_sub(1);
                cursor.anchor_char_index = Some((self.text.line_to_char(new_al) + anchor_col.min(new_al_len)) as _);
            }
        }

        self.last_edit_generation += 1;
        self.is_dirty = true;
        self.set_undo_separate();
        self.invalidate_cache_from_char(0);
    }
}

#[derive(Default, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct UndoNodeRef(u32);
cranelift_entity::entity_impl!(UndoNodeRef);

#[derive(Clone, Debug)]
pub struct UndoNode {
    pub snapshot: HistoryEntry,                // 8 bytes

    pub parent:     PackedOption<UndoNodeRef>, // 4 bytes
    pub last_child: PackedOption<UndoNodeRef>, // 4 bytes
    pub children:   EntityList<UndoNodeRef>,   // 4 bytes

    // Nov 2023 epoch
    pub timestamp:     u32,

    pub cursor_before: u32,                    // 4 bytes
    pub cursor_after:  u32,                    // 4 bytes

    // Packed bits:
    // 31..7: depth,       25 bits, max 33,554,431
    // 6 ..2: merge_count,  5 bits, max 31
    // 1 ..0: kind,         2 bits, max 3
    pub packed_meta:   u32,                    // 4 bytes
}

#[inline]
pub fn now_timestamp() -> u32 {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    (unix - 1_700_000_000) as u32
}

#[inline]
pub fn seconds_ago(ts: u32) -> u64 {
    let now = now_timestamp() as u64;
    now.saturating_sub(ts as u64)
}

#[inline]
pub fn format_age(seconds_ago: u64) -> SmallString<[u8; 32]> {
    const MINUTE: u64 = 60;
    const HOUR:   u64 = 60 * MINUTE;
    const DAY:    u64 = 24 * HOUR;
    const WEEK:   u64 = 7  * DAY;
    const MONTH:  u64 = 30 * DAY;

    let mut out = SmallString::new();

    let s  = seconds_ago;
    let m  = s  / MINUTE;
    let h  = s  / HOUR;
    let d  = s  / DAY;
    let w  = s  / WEEK;
    let mo = s  / MONTH;

    _ = match s {
        s if s < MINUTE => write!(&mut out, "{}s", s),
        s if s < HOUR   => write!(&mut out, "{}m {}s",  m,  s % MINUTE),
        s if s < DAY    => write!(&mut out, "{}h {}m",  h,  s % HOUR  / MINUTE),
        s if s < WEEK   => write!(&mut out, "{}d {}h",  d,  s % DAY   / HOUR),
        s if s < MONTH  => write!(&mut out, "{}w {}d",  w,  s % WEEK  / DAY),
        _               => write!(&mut out, "{}mo {}w", mo, s % MONTH / WEEK),
    };

    out
}

impl UndoNode {
    #[inline]
    pub fn new(
        snapshot: HistoryEntry,
        parent: Option<UndoNodeRef>,
        kind: EditKind,
        cursor_before: u32,
        cursor_after: u32,
        depth: u32,
    ) -> Self {
        let depth = depth & 0x1FFFFFF;
        let packed_meta = (kind as u32) | (depth << 7);

        Self {
            timestamp: now_timestamp(),
            snapshot,
            parent: parent.into(),
            last_child: None.into(),
            children: EntityList::new(),
            cursor_before,
            cursor_after,
            packed_meta,
        }
    }

    #[inline]
    pub fn kind(&self) -> EditKind {
        EditKind::from_u32(self.packed_meta & 0b11)
    }

    #[inline]
    pub fn depth(&self) -> u32 {
        (self.packed_meta >> 7) & 0x1FFFFFF
    }

    #[inline]
    pub fn merge_count(&self) -> u32 {
        (self.packed_meta >> 2) & 0b11111
    }

    #[inline]
    pub fn increment_merge_count(&mut self) {
        let current = self.merge_count();
        if current < (1<<15)-1 {
            // Clear the 5 bits, then OR the new count
            self.packed_meta = self.packed_meta & !(0b11111 << 2) | ((current + 1) << 2);
        }
    }

    #[inline]
    pub fn is_mergeable_with(&self, next_kind: EditKind, next_cursor_before: u32) -> bool {
        let kind = self.kind();

        if kind == EditKind::Boundary || next_kind == EditKind::Boundary {
            return false;
        }

        // :Configuration?
        //
        // Don't merge an edit after 15 subsequent Insert's
        if kind == EditKind::Insert && next_kind == EditKind::Insert && self.merge_count() >= 15 {
            return false;
        }

        kind == next_kind && self.cursor_after == next_cursor_before
    }
}

#[derive(Default)]
pub struct UndoTree {
    pub nodes: PrimaryMap<UndoNodeRef, UndoNode>,

    pub child_pool: ListPool<UndoNodeRef>,

    pub root: UndoNodeRef,
    pub head: UndoNodeRef,

    pub layout_cache: UndoGraphLayout,

    /// Forces the next edit transaction to start a new node
    pub break_merge: bool,
}

impl UndoTree {
    #[inline]
    pub fn new(initial_snapshot: HistoryEntry) -> Self {
        let mut nodes = PrimaryMap::new();
        let child_pool = ListPool::new();

        let root = nodes.push(UndoNode::new(
            initial_snapshot,
            None,
            EditKind::Boundary,
            0, 0, 0,
        ));

        let mut tree = Self {
            nodes,
            child_pool,
            root,
            head: root,
            layout_cache: UndoGraphLayout {
                nodes: Vec::with_capacity(256),
                is_dirty: true,
            },
            break_merge: false,
        };

        _ = tree.get_or_build_layout();
        tree
    }

    #[inline]
    pub fn get_or_build_layout(&mut self) -> &UndoGraphLayout {
        if self.layout_cache.is_dirty {
            self.rebuild_graph_layout();
        }

        &self.layout_cache
    }

    #[inline]
    pub fn invalidate_layout(&mut self) {
        self.layout_cache.is_dirty = true;
    }

    #[inline]
    pub fn commit(&mut self, snapshot: HistoryEntry, meta: EditMeta) -> UndoNodeRef {
        let parent_depth = self.nodes[self.head].depth();
        let new_id = self.nodes.push(UndoNode::new(
            snapshot,
            Some(self.head),
            meta.kind,
            meta.cursor_before,
            meta.cursor_after,
            parent_depth + 1,
        ));

        let head_id = self.head;
        self.nodes[head_id].children.push(new_id, &mut self.child_pool);
        self.nodes[head_id].last_child = Some(new_id).into();

        self.head = new_id;
        self.invalidate_layout();

        head_id
    }

    #[inline]
    pub fn head(&self) -> UndoNodeRef {
        self.head
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoCommitMode {
    Merge,
    Separate,
}

impl Default for UndoCommitMode {
    fn default() -> Self {
        Self::Separate
    }
}

impl UndoTree {
    /// Returns (nodes_bytes, child_pool_bytes, layout_cache_bytes)
    #[inline(always)]
    #[must_use]
    pub fn memory_bytes(&self) -> (u32, u32, u32) {
        let nodes_bytes = (self.nodes.len() * size_of::<UndoNode>()) as u32;

        //
        // In Cranelift's ListPool, every item takes up 8 bytes (Value + Next Index),
        // since every node (except the root) is a child of exactly one node,
        // the pool contains exactly (nodes - 1) items
        //
        let child_pool_items = self.nodes.len().saturating_sub(1);
        let pool_bytes = (child_pool_items * 8) as u32;

        let layout_bytes = (self.layout_cache.nodes.capacity() * std::mem::size_of::<UndoGraphNode>()) as u32;

        (nodes_bytes, pool_bytes, layout_bytes)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BufferMemoryUsage {
    /// The memory usage of the underlying text piece-tree
    pub piece_tree: piece_tree::MemoryUsage,

    /// Bytes consumed by the packed UndoNode primary map
    pub undo_nodes: u32,

    /// Bytes consumed by the Cranelift EntityList pool
    pub undo_pool: u32,

    /// Bytes consumed by the cached visual layout vector
    pub undo_layout: u32,
}

impl BufferMemoryUsage {
    #[inline(always)]
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.piece_tree.total() + self.undo_nodes + self.undo_pool + self.undo_layout
    }

    /// Overhead = everything except the actual document content in buffers.
    #[inline(always)]
    #[must_use]
    pub const fn overhead(&self) -> u32 {
        self.piece_tree.overhead() + self.undo_nodes + self.undo_pool + self.undo_layout
    }
}

impl Buffer {
    /// Aggregate memory usage breakdown for the entire buffer
    #[inline(always)]
    #[must_use]
    pub fn memory_usage(&self) -> BufferMemoryUsage {
        let (undo_nodes, undo_pool, undo_layout) = self.undo.memory_bytes();

        BufferMemoryUsage {
            piece_tree: self.text.memory_usage(),
            undo_nodes,
            undo_pool,
            undo_layout,
        }
    }
}

impl Buffer {
    #[inline]
    pub fn undo(&mut self, cursor: &mut Cursor) {
        //
        // Force-close any active typing groups before jumping!
        //
        while self.text.transaction_depth > 0 {
            self.text.end_undo_group();
        }

        let head_id = self.undo.head;
        if head_id == self.undo.root { return; }

        let cursor_before = self.undo.nodes[head_id].cursor_before;

        let parent_id = self.undo.nodes[head_id].parent.unwrap();
        let snapshot = self.undo.nodes[parent_id].snapshot;

        _ = self.text.snap_to(snapshot, cursor.char_index as u32);

        cursor.char_index = cursor_before as _;
        cursor.anchor_char_index = None;

        self.undo.head = parent_id;

        self.invalidate_cache_from_char(0);
        self.is_dirty = true;
        self.last_edit_generation += 1;

        self.undo.break_merge = true;
        self.did_snap_state = true;
        self.did_snap_state_tree_sitter = true;
    }

    pub fn redo(&mut self, cursor: &mut Cursor) {
        // @Cutnpaste from undo

        //
        // Force-close any active typing groups before jumping!
        //
        while self.text.transaction_depth > 0 {
            self.text.end_undo_group();
        }

        let head_id = self.undo.head;

        //
        // Follow the most recent branch
        //
        let Some(child_id) = self.undo.nodes[head_id].last_child.expand() else {
            return;
        };

        let snapshot = self.undo.nodes[child_id].snapshot;

        let cursor_before = self.undo.nodes[child_id].cursor_before;

        _ = self.text.snap_to(snapshot, cursor.char_index as u32);

        cursor.char_index = cursor_before as _;
        cursor.anchor_char_index = None;

        self.undo.head = child_id;

        self.invalidate_cache_from_char(0);
        self.is_dirty = true;
        self.last_edit_generation += 1;

        self.undo.break_merge = true;
        self.did_snap_state = true;
        self.did_snap_state_tree_sitter = true;
    }

    pub fn jump_to(&mut self, target: UndoNodeRef, cursor: &mut Cursor) -> bool {
        if target == self.undo.head { return false; }

        //
        // Force-close any active typing groups before jumping!
        //
        while self.text.transaction_depth > 0 {
            self.text.end_undo_group();
        }

        //
        // Walk the target path from root to target, setting last_child on each
        // parent so that subsequent linear redo follows this branch
        //
        let mut path = Vec::new();  // @Memory
        let mut cur = target;
        while let Some(parent) = self.undo.nodes[cur].parent.expand() {
            path.push((parent, cur));
            cur = parent;
        }
        for (parent_id, child_id) in path {
            self.undo.nodes[parent_id].last_child = Some(child_id).into();
        }

        let is_forward = self.undo.nodes[target].depth() > self.undo.nodes[self.undo.head].depth();

        let cursor_pos = self.undo.nodes[target].cursor_after;

        self.undo.head = target;
        let target_snapshot = self.undo.nodes[target].snapshot;

        _ = self.text.snap_to(target_snapshot, cursor.char_index as u32);

        cursor.char_index = cursor_pos as _;
        cursor.anchor_char_index = None;

        self.invalidate_cache_from_char(0);
        self.is_dirty = true;
        self.last_edit_generation += 1;
        self.did_snap_state = true;
        self.did_snap_state_tree_sitter = true;

        is_forward
    }
}

#[inline(always)]
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

pub struct UndoGraphNode {
    pub id:     UndoNodeRef,
    pub parent: PackedOption<UndoNodeRef>,

    pub depth: u32,  // @Redundant?  @Memory
    pub lane:  u32,

    pub x: f32,
    pub y: f32,
}

#[derive(Default)]
pub struct UndoGraphLayout {
    pub nodes: Vec<UndoGraphNode>,
    pub is_dirty: bool
}

impl UndoTree {
    pub fn rebuild_graph_layout(&mut self) -> &UndoGraphLayout {
        self.layout_cache.nodes.clear();
        Self::layout_node(
            self.root,
            0,
            0,
            &mut 0,
            &self.nodes,
            &self.child_pool,
            &mut self.layout_cache.nodes
        );

        &self.layout_cache
    }

    fn layout_node(
        node_id: UndoNodeRef,
        lane: u32,
        depth: u32,
        next_lane: &mut u32,
        nodes: &PrimaryMap<UndoNodeRef, UndoNode>,
        child_pool: &ListPool<UndoNodeRef>,
        out: &mut Vec<UndoGraphNode>
    ) {
        fn deterministic_jitter(id: UndoNodeRef, axis_seed: u32) -> f32 {
            let mut hash = id.0.wrapping_mul(314159265).wrapping_add(axis_seed.wrapping_mul(271828182));
            hash ^= hash >> 13;
            hash = hash.wrapping_mul(2654435761);

            // Normalize
            let val = hash % 2000;
            (val as f32 - 1000.0) / 1000.0
        }

        let pad_x = 40.0;
        let pad_y = 40.0;

        let base_x = pad_x + (depth as f32 * 60.0);
        let base_y = pad_y + (lane  as f32 * 50.0);

        // :Design?
        //
        // Apply a jitter so it looks scattered/random
        //
        let jitter_x = deterministic_jitter(node_id, 1) * 18.0;
        let jitter_y = deterministic_jitter(node_id, 2) * 18.0;

        out.push(UndoGraphNode {
            id: node_id,
            parent: nodes[node_id].parent.into(),
            depth,
            lane,
            x: base_x + jitter_x,
            y: base_y + jitter_y,
        });

        let node = &nodes[node_id];

        for (i, child) in node.children.as_slice(child_pool).iter().enumerate() {
            let child_lane = if i == 0 {
                lane             // The oldest/original branch   ALWAYS gets the straight line
            } else {
                *next_lane += 1; // Newer               branches ALWAYS get pushed down
                *next_lane
            };

            Self::layout_node(
                *child,
                child_lane,
                depth + 1,
                next_lane,
                nodes,
                child_pool,
                out
            );
        }
    }
}
