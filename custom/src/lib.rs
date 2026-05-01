// #[cfg(feature = "dhat")]
// #[global_allocator]
// static ALLOC: dhat::Alloc = dhat::Alloc;

// #[cfg(feature = "mimalloc")]
// #[global_allocator]
// static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use editor::buffer::Buffer;
use editor::director::{EntryKind, ScanState};
use editor::session::{default_session_path, pretty_path};
use editor::command::{CommandContext, CommandEntry};
use editor::*;

use editor_macros::{collect_commands, command, export};

use std::path::{MAIN_SEPARATOR, Path, PathBuf};

use smallstr::SmallString;
use cranelift_entity::EntityRef;
use winit::keyboard::{Key, NamedKey};

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
    cx.editor.reset_blink();
}

#[command]
pub fn move_page_down(cx: &mut CommandContext) {
    scroll_page(cx.editor, cx.gpu, 1);
    cx.editor.reset_blink();
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
    let is_lister_buffer = cx.editor.active_view().buffer_id == cx.editor.lister_query_buffer;

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

    let line_slice = buf.text.slice(view.cursor.char_index..);

    buf.scratch_space_to_flatten_rope_into.clear();
    buf.scratch_space_to_flatten_rope_into.extend(line_slice.chars());  // :BufferScratch
    let chars_to_delete = memchr::memchr(b'\n', buf.scratch_space_to_flatten_rope_into.as_bytes())
        .map(|p| p.max(1))
        .unwrap_or(len - view.cursor.char_index);

    if chars_to_delete == 0 { return; }

    view.cursor.anchor_char_index = Some(view.cursor.char_index + chars_to_delete);
    copy_impl(cx, false);

    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.delete_selection_without_animation(&mut view.cursor);
}

#[command]
pub fn insert_newline(cx: &mut CommandContext) {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.insert_char('\n', &mut view.cursor);
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
    let Some(c) = (match &cx.event.map(|e| &e.logical_key) {
        Some(Key::Character(s))           => s.chars().next(),
        Some(Key::Named(NamedKey::Space)) => Some(' '),
        _ => None,
    }) else {
        return
    };

    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    let cursor = &mut view.cursor;
    cursor.unset_anchor();
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
    let win_rect = Rect::full(cx.gpu.win_w, cx.gpu.win_h);
    cx.editor.split_active(true, 0.5, win_rect);
}

#[command]
pub fn split_horizontally(cx: &mut CommandContext) {
    let win_rect = Rect::full(cx.gpu.win_w, cx.gpu.win_h);
    cx.editor.split_active(false, 0.5, win_rect);
}

#[command]
pub fn close_focused_split(cx: &mut CommandContext) {
    let win_rect = Rect::full(cx.gpu.win_w, cx.gpu.win_h);
    cx.editor.close_active();
    cx.editor.layout_panels(win_rect);
}

#[command]
pub fn toggle_focused_split(cx: &mut CommandContext) {
    cx.editor.toggle_active_panel();
}

#[command]
pub fn scale_down(cx: &mut CommandContext) {
    rescale(cx.editor, cx.gpu, cx.editor.scale - SCALE_STEP);
}

#[command]
pub fn scale_up(cx: &mut CommandContext) {
    rescale(cx.editor, cx.gpu, cx.editor.scale + SCALE_STEP);
}

#[command]
pub fn scale_reset(cx: &mut CommandContext) {
    rescale(cx.editor, cx.gpu, 1.0);
}

