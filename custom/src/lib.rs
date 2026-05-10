// @Note: See @Note at the top of core/src/main.rs
//
// #[cfg(feature = "dhat")]
// #[global_allocator]
// static ALLOC: dhat::Alloc = dhat::Alloc;
//
// #[cfg(feature = "mimalloc")]
// #[global_allocator]
// static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod lsp;

use lsp::*;

use crossbeam_channel::{Receiver, Sender};
use editor::buffer::Buffer;
use editor::color::Color;
use editor::director::{EntryKind, ScanState};
use editor::gpu::Gpu;
use editor::session::{CustomChunkId, apply_session, default_session_path, load_session, pretty_path};
use editor::command::{CommandContext, CommandEntry, CommandTable, KeyCombo, Keymap, LoadedLib, Mods};
use editor::*;

use editor_macros::{collect_commands, command, export};
use memmap2::MmapOptions;

use std::borrow::Cow;
use std::io::Read;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::fmt::Write;

use smallvec::smallvec;
use smallstr::SmallString;
use winit::event::KeyEvent;
use cranelift_entity::EntityRef;
use winit::keyboard::{Key, NamedKey};
use cranelift_entity::packed_option::ReservedValue;

pub const    LISTER_CUSTOM_CHUNK_ID: CustomChunkId = 42;
pub const COMMANDER_CUSTOM_CHUNK_ID: CustomChunkId = 1337;

pub const LISTER_SPLIT_CUSTOM_PANEL: CustomPanel = CustomPanel { extra0: 420, extra1: 67, extra2: 69 };
pub const LISTER_SPLIT_PANEL_KIND: PanelKind = PanelKind::Custom(LISTER_SPLIT_CUSTOM_PANEL);

