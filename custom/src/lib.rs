// @Note: See @Note at the top of core/src/main.rs
//
// #[cfg(feature = "dhat")]
// #[global_allocator]
// static ALLOC: dhat::Alloc = dhat::Alloc;
//
// #[cfg(feature = "mimalloc")]
// #[global_allocator]
// static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use editor::buffer::Buffer;
use editor::color::Color;
use editor::director::{EntryKind, ScanState};
use editor::gpu::Gpu;
use editor::session::{default_session_path, pretty_path};
use editor::command::{CommandContext, CommandEntry, CommandTable, Keymap, LoadedLib, Mods};
use editor::*;

use editor_macros::{collect_commands, command, export};

use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::time::Instant;
use std::fmt::Write;

use winit::event::KeyEvent;
use winit::keyboard::{Key, NamedKey};
use smallvec::smallvec;
use smallstr::SmallString;
use cranelift_entity::EntityRef;
use cranelift_entity::packed_option::ReservedValue;

pub const LISTER_SPLIT_CUSTOM_PANEL: CustomPanel = CustomPanel { extra0: 420, extra1: 67, extra2: 69 };
pub const LISTER_SPLIT_PANEL_KIND: PanelKind = PanelKind::Custom(LISTER_SPLIT_CUSTOM_PANEL);