#[command]
pub fn open_new_buffer(cx: &mut CommandContext) {
    let buffer  = Buffer::new();
    let buf_id  = cx.editor.buffers.push(buffer);
    let view_id = ViewId::new(cx.editor.views.len());
    cx.editor.views.push(View::new(view_id, buf_id));
    cx.editor.mru_register_new_buffer(buf_id);

    let root_id   = cx.editor.root_panel;
    let win_rect = Rect::full(cx.gpu.win_w, cx.gpu.win_h);

    if matches!(&cx.editor.panel(root_id).kind, PanelKind::Leaf { .. }) {
        //
        // Ensure root is a split
        //

        cx.editor.active_panel = root_id;
        cx.editor.split_active(true, 0.5, win_rect);
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
        if buffer_id == cx.editor.lister_query_buffer { return None; }

        let buffer = &cx.editor.buffers[buffer_id];
        let label: SmallString<[u8; 32]> = buffer.path.as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("[scratch]")
            .into();

        let sublabel: SmallString<[u8; 64]> = buffer.path.as_ref()
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
    cx.editor.lister.is_query_dirty = true;
    cx.editor.lister.rebuild_filtered();
    cx.editor.open_lister(items, |cx, item_data| {
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
    buf.append_last_insertion_to_currently_animated_insertions();
    view.cursor.unset_anchor();
}

pub fn copy_impl(cx: &mut CommandContext, unset_anchor: bool) { // :BufferScratch
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

    if anchor_char_index < char_index {
        // @Hack nocheckin
        buf.last_insert = Some((anchor_char_index, slice.chars().map(|c| c.len_utf8() as u32).sum()));
        buf.append_last_insertion_to_currently_animated_insertions();
        buf.last_insert = None;
    } else {
        // @Hack nocheckin
        buf.last_insert = Some((char_index, slice.chars().map(|c| c.len_utf8() as u32).sum()));
        buf.append_last_insertion_to_currently_animated_insertions();
        buf.last_insert = None;
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
    copy_impl(cx, true);
}

#[command]
pub fn delete_selection_and_copy(cx: &mut CommandContext) {
    copy_impl(cx, false);

    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.delete_selection_with_animation(&mut view.cursor);
}

#[command]
pub fn switch_buffer(cx: &mut CommandContext) {
    cx.editor.lister.set_selected_index_to_1_instead_of_0 = true;

    let items = lister_item_list_from_buffer_list(cx);

    cx.editor.open_lister(items, |cx, item_data| {
        let buffer_id = BufferId::new(item_data as usize);
        cx.editor.active_view_mut().switch_buffer(buffer_id);
        cx.editor.mru_focus(buffer_id); // @Refactor
    });

    cx.editor.lister.selected_index = 1; // Start from 1, since 0 is the current buffer
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

    let items = Vec::new();
    cx.editor.open_lister_with_frame_callback(
        items,

        // Called on select
        |cx, item_data| {
            let entry_kind: EntryKind = unsafe { core::mem::transmute(item_data as u8) };

            let selected_item = &cx.editor.lister.items[cx.editor.lister.filtered[cx.editor.lister.selected_index as usize] as usize];
            let path = &selected_item.sublabel;

            if entry_kind == EntryKind::Dir {
                let path: &Path = path.as_str().as_ref();
                if let Ok(canon) = path.canonicalize() {
                    cx.editor.canonicalized_current_working_directory = canon.into_os_string().into_string().unwrap().into();
                }
                open_file(cx);
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

            let Ok(new_buffer) = Buffer::from_file(path.as_str().as_ref()) else { return };

            let new_buffer_id = cx.editor.buffers.push(new_buffer);
            if let Some(canon) = cx.editor.buffers[new_buffer_id].path.clone().and_then(|p| p.canonicalize().ok()) {
                cx.editor.canonicalized_path_to_buffer_id.insert(canon.into() , new_buffer_id);  // @Clone @Refactor
            }
            cx.editor.mru_register_new_buffer(new_buffer_id);
            cx.editor.active_view_mut().switch_buffer(new_buffer_id);
            cx.editor.mru_focus(new_buffer_id); // @Refactor
        },

        // Called on every frame redraw
        |cx| {
            let dir: &Path = cx.editor.canonicalized_last_scanned_directory.as_str().as_ref();
            let got_new_chunks = cx.editor.director.poll(dir);

            if got_new_chunks {
                if let Some(cached) = cx.editor.director.entries.get(dir)
                    && (cached.entries.generation != cx.editor.lister.last_seen_cached_dir_generation
                     || cached.state == ScanState::Ready)
                {
                    cx.editor.lister.last_seen_cached_dir_generation = cached.entries.generation;
                    cx.editor.lister.items.clear();
                    for entry in cached.entries.iter() {
                        cx.editor.lister.items.push(ListerItem {
                            data:     entry.kind as u64,
                            label:    entry.name.into(),
                            sublabel: entry.path.into(),
                        });
                    }
                    cx.editor.lister.is_query_dirty = true;
                    cx.editor.lister.rebuild_filtered();
                    cx.editor.lister.is_query_dirty = true; // nocheckin @DocumentThis
                }
            }

            if !cx.editor.lister.is_query_dirty { return got_new_chunks; }
            cx.editor.lister.is_query_dirty = false;

            let query_path = display_to_path(cx.editor.lister.query.as_str()); // @Clone
            let query_path: &Path = query_path.as_ref();

            let candidate = if cx.editor.lister.query.chars().last() == Some(MAIN_SEPARATOR) {
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
                cx.editor.lister.items.clear();
                cx.editor.lister.last_seen_cached_dir_generation = u64::MAX;
                cx.editor.lister.rebuild_filtered();

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

            true
        }
    );

    cx.editor.canonicalized_current_working_directory = start_dir.as_str().into();

    // Pre-fill query with current working directory
    let mut display_path = path_to_display(&start_dir);
    if !display_path.ends_with(MAIN_SEPARATOR) { display_path.push(MAIN_SEPARATOR); }
    cx.editor.buffers[cx.editor.lister_query_buffer].clear();
    cx.editor.buffers[cx.editor.lister_query_buffer].insert_literal(
        &display_path,
        &mut cx.editor.views[cx.editor.lister_query_view].cursor,
    );

    // @Redundant?
    // Sync the lister query string
    cx.editor.lister.query.clear();
    cx.editor.lister.query.push_str(&display_path);
    cx.editor.lister.is_query_dirty = true;
    cx.editor.lister.is_listing_file_entries = true;
    cx.editor.lister.last_seen_cached_dir_generation = u64::MAX;
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
pub fn custom_layer_init(_cx: &mut CommandContext) {
    // ...
}