// @Cleanup: Move this out of here
macro_rules! custom_data {
    (
        $(#[$struct_meta:meta])*
        $vis:vis struct $name:ident -> $transient_name:ident {
            persistent {
                $(
                    $(#[$p_field_meta:meta])*
                    $p_field_vis:vis $p_field:ident : $p_ty:ty
                ),* $(,)?
            }
            transient {
                $(
                    $(#[$t_field_meta:meta])*
                    $t_field_vis:vis $t_field:ident : $t_ty:ty
                ),* $(,)?
            }
        }
    ) => {
        paste::paste! {
            #[allow(dead_code, unused)]
            $(#[$struct_meta])*
            $vis struct $name {
                $($p_field_vis $p_field: $p_ty,)*
            }

            #[allow(dead_code, unused)]
            $vis struct $transient_name {
                $($t_field_vis $t_field: $t_ty,)*
            }

            #[allow(dead_code, unused)]
            $vis trait CustomDataAccess {
                $(
                    fn $p_field(&self) -> &$p_ty;
                    fn [<$p_field _mut>](&mut self) -> &mut $p_ty;
                )*
                $(
                    fn $t_field(&self) -> &$t_ty;
                    fn [<$t_field _mut>](&mut self) -> &mut $t_ty;
                )*
            }

            #[allow(dead_code, unused, clippy::all)]
            impl CustomDataAccess for editor::EditorCustomData {
                $(
                    #[inline]
                    #[cfg_attr(debug_assertions, track_caller)]
                    fn $p_field(&self) -> &$p_ty {
                        &self.get::<$name>().$p_field
                    }
                    #[inline]
                    #[cfg_attr(debug_assertions, track_caller)]
                    fn [<$p_field _mut>](&mut self) -> &mut $p_ty {
                        &mut self.get_mut::<$name>().$p_field
                    }
                )*
                $(
                    #[inline]
                    #[cfg_attr(debug_assertions, track_caller)]
                    fn $t_field(&self) -> &$t_ty {
                        &self.get_transient::<$transient_name>().$t_field
                    }
                    #[inline]
                    #[cfg_attr(debug_assertions, track_caller)]
                    fn [<$t_field _mut>](&mut self) -> &mut $t_ty {
                        &mut self.get_transient_mut::<$transient_name>().$t_field
                    }
                )*
            }
        }
    };
}

custom_data! {
    struct CustomData -> CustomDataTransient {
        persistent {
            // IMPORTANT IMPORTANT IMPORTANT IMPORTANT IMPORTANT IMPORTANT IMPORTANT IMPORTANT
            // @Important @Important @Important @Important @Important @Important @Important @Important

            //
            // SAFETY: types here MUST not contain vtable pointers into the dylib.
            // Only use types from std or types defined in the shared crate.
            //
            // If you know your types are gonna use virtual dispatch, put them inside the `transient {}` block below.
            //

            lister: Lister,
            commander: Commander,

            lsp: LspClient,
        }

        transient {}
    }
}

#[command]
pub fn move_right(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_right(&mut view.cursor);
}

#[command]
pub fn move_left(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_left(&mut view.cursor);
}

#[command]
pub fn move_up(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_vertical(-1, &mut view.cursor);
}

#[command]
pub fn move_down(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_vertical(1, &mut view.cursor);
}

#[command]
pub fn move_page_up(cx: &mut CommandContext) {
    scroll_page(cx.editor, cx.gpu, -1);
}

#[command]
pub fn move_page_down(cx: &mut CommandContext) {
    scroll_page(cx.editor, cx.gpu, 1);
}

#[command]
pub fn move_line_start(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_line_start(&mut view.cursor);
}

#[command]
pub fn move_line_end(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_line_end(&mut view.cursor);
}

#[command]
pub fn move_file_start(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_file_start(&mut view.cursor);
}

#[command]
pub fn move_file_end(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_file_end(&mut view.cursor);
}

#[command]
pub fn move_word_forward(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_word_forward(&mut view.cursor);
}

#[command]
pub fn move_word_backward(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_word_backward(&mut view.cursor);
}

#[command]
pub fn move_to_first_character_in_current_line(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();

    let (line, _col) = buf.char_to_line_col(view.cursor.char_index);

    let char_count_before_first_non_whitespace_in_line = buf.text.line(line as usize)
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();

    let character_index_of_line = buf.text.line_to_char(line as usize);

    view.cursor.char_index = character_index_of_line + char_count_before_first_non_whitespace_in_line;
    view.cursor.preferred_col = None;
}

#[command]
pub fn delete_word_forward(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.delete_word_forward(&mut view.cursor);
}

#[command]
pub fn delete_word_backward(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.delete_word_backward(&mut view.cursor);
}

#[command]
pub fn delete_forward(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.delete_forward(&mut view.cursor);
}

#[command]
pub fn delete_backward(cx: &mut CommandContext) {
    //
    // Identify if we are in a path query
    //
    let is_lister_buffer = cx.editor.active_view().buffer_id == cx.editor.lister().query_buffer;

    let (view, buf) = cx.editor.active_view_and_buffer_mut();

    //
    // If there's a selection, always just delete the selection
    //
    if view.cursor.anchor_char_index.is_some() {
        buf.delete_selection_with_animation(&mut view.cursor);
        return;
    }

    let cursor_pos = view.cursor.char_index;
    if cursor_pos == 0 { return; }

    if is_lister_buffer {
        let char_to_left = buf.text.char(cursor_pos - 1);

        if char_to_left == MAIN_SEPARATOR {
            //
            // We are at a slash (e.g., "~/Documents/|").
            // We want to delete "Documents/" so we end at "~/".
            //

            // Start the deletion range at the current cursor
            let mut target_start = cursor_pos - 1;

            let iter = buf.text.chars_at(cursor_pos - 1).reversed();
            for c in iter {
                if c == MAIN_SEPARATOR { break; }
                target_start -= 1;
            }

            view.cursor.anchor_char_index = Some(cursor_pos);
            view.cursor.char_index = target_start;
            buf.delete_selection_with_animation(&mut view.cursor);
            return;
        }
    }

    // Default: Just a normal character backspace
    buf.delete_backward(&mut view.cursor);
}

#[command]
pub fn delete_forward_until_newline(cx: &mut CommandContext) {  // :BufferScratch
    let (view, buf) = cx.editor.active_view_and_buffer_mut();

    let len = buf.text.len_chars();
    if view.cursor.char_index >= len { return; }

    buf.flatten_rope_into_scratch(  // :BufferScratch
        buf.text.char_to_byte(view.cursor.char_index),
        buf.text.len_bytes(),
    );

    let mut chars_to_delete = 0;
    {
        let slice = &buf.scratch_space_to_flatten_rope_into;

        let mut all_whitespace = true;

        for c in slice.chars() {
            if c == '\n' {
                if chars_to_delete == 0 {
                    chars_to_delete = 1;
                } else if all_whitespace {
                    chars_to_delete += 1;
                }
                break;
            }

            if !c.is_whitespace() {
                all_whitespace = false;
            }

            chars_to_delete += 1;
        }

        if chars_to_delete == 0 {
            chars_to_delete = slice.chars().count();
        }
    }

    if chars_to_delete == 0 { return; }

    view.cursor.anchor_char_index = Some(view.cursor.char_index + chars_to_delete);
    copy_impl(cx, false, false);

    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.delete_selection_without_animation(&mut view.cursor);
}

#[command]
pub fn insert_newline(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();

    let cursor_byte = buf.text.char_to_byte(view.cursor.char_index);

    // Cap context line search to 4KB
    let start_byte = cursor_byte.saturating_sub(4096); // :Configuration
    buf.flatten_rope_into_scratch(start_byte, cursor_byte);

    let flat = buf.scratch_space_to_flatten_rope_into.as_bytes();

    // Walk backwards to find last non-blank line
    let context_start = {
        let mut pos = flat.len();
        loop {
            let line_end = pos;
            match memchr::memrchr(b'\n', &flat[..pos]) {
                None => break 0,  // No newline found, start of buffer

                Some(nl) => {
                    let line_bytes = &flat[nl + 1..line_end];
                    if line_bytes.iter().any(|&b| b != b' ' && b != b'\t') {
                        break nl + 1;  // Start of the non-blank line, after the \n
                    }

                    pos = nl;  // Step back past this \n
                    if pos == 0 { break 0; }
                }
            }
        }
    };

    // Find indent of context line
    let line_end = memchr::memchr(b'\n', &flat[context_start..])
        .map(|p| context_start + p)
        .unwrap_or(flat.len());

    let line_bytes = &flat[context_start..line_end];

    //
    // Count leading whitespace bytes (all spaces/tabs are single-byte)
    //
    let indent_len = line_bytes.iter().take_while(|&&b| b == b' ' || b == b'\t').count();

    //
    // Last meaningful char before cursor
    //
    let last_meaningful = line_bytes.iter().filter(|&&b| b != b' ' && b != b'\t').last().copied();

    let open = matches!(last_meaningful, Some(b'{') | Some(b'(') | Some(b'['));

    let mut indent = SmallString::<[u8; 128]>::new();

    //
    // Preserve tabs vs spaces
    //
    indent.push_str(unsafe { std::str::from_utf8_unchecked(&line_bytes[..indent_len]) });
    if open {
        // :Configuration
        // Fill the extra 4 with spaces
        for _ in 0..4 { indent.push(' '); }
    }

    buf.insert_char('\n', &mut view.cursor);
    if !indent.is_empty() {
        buf.insert_literal(&indent, &mut view.cursor);
    }
}

#[command]
pub fn insert_newline_after(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.insert_char_after('\n', &mut view.cursor);
}

#[command]
pub fn set_anchor(cx: &mut CommandContext) {
    let (view, _buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.set_anchor();
}

#[command]
pub fn unset_anchor(cx: &mut CommandContext) {
    let (view, _buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
}

#[command]
pub fn basic_character(cx: &mut CommandContext) {
    let Some(c) = (match &cx.event_and_mods.map(|(e, _)| &e.logical_key) {
        Some(Key::Character(s))           => s.chars().next(),
        Some(Key::Named(NamedKey::Space)) => Some(' '),
        _ => None,
    }) else {
        return
    };

    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    let cursor = &mut view.cursor;
    cursor.unset_anchor();

    if matches!(c, '}' | ')' | ']') {  // :Configuration
        let (line, col) = buf.cursor_line_col(cursor);
        let line_str = buf.text.line(line as usize);
        let only_ws = col > 0 && line_str.chars().take(col as usize).all(|c| c == ' ' || c == '\t');
        if only_ws && col >= 4 {
            for _ in 0..4 {
                buf.delete_backward(cursor);
            }
        }
    }

    buf.insert_char(c, cursor);
}

#[command]
pub fn tab(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    let cursor = &mut view.cursor;
    cursor.unset_anchor();
    buf.insert_literal("    ", &mut view.cursor);
}

#[command]
pub fn split_vertically(cx: &mut CommandContext) {
    cx.editor.split_active(true, 0.5);
}

#[command]
pub fn split_horizontally(cx: &mut CommandContext) {
    cx.editor.split_active(false, 0.5);
}

#[command]
pub fn close_focused_split(cx: &mut CommandContext) {
    cx.editor.close_active();
}

#[command]
pub fn toggle_focused_split(cx: &mut CommandContext) {
    cx.editor.toggle_active_panel();
}

#[command]
pub fn scale_down(cx: &mut CommandContext) {
    rescale(cx.editor, cx.editor.scale - SCALE_STEP);
}

#[command]
pub fn scale_up(cx: &mut CommandContext) {
    rescale(cx.editor, cx.editor.scale + SCALE_STEP);
}

#[command]
pub fn scale_reset(cx: &mut CommandContext) {
    rescale(cx.editor, 1.0);
}

#[command]
pub fn open_new_buffer(cx: &mut CommandContext) {
    let buffer_id = cx.editor.push_buffer(Buffer::new());
    let view_id = cx.editor.views.next_key();
    cx.editor.views.push(View::new(view_id, buffer_id));

    let root_id  = cx.editor.root_panel;

    if matches!(&cx.editor.panel(root_id).kind, PanelKind::Leaf { .. }) {
        //
        // Ensure root is a split
        //

        cx.editor.active_panel = root_id;
        cx.editor.split_active(true, 0.5);
    }

    if let PanelKind::Split(split) = cx.editor.panel(root_id).kind {
        let unfocused_id = if cx.editor.active_panel == split.left_id {
            split.right_id
        } else {
            split.left_id
        };

        cx.editor.panel_mut(unfocused_id).kind = PanelKind::Leaf { view_id };
        cx.editor.toggle_active_panel();
    }
}

#[command]
pub fn cycle_buffers_left(cx: &mut CommandContext) {
    let buffer_id = cx.editor.previous_buffer();
    cx.editor.active_view_mut().switch_buffer(buffer_id);
    cx.editor.mru_focus(buffer_id); // @Refactor
}

#[command]
pub fn cycle_buffers_right(cx: &mut CommandContext) {
    let buffer_id = cx.editor.next_buffer();
    cx.editor.active_view_mut().switch_buffer(buffer_id);
    cx.editor.mru_focus(buffer_id); // @Refactor
}

#[command]
pub fn write_buffer_to_disk(cx: &mut CommandContext) {
    let buffer_id = cx.editor.active_view().buffer_id;
    _ = editor_save_buffer_onto_disk(cx.editor, buffer_id);
}

pub fn lister_item_list_from_command_table(cx: &CommandContext) -> Vec<ListerItem> {
    cx.command_table.iter().enumerate().map(|(index, (atom, _cmd))| {
        ListerItem {
            data: index as u64,
            label: cx.command_table.resolve(*atom).into(),
            sublabel: "".into(),
        }
    }).collect()
}

pub fn lister_item_list_from_buffer_list(cx: &CommandContext) -> Vec<ListerItem> {
    cx.editor.most_recently_used_buffers.iter().filter_map(|&buffer_id| {
        // Skip internal buffers
        if buffer_id == cx.editor.lister().query_buffer { return None; }

        let buffer = &cx.editor.buffers[buffer_id];
        let label: SmallString<_> = buffer.path.as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("[scratch]")
            .into();

        let sublabel: SmallString<_> = buffer.path.as_ref()
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .into();

        Some(ListerItem {
            data: buffer_id.index() as u64,
            label,
            sublabel,
        })
    }).collect()
}

#[command]
pub fn open_command_lister(cx: &mut CommandContext) {
    let items = lister_item_list_from_command_table(cx);
    let lister = cx.editor.lister_mut();
    lister.is_query_dirty = true;
    lister.rebuild_filtered();
    editor_open_lister(cx.editor, items, |cx, item_data| {
        (cx.command_table[item_data as usize].func)(cx);
    });
}

#[command]
pub fn paste(cx: &mut CommandContext) {
    let Some(clipboard) = cx.editor.get_clipboard() else {
        return;
    };

    let (view, buf) = cx.editor.active_view_and_buffer_mut();

    buf.delete_selection_without_animation(&mut view.cursor);
    buf.insert_literal(&clipboard, &mut view.cursor);
    buf.append_last_insertion_to_currently_animated_pastes();
}

pub fn copy_impl(cx: &mut CommandContext, unset_anchor: bool, animate: bool) { // :BufferScratch
    let (view, buf) = cx.editor.active_view_and_buffer_mut();

    let Some(anchor_char_index) = view.cursor.anchor_char_index else {
        return;
    };

    let char_index = view.cursor.char_index;
    let slice = if anchor_char_index < char_index {
        buf.text.slice(anchor_char_index..char_index)
    } else {
        buf.text.slice(char_index..anchor_char_index)
    };

    buf.scratch_space_to_flatten_rope_into.clear(); // :BufferScratch
    buf.scratch_space_to_flatten_rope_into.extend(slice.chars());

    if animate {
        let (start, end) = if anchor_char_index < char_index {
            (anchor_char_index, char_index)
        } else {
            (char_index, anchor_char_index)
        };
        let byte_start = buf.text.char_to_byte(start);
        let byte_end   = buf.text.char_to_byte(end);
        buf.animate_copy(byte_start as _, (byte_end - byte_start) as _);
    }

    if unset_anchor {
        view.cursor.unset_anchor();
    }

    let buffer_id = view.buffer_id;
    Editor::set_clipboard(
        &mut cx.editor.clipboard,
        &cx.editor.buffers[buffer_id].scratch_space_to_flatten_rope_into
    );
}

#[command]
pub fn copy(cx: &mut CommandContext) {
    copy_impl(cx, true, true);
}

#[command]
pub fn delete_selection_and_copy(cx: &mut CommandContext) {
    copy_impl(cx, false, false);

    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.delete_selection_with_animation(&mut view.cursor);
}

#[command]
pub fn switch_buffer(cx: &mut CommandContext) { // @Refactor: Rename switch_buffer -> open_buffer_lister
    cx.editor.lister_mut().set_selected_index_to_1_instead_of_0 = true;

    let items = lister_item_list_from_buffer_list(cx);

    editor_open_lister(cx.editor, items, |cx, item_data| {
        let buffer_id = BufferId::new(item_data as usize);
        cx.editor.active_view_mut().switch_buffer(buffer_id);
        cx.editor.mru_focus(buffer_id); // @Refactor
    });
}

pub fn path_to_display(path: &str) -> String {     // @Refactor
    if let Ok(home) = std::env::var("HOME") {
        if path.starts_with(&home) {
            return format!("~{}", &path[home.len()..]);
        }
    }
    path.to_string()
}

pub fn display_to_path(display: &str) -> String {  // @Refactor
    if display.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home, &display[1..]);
        }
    }

    display.to_string()
}

#[command]
pub fn open_file(cx: &mut CommandContext) {
    // @Note: Compute start_dir here because after the open_lister call,
    // active_view() is gonna return the View into the lister's query buffer.
    //
    // Inherit start dir from active buffer, fall back to cwd
    //
    let start_dir = cx.editor.buffers[cx.editor.active_view().buffer_id].path
        .as_deref()
        .and_then(|p| p.parent())
        .and_then(|p| p.canonicalize().ok())  // @SlowFileSystem
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| cx.editor.canonicalized_current_working_directory.as_str().to_owned());

    open_file_impl(cx, start_dir);
}

pub fn open_file_impl(cx: &mut CommandContext, start_dir: String) {
    let items = Vec::new();
    editor_open_lister_with_frame_callback(
        cx.editor,

        items,

        // Called on select
        |cx, item_data| {
            let entry_kind: EntryKind = unsafe { core::mem::transmute(item_data as u8) };

            let selected_item = &cx.editor.lister().items[cx.editor.lister().filtered[cx.editor.lister().selected_index as usize] as usize];
            let path = &selected_item.sublabel;

            if entry_kind == EntryKind::Dir {
                let path: &Path = path.as_str().as_ref();
                if let Ok(canon) = path.canonicalize() {
                    cx.editor.canonicalized_current_working_directory = canon.into_os_string().into_string().unwrap().into();
                    open_file_impl(cx, cx.editor.canonicalized_current_working_directory.to_string()); // @Clone
                }
                return;
            }

            let view = cx.editor.active_view_id();
            let path: &Path = path.as_str().as_ref();
            let path = Box::from(path);
            open_buffer_from_path_in(cx.editor, view, path);
        },

        // Called on every frame redraw
        |cx| {
            let dir: &Path = cx.editor.canonicalized_last_scanned_directory.as_str().as_ref();
            let got_new_chunks = cx.editor.director.poll(dir);

            let mut redraw = ShouldRequestFrameRedraw::No;

            let lister = cx.editor.custom_data.lister_mut();

            if got_new_chunks {
                if let Some(cached) = cx.editor.director.entries.get(dir)
                    && (cached.entries.generation != lister.last_seen_cached_dir_generation
                     || cached.state == ScanState::Ready)
                {
                    lister.last_seen_cached_dir_generation = cached.entries.generation;
                    lister.items.clear();
                    for entry in cached.entries.iter() {
                        lister.items.push(ListerItem {
                            data:     entry.kind as u64,
                            label:    entry.name.into(),
                            sublabel: entry.path.into(),
                        });
                    }
                    lister.is_query_dirty = true;
                    lister.rebuild_filtered();
                    lister.is_query_dirty = true; // nocheckin @DocumentThis
                }
            }

            if !lister.is_query_dirty {
                redraw = redraw.or_if(got_new_chunks, "File Lister new chunks", &mut cx.editor.redraw_reasons);
            }
            lister.is_query_dirty = false;

            let query_path = display_to_path(lister.query.as_str()); // @Clone
            let query_path: &Path = query_path.as_ref();

            let candidate = if lister.query.chars().last() == Some(MAIN_SEPARATOR) {
                query_path.to_path_buf()
            } else {
                query_path.parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from(  // @Clone
                        cx.editor.canonicalized_current_working_directory.as_str()
                    ))
            };

            let last_scanned: &Path = cx.editor.canonicalized_last_scanned_directory.as_str().as_ref();

            let dir_to_scan = if candidate != last_scanned {
                candidate.canonicalize().unwrap_or(candidate)  // @SlowFileSystem
            } else {
                last_scanned.to_path_buf()
            };

            let last_scanned: &Path = cx.editor.canonicalized_last_scanned_directory.as_str().as_ref();

            if dir_to_scan != last_scanned {
                let cwd_as_path: &Path = cx.editor.canonicalized_current_working_directory.as_str().as_ref();
                if dir_to_scan.as_path() != cwd_as_path {
                    cx.editor.canonicalized_current_working_directory = dir_to_scan.to_string_lossy().into(); // @Clone
                }

                cx.editor.canonicalized_last_scanned_directory = dir_to_scan.to_string_lossy().into(); // @Clone

                //
                // Clear immediately so stale entries from previous dir don't linger
                //
                lister.items.clear();
                lister.last_seen_cached_dir_generation = u64::MAX;
                lister.rebuild_filtered();

                redraw = redraw.or_msg("File Lister clear", &mut cx.editor.redraw_reasons);

                //
                // Also pre-scan parent so navigating up is instant
                //
                if let Some(parent) = dir_to_scan.parent() {
                    if cx.editor.director.entries.get(parent).is_none() {
                        cx.editor.director.kick_scan(parent, false, false, false);
                    }
                }

                cx.editor.director.kick_scan(dir_to_scan.as_path(), false, true, true);
            } else {
                cx.editor.director.get(dir_to_scan.as_path());
            }

            redraw
        }
    );

    cx.editor.canonicalized_current_working_directory = start_dir.as_str().into();

    // Pre-fill query with current working directory
    let mut display_path = path_to_display(&start_dir);
    if !display_path.ends_with(MAIN_SEPARATOR) { display_path.push(MAIN_SEPARATOR); }

    let lister = cx.editor.custom_data.lister_mut();

    let query_buffer = lister.query_buffer;
    let query_view   = lister.query_view;

    cx.editor.buffers[query_buffer].clear();
    cx.editor.buffers[query_buffer].insert_literal(
        &display_path,
        &mut cx.editor.views[query_view].cursor,
    );

    // @Redundant?
    // Sync the lister query string
    lister.query.clear();
    lister.query.push_str(&display_path);
    lister.is_query_dirty = true;
    lister.is_listing_file_entries = true;
    lister.last_seen_cached_dir_generation = u64::MAX;
}

#[cfg(test)]
mod indent_tests {
    use super::*;

    fn do_indent(input: &str) -> Cow<'_, str> {
        indent_region_impl(input, 0, input.lines().count().saturating_sub(1), 4)
    }

    fn do_indent_lines(input: &str, start: usize, end: usize) -> Cow<'_, str> {
        indent_region_impl(input, start, end, 4)
    }

    #[test]
    fn test_basic_block() {
        let input = "\
fn foo() {
        let x = 1;
        let y = 2;
}";
        let expected = "\
fn foo() {
    let x = 1;
    let y = 2;
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_closing_brace() {
        let input = "\
fn foo() {
        let x = 1;
        }";
        let expected = "\
fn foo() {
    let x = 1;
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_method_chain_preserved() {
        let input = "\
fn foo() {
    let x = something
        .and_then(|x| x)
        .unwrap();
}";
        // method chain relative indent should be preserved
        let expected = "\
fn foo() {
    let x = something
        .and_then(|x| x)
        .unwrap();
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_nested() {
        let input = "\
fn foo() {
            if true {
                let x = 1;
            }
}";
        let expected = "\
fn foo() {
    if true {
        let x = 1;
    }
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_blank_lines_preserved() {
        let input = "\
fn foo() {
        let x = 1;

        let y = 2;
}";
        let expected = "\
fn foo() {
    let x = 1;

    let y = 2;
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_already_correct() {
        let input = "\
fn foo() {
    let x = 1;
}";
        assert_eq!(do_indent(input), input);
    }

    #[test]
    fn test_partial_selection() {
        // Only indent lines 1..=2, leave line 0 alone
        let input = "\
fn foo() {
        let x = 1;
        let y = 2;
    let z = 3;
}";
        let expected = "\
fn foo() {
    let x = 1;
    let y = 2;
    let z = 3;
}";
        assert_eq!(do_indent_lines(input, 1, 2), expected);
    }

    #[test]
    fn test_closure_block() {
        let input = "\
fn foo() {
    vec.iter().map(|x| {
            let y = x + 1;
            y
        }).collect()
}";
        let expected = "\
fn foo() {
    vec.iter().map(|x| {
        let y = x + 1;
        y
    }).collect()
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_deeply_nested() {
        let input = "\
fn foo() {
                if true {
                        for i in 0..10 {
                                let x = i;
                        }
                }
}";
        let expected = "\
fn foo() {
    if true {
        for i in 0..10 {
            let x = i;
        }
    }
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_else_block() {
        let input = "\
fn foo() {
        if true {
                let x = 1;
        } else {
                let y = 2;
        }
}";
        let expected = "\
fn foo() {
    if true {
        let x = 1;
    } else {
        let y = 2;
    }
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_chained_closers() {
        // }).collect() - the }) is a closer followed by more content
        let input = "\
fn foo() {
    let v = vec![1, 2, 3]
        .iter()
        .map(|x| {
                *x + 1
                }).collect::<Vec<_>>();
}";
        let expected = "\
fn foo() {
    let v = vec![1, 2, 3]
        .iter()
        .map(|x| {
            *x + 1
        }).collect::<Vec<_>>();
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_match_block() {
        let input = "\
fn foo() {
        match x {
                Foo::A => 1,
                Foo::B => 2,
        }
}";
        let expected = "\
fn foo() {
    match x {
        Foo::A => 1,
        Foo::B => 2,
    }
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_empty_body() {
        let input = "\
fn foo() {
}";
        assert_eq!(do_indent(input), input);
    }

    #[test]
    fn test_multiple_blank_lines_between_blocks() {
        let input = "\
fn foo() {
        let x = 1;


        let y = 2;
}";
        let expected = "\
fn foo() {
    let x = 1;


    let y = 2;
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_struct_definition() {
        let input = "\
struct Foo {
        x: i32,
        y: i32,
}";
        // struct fields - same logic should apply
        let expected = "\
struct Foo {
    x: i32,
    y: i32,
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_multiline_args() {
        // opening paren continuation
        let input = "\
fn foo() {
    some_call(
            arg1,
            arg2,
    );
}";
        let expected = "\
fn foo() {
    some_call(
        arg1,
        arg2,
    );
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_some_stuff() {
        // opening paren continuation
        let input = "\
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
    }\