// @Cleanup: Move this out of here
macro_rules! custom_data {
    (
        $(#[$struct_meta:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$field_meta:meta])*
                $field_vis:vis $field:ident : $ty:ty
            ),* $(,)?
        }
    ) => {
        #[allow(dead_code)]
        #[allow(unused)]
        $(#[$struct_meta])*
        $vis struct $name {
            $(
                #[allow(dead_code)]
                #[allow(unused)]
                $(#[$field_meta])*
                $field_vis $field: $ty,
            )*
        }

        #[allow(dead_code)]
        #[allow(unused)]
        $vis trait CustomDataAccess {
            $(
                #[allow(dead_code)]
                #[allow(unused)]
                fn $field(&self) -> &$ty;

                paste::paste! {
                    #[allow(dead_code)]
                    #[allow(unused)]
                    fn [<$field _mut>](&mut self) -> &mut $ty;
                }
            )*
        }

        #[allow(dead_code)]
        #[allow(unused)]
        #[allow(clippy::all)]
        #[allow(clippy::pedantic)]
        impl CustomDataAccess for editor::EditorCustomData {
            $(
                #[inline]
                #[allow(dead_code)]
                #[allow(unused)]
                #[cfg_attr(debug_assertions, track_caller)]
                fn $field(&self) -> &$ty {
                    &self.get::<$name>().$field
                }

                paste::paste! {
                    #[inline]
                    #[allow(dead_code)]
                    #[allow(unused)]
                    #[cfg_attr(debug_assertions, track_caller)]
                    fn [<$field _mut>](&mut self) -> &mut $ty {
                        &mut self.get_mut::<$name>().$field
                    }
                }
            )*
        }
    };
}

custom_data! {
    struct CustomData {
        lister: Lister,
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
    gpu::print_atlas_usage(cx.gpu);
}

#[command]
pub fn scale_up(cx: &mut CommandContext) {
    rescale(cx.editor, cx.editor.scale + SCALE_STEP);
    gpu::print_atlas_usage(cx.gpu);
}

#[command]
pub fn scale_reset(cx: &mut CommandContext) {
    rescale(cx.editor, 1.0);
}

#[command]
pub fn open_new_buffer(cx: &mut CommandContext) {
    let buffer_id = cx.editor.push_buffer(Buffer::new());
    let view_id = ViewId::new(cx.editor.views.len());
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

    buf.insert_literal(&clipboard, &mut view.cursor);
    buf.append_last_insertion_to_currently_animated_pastes();
    view.cursor.unset_anchor();
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
pub fn switch_buffer(cx: &mut CommandContext) {
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

            {
                let path: &Path = path.as_str().as_ref();
                if let Ok(canon) = path.canonicalize()
                && let Some(&old_buffer_id) = cx.editor.canonicalized_path_to_buffer_id.get(canon.as_path())
                {
                    cx.editor.active_view_mut().switch_buffer(old_buffer_id);
                    cx.editor.mru_focus(old_buffer_id); // @Refactor
                    return;
                }
            }

            let Ok(new_buffer) = Buffer::from_file(path.as_str().as_ref()) else {
                return
            };

            let new_buffer_id = cx.editor.push_buffer(new_buffer);
            cx.editor.active_view_mut().switch_buffer(new_buffer_id);
            cx.editor.mru_focus(new_buffer_id); // @Refactor
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

#[export]
pub static COMMANDS: &[CommandEntry] = collect_commands!();

#[export]
pub fn custom_layer_init(cx: &mut CommandContext, loaded: &LoadedLib) {
    eprintln!("[Loaded commands count]: {}", loaded.commands.len());

    *cx.command_table = CommandTable::from_commands(loaded.commands);
    *cx.keymap = Keymap::default_keymap(&mut cx.command_table);

    editor_init_custom_data(cx.editor);

    setup_hooks(cx);
}

fn setup_hooks(cx: &mut CommandContext) {
    cx.editor.hooks.layout_panels = Some(|editor, win_rect| {
        editor.layout_panel(editor.lister().query_panel, lister_rect(win_rect.w, win_rect.h, 1.0, editor.scale));
        editor.layout_panel(editor.lister().split_panel, lister_rect(win_rect.w, win_rect.h, 1.0, editor.scale));
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
            LISTER_SPLIT_CUSTOM_PANEL => smallvec![
                (id, editor.lister().query_view, editor.panels[editor.lister().query_panel].rect, editor.panels[editor.lister().query_panel].rect_including_panel_bar)
            ],

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
            stack.push(editor.lister().split_panel);
        }
    });

    cx.editor.hooks.set_active_panel = Some(|editor, panel_id| {
        if panel_id == editor.lister().split_panel {
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
        if editor.lister().open_anim > 0.0 && !editor.lister().is_open {
            return ShouldRequestFrameRedraw::yes("Lister opening animation", &mut editor.redraw_reasons);
        }

        ShouldRequestFrameRedraw::No
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

    cx.editor.hooks.inside_redraw_should_request_redraw = Some(|editor| {
        let mut redraw = ShouldRequestFrameRedraw::No;

        let lister = editor.custom_data.lister();

        redraw = redraw.or_if(lister.is_open() != lister.last_is_lister_open, "Lister opening animation", &mut editor.redraw_reasons);
        redraw = redraw.or_if(lister.open_anim > 0.0 && !lister.is_open, "Lister opening animation", &mut editor.redraw_reasons);

        redraw
    });

    cx.editor.hooks.about_to_redraw_a_frame = Some(|cx, _dt| {
        cx.editor.lister_mut().last_is_lister_open = cx.editor.lister().is_open();

        ShouldRequestFrameRedraw::No
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
        cx.lister().query_view != view_id
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

        if let Some((m_line, m_col)) = find_matching_paren(buffer, *cursor_line, *cursor_col, &mut editor.scratch_paren) {
            let _tracy = tracy::span!("render_text_layout::matching_paren_render");

            // Cursor paren
            if cursor_line >= first_visible_line && cursor_line < last_visible_line {
                if let Some(ll) = layout.line_for_buffer_line(*cursor_line) {
                    let x = layout.x_for_col(*origin_x, *cursor_col, ll);
                    let w = layout.glyph_width_at_col(*cursor_col, *min_cursor_w, ll);
                    let y = context.line_y(*cursor_line);
                    gpu::draw_rect(gpu, x, y + cursor_h, w, line_h + cursor_h, palette().paren_match);
                }
            }

            // Matching paren
            if m_line >= *first_visible_line && m_line < *last_visible_line {
                if let Some(ll) = layout.line_for_buffer_line(m_line) {
                    let x = layout.x_for_col(*origin_x, m_col, ll);
                    let w = layout.glyph_width_at_col(m_col, *min_cursor_w, ll);
                    let y = context.line_y(m_line);
                    gpu::draw_rect(gpu, x, y + cursor_h, w, line_h + cursor_h, palette().paren_match);
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

pub fn editor_init_custom_data(editor: &mut Editor) {
    editor.custom_data.set(CustomData {
        lister: Lister::new()
    });

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
    lister.split_panel  = lister_split_panel;
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
    if editor.active_panel != editor.lister().split_panel { return; }

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

    editor.set_active_panel(editor.lister().split_panel);

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

#[derive(Debug)]
pub struct ListerItem {
    pub label:    SmallString<[u8; 32]>,
    pub sublabel: SmallString<[u8; 64]>,
    pub data:     u64,
}

pub type ListerFrameUpdateCallback = fn(&mut CommandContext) -> ShouldRequestFrameRedraw;
pub type ListerSelectFn = fn(&mut CommandContext, u64);

pub struct Lister {
    pub is_open:        bool,
    pub is_listing_file_entries: bool,
    pub is_query_dirty: bool,

    pub last_seen_cached_dir_generation: u64, // u64::MAX if we didnt see any generations

    pub query:         SmallString<[u8; 128]>,

    pub query_buffer: BufferId, // :CustomData
    pub query_view:   ViewId,   // :CustomData
    pub query_panel:  PanelId,  // :CustomData @Redundant?
    pub split_panel:  PanelId,  // :CustomData

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
            split_panel: PanelId::reserved_value(),
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
