#![allow(unused, dead_code)]

use std::{hash::Hash, ops::Deref, path::{Path, PathBuf}};

use cranelift_entity::EntityRef;
use smallstr::SmallString;
use wgpu::naga::{FastHashMap, FastIndexMap};
use winit::{event::KeyEvent, keyboard::{Key, KeyCode, NamedKey, PhysicalKey}};

use crate::{BufferId, Editor, ListerItem, PanelKind, Rect, SCALE_STEP, View, ViewId, adjust_cursors_after_buffer_mutation, buffer::Buffer, director::{EntryKind, ScanState}, editor_save_buffer_onto_disk, gpu::Gpu, rescale, scroll_page, scroll_to_cursor};

pub struct CommandContext<'a> {
    pub editor: &'a mut Editor,
    pub gpu:    &'a mut Gpu,

    pub command_table: &'a CommandTable,

    pub event:  Option<&'a KeyEvent>,
}

impl<'a> CommandContext<'a> {
    pub fn finish(&mut self) {
        adjust_cursors_after_buffer_mutation(self.editor);
        scroll_to_cursor(self.editor);
        self.editor.reset_blink();
    }
}

impl<'a> Drop for CommandContext<'a> {
    fn drop(&mut self) {
        self.finish();
    }
}

pub type CommandFn = fn(&mut CommandContext);

#[derive(Debug)]
pub struct CommandEntry {
    pub name: &'static str,
    pub func: CommandFn,
}

impl CommandEntry {
    pub const fn new(name: &'static str, func: CommandFn) -> Self {
        Self { name, func, }
    }
}

inventory::collect!(CommandEntry);

macro_rules! command {
    ($name:ident |$cx:ident| $body:block) => {
        fn $name($cx: &mut CommandContext) $body
        inventory::submit! { CommandEntry::new(stringify!($name), $name) }
    };
}

command!(move_right |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_right(&mut view.cursor);
});

command!(move_left |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_left(&mut view.cursor);
});

command!(move_up |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_vertical(-1, &mut view.cursor);
});

command!(move_down |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_vertical(1, &mut view.cursor);
});

command!(move_page_up |cx| {
    scroll_page(cx.editor, cx.gpu, -1);
    cx.editor.reset_blink();
});

command!(move_page_down |cx| {
    scroll_page(cx.editor, cx.gpu, 1);
    cx.editor.reset_blink();
});

command!(move_line_start |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_line_start(&mut view.cursor);
});

command!(move_line_end |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_line_end(&mut view.cursor);
});

command!(move_file_start |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_file_start(&mut view.cursor);
});

command!(move_file_end |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_file_end(&mut view.cursor);
});

command!(move_word_forward |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_word_forward(&mut view.cursor);
});

command!(move_word_backward |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    buf.move_word_backward(&mut view.cursor);
});

command!(delete_word_forward |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.delete_word_forward(&mut view.cursor);
});

command!(delete_word_backward |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.delete_word_backward(&mut view.cursor);
});

command!(delete_forward |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.delete_forward(&mut view.cursor);
});

command!(delete_backward |cx| {
    // Identify if we are in a path query
    let is_lister_buffer = cx.editor.active_view().buffer_id == cx.editor.lister_query_buffer;

    let (view, buf) = cx.editor.active_view_and_buffer_mut();

    // If there's a selection, always just delete the selection
    if view.cursor.anchor_char_index.is_some() {
        buf.delete_selection(&mut view.cursor);
        return;
    }

    let cursor_pos = view.cursor.char_index;
    if cursor_pos == 0 { return; }

    if is_lister_buffer {
        let char_to_left = buf.text.char(cursor_pos - 1);

        if char_to_left == '/' {
            // We are at a slash (e.g., "~/Documents/|").
            // We want to delete "Documents/" so we end at "~/".

            // Start the deletion range at the current cursor
            let mut target_start = cursor_pos - 1;

            // Iterate backward from the character before the current slash
            let iter = buf.text.chars_at(cursor_pos - 1).reversed();
            for c in iter {
                if c == '/' { break; } // Stop when we hit the parent slash
                target_start -= 1;
            }

            // Select from current position back to the parent slash and delete
            view.cursor.anchor_char_index = Some(cursor_pos);
            view.cursor.char_index = target_start;
            buf.delete_selection(&mut view.cursor);
            return;
        }
    }

    // Default: Just a normal character backspace
    buf.delete_backward(&mut view.cursor);
});