";


        let expected = "\
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
    }\
";

        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_some_other_stuff() {
        // opening paren continuation
        let input = "\
#[command]
pub fn basic_character(cx: &mut CommandContext) {
    let Some(c) = (match &cx.event_and_mods.map(|(e, _)| &e.logical_key) {
        Some(Key::Character(s))           => s.chars().next(),
            Some(Key::Named(NamedKey::Space)) => Some(' '),
        _ => None,
    }) else {
        return
    };

    let (view, buf) = cx.editor.active_view_and_buffer_mut();
        let cursor = &mut view.cursor;
    cursor.unset_anchor();

    if matches!(c, '}' | ')' | ']') {  // :Configuration
            let (line, col) = buf.cursor_line_col(cursor);
            let line_str = buf.text.line(line as usize);
        let only_ws = col > 0 && line_str.chars().take(col as usize).all(|c| c == ' ' || c == '\t');
        if only_ws && col >= 4 {
            for _ in 0..4 {
                buf.delete_backward(cursor);
                }
        }
    }

        buf.insert_char(c, cursor);
}\
";


        let expected = "\
#[command]
pub fn basic_character(cx: &mut CommandContext) {
    let Some(c) = (match &cx.event_and_mods.map(|(e, _)| &e.logical_key) {
        Some(Key::Character(s))           => s.chars().next(),
        Some(Key::Named(NamedKey::Space)) => Some(' '),
        _ => None,
    }) else {
        return
    };

    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    let cursor = &mut view.cursor;
    cursor.unset_anchor();

    if matches!(c, '}' | ')' | ']') {  // :Configuration
        let (line, col) = buf.cursor_line_col(cursor);
        let line_str = buf.text.line(line as usize);
        let only_ws = col > 0 && line_str.chars().take(col as usize).all(|c| c == ' ' || c == '\t');
        if only_ws && col >= 4 {
            for _ in 0..4 {
                buf.delete_backward(cursor);
            }
        }
    }

    buf.insert_char(c, cursor);
}\
";

        assert_eq!(do_indent(input), expected);
    }

