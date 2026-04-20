#![allow(unused, dead_code)]

use std::{collections::HashMap, ops::Deref};

use cranelift_entity::EntityRef;
use winit::{event::KeyEvent, keyboard::{Key, KeyCode, NamedKey, PhysicalKey}};

use crate::{BufferId, EditorState, Panel, PanelId, PanelKind, PanelSplit, Rect, SCALE_STEP, View, ViewId, buffer::Buffer, collect_leaves, force_layouts_from_all_views_to_rebuild, gpu::Gpu, rescale, scroll_page, scroll_to_cursor};

pub struct CommandContext<'a> {
    pub editor: &'a mut EditorState,
    pub gpu:    &'a mut Gpu,

    pub command_table: &'a CommandTable,

    pub event:  &'a KeyEvent,
    pub mods:   winit::event::Modifiers,

    pub last_executed_command: Option<&'static CommandEntry>,
}

impl<'a> CommandContext<'a> {
    pub fn finish(&mut self) {
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

command!(delete_forward |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
    buf.delete_forward(&mut view.cursor);
});

command!(delete_backward |cx| {
    let (view, buf) = cx.editor.active_view_and_buffer_mut();
    view.cursor.unset_anchor();
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
    let Some(c) = (match &cx.event.logical_key {
        Key::Character(s) => s.chars().next(),
        Key::Named(NamedKey::Space) => Some(' '),
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

    let active_id = cx.editor.active_panel;
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
    let buf_id = cx.editor.previous_buffer();
    cx.editor.active_view_mut().switch_buffer(buf_id);
});

command!(cycle_buffers_right |cx| {
    let buf_id = cx.editor.next_buffer();
    cx.editor.active_view_mut().switch_buffer(buf_id);
});

#[derive(Hash, Copy, Clone, Debug)]
pub struct CommandAtom(pub &'static str);

impl Into<CommandAtom> for &'static str {
    fn into(self) -> CommandAtom { CommandAtom(self) }
}

impl Eq for CommandAtom {}
impl PartialEq for CommandAtom {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self.0, other.0)
    }
}

#[derive(Debug, Default)]
pub struct CommandTable {
    cmds: HashMap<CommandAtom, &'static CommandEntry>,
}

impl Deref for CommandTable {
    type Target = HashMap<CommandAtom, &'static CommandEntry>;
    fn deref(&self) -> &Self::Target {
        &self.cmds
    }
}

impl CommandTable {
    /// Harvest every `inventory::submit!` from all linked crates.
    pub fn from_inventory() -> Self {
        let mut cmds = HashMap::new();
        for entry in inventory::iter::<CommandEntry> {
            cmds.insert(entry.name.into(), entry);
        }
        Self { cmds }
    }

    pub fn exec(&self, name: impl Into<CommandAtom>, context: &mut CommandContext) {
        let name = name.into();
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
    bindings: HashMap<KeyCombo, CommandAtom>,
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

        // Ctrl chords
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

        km
    }
}

impl Keymap {
    pub fn bind(&mut self, key: KeyCombo, cmd: impl Into<CommandAtom>) {
        self.bindings.insert(key, cmd.into());
    }

    pub fn lookup(&self, event: &KeyEvent, mods: Mods) -> Option<CommandAtom> {
        // bare character insert
        if !mods.ctrl && !mods.alt {
            match &event.logical_key {
                Key::Character(_) =>           return Some("basic_character".into()),
                Key::Named(NamedKey::Space) => return Some("basic_character".into()),
                _ => {}
            }
        }

        let combo = match &event.logical_key {
            Key::Named(k) => KeyCombo::Named(k.clone(), mods.clone()),
            Key::Character(s) => {
                let c = s.chars().next()?;
                KeyCombo::Char(c, mods)
            }
            _ => return None,
        };

        self.bindings.get(&combo).or_else(|| {
            if let PhysicalKey::Code(code) = event.physical_key {
                self.bindings.get(&KeyCombo::Physical(code, mods.clone()))
            } else {
                None
            }
        }).copied()
    }
}