command!(delete_forward_until_newline |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.delete_forward_until_newline(&mut view.cursor);
});

command!(insert_newline |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.insert_char('\n', &mut view.cursor);
});

command!(insert_newline_after |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.insert_char_after('\n', &mut view.cursor);
});

command!(set_anchor |cx| {
    let (view, _buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.set_anchor();
});

command!(unset_anchor |cx| {
    let (view, _buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
});

command!(basic_character |cx| {
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
});

command!(tab |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    let cursor = &mut view.cursor;
    cursor.unset_anchor();
    buf.insert_literal("    ", &mut view.cursor);
});

command!(split_vertically |cx| {
    let win_rect = Rect::full(cx.gpu.win_w, cx.gpu.win_h);
    cx.editor.split_active(true, 0.5, win_rect);
});

command!(split_horizontally |cx| {
    let win_rect = Rect::full(cx.gpu.win_w, cx.gpu.win_h);
    cx.editor.split_active(false, 0.5, win_rect);
});

command!(close_focused_split |cx| {
    let win_rect = Rect::full(cx.gpu.win_w, cx.gpu.win_h);
    cx.editor.close_active();
    cx.editor.layout_panels(win_rect);
});

command!(toggle_focused_split |cx| {
    cx.editor.toggle_active_panel();
});

command!(scale_down |cx| {
    rescale(cx.editor, cx.gpu, cx.editor.scale - SCALE_STEP);
});

command!(scale_up |cx| {
    rescale(cx.editor, cx.gpu, cx.editor.scale + SCALE_STEP);
});

command!(scale_reset |cx| {
    rescale(cx.editor, cx.gpu, 1.0);
});

command!(open_new_buffer |cx| {
    let buffer  = Buffer::default();
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
});

command!(cycle_buffers_left |cx| {
    let buffer_id = cx.editor.previous_buffer();
    cx.editor.active_view_mut().switch_buffer(buffer_id);
    cx.editor.mru_focus(buffer_id); // @Refactor
});

command!(cycle_buffers_right |cx| {
    let buffer_id = cx.editor.next_buffer();
    cx.editor.active_view_mut().switch_buffer(buffer_id);
    cx.editor.mru_focus(buffer_id); // @Refactor
});

command!(write_buffer_to_disk |cx| {
    let buffer_id = cx.editor.active_view().buffer_id;
    _ = editor_save_buffer_onto_disk(cx.editor, buffer_id);
});

fn lister_item_list_from_command_table(cx: &CommandContext) -> Vec<ListerItem> {
    cx.command_table.iter().enumerate().map(|(index, (atom, _cmd))| {
        ListerItem {
            data: index as u64,
            label: atom.0.into(),
            sublabel: "".into(),
        }
    }).collect()
}

fn lister_item_list_from_buffer_list(cx: &CommandContext) -> Vec<ListerItem> {
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

command!(open_lister_test |cx| {
    let items = lister_item_list_from_command_table(cx);
    cx.editor.lister.is_query_dirty = true;
    cx.editor.lister.rebuild_filtered();
    cx.editor.open_lister(items, |cx, item_data| {
        (cx.command_table[item_data as usize].func)(cx);
    });
});

command!(switch_buffer |cx| {
    cx.editor.lister.set_selected_index_to_1_instead_of_0 = true;

    let items = lister_item_list_from_buffer_list(cx);

    cx.editor.open_lister(items, |cx, item_data| {
        let buffer_id = BufferId::new(item_data as usize);
        cx.editor.active_view_mut().switch_buffer(buffer_id);
        cx.editor.mru_focus(buffer_id); // @Refactor
    });

    cx.editor.lister.selected_index = 1; // Start from 1, since 0 is the current buffer
});

fn path_to_display(path: &str) -> String {     // @Refactor
    if let Ok(home) = std::env::var("HOME") {
        if path.starts_with(&home) {
            return format!("~{}", &path[home.len()..]);
        }
    }
    path.to_string()
}

fn display_to_path(display: &str) -> String {  // @Refactor
    if display.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home, &display[1..]);
        }
    }

    display.to_string()
}