#[test]
    fn test_where_clause() {
        let input = "\
fn foo<T>(x: T) -> T
where
    T: Clone + std::fmt::Debug,
{
        x.clone()
}";
        let expected = "\
fn foo<T>(x: T) -> T
where
    T: Clone + std::fmt::Debug,
{
    x.clone()
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_multiline_match() {
        let input = "\
fn foo(x: Option<i32>) -> i32 {
        match x {
                Some(v) if v > 0 => {
                        v * 2
                }
                Some(v) => v,
                None => {
                        eprintln!(\"none\");
                        0
                }
        }
}";
        let expected = "\
fn foo(x: Option<i32>) -> i32 {
    match x {
        Some(v) if v > 0 => {
            v * 2
        }
        Some(v) => v,
        None => {
            eprintln!(\"none\");
            0
        }
    }
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_let_else() {
        let input = "\
fn foo(x: Option<i32>) -> i32 {
        let Some(v) = x else {
                return 0;
        };
        v + 1
}";
        let expected = "\
fn foo(x: Option<i32>) -> i32 {
    let Some(v) = x else {
        return 0;
    };
    v + 1
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_impl_trait_block() {
        let input = "\
impl Foo for Bar {
        fn method_a(&self) -> i32 {
                self.x + 1
        }

        fn method_b(&self) -> bool {
                self.x > 0
        }
}";
        let expected = "\
impl Foo for Bar {
    fn method_a(&self) -> i32 {
        self.x + 1
    }

    fn method_b(&self) -> bool {
        self.x > 0
    }
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_nested_closures() {
        let input = "\
fn foo() {
        let result = outer.map(|x| {
                        inner.map(|y| {
                                x + y
                        }).sum::<i32>()
                }).collect::<Vec<_>>();
}";
        let expected = "\
fn foo() {
    let result = outer.map(|x| {
        inner.map(|y| {
            x + y
        }).sum::<i32>()
    }).collect::<Vec<_>>();
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_macro_with_braces() {
        let input = "\
fn foo() {
        let v = vec![
                1,
                2,
                3,
        ];
        println!(\"{:?}\", v);
}";
        let expected = "\
fn foo() {
    let v = vec![
        1,
        2,
        3,
    ];
    println!(\"{:?}\", v);
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_if_let_chain() {
        let input = "\
fn foo(a: Option<i32>, b: Option<i32>) {
        if let Some(x) = a {
                if let Some(y) = b {
                        println!(\"{} {}\", x, y);
                } else {
                        println!(\"no b\");
                }
        }
}";
        let expected = "\
fn foo(a: Option<i32>, b: Option<i32>) {
    if let Some(x) = a {
        if let Some(y) = b {
            println!(\"{} {}\", x, y);
        } else {
            println!(\"no b\");
        }
    }
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_struct_impl_combined() {
        let input = "\
pub struct Foo {
        x: i32,
        y: String,
}

impl Foo {
        pub fn new(x: i32, y: String) -> Self {
                Self { x, y }
        }

        pub fn process(&self) -> String {
                format!(\"{}: {}\", self.x, self.y)
        }
}";
        let expected = "\
pub struct Foo {
    x: i32,
    y: String,
}

impl Foo {
    pub fn new(x: i32, y: String) -> Self {
        Self { x, y }
    }

    pub fn process(&self) -> String {
        format!(\"{}: {}\", self.x, self.y)
    }
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_multiline_string_let() {
        // string contents should not be touched
        let input = "\
fn foo() {
        let s = \"hello\";
        let t = \"world\";
}";
        let expected = "\
fn foo() {
    let s = \"hello\";
    let t = \"world\";
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_attributes_on_methods() {
        let input = "\
impl Foo {
        #[inline(always)]
        pub fn fast(&self) -> i32 {
                self.x
        }

        #[cfg(test)]
        fn test_helper() {
                println!(\"hi\");
        }
}";
        let expected = "\
impl Foo {
    #[inline(always)]
    pub fn fast(&self) -> i32 {
        self.x
    }

    #[cfg(test)]
    fn test_helper() {
        println!(\"hi\");
    }
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_return_early() {
        let input = "\
fn foo(x: i32) -> i32 {
        if x < 0 {
                return -1;
        }
        if x == 0 {
                return 0;
        }
        x + 1
}";
        let expected = "\
fn foo(x: i32) -> i32 {
    if x < 0 {
        return -1;
    }
    if x == 0 {
        return 0;
    }
    x + 1
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_tuple_struct_and_impl() {
        let input = "\
pub struct Foo(i32, i32);

impl Foo {
        pub fn sum(&self) -> i32 {
                self.0 + self.1
        }
}";
        let expected = "\
pub struct Foo(i32, i32);

impl Foo {
    pub fn sum(&self) -> i32 {
        self.0 + self.1
    }
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_chained_methods_with_args() {
        let input = "\
fn foo() {
    let result = some_iter
        .filter(|x| x.is_valid())
        .map(|x| x.transform())
        .fold(0, |acc, x| acc + x);
}";
        let expected = "\
fn foo() {
    let result = some_iter
        .filter(|x| x.is_valid())
        .map(|x| x.transform())
        .fold(0, |acc, x| acc + x);
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_nested_if_match() {
        let input = "\
fn foo(x: Option<i32>) {
        if true {
                match x {
                        Some(v) => {
                                println!(\"{}\", v);
                        }
                        None => {}
                }
        }
}";
        let expected = "\
fn foo(x: Option<i32>) {
    if true {
        match x {
            Some(v) => {
                println!(\"{}\", v);
            }
            None => {}
        }
    }
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_closure_as_arg() {
        let input = "\
fn foo() {
        register(\"name\", |cx| {
                cx.do_thing();
                cx.do_other_thing();
        });
}";
        let expected = "\
fn foo() {
    register(\"name\", |cx| {
        cx.do_thing();
        cx.do_other_thing();
    });
}";
        assert_eq!(do_indent(input), expected);
    }

    #[test]
    fn test_multiple_top_level_fns() {
        let input = "\
fn foo() {
        let x = 1;
}

fn bar() {
        let y = 2;
}

fn baz() {
        let z = 3;
}";
        let expected = "\
fn foo() {
    let x = 1;
}

fn bar() {
    let y = 2;
}

fn baz() {
    let z = 3;
}";

        assert_eq!(do_indent(input), expected);
    }
}

/// Indent lines [start_line..=end_line] in `text`.
/// tab_width is used only for tab->spaces expansion when measuring indent.
pub fn indent_region_impl(text: &str, start_line: usize, end_line: usize, indent_size: usize) -> Cow<'_, str> {
    fn count_indent(line: &str) -> usize {
        line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
    }

    // Strip trailing line comments before checking what a line opens/closes with.
    // Naive find("//") is fine for indentation purposes.
    fn strip_comment(l: &str) -> &str {
        if let Some(idx) = l.find("//") { &l[..idx] } else { l }
    }

    fn is_bare_brace(l: &str) -> bool {
        strip_comment(l).trim() == "{"
    }

    // Only dot-chains are treated as continuations; anything else is
    // just wrong indentation that should snap to current_expected.
    fn is_continuation(l: &str) -> bool {
        matches!(
            l.chars().find(|c| !c.is_whitespace()),
            Some('.')
        )
    }

    let lines = text.lines().collect::<Vec<_>>(); // @Memory @Speed
    if lines.is_empty() {
        return Cow::Borrowed(text)
    }

    let end_line = end_line.min(lines.len().saturating_sub(1));

    let line_opens = |l: &str| {
        let stripped = strip_comment(l);
        stripped.trim() == "where" || matches!(
            stripped.chars().filter(|c| !c.is_whitespace()).last(),
            Some('{') | Some('(') | Some('[')
        )
    };
    let line_closes = |l: &str| matches!(
        l.chars().find(|c| !c.is_whitespace()),
        Some('}') | Some(')') | Some(']')
    );

    let first_line_is_opener = start_line == 0
        && count_indent(lines[0]) == 0
        && line_opens(lines[0]);
    let reindent_start = if first_line_is_opener { 1 } else { start_line };
    if reindent_start > end_line {
        return Cow::Borrowed(text)
    }

    let context_line = if first_line_is_opener {
        Some(lines[0])
    } else {
        (0..reindent_start).rev()
            .find(|&l| lines[l].chars().any(|c| !c.is_whitespace()))
            .map(|l| lines[l])
    };

    let mut current_expected = match context_line {
        None => 0,
        Some(cl) => {
            let ci = count_indent(cl);
            if line_opens(cl) { ci + indent_size } else { ci }
        }
    };

    //
    // Stack stores (opener_visual_indent, current_expected_to_restore).
    // When we see a closer we pop to find where to place it and what
    // current_expected becomes, decoupling visual position from bracket depth.
    //
    let mut stack: Vec<(usize, usize)> = Default::default();  // @Memory @Speed
    if first_line_is_opener {
        stack.push((0, 0));
    }

    let mut last_old   = context_line.map(count_indent).unwrap_or(0);
    let mut last_new    = last_old;
    let mut last_opened = context_line.map(|cl| line_opens(cl)).unwrap_or(false);

    let mut continuation_indent = None;

    let mut out = String::with_capacity(text.len());  // @Memory @Speed
    for (i, &line) in lines.iter().enumerate() {
        if i > 0 { out.push('\n') }

        let in_range = i >= reindent_start && i <= end_line;
        let is_blank = line.chars().all(|c| c.is_whitespace());

        if !in_range || is_blank {
            out.push_str(line);
            continue;
        }

        let old    = count_indent(line);
        let closes = line_closes(line) || is_bare_brace(line);
        let opens  = line_opens(line);

        if closes {
            continuation_indent = None;

            let new_indent = if let Some((opener_indent, restore)) = stack.pop() {
                current_expected = restore;
                opener_indent
            } else {
                current_expected = current_expected.saturating_sub(indent_size);
                current_expected
            };

            for _ in 0..new_indent { out.push(' ') }
            out.push_str(line.trim_start());

            if opens {
                stack.push((new_indent, current_expected));
                current_expected = new_indent + indent_size;
            }

            last_old    = old;
            last_new    = new_indent;
            last_opened = opens;
            continue;
        }

        //
        // Non-closer: dot-continuation preserves relative offset, everything
        // else snaps to the bracket-tracked level.
        //
        let new_indent = if is_continuation(line) && !last_opened {
            *continuation_indent.get_or_insert_with(|| {
                (last_new as i32 + old as i32 - last_old as i32).max(0) as usize
            })
        } else {
            continuation_indent = None;
            current_expected
        };

        for _ in 0..new_indent { out.push(' ') }
        out.push_str(line.trim_start());

        if opens {
            stack.push((new_indent, current_expected));
            current_expected = new_indent + indent_size;
        }

        last_old   = old;
        last_new    = new_indent;
        last_opened = opens;
    }

    if text.ends_with('\n') { out.push('\n') }
    Cow::Owned(out)
}

#[command]
pub fn indent_region(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();

    let cursor_char = view.cursor.char_index;

    let (start_char, end_char) = if let Some(anchor) = view.cursor.anchor_char_index {
        let c = view.cursor.char_index;
        if anchor <= c { (anchor, c) } else { (c, anchor) }
    } else {
        let line  = buf.text.char_to_line(view.cursor.char_index);
        let start = buf.text.line_to_char(line);
        let end   = buf.text.line_to_char((line + 1).min(buf.text.len_lines()));
        (start, end)
    };

    let start_line = buf.text.char_to_line(start_char);
    let end_line   = buf.text.char_to_line(end_char);

    let total_bytes = buf.text.len_bytes();
    buf.flatten_rope_into_scratch(0, total_bytes);

    view.cursor.unset_anchor();

    let text = &buf.scratch_space_to_flatten_rope_into;
    let reindented = indent_region_impl(
        text, start_line, end_line, 4  // :Configuration
    );
    if &reindented == text {
        return;
    }

    let cursor_line = buf.text.char_to_line(cursor_char);
    let cursor_col  = cursor_char - buf.text.line_to_char(cursor_line);

    buf.text     = reindented.into();
    buf.is_dirty = true;

    let cursor_line = cursor_line.min(buf.text.len_lines().saturating_sub(1));
    let line_start  = buf.text.line_to_char(cursor_line);
    let line_len    = buf.text.line(cursor_line).len_chars();
    view.cursor.char_index = (line_start + cursor_col).min(line_start + line_len.saturating_sub(1));
}

#[command]
pub fn save_session(cx: &mut CommandContext) {
    let path = default_session_path();
    let result = editor::session::save_session(cx.editor, &path);

    match result {
        Ok(time) => {
            let path = pretty_path(&path);
            let message = format!("Saved session in {time}us at '{path}'");
            cx.editor.messager.push(&message, cx.gpu);

            cx.editor.audioer.play_startup_sound();
        }

        Err(e) => {
            let path = pretty_path(&path);
            let message = format!("Couldn't save session at '{path}': {e}");
            cx.editor.messager.push(&message, cx.gpu);
        }
    }
}

pub fn goto_location(editor: &mut Editor, view_id: ViewId, path: &str, line: u32, col: u32) {
    open_buffer_from_path_in(editor, view_id, path.as_ref());

    let line_char_index = {
        let (_view, buf) = editor.view_and_buffer_mut(view_id);
        buf.text.line_to_char(line as _)
    };

    let line_h = editor.line_h();

    let panel_id = {
        let (view, _buf) = editor.view_and_buffer_mut(view_id);
        view.panel_id
    };

    let rect = editor.panels[panel_id].rect;

    let (view, _buf) = editor.view_and_buffer_mut(view_id);
    view.cursor.char_index = line_char_index + col as usize;
    view.scroll_to_cursor_centered(line, line_h, rect);

    view.cursor_target_col = col;
    view.cursor_target_line = line;
}

#[command]
pub fn goto_definition(cx: &mut CommandContext) {
    let (line, col, path) = {
        let (view, buf) = cx.editor.active_view_and_buffer_mut();
        let (line, col) = buf.cursor_line_col(&view.cursor);
        (line, col, buf.path.clone())
    };

    let Some(Ok(canon)) = path.map(std::fs::canonicalize) else { return };

    let lsp = cx.editor.custom_data.lsp_mut();
    lsp.goto_definition_async(canon.to_str().unwrap(), line, col);
}

#[command]
pub fn cargo_build(cx: &mut CommandContext) { // nocheckin
    let buffer = cx.editor.commander().command_buffer;
    clear_buffer(cx.editor, buffer);

    let view_id = cx.editor.views.next_key();
    cx.editor.views.push(View::new(view_id, buffer));

    let root_id  = cx.editor.root_panel;

    if matches!(&cx.editor.panel(root_id).kind, PanelKind::Leaf { .. }) {
        //
        // Ensure root is a split
        //

        cx.editor.active_panel = root_id;
        cx.editor.split_active(true, 0.5);
    }

    if let PanelKind::Split(split) = cx.editor.panel(root_id).kind {
        let unfocused_id = if cx.editor.active_panel == split.left_id {
            split.right_id
        } else {
            split.left_id
        };

        cx.editor.panel_mut(unfocused_id).kind = PanelKind::Leaf { view_id };
    }

    _ = cx.editor.commander().command_tx.send("cargo build".into());
}

#[export]
pub static COMMANDS: &[CommandEntry] = collect_commands!();

#[export]
pub fn custom_layer_init(cx: &mut CommandContext, loaded: &LoadedLib) {
    eprintln!("[Loaded commands count]: {}", loaded.commands.len());

    *cx.command_table = CommandTable::from_commands(loaded.commands);
    *cx.keymap = Keymap::default_keymap(&mut cx.command_table);

    cx.keymap.bind(KeyCombo::alt('r'), cx.command_table.intern("cargo_build")); // nocheckin
    cx.keymap.bind(KeyCombo::alt('.'), cx.command_table.intern("goto_definition")); // nocheckin
    cx.keymap.bind(KeyCombo::char_mods('\\', Mods { alt: true, ctrl: true, ..Default::default() }), cx.command_table.intern("indent_region")); // nocheckin

    setup_hooks(cx);
    editor_initialize_custom_data(cx.editor, cx.gpu);
}

fn editor_initialize_custom_data(editor: &mut Editor, gpu: &mut Gpu) {
    let was_custom_data_ever_initialized = editor.was_custom_data_ever_initialized();
    let mut did_we_apply_any_sessions = false;

    if was_custom_data_ever_initialized {
        return; // nocheckin
    }

    editor.set_custom_data(CustomData {
        lister: Lister::new(),
        commander: Commander::new(),

        lsp: LspClient::start("rust-analyzer", &[], ".")
    });
    editor.set_custom_transient_data(CustomDataTransient {});

    //
    // Try to restore session first
    //
    let session_path = &default_session_path();

    if let Ok(file)      = std::fs::File::open(session_path)
    && let Ok(mmap)      = unsafe { MmapOptions::new().populate().map(&file) }
    && let Some(session) = load_session(&mmap[..])
    {
        let time = apply_session(editor, session);

        let pretty = pretty_path(&session_path);
        let message = format!("Applied session in {time}us from '{pretty}'");
        editor.messager.push(&message, gpu);

        did_we_apply_any_sessions = true;
    }

    //
    // Open the file from argv if user provided it
    //
    open_initial_buffer(editor);

    if !did_we_apply_any_sessions {
        lister_create_fresh_buffers_views_panels(editor);
        commander_create_fresh_buffers_views_panels(editor);
    }
}

fn lister_create_fresh_buffers_views_panels(editor: &mut Editor) {
    let lister_query_buffer = editor.buffers.push(Buffer::new());

    let lister_query_view = editor.views.next_key();
    editor.views.push(View::new(lister_query_view, lister_query_buffer));

    let lister_query_panel = editor.panels.next_key();
    editor.panels.push(Panel {
        id:   lister_query_panel,
        rect: Rect::default(),
        rect_including_panel_bar: Rect::default(),
        kind: PanelKind::Leaf { view_id: lister_query_view },
    });
    editor.views[lister_query_view].panel_id = lister_query_panel;

    let lister_split_panel = editor.panels.next_key();
    editor.panels.push(Panel {
        id:   lister_split_panel,
        rect: Rect::default(),
        rect_including_panel_bar: Rect::default(),
        kind: LISTER_SPLIT_PANEL_KIND,
    });

    let lister = editor.lister_mut();
    lister.query_buffer = lister_query_buffer;
    lister.query_view   = lister_query_view;
    lister.query_panel  = lister_query_panel;
    lister.query_split  = lister_split_panel;
}

fn commander_create_fresh_buffers_views_panels(editor: &mut Editor) { // nocheckin
    let commander_command_buffer = editor.buffers.push(Buffer::new());

    let commander_command_view = editor.views.next_key();
    editor.views.push(View::new(commander_command_view, commander_command_buffer));

    let commander_command_panel = editor.panels.next_key();
    editor.panels.push(Panel {
        id:   commander_command_panel,
        rect: Rect::default(),
        rect_including_panel_bar: Rect::default(),
        kind: PanelKind::Leaf { view_id: commander_command_view },
    });
    editor.views[commander_command_view].panel_id = commander_command_panel;

    let commander_split_panel = editor.panels.next_key();
    editor.panels.push(Panel {
        id:   commander_split_panel,
        rect: Rect::default(),
        rect_including_panel_bar: Rect::default(),
        kind: PanelKind::Leaf { view_id: commander_command_view },
    });

    let commander = editor.commander_mut();
    commander.command_buffer = commander_command_buffer;
    commander.command_view   = commander_command_view;
    commander.command_panel  = commander_command_panel;

    editor.mru_register_new_buffer(commander_command_buffer);
}

fn find_panel_by_kind(editor: &Editor, root: PanelId, kind: &PanelKind) -> Option<PanelId> {
    let mut out = Default::default();
    collect_leaves(editor, root, &mut out); // @Memory

    for (id, ..) in out {
        if &editor.panels[id].kind == kind {
            return Some(id);
        }
    }

    None
}

pub fn find_matching_paren(
    buffer: &Buffer,
    start_line: u32, start_col: u32,
    scratch_paren: &mut Vec<char>,
) -> Option<(u32, u32)> {
    let _tracy = tracy::span!("find_matching_paren");

    let (open, close, dir) = {
        let ch = char_at_line_col(buffer, start_line, start_col)?;

        match ch {
            '(' => ('(', ')',  1),
            '[' => ('[', ']',  1),
            '{' => ('{', '}',  1),
            ')' => ('(', ')', -1),
            ']' => ('[', ']', -1),
            '}' => ('{', '}', -1),
            _ => return None,
        }
    };

    let mut depth = 0;

    const MAX_SCAN_LINES: u32 = 128;

    if dir > 0 {
        let mut line = start_line;

        while line < buffer.text.len_lines() as u32 && line < start_line + MAX_SCAN_LINES {
            let text = buffer.text.line(line as usize);

            let start_col_in_line = if line == start_line { start_col } else { 0 };

            for (col, ch) in text.chars().enumerate().skip(start_col_in_line as usize) {
                if ch == open {
                    depth += 1;
                } else if ch == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some((line, col as u32));
                    }
                }
            }

            line += 1;
        }
    } else {
        let mut line = start_line;
        loop {
            let text = buffer.text.line(line as usize);
            let end_col = if line == start_line { start_col + 1 } else { text.chars().count() as u32 };

            scratch_paren.clear();
            scratch_paren.extend(text.chars().take(end_col as usize));

            for col in (0..scratch_paren.len()).rev() {
                let ch = scratch_paren[col];
                if ch == close      { depth += 1; }
                else if ch == open  { depth -= 1; if depth == 0 { return Some((line, col as u32)); } }
            }

            if line == 0 { break; }
            line -= 1;

            if start_line - line > MAX_SCAN_LINES { break; }
        }
    }

    None
}

fn setup_hooks(cx: &mut CommandContext) {
    cx.editor.hooks.layout_panels = Some(|editor, win_rect| {
        {
            editor.layout_panel(editor.lister().query_panel, lister_rect(win_rect.w, win_rect.h, 1.0, editor.scale));
            editor.layout_panel(editor.lister().query_split, lister_rect(win_rect.w, win_rect.h, 1.0, editor.scale));
        }

        editor.layout_panel(editor.commander().command_panel, win_rect);
    });

    cx.editor.hooks.layout_panel = Some(|editor, id, _rect| {
        match editor.panels[id].kind.as_custom() {
            LISTER_SPLIT_CUSTOM_PANEL => {}

            _ => {}
        }
    });

    cx.editor.hooks.active_view_id = Some(|editor, custom_panel| {
        match custom_panel {
            LISTER_SPLIT_CUSTOM_PANEL => editor.lister().query_view,

            _ => unreachable!()
        }
    });

    cx.editor.hooks.collect_leaf_panels = Some(|editor, id, custom_panel, _stack| {
        match custom_panel {
            LISTER_SPLIT_CUSTOM_PANEL => {
                let l = editor.lister();
                smallvec![
                    (id, l.query_view, editor.panels[l.query_panel].rect, editor.panels[l.query_panel].rect_including_panel_bar)
                ]
            }

            _ => unreachable!()
        }
    });

    cx.editor.hooks.animate = Some(|editor, dt, should_redraw| {
        let epsilon = 0.5f32;  // Stop animating when close enough

        let lister = editor.custom_data.lister_mut();

        //
        // Lister smooth scrolling
        //
        let ds = lister.scroll - lister.scroll_anim;
        if ds.abs() > epsilon {
            lister.scroll_anim += ds * (1.0 - (-SCROLL_ANIM_RATE * dt).exp());
            *should_redraw = should_redraw.or_msg("Lister scrolling animation", &mut editor.redraw_reasons);
        } else {
            lister.scroll_anim = lister.scroll;
        }

        //
        // Lister opening animation
        //
        let target = if lister.is_open { 1.0_f32 } else { 0.0 };
        let speed = if lister.open_anim > target { 55.0 } else { 25.0 }; // @Tune
        let remaining = target - lister.open_anim;
        if remaining.abs() < 0.08 {
            lister.open_anim = target;
        } else {
            let step = (remaining * speed * dt).clamp(-0.15, 0.15);
            lister.open_anim += step;
            lister.open_anim = lister.open_anim.clamp(0.0, 1.0);
        }

        *should_redraw = should_redraw.or_if(lister.open_anim != target, "Lister opening animation", &mut editor.redraw_reasons);
    });


    cx.editor.hooks.left_mouse_clicked = Some(|cx| {
        if cx.editor.lister().is_open() {
            let (mx, my) = cx.editor.mouse_pos;

            let lister_rect = lister_rect(cx.gpu.win_w, cx.gpu.win_h, cx.editor.lister().open_anim, cx.editor.scale);
            if !lister_rect.contains(mx, my) {
                let lister = cx.editor.custom_data.lister_mut();

                //
                // Click outside lister closes it
                //
                lister.is_open = false;
                lister.is_listing_file_entries = true;

                let panel = lister.panel_before_opening_lister.take().unwrap();
                cx.editor.set_active_panel(panel);

                return (true, true);
            }

            let line_h   = cx.editor.line_h();
            let scale    = cx.editor.scale;
            let pad      = (8.0 * scale).round();
            let item_h   = cx.editor.lister().item_h;
            let input_h  = (line_h + pad).round();
            let sep      = scale.max(1.0);
            let list_y   = lister_rect.y + input_h + sep;

            let lister = cx.editor.custom_data.lister_mut();

            if my >= list_y {
                let local_y = my - list_y + lister.scroll_anim;
                let clicked = (local_y / item_h) as u32;
                if clicked < lister.filtered.len() as u32 {
                    //
                    // Click outside lister closes it
                    //

                    lister.selected_index = clicked;
                    lister.is_open = false;
                    lister.is_listing_file_entries = true;

                    let panel = cx.editor.lister_mut().panel_before_opening_lister.take().unwrap();
                    cx.editor.set_active_panel(panel);
                    cx.editor.reset_blink();

                    editor_dispatch_lister_confirm(cx);

                    return (true, true);
                } else {
                    //
                    // Clicked onto an empty item slot in Lister, just do nothing
                    //
                    return (true, true);
                }
            }
        }

        (false, false)
    });

    cx.editor.hooks.key_pressed = Some(|cx| {
        let is_active_view_query = cx.editor.active_view_id() == cx.editor.lister().query_view;

        if !is_active_view_query {
            //
            // Let core handle input
            //
            return (false, false);
        }

        let Some((event, mods)) = cx.event_and_mods else {
            return (false, false);
        };

        let lister = cx.editor.custom_data.lister_mut();

        let result = lister.lister_key(event, mods);
        let is_selected = matches!(result, ListerKeyDispatch::Selected);
        match result {
            ListerKeyDispatch::Selected | ListerKeyDispatch::Close => {
                lister.is_open = false;
                lister.is_listing_file_entries = true;

                let panel_before_opening_lister = cx.editor.lister_mut().panel_before_opening_lister.take().unwrap();
                cx.editor.set_active_panel(panel_before_opening_lister);

                if is_selected {
                    editor_dispatch_lister_confirm(cx);
                }

                (true, true)
            }

            ListerKeyDispatch::Other => {
                cx.editor.reset_blink();
                (true, true)
            }

            ListerKeyDispatch::None => (false, false)
        }
    });

    cx.editor.hooks.pre_command_execution = Some(|cx, command_atom| {
        //
        // Commit cycle if switching to a non-cycle command
        //
        if [cx.keymap.cycle_buffers_left_atom, cx.keymap.cycle_buffers_right_atom, cx.keymap.switch_buffer_atom].contains(&command_atom) {
            cx.editor.commit_buffer_cycle();
        }

        (false, false)
    });

    cx.editor.hooks.post_command_execution = Some(|cx, _command_atom| {
        if editor_is_lister_buffer_dirty(cx.editor) {
            let lister = cx.editor.custom_data.lister_mut();

            //
            // Keep lister query updated
            //

            let query = cx.editor.buffers[lister.query_buffer].text.chars();
            lister.query.clear();
            lister.query.extend(query);
            lister.scroll = 0.0;
            lister.is_query_dirty = true;
            lister.rebuild_filtered();
            lister.is_query_dirty = true; // nocheckin @DocumentThis
            lister.selected_index = if lister.set_selected_index_to_1_instead_of_0 {
                (lister.filtered.len() > 1) as u32
            } else {
                0
            };
        }

        (false, false)
    });

    cx.editor.hooks.collect_leaf_panels_init_stack = Some(|editor, _id, stack| {
        if editor.lister().is_open {
            stack.push(editor.lister().query_split);
        }
    });

    cx.editor.hooks.set_active_panel = Some(|editor, panel_id| {
        if panel_id == editor.lister().query_split {
            editor.lister_mut().panel_before_opening_lister = Some(editor.active_panel);
        }

        false
    });

    cx.editor.hooks.register_new_buffer_in_most_recently_used_list = Some(|editor, buffer_id| {
        if buffer_id == editor.lister().query_buffer {
            // Don't register lister's internal buffer in the MRU list.
            return true;
        }

        false
    });

    cx.editor.hooks.text_layout_render_settings = Some(|editor, view_id| {
        let buffer_id = editor.views[view_id].buffer_id;
        TextLayoutRenderSettings {
            //
            // Only pad left if this is a prompt buffer
            //
            should_pad_left_when_rendering: buffer_id == editor.lister().query_buffer,

            cursor_style: if buffer_id == editor.lister().query_buffer {
                CursorStyle::Stick
            } else {
                CursorStyle::Block
            },

            ..Default::default()
        }
    });

    cx.editor.hooks.inside_about_to_wait_should_request_redraw = Some(|editor| {
        let mut redraw = ShouldRequestFrameRedraw::No;

        if editor.lister().open_anim > 0.0 && !editor.lister().is_open {
            redraw = redraw.or_msg("Lister opening animation", &mut editor.redraw_reasons);
        }

        {
            redraw |= drain_pending_lsp_goto(editor);
        }

        {
            let commander_buffer = editor.commander().command_buffer;
            redraw = redraw.or_if(editor.buffers[commander_buffer].is_dirty, "Commander buffer is dirty", &mut editor.redraw_reasons);

            redraw |= drain_commander_output(editor);
        }

        redraw
    });

    cx.editor.hooks.mouse_wheel_scrolled = Some(|cx, dy| {
        let editor = &mut cx.editor;

        if editor.lister().is_open() {
            //
            // Lister scroll takes priority if open and mouse is over it
            //

            let lister_rect = lister_rect(editor.win_w, editor.win_h, editor.lister().open_anim, editor.scale);
            let (mx, my) = editor.mouse_pos;
            if lister_rect.contains(mx, my) {
                let line_h  = editor.line_h();
                let scale   = editor.scale;
                let pad     = (8.0 * scale).round();
                let input_h = (line_h + pad).round();
                let sep     = scale.max(1.0);
                let list_y  = lister_rect.y + input_h + sep;

                let lister = editor.custom_data.lister_mut();

                let max_scroll = (
                    lister.filtered.len() as f32 * lister.item_h
                        + lister.item_h * 2.0 - lister.list_h
                ).max(0.0);

                lister.scroll = (lister.scroll - dy * 2.0).clamp(0.0, max_scroll);

                //
                // Update hovered index for new scroll position
                //
                if my >= list_y {
                    let local_y = my - list_y + lister.scroll_anim;
                    let hovered = (local_y / lister.item_h) as usize;
                    let hovered_index_before = lister.hovered_index;
                    lister.hovered_index = if hovered < lister.filtered.len() {
                        if hovered_index_before != Some(hovered as u32) {
                            editor.audioer.play_lister_item_hover_sound();
                        }

                        Some(hovered as u32)
                    } else {
                        None
                    };
                }

                return (true, true);
            }
        }

        (false, false)
    });

    cx.editor.hooks.mouse_moved = Some(|cx| {
        let editor = &mut cx.editor;

        if editor.lister().is_open() {
            // @Cutnpaste from above

            let lister_rect = lister_rect(editor.win_w, editor.win_h, editor.lister().open_anim, editor.scale);
            let (mx, my) = editor.mouse_pos;
            let line_h  = editor.line_h();
            let scale   = editor.scale;
            let pad     = (8.0 * scale).round();
            let item_h  = editor.lister().item_h;
            let input_h = (line_h + pad).round();
            let sep     = scale.max(1.0);
            let list_y  = lister_rect.y + input_h + sep;

            let lister = editor.custom_data.lister_mut();

            if lister_rect.contains(mx, my) && my >= list_y {
                let local_y = my - list_y + lister.scroll_anim;
                let hovered = (local_y / item_h) as usize;
                let hovered_index_before = lister.hovered_index;
                lister.hovered_index = if hovered < lister.filtered.len() {
                    if hovered_index_before != Some(hovered as u32) {
                        editor.audioer.play_lister_item_hover_sound();
                    }

                    Some(hovered as u32)
                } else {
                    None
                };
            } else {
                lister.hovered_index = None;
            }

            return (true, false);
        }

        (false, false)
    });

    cx.editor.hooks.at_the_end_of_redraw_should_request_redraw = Some(|editor| {
        let mut redraw = ShouldRequestFrameRedraw::No;

        let lister = editor.custom_data.lister();

        redraw = redraw.or_if(lister.is_open() != lister.last_is_lister_open, "Lister opening animation", &mut editor.redraw_reasons);
        redraw = redraw.or_if(lister.open_anim > 0.0 && !lister.is_open, "Lister opening animation", &mut editor.redraw_reasons);

        {
            redraw |= peek_pending_lsp_goto(editor);
        }

        {
            let commander_buffer = editor.commander().command_buffer;

            let char_count = editor.buffers[commander_buffer].text.len_chars();

            redraw = redraw.or_if(char_count != editor.commander().last_command_buffer_character_count, "Commander buffer updated", &mut editor.redraw_reasons);
            redraw = redraw.or_if(editor.buffers[commander_buffer].is_dirty, "Commander buffer is dirty", &mut editor.redraw_reasons);

            redraw |= peek_commander_output(editor);
        }

        redraw
    });

    cx.editor.hooks.about_to_redraw_a_frame = Some(|cx, _dt| {
        let mut redraw = ShouldRequestFrameRedraw::No;

        {
            redraw |= drain_pending_lsp_goto(cx.editor);
        }

        {
            cx.editor.lister_mut().last_is_lister_open = cx.editor.lister().is_open();
        }

        {
            let commander_buffer = cx.editor.commander().command_buffer;
            let char_count = cx.editor.buffers[commander_buffer].text.len_chars();
            cx.editor.commander_mut().last_command_buffer_character_count = char_count;

            redraw = redraw.or_if(cx.editor.buffers[commander_buffer].is_dirty, "Commander buffer is dirty", &mut cx.editor.redraw_reasons);

            redraw |= drain_commander_output(cx.editor);
        }

        redraw
    });

    cx.editor.hooks.about_to_rebuild_dirty_layouts = Some(|cx| {
        let mut should_request_redraw = ShouldRequestFrameRedraw::No;

        if let Some(Some(callback)) = cx.editor.lister().items_update_frame_update_callback.last().copied() {
            should_request_redraw |= callback(cx);
        }

        should_request_redraw
    });

    cx.editor.hooks.about_to_draw_this_panel = Some(|cx, _panel, view, _rect| {
        let mut should_skip = false;

        //
        // Don't draw lister query buffer with all buffers,
        // it should be drawn after, on top of all other buffers.
        //
        should_skip |= cx.editor.views[view].buffer_id == cx.editor.lister().query_buffer;

        should_skip
    });

    cx.editor.hooks.should_view_have_panel_bar = Some(|cx, view_id| {
        ![cx.lister().query_view].contains(&view_id)
    });

    cx.editor.hooks.drew_all_leaf_panels = Some(|cx| {
        let editor = &mut cx.editor;
        let gpu = &mut cx.gpu;

        let is_cursor_visible_due_to_blinking = editor.cursor_visible();
        let active_panel = editor.active_panel;

        if editor.lister().is_open() {
            //
            // Prepare lister bg
            //

            let t1 = Instant::now();
            {
                let lister = lister_rect(gpu.win_w, gpu.win_h, editor.lister().open_anim, editor.scale);
                let t = 1.0 - (1.0 - editor.lister().open_anim).powi(4);  // Same easing as lister_rect
                render_lister_background_frosted(gpu, lister, editor.scale, t);
                render_lister_background(gpu, editor);
            }
            editor.render_us_acc += t1.elapsed().as_micros() as f32;

            //
            // Render lister query buffer
            //

            let view_id = editor.lister().query_view;
            let panel_id = editor.lister().query_panel;
            let rect = editor.panels[editor.lister().query_panel].rect_including_panel_bar;

            let show_cursor = if panel_id == active_panel {
                //
                // Only make cursor blink on the active panel.
                //
                is_cursor_visible_due_to_blinking
            } else {
                true
            };

            gpu::push_clip(gpu, rect.x, rect.y, rect.w, rect.h);
            let t1 = Instant::now();
            render_text_layout(editor, gpu, view_id, show_cursor);
            editor.render_us_acc += t1.elapsed().as_micros() as f32;
            gpu::pop_clip(gpu);

            let t1 = Instant::now();
            {
                render_lister_foreground(gpu, editor);
            }
            editor.render_us_acc += t1.elapsed().as_micros() as f32;
        }

        ShouldRequestFrameRedraw::No
    });

    cx.editor.hooks.additional_font_sizes_to_prewarm = Some(|editor| {
        smallvec![
            lister_smaller_font_size(editor.font_size())
        ]
    });

    cx.editor.hooks.format_panel_bar = Some(|editor, view_id| {
        let view = &editor.views[view_id];
        let buffer_id = view.buffer_id;
        let buffer = &editor.buffers[buffer_id];
        let (line, col) = buffer.cursor_line_col(&view.cursor);
        _ = write!(
            &mut editor.scratch_panel_bar,

            "{}  {}:{}  {}", buffer.pretty_path, line+1, col+1, editor.scale
        );
    });

    cx.editor.hooks.panel_bar_color = Some(|editor, view_id| {
        let view = &editor.views[view_id];
        let panel_color = Color::hex(0x1a1a2e);
        if Some(editor.active_panel) == view.panel_id() { // @PaletteRefactor
            (panel_color, Some(Color::hex(0x312815)))  // active: gold border
        } else {
            (panel_color.darken(0.5), None)            // inactive: no border
        }
    });

    cx.editor.hooks.drew_current_line_highlight_about_to_draw_cursor = Some(|editor, gpu, view_id, context| {
        //
        //
        // Matching paren
        //
        //

        let LayoutRenderingContext {
            cursor_col, cursor_line,
            line_h,
            first_visible_line, last_visible_line,
            min_cursor_w, origin_x, cursor_h, ..
        } = context;

        let view = &editor.views[view_id];
        let Some(layout) = &view.layout else { return };

        let buffer = &editor.buffers[view.buffer_id];

        let cols_to_check: &[_] = if *cursor_col > 0 {
            &[*cursor_col, *cursor_col - 1]
        } else {
            &[*cursor_col]
        };

        for &check_col in cols_to_check {
            let Some((matching_line, matching_col)) = find_matching_paren(
                buffer, *cursor_line, check_col, &mut editor.scratch_paren
            ) else { continue };

            for (line, col) in [(*cursor_line, check_col), (matching_line, matching_col)] {
                if line >= *first_visible_line && line < *last_visible_line {
                    if let Some(ll) = layout.line_for_buffer_line(line) {
                        let x = layout.x_for_col(*origin_x, col, ll);
                        let w = layout.glyph_width_at_col(col, *min_cursor_w, ll);
                        let y = context.line_y(line);
                        gpu::draw_rect(gpu, x, y + cursor_h, w, line_h + cursor_h, palette().paren_match);
                    }
                }
            }
        }
    });

    cx.editor.hooks.drew_cursor_about_to_draw_text = Some(|editor, gpu, view_id, context| {
        //
        //
        // Matching paren
        //
        //

        let LayoutRenderingContext { rect, origin_x, .. } = context;

        let view = &editor.views[view_id];
        let Some(layout) = &view.layout else { return };
        let buffer = &editor.buffers[view.buffer_id];

        //
        //
        // Deletion animations
        //
        //
        for anim in &buffer.currently_animated_deletions {
            let alpha = ((1.0 - anim.t) * 160.0) as u8;  // Linear fade
            if alpha == 0 { continue }

            let color = Color::rgba(
                palette().delete_highlight.r,
                palette().delete_highlight.g,
                palette().delete_highlight.b,
                alpha
            );

            for line in anim.start_line..=anim.end_line {
                if line == anim.end_line && anim.end_col == 0         { continue }
                let Some(ll) = layout.line_for_buffer_line(line) else { continue };

                let full_x0 = if line == anim.start_line { layout.x_for_col(*origin_x, anim.start_col, ll) } else { rect.x };
                let full_x1 = if line == anim.end_line   { layout.x_for_col(*origin_x, anim.end_col,   ll) } else { rect.x + rect.w };
                let y = layout.rect.y + line as f32 * layout.line_h - view.scroll_anim.round();
                if full_x1 <= full_x0 { continue; }

                gpu::draw_rect(gpu, full_x0, y, full_x1 - full_x0, layout.line_h, color);

                if line == anim.start_line
                    && anim.start_line != anim.end_line
                    && anim.start_col > 0
                    && full_x0 > rect.x + 1.0
                {
                    gpu::draw_rect(gpu, rect.x, y, full_x0 - rect.x, layout.line_h, color);
                }
            }
        }
    });

    use editor::session::*;

    cx.editor.hooks.session_save_chunks.get_or_insert_default().push(|editor, view_index, _buf_index| {
        let lister = editor.lister();
        let mut data = Vec::with_capacity(8);
        // Store the serial view indices for query_view and any result view
        if let Some(&vi) = view_index.get(&lister.query_view) {
            write_u32(&mut data, vi);
        } else {
            return None;
        }
        Some((LISTER_CUSTOM_CHUNK_ID, data))
    });

    cx.editor.hooks.session_restore_chunks.get_or_insert_default().push(|editor, chunk_id, data, view_ids, _buf_ids| {
        if chunk_id != LISTER_CUSTOM_CHUNK_ID { return; }
        let mut r = Reader::new(data);
        if let Some(vi) = r.u32() {
            if let Some(&real_view_id) = view_ids.get(vi as usize) {
                let lister = editor.custom_data.lister_mut();
                lister.query_view   = real_view_id;
                lister.query_buffer = editor.views[real_view_id].buffer_id;
                lister.query_panel = if let Some(existing) = editor.views[lister.query_view].panel_id() {
                    existing
                } else {
                    let panel_id = editor.panels.next_key();
                    editor.panels.push(Panel {
                        id:   panel_id,
                        rect: Rect::default(),
                        rect_including_panel_bar: Rect::default(),
                        kind: PanelKind::Leaf { view_id: lister.query_view },
                    });
                    editor.views[lister.query_view].panel_id = panel_id;
                    panel_id
                };
                editor.lister_mut().query_split = find_panel_by_kind(editor, editor.root_panel, &LISTER_SPLIT_PANEL_KIND)
                    .unwrap_or_else(|| {
                        let panel_id = editor.panels.next_key();
                        editor.panels.push(Panel {
                            id:   panel_id,
                            rect: Rect::default(),
                            rect_including_panel_bar: Rect::default(),
                            kind: LISTER_SPLIT_PANEL_KIND,
                        });
                        panel_id
                    });
            }
        }
    });

    cx.editor.hooks.session_save_chunks.get_or_insert_default().push(|editor, view_index, _| {
        let commander = editor.commander();
        let mut data = Vec::with_capacity(8);
        if let Some(&vi) = view_index.get(&commander.command_view) {
            write_u32(&mut data, vi);
            Some((COMMANDER_CUSTOM_CHUNK_ID, data))
        } else {
            None
        }
    });

    cx.editor.hooks.session_restore_chunks.get_or_insert_default().push(|editor, chunk_id, data, view_ids, _| {
        if chunk_id != COMMANDER_CUSTOM_CHUNK_ID { return; }

        let mut r = Reader::new(data);
        if let Some(vi) = r.u32() {
            if let Some(&real_view_id) = view_ids.get(vi as usize) {
                let commander = editor.custom_data.commander_mut();
                commander.command_view   = real_view_id;
                commander.command_buffer = editor.views[real_view_id].buffer_id;
                commander.command_panel = if let Some(existing) = editor.views[commander.command_view].panel_id() {
                    existing
                } else {
                    let panel_id = editor.panels.next_key();
                    editor.panels.push(Panel {
                        id:   panel_id,
                        rect: Rect::default(),
                        rect_including_panel_bar: Rect::default(),
                        kind: PanelKind::Leaf { view_id: commander.command_view },
                    });
                    editor.views[commander.command_view].panel_id = panel_id;
                    panel_id
                };
            }
        }
    });

    cx.editor.hooks.opened_file = Some(|editor, buffer_id| {  // :BufferScratch nocheckin
        let buffer = &mut editor.buffers[buffer_id];
        let Some(path) = buffer.path.clone() else { return };
        let end = buffer.text.len_bytes();
        buffer.flatten_rope_into_scratch(0, end);  // :BufferScratch
        editor.custom_data.lsp_mut().did_open_buf(path.to_str().unwrap(), &buffer.scratch_space_to_flatten_rope_into);
    });

    cx.editor.hooks.modified_file = Some(|editor, buffer_id| {  // :BufferScratch
        let buffer = &mut editor.buffers[buffer_id];
        let Some(path) = buffer.path.clone() else { return };
        let end = buffer.text.len_bytes();
        buffer.flatten_rope_into_scratch(0, end);  // :BufferScratch
        editor.custom_data.lsp_mut().did_change_buf(path.to_str().unwrap(), &buffer.scratch_space_to_flatten_rope_into, 1); // nocheckin
    });

    cx.editor.hooks.exiting = Some(|editor| editor.lsp_mut().shutdown_blocking());
}

pub fn editor_dispatch_lister_confirm(cx: &mut CommandContext) {
    let lister = cx.editor.custom_data.lister_mut();

    let index = lister.selected_index;
    let Some(index) = lister.filtered.get(index as usize) else { return };
    let item_data = lister.items[*index as usize].data;

    let Some(on_confirm) = lister.on_confirm.pop()        else { return };

    lister.pending_datas.push(item_data);
    _ = lister.items_update_frame_update_callback.pop();
    lister.set_selected_index_to_1_instead_of_0 = false;

    on_confirm(cx, item_data);
}

// Frosted glass approximation - layered semi-transparent rects
// with slight size variations to fake depth
pub fn render_lister_background_frosted(gpu: &mut Gpu, lister: Rect, scale: f32, open_anim: f32) {
    let a = |base: u8| -> u8 { ((base as f32) * open_anim) as u8 };

    // Base dark fill
    gpu::draw_rect(gpu, lister.x, lister.y, lister.w, lister.h,
        Color::rgba(12, 9, 4, a(200)));

    // Warm tint layer
    gpu::draw_rect(gpu, lister.x, lister.y, lister.w, lister.h,
        Color::rgba(40, 25, 8, a(40)));

    // Slightly inset lighter layer - gives illusion of depth/glass
    let i = scale * 1.0;
    gpu::draw_rect(gpu, lister.x + i, lister.y + i, lister.w - i*2.0, lister.h - i*2.0,
        Color::rgba(255, 200, 120, a(12)));

    // Top edge highlight - light catches the glass rim
    gpu::draw_rect(gpu, lister.x, lister.y, lister.w, scale,
        Color::rgba(255, 210, 140, a(60)));

    // Left edge highlight
    gpu::draw_rect(gpu, lister.x, lister.y, scale, lister.h,
        Color::rgba(255, 210, 140, a(30)));

    // Bottom edge shadow
    gpu::draw_rect(gpu, lister.x, lister.y + lister.h - scale, lister.w, scale,
        Color::rgba(0, 0, 0, a(80)));
}

pub fn render_lister_background(gpu: &mut Gpu, editor: &Editor) {
    if editor.active_panel != editor.lister().query_split { return; }

    if !editor.lister().renderer_is_open() { return; }

    // Dim the whole screen
    gpu::draw_rect(gpu, 0.0, 0.0, gpu.win_w, gpu.win_h, Color::rgba(0, 0, 0, 100));
}

const fn lister_smaller_font_size(scaled_font_size: f32) -> f32 {
    scaled_font_size * 0.80
}

pub fn render_lister_foreground(gpu: &mut Gpu, editor: &mut Editor) {
    debug_assert_eq!(
        gpu.clip_depth, 0,
        "render_lister_foreground entered with unbalanced clip depth {}, expected 0",
        gpu.clip_depth
    );

    let scale     = editor.scale;
    let font_size = editor.font_size();
    let line_h    = editor.line_h();

    let smaller_font_size = lister_smaller_font_size(font_size);

    let lister = editor.custom_data.lister_mut();

    if !lister.renderer_is_open() { return; }

    let open_anim = lister.open_anim;
    let a = |base: u8| -> u8 { ((base as f32) * open_anim) as u8 };

    let lister_rect = lister_rect(gpu.win_w, gpu.win_h, lister.open_anim, editor.scale);
    let Rect { x: px, y: py, w: pw, h: ph } = lister_rect;

    let pad     = (8.0 * scale).round();
    let item_h  = (line_h + pad).round();
    let input_h = (line_h + pad).round();
    let sep     = scale.max(1.0);
    let list_y  = py + input_h + sep;
    let list_h  = ph - input_h - sep;

    let is_mouse_cursor_hidden = editor.is_cursor_visible;

    lister.item_h = item_h;
    lister.list_h = list_h;

    // Outer border
    gpu::draw_rect_outline(gpu, px, py, pw, ph, sep,
                           Color::rgba(180, 140, 80, a(200)));

    // Inner border
    gpu::draw_rect_outline(gpu, px + sep, py + sep, pw - sep*2.0, ph - sep*2.0, sep,
                           Color::rgba(80, 60, 30, a(80)));

    // Separator
    gpu::draw_rect(gpu, px, py + input_h, pw, sep,
                   Color::rgba(180, 140, 80, a(160)));

    // Item count
    lister.scratch_str.clear();
    _ = write!(&mut lister.scratch_str, "{} results", lister.filtered.len());
    let count_w = gpu::measure_str(gpu, &lister.scratch_str, smaller_font_size);
    gpu::draw_text(gpu, &lister.scratch_str,
                   px + pw - pad - count_w,
                   py + input_h * 0.44 + line_h * 0.35,
                   smaller_font_size,
                   Color::rgba(160, 120, 60, a(150)));

    // Items
    let first   = (lister.scroll_anim / item_h) as usize;
    let visible = (list_h / item_h) as usize + 2;
    let frac    = lister.scroll_anim % item_h;

    gpu::push_clip(gpu, px, list_y, pw, list_h);

    for slot in 0..visible {
        let index      = first + slot;
        let Some(&item_index) = lister.filtered.get(index)            else { break };
        let Some(item)        = lister.items.get(item_index as usize) else { break };

        let iy       = list_y + slot as f32 * item_h - frac;

        let is_selected = index == lister.selected_index as usize;
        let is_hovered  = lister.hovered_index == Some(index as u32);

        if iy > list_y + list_h { break; }

        // Alternating row tint  very subtle, just enough to separate rows
        if index % 2 == 0 {
            gpu::draw_rect(gpu, px, iy, pw, item_h, Color::rgba(255, 200, 100, a(8)));
        }

        if is_mouse_cursor_hidden && is_hovered && !is_selected {
            gpu::draw_rect(gpu, px + sep*2.0, iy, pw - sep*4.0, item_h, Color::rgba(60, 45, 15, a(120)));
        }

        if is_selected {
            gpu::draw_rect(gpu, px + sep*2.0, iy, pw - sep*4.0, item_h, Color::rgba(80, 55, 20, a(180)));
            gpu::draw_rect(gpu, px + sep, iy, sep * 3.0, item_h, Color::rgba(195, 169, 131, a(255)));
            gpu::draw_rect(gpu, px, iy, pw, sep, Color::rgba(180, 140, 80, a(60)));
            gpu::draw_rect(gpu, px, iy + item_h - sep, pw, sep, Color::rgba(180, 140, 80, a(60)));
        }

        let label_x = px + pad + sep * 5.0;
        let label_y = iy + item_h * 0.5 + line_h * 0.35;

        gpu::draw_text(
            gpu, &item.label, label_x, label_y, font_size,
            if is_selected { Color::rgba(240, 208, 144, a(255)) } else { Color::rgba(200, 190, 165, a(220)) }
        );

        if !item.sublabel.is_empty() {
            let sub_w = gpu::measure_str(gpu, &item.sublabel, smaller_font_size);

            gpu::draw_text(
                gpu, &item.sublabel,
                px + pw - pad - sub_w,
                label_y,
                smaller_font_size,
                if is_selected { Color::rgba(180, 140, 80, a(200)) }
                else           { Color::rgba(120, 100, 60, a(120)) }
            );
        }
    }

    gpu::pop_clip(gpu);

    //
    // Scrollbar
    //
    let total_items = lister.filtered.len();
    if total_items > 0 {
        let total_h = total_items as f32 * item_h + item_h;
        let bar_h    = (list_h * (list_h / total_h).min(1.0)).max(sep * 4.0);
        let bar_frac = (lister.scroll_anim / (total_h - list_h).max(1.0)).clamp(0.0, 1.0);
        let bar_y    = list_y + bar_frac * (list_h - bar_h);

        // Scrollbar track - very faint
        gpu::draw_rect(gpu, px + pw - sep*3.0 - sep, list_y, sep*3.0, list_h, Color::rgba(255, 200, 100, a(15)));

        // Scrollbar thumb
        gpu::draw_rect(gpu, px + pw - sep*3.0 - sep, bar_y, sep*3.0, bar_h, Color::rgba(180, 140, 80, a(140)));
    }
}

pub fn lister_rect(win_w: f32, win_h: f32, open_anim: f32, scale: f32) -> Rect {
    let t = 1.0 - (1.0 - open_anim).powi(4);

    let panel_w = (win_w * 0.45).clamp(320.0, 720.0);
    let panel_h = (win_h * 0.65).clamp(200.0, 600.0);

    let cx = win_w * 0.50;
    let cy = (win_h - panel_h) * 0.40 + panel_h * 0.50;

    let min_w = 60.0 * scale;
    let min_h = 40.0 * scale;

    let w = (panel_w * t).max(min_w);
    let h = (panel_h * t).max(min_h);

    Rect {
        x: cx - w * 0.5,
        y: cy - h * 0.5,
        w,
        h,
    }
}

#[inline]
pub fn editor_is_lister_buffer_dirty(editor: &Editor) -> bool {
    editor.buffers[editor.lister().query_buffer].is_dirty
}

#[inline]
pub fn editor_open_lister(editor: &mut Editor, items: Vec<ListerItem>, on_confirm: ListerSelectFn) {
    editor_open_lister_impl(editor, items, on_confirm, None)
}

#[inline]
pub fn editor_open_lister_with_frame_callback(editor: &mut Editor, items: Vec<ListerItem>, on_confirm: ListerSelectFn, frame_callback: ListerFrameUpdateCallback) {
    editor_open_lister_impl(editor, items, on_confirm, Some(frame_callback))
}

#[inline]
pub fn editor_open_lister_impl(editor: &mut Editor, items: Vec<ListerItem>, on_confirm: ListerSelectFn, frame_callback: Option<ListerFrameUpdateCallback>) {
    clear_buffer(editor, editor.lister().query_buffer);

    editor.set_active_panel(editor.lister().query_split);

    let lister = editor.custom_data.lister_mut();

    lister.items_update_frame_update_callback.push(frame_callback);
    lister.on_confirm.push(on_confirm);

    lister.query.clear();
    lister.filtered.clear();
    lister.is_query_dirty   = true;
    editor.canonicalized_last_scanned_directory = SmallString::new();
    lister.scroll          = 0.0;
    lister.scroll_anim     = 0.0;
    lister.is_open         = true;
    lister.items           = items;
    lister.rebuild_filtered();
    lister.selected_index  = if lister.set_selected_index_to_1_instead_of_0 {
        (lister.filtered.len() > 1) as u32
    } else {
        0
    };
}

pub enum ListerKeyDispatch {
    Selected,
    Close,
    Other,
    None
}

#[derive(Debug, Clone)]
pub struct ListerItem {
    pub label:    SmallString<[u8; 32]>,
    pub sublabel: SmallString<[u8; 64]>,
    pub data:     u64,
}

pub type ListerFrameUpdateCallback = fn(&mut CommandContext) -> ShouldRequestFrameRedraw;
pub type ListerSelectFn = fn(&mut CommandContext, u64);

#[derive(Clone)]
pub struct Lister {
    pub is_open:        bool,
    pub is_listing_file_entries: bool,
    pub is_query_dirty: bool,

    pub last_seen_cached_dir_generation: u64, // u64::MAX if we didnt see any generations

    pub query:         SmallString<[u8; 128]>,

    pub query_buffer: BufferId, // :CustomData
    pub query_view:   ViewId,   // :CustomData
    pub query_panel:  PanelId,  // :CustomData @Redundant?
    pub query_split:  PanelId,  // :CustomData

    pub selected_index: u32,
    pub  hovered_index: Option<u32>,

    pub panel_before_opening_lister: Option<PanelId>, // :CustomData
    pub last_is_lister_open: bool, // :CustomData

    pub set_selected_index_to_1_instead_of_0: bool,

    pub on_confirm:    Vec<ListerSelectFn>,
    pub pending_datas: Vec<u64>,
    pub items_update_frame_update_callback: Vec<Option<ListerFrameUpdateCallback>>,

    pub scroll:        f32,
    pub scroll_anim:   f32,
    pub open_anim:     f32,  // 0.0 = closed, 1.0 = fully open

    pub item_h:        f32,
    pub list_h:        f32,

    // Storage - cleared and refilled when lister opens
    pub items:           Vec<ListerItem>,

    // Scratch - rebuilt only when query changes (dirty flag)
    pub filtered:        Vec<u32>,        // Indices into items

    pub scratch_str:     String,          // Reused for formatting
    pub scoring_scratch: Vec<(u32, u32)>, // Reused rebuild_filtered
}

impl Lister {
    pub fn new() -> Self {
        Self {
            is_open: false,
            open_anim: 0.0,
            query_panel: PanelId::reserved_value(),
            query_buffer: BufferId::reserved_value(),
            query_view: ViewId::reserved_value(),
            query_split: PanelId::reserved_value(),
            query: SmallString::new(),
            filtered: Default::default(),
            panel_before_opening_lister: None,
            last_is_lister_open: false,
            last_seen_cached_dir_generation: u64::MAX,
            items_update_frame_update_callback: Default::default(),
            hovered_index: None,
            is_listing_file_entries: false,
            items: Default::default(),
            on_confirm: Default::default(),
            pending_datas: Default::default(),
            is_query_dirty: false,
            scratch_str: String::with_capacity(512),
            scroll: 0.0,
            set_selected_index_to_1_instead_of_0: false,
            scroll_anim: 0.0,
            selected_index: 0,
            scoring_scratch: Vec::with_capacity(256),
            list_h: 0.0,
            item_h: 0.0
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    pub fn renderer_is_open(&self) -> bool {
        self.open_anim > 0.10
    }

    pub fn rebuild_filtered(&mut self) {
        if !self.is_query_dirty { return; }

        self.filtered.clear();

        let filter_str = if self.is_listing_file_entries {
            // For filtering entries, only use the filename part of the query
            // (the part after the last /)

            let after_last_slash = self.query.as_str()
                .rsplit(MAIN_SEPARATOR)
                .next()
                .unwrap_or(self.query.as_str());

            // If query ends with / or the part before the last slash is a dir,
            // show everything - user navigated into a directory
            if after_last_slash.is_empty() {
                ""
            } else {
                after_last_slash
            }
        } else {
            &self.query
        };

        if filter_str.is_empty() {
            self.filtered.extend(0..self.items.len() as u32);
            self.is_query_dirty = false;
            return;
        }

        //
        // Filter by subsequence,
        // then score by edit distance on matched items only for sorting
        //
        self.scoring_scratch.clear();
        self.scoring_scratch.extend(self.items.iter()
            .enumerate()
            .filter(|(_, item)| Self::fuzzy_match(&item.label, filter_str))
            .map(|(i, item)| {
                let score = rustc_edit_distance::edit_distance_with_substrings(
                    filter_str,
                    &item.label,
                    // Use a large limit since we already know it's a subsequence match
                    filter_str.len() * 3,
                ).unwrap_or(usize::MAX);

                (i as u32, score as u32)
            }));

        self.scoring_scratch.sort_unstable_by_key(|&(_, score)| score);
        self.filtered.extend(self.scoring_scratch.iter().map(|&(i, _)| i));
        self.is_query_dirty = false;
    }

    fn fuzzy_match(text: &str, query: &str) -> bool {
        let mut qchars = query.chars();
        let mut qc = match qchars.next() { Some(c) => c, None => return true };
        for tc in text.chars() {
            if tc.to_ascii_lowercase() == qc.to_ascii_lowercase() {
                qc = match qchars.next() { Some(c) => c, None => return true };
            }
        }

        false
    }

    pub fn lister_key(&mut self, event: &KeyEvent, mods: Mods) -> ListerKeyDispatch {
        if !self.is_open { return ListerKeyDispatch::None; }

        let Mods { ctrl, alt, shift: _ } = mods;

        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                ListerKeyDispatch::Close
            }

            Key::Named(NamedKey::Enter) => {
                ListerKeyDispatch::Selected
            }

            Key::Named(NamedKey::ArrowDown) => {
                if self.selected_index + 1 < self.filtered.len() as u32 {
                    self.selected_index += 1;
                    self.scroll_to_selected();
                }
                ListerKeyDispatch::Other
            }

            Key::Named(NamedKey::ArrowUp) => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                    self.scroll_to_selected();
                }
                ListerKeyDispatch::Other
            }

            Key::Character(s) if ctrl => match s.chars().next() {
                Some('n') => {
                    if self.selected_index + 1 < self.filtered.len() as u32 {
                        self.selected_index += 1;
                        self.scroll_to_selected();
                    }

                    ListerKeyDispatch::Other
                }

                Some('p') => {
                    if self.selected_index > 0 {
                        self.selected_index -= 1;
                        self.scroll_to_selected();
                    }

                    ListerKeyDispatch::Other
                }

                Some('v') => {
                    let page = (self.list_h / self.item_h) as u32;
                    self.selected_index = (self.selected_index + page).min(self.filtered.len().saturating_sub(1) as u32);
                    self.scroll_to_selected();
                    ListerKeyDispatch::Other
                }

                Some('g') => ListerKeyDispatch::Close,

                _ => ListerKeyDispatch::None
            }

            Key::Character(s) if alt => match s.chars().next() {
                Some('v') => {
                    let page = (self.list_h / self.item_h) as u32;
                    self.selected_index = self.selected_index.saturating_sub(page);
                    self.scroll_to_selected();
                    ListerKeyDispatch::Other
                }
                _ => ListerKeyDispatch::None
            }

            _ => ListerKeyDispatch::None
        }
    }

    fn scroll_to_selected(&mut self) {
        let top    = self.selected_index as f32 * self.item_h;
        let bottom = top + self.item_h;

        // Only scroll when item is fully outside the visible area
        if top < self.scroll {
            self.scroll = top;
        }
        if bottom > self.scroll + self.list_h {
            self.scroll = bottom - self.list_h;
        }
    }
}

#[derive(Clone)]
pub struct Commander {
    pub command_view: ViewId,
    pub command_buffer: BufferId,
    pub command_panel: PanelId,

    pub command_tx: Sender<Box<str>>,
    pub output_rx: Receiver<Box<str>>,
    pub current_child: Arc<Mutex<Option<Child>>>,

    pub last_command_buffer_character_count: usize,
}

impl Commander {
    pub fn new() -> Self {
        let (command_tx, command_rx) = crossbeam_channel::unbounded::<Box<str>>();
        let (output_tx, output_rx) = crossbeam_channel::unbounded::<Box<str>>();

        let current_child: Arc<Mutex<Option<Child>>> = Default::default();
        let current_child2 = Arc::clone(&current_child);

        std::thread::spawn(move || {
            let current_child = current_child2;

            while let Ok(command) = command_rx.recv() {
                //
                // Kill previous child if still running
                //
                if let Ok(mut guard) = current_child.lock() {
                    if let Some(child) = guard.as_mut() {
                        child.kill().ok();
                    }
                    *guard = None;
                }

                let (c1, c2) = {
                    #[cfg(unix)]    { ("sh",  "-c") }
                    #[cfg(windows)] { ("cmd", "/C") }
                };

                let mut child = match Command::new(c1)
                    .arg(c2)
                    .arg(format!("{command} 2>&1"))
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(c) => c,
                    Err(e) => {
                        _ = output_tx.send(format!("error: {e}\n").into_boxed_str());
                        continue;
                    }
                };

                let stdout = child.stdout.take();
                *current_child.lock().unwrap() = Some(child);

                //
                // Read thread
                //
                if let Some(mut stdout) = stdout {
                    let output_tx = output_tx.clone();
                    let current_child = current_child.clone();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 4096];
                        loop {
                            match stdout.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    let s = String::from_utf8_lossy(&buf[..n]).into_owned();
                                    let _ = output_tx.send(s.into_boxed_str());
                                }
                                Err(_) => break,
                            }
                        }

                        //
                        // Reap the child
                        //
                        if let Ok(mut guard) = current_child.lock() {
                            if let Some(mut child) = guard.take() {
                                child.wait().ok();
                            }
                        }
                    });
                }
            }
        });

        Self {
            command_buffer: BufferId::reserved_value(),
            command_view: ViewId::reserved_value(),
            command_panel: PanelId::reserved_value(),
            command_tx,
            last_command_buffer_character_count: 0,
            output_rx,
            current_child,
        }
    }

    pub fn cancel(&self) {
        if let Ok(mut guard) = self.current_child.lock() {
            if let Some(child) = guard.as_mut() {
                child.kill().ok();
            }
        }
    }
}