command!(open_file |cx| {
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
                && let Some(&old_buffer_id) = cx.editor.canonicalized_path_to_buffer_id.get(path)
                {
                    cx.editor.active_view_mut().switch_buffer(old_buffer_id);
                    cx.editor.mru_focus(old_buffer_id); // @Refactor
                    return;
                }
            }

            let Ok(new_buffer) = Buffer::from_file(path.as_str().as_ref()) else { return };

            let new_buffer_id = cx.editor.buffers.push(new_buffer);
            cx.editor.canonicalized_path_to_buffer_id.insert(cx.editor.buffers[new_buffer_id].path.clone().unwrap().into(), new_buffer_id);  // @Clone @Refactor
            cx.editor.mru_register_new_buffer(new_buffer_id);
            cx.editor.active_view_mut().switch_buffer(new_buffer_id);
            cx.editor.mru_focus(new_buffer_id); // @Refactor
        },

        // Called on every frame redraw
        |cx| {
            let got_new_chunks = cx.editor.director.poll();

            if got_new_chunks {
                let dir: &Path = cx.editor.canonicalized_last_scanned_directory.as_str().as_ref();
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

            let candidate = if cx.editor.lister.query.chars().last() == Some('/') {
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
                std::fs::canonicalize(&candidate).unwrap_or(candidate)  // @SlowFileSystem
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

                cx.editor.director.kick_scan(dir_to_scan.as_path(), false, true, true);

                //
                // Also pre-scan parent so navigating up is instant
                //
                if let Some(parent) = dir_to_scan.parent() {
                    if cx.editor.director.entries.get(parent).is_none() {
                        cx.editor.director.kick_scan(parent, false, false, false);
                    }
                }
            } else {
                cx.editor.director.get(dir_to_scan.as_path());
            }

            true
        }
    );

    //
    // Inherit start dir from active buffer, fall back to cwd
    //
    let start_dir = cx.editor.buffers[cx.editor.active_view().buffer_id].path
        .as_deref()
        .and_then(|p| p.parent())
        .and_then(|p| std::fs::canonicalize(p).ok())  // @SlowFileSystem
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| cx.editor.canonicalized_current_working_directory.as_str().to_owned());

    cx.editor.canonicalized_current_working_directory = start_dir.as_str().into();

    // Pre-fill query with current working directory
    let mut display_path = path_to_display(&start_dir);
    if !display_path.ends_with('/') { display_path.push('/'); }
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
});

#[derive(Copy, Clone, Debug)]
pub struct CommandAtom(pub &'static str);

impl Deref for CommandAtom {
    type Target = str;
    fn deref(&self) -> &Self::Target { self.0 }
}

impl Into<CommandAtom> for &'static str {
    fn into(self) -> CommandAtom { CommandAtom(self) }
}

impl Eq for CommandAtom {}
impl PartialEq for CommandAtom {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self.0, other.0)
    }
}
impl Hash for CommandAtom {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.as_ptr().hash(state);
    }
}

#[derive(Debug, Default)]
pub struct CommandTable {
    cmds: FastIndexMap<CommandAtom, &'static CommandEntry>,
}

impl Deref for CommandTable {
    type Target = FastIndexMap<CommandAtom, &'static CommandEntry>;
    fn deref(&self) -> &Self::Target {
        &self.cmds
    }
}