pub fn drain_commander_output(editor: &mut Editor) -> ShouldRequestFrameRedraw {
    let mut redraw = ShouldRequestFrameRedraw::No;

    let commander_buffer = editor.commander().command_buffer;

    while let Ok(chunk) = editor.commander().output_rx.try_recv() {
        redraw = redraw.or_msg("Commander update", &mut editor.redraw_reasons);

        let buf = &mut editor.buffers[commander_buffer];
        buf.is_dirty = true;
        let end = buf.text.len_chars();
        buf.text.insert(end, &chunk);
    }

    redraw
}

pub fn peek_commander_output(editor: &mut Editor) -> ShouldRequestFrameRedraw {
    let mut redraw = ShouldRequestFrameRedraw::No;

    if !editor.commander().output_rx.is_empty() {
        redraw = redraw.or_msg("Commander update", &mut editor.redraw_reasons);
    }

    redraw
}

pub fn drain_pending_lsp_goto(editor: &mut Editor) -> ShouldRequestFrameRedraw {
    let mut redraw = ShouldRequestFrameRedraw::No;

    if let Some(polled_jump_loc) = editor.custom_data.lsp_mut().poll_goto_definition() {
        redraw = redraw.or_msg("Location jump", &mut editor.redraw_reasons);

        goto_location(
            editor, editor.active_view_id(),
            &polled_jump_loc.path, polled_jump_loc.line, polled_jump_loc.col
        );
    }

    redraw
}

pub fn peek_pending_lsp_goto(editor: &mut Editor) -> ShouldRequestFrameRedraw {
    let mut redraw = ShouldRequestFrameRedraw::No;

    if editor.lsp().goto_definition_is_some() {
        redraw = redraw.or_msg("Location jump update", &mut editor.redraw_reasons);
    }

    redraw
}