impl CommandTable {
    /// Harvest every `inventory::submit!` from all linked crates.
    #[inline]
    pub fn from_inventory() -> Self {
        let mut cmds = FastIndexMap::with_capacity_and_hasher(128, Default::default());
        for entry in inventory::iter::<CommandEntry> {
            cmds.insert(entry.name.into(), entry);
        }

        cmds.sort_unstable_by(|a: &CommandAtom, _, b, _| a.cmp(b));

        Self { cmds }
    }

    #[inline]
    pub fn exec(&self, name: impl Into<CommandAtom>, context: &mut CommandContext) {
        let name = name.into();
        dbg!(name.0.as_ptr());
        for cmd in self.cmds.keys() {
            dbg!(cmd, cmd.0.as_ptr());
        }
        match self.cmds.get(&name) {
            Some(command) => (command.func)(context),
            None    => eprintln!("unknown command: {}", name.0),
        }
    }
}

#[derive(Hash, PartialEq, Eq, Clone)]
pub enum KeyCombo {
    Named(NamedKey, Mods),
    Char(char, Mods),
    Physical(KeyCode, Mods),
}

#[derive(Hash, PartialEq, Eq, Clone, Copy, Default)]
pub struct Mods {
    pub ctrl:  bool,
    pub alt:   bool,
    pub shift: bool,
}

impl Mods {
    pub fn ctrl()  -> Self { Self { ctrl:  true, ..Default::default() } }
    pub fn alt()   -> Self { Self { alt:   true, ..Default::default() } }
    pub fn shift() -> Self { Self { shift: true, ..Default::default() } }
}

// Convenience constructors
impl KeyCombo {
    pub fn named(k: NamedKey) -> Self { Self::Named(k, Mods::default()) }
    pub fn named_mods(k: NamedKey, mods: Mods) -> Self { Self::Named(k, mods) }
    pub fn physical_mods(k: KeyCode, mods: Mods) -> Self { Self::Physical(k, mods) }
    pub fn ctrl(c: char)  -> Self { Self::Char(c, Mods::ctrl()) }
    pub fn alt(c: char)   -> Self { Self::Char(c, Mods::alt()) }
    pub fn physical(k: KeyCode) -> Self { Self::Physical(k, Mods::default()) }
}

#[derive(Default)]
pub struct Keymap {
    bindings: FastHashMap<KeyCombo, CommandAtom>,
}

impl Keymap {
    pub fn default_keymap() -> Self {
        let mut km = Keymap::default();
        use NamedKey::*;

        // Movement
        km.bind(KeyCombo::named(ArrowLeft),  "move_left");
        km.bind(KeyCombo::named(ArrowRight), "move_right");
        km.bind(KeyCombo::named(ArrowUp),    "move_up");
        km.bind(KeyCombo::named(ArrowDown),  "move_down");
        km.bind(KeyCombo::named(Home),       "move_line_start");
        km.bind(KeyCombo::named(End),        "move_line_end");
        km.bind(KeyCombo::named(Tab),        "tab");
        km.bind(KeyCombo::named(Escape),     "unset_anchor");

        // ctrl+home/end need their own entries
        km.bind(KeyCombo::Named(Home, Mods { ctrl: true, ..Default::default() }), "move_file_start");
        km.bind(KeyCombo::Named(End,  Mods { ctrl: true, ..Default::default() }), "move_file_end");

        // Editing
        km.bind(KeyCombo::named(Backspace), "delete_backward");
        km.bind(KeyCombo::named(Delete),    "delete_forward");
        km.bind(KeyCombo::named(Enter),     "insert_newline");
        km.bind(KeyCombo::alt('f'),         "move_word_forward");
        km.bind(KeyCombo::alt('b'),         "move_word_backward");
        km.bind(KeyCombo::alt('d'),         "delete_word_forward");
        km.bind(KeyCombo::named_mods(NamedKey::Backspace, Mods::alt()),   "delete_word_backward");  // M-DEL
        km.bind(KeyCombo::named_mods(NamedKey::Backspace, Mods::ctrl()),  "delete_word_backward");  // common alternative

        km.bind(KeyCombo::ctrl('a'), "move_line_start");
        km.bind(KeyCombo::ctrl('e'), "move_line_end");
        km.bind(KeyCombo::ctrl('o'), "insert_newline_after");
        km.bind(KeyCombo::ctrl('f'), "move_right");
        km.bind(KeyCombo::ctrl('b'), "move_left");
        km.bind(KeyCombo::ctrl('n'), "move_down");
        km.bind(KeyCombo::ctrl('p'), "move_up");
        km.bind(KeyCombo::ctrl('k'), "delete_forward_until_newline");
        km.bind(KeyCombo::ctrl('d'), "delete_forward");
        km.bind(KeyCombo::ctrl('v'), "move_page_down");
        km.bind(KeyCombo::named_mods(Space, Mods::ctrl()), "set_anchor");
        km.bind(KeyCombo::ctrl('g'), "unset_anchor");
        km.bind(KeyCombo::alt('v'),  "move_page_up");
        km.bind(KeyCombo::alt('q'),  "open_file");

        // Splits - physical keys so they're layout-independent
        km.bind(KeyCombo::ctrl('3'), "split_vertically");
        km.bind(KeyCombo::ctrl('2'), "split_horizontally");
        km.bind(KeyCombo::alt('0'),  "close_focused_split");
        km.bind(KeyCombo::alt('2'),  "toggle_focused_split");

        // Scale
        km.bind(KeyCombo::ctrl('='), "scale_up");
        km.bind(KeyCombo::ctrl('-'), "scale_down");
        km.bind(KeyCombo::ctrl('0'), "scale_reset");

        // Buffers
        km.bind(KeyCombo::ctrl(';'), "open_new_buffer");
        km.bind(KeyCombo::alt ('1'), "cycle_buffers_left");
        km.bind(KeyCombo::alt ('3'), "cycle_buffers_right");

        // nocheckin
        km.bind(KeyCombo::alt ('x'), "open_lister_test");
        km.bind(KeyCombo::alt ('`'), "switch_buffer");

        km
    }
}

impl Keymap {
    pub fn bind(&mut self, key: KeyCombo, cmd: impl Into<CommandAtom>) {
        self.bindings.insert(key, cmd.into());
    }

    pub fn lookup(&self, event: &KeyEvent, mods: Mods) -> Option<CommandAtom> {
        let combo = match &event.logical_key {
            Key::Named(k) => KeyCombo::Named(k.clone(), mods),
            Key::Character(s) => {
                let c = s.chars().next()?;
                KeyCombo::Char(c, mods)
            }
            _ => return None,
        };

        //
        // Check explicit binding first
        //
        let found = self.bindings.get(&combo).or_else(|| {
            if let PhysicalKey::Code(code) = event.physical_key {
                self.bindings.get(&KeyCombo::Physical(code, mods))
            } else {
                None
            }
        }).copied();

        if found.is_some() {
            return found;
        }

        //
        // For named keys (non-printable), fall back to unshifted version
        //
        if mods.shift {
            if let Key::Named(k) = &event.logical_key {
                let unshifted = Mods { shift: false, ..mods };
                let unshifted_combo = KeyCombo::Named(k.clone(), unshifted);
                let found = self.bindings.get(&unshifted_combo).or_else(|| {
                    if let PhysicalKey::Code(code) = event.physical_key {
                        self.bindings.get(&KeyCombo::Physical(code, unshifted))
                    } else {
                        None
                    }
                }).copied();

                if found.is_some() {
                    return found;
                }
            }
        }

        //
        // Fall back to basic_character for printable input
        //
        match &event.logical_key {
            Key::Character(_) | Key::Named(NamedKey::Space) => Some("basic_character".into()),
            _ => None,
        }
    }
}
