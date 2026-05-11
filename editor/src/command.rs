use std::{hash::Hash, ops::Deref, path::Path};

use cranelift_entity::{PrimaryMap, entity_impl};
use libloading::Library;
use wgpu::naga::{FastHashMap, FastIndexMap};
use winit::{event::KeyEvent, keyboard::{Key, KeyCode, NamedKey, PhysicalKey}};

use crate::{Editor, Hooks, gpu::Gpu};

pub struct CommandContext<'a> {
    pub editor: &'a mut Editor,
    pub gpu:    &'a mut Gpu,

    pub command_table: &'a mut CommandTable,
    pub keymap:        &'a mut Keymap,

    pub dont_reset_blink: bool,

    // @Cleanup: This shouldn't take in KeyEvent, make our own thing
    pub event_and_mods: Option<(&'a KeyEvent, Mods)>,
}

impl<'a> CommandContext<'a> {
    pub fn finish(&mut self) {
        self.editor.command_finish(self.dont_reset_blink);
    }
}

impl<'a> Drop for CommandContext<'a> {
    fn drop(&mut self) {
        self.finish();
    }
}

pub type CommandFn = extern "C" fn(&mut CommandContext);

pub type InitLayerFn = extern "C" fn(&mut CommandContext, &LoadedLib);

pub struct LoadedLib {
    _lib: Library,

    pub commands: &'static [CommandEntry],  // 'static is a @Hack, but lib keeps it alive

    pub init: InitLayerFn,

    pub hooks: Hooks
}

impl LoadedLib {
    pub unsafe fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        //
        // Copy to a unique path so dlopen is forced to map a fresh image
        //
        let tmp = tempfile::NamedTempFile::new()?;
        let unique_path = tmp.into_temp_path();
        std::fs::copy(path, &unique_path)?;

        let _lib = unsafe { Library::new(&*unique_path.to_string_lossy())? };

        let init = unsafe { *_lib.get::<InitLayerFn>(b"custom_layer_init")?.into_raw() };

        let commands = unsafe { **_lib.get::<&&[CommandEntry]>(b"COMMANDS")? };
        let commands = unsafe { core::mem::transmute(commands) };

        Ok(LoadedLib { _lib, commands, init, hooks: Default::default() })
    }
}

#[derive(Copy, Clone, Debug)]
pub struct CommandEntry {
    pub name: &'static str,
    pub func: CommandFn,
}

impl CommandEntry {
    pub const fn new(name: &'static str, func: CommandFn) -> Self {
        Self { name, func }
    }
}

#[derive(Hash, Ord, Eq, PartialEq, PartialOrd, Clone, Copy, Debug)]
pub struct CommandAtom(pub u32);
entity_impl!(CommandAtom);

#[derive(Debug, Default)]
pub struct CommandTable {
    /// Forward: index  -> string (for resolve/display)
    strings: PrimaryMap<CommandAtom, Box<str>>,

    /// Reverse: string -> atom (for O(1) intern dedup)
    index:   FastHashMap<Box<str>, CommandAtom>,

    cmds:    FastIndexMap<CommandAtom, CommandEntry>,
}

impl Deref for CommandTable {
    type Target = FastIndexMap<CommandAtom, CommandEntry>;
    fn deref(&self) -> &Self::Target {
        &self.cmds
    }
}

impl CommandTable {
    #[inline]
    pub fn from_commands(commands: &[CommandEntry]) -> Self {
        let mut table = Self::default();
        table.index.reserve(128);
        table.cmds.reserve(128);

        for &entry in commands {
            let atom = table.intern(entry.name);
            table.cmds.insert(atom, entry);
        }

        table.cmds.sort_unstable_by(|a: &CommandAtom, _, b, _| a.cmp(b));

        table
    }

    #[inline]
    pub fn intern(&mut self, s: &str) -> CommandAtom { // @Memory @Speed
        if let Some(&atom) = self.index.get(s) {
            return atom;
        }
        let atom = CommandAtom(self.strings.len() as u32);
        let owned: Box<str> = s.into();
        self.index.insert(owned.clone(), atom);
        self.strings.push(owned);
        atom
    }

    #[inline]
    pub fn resolve(&self, atom: CommandAtom) -> &str {
        &self.strings[atom]
    }

    #[inline]
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

#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy, Default)]
pub struct Mods {  // @Memory: Make Mods bitflags
    pub ctrl:  bool,
    pub alt:   bool,
    pub shift: bool,
}

impl Mods {
    pub fn ctrl()            -> Self { Self { ctrl:  true, ..Default::default() } }
    pub fn alt()             -> Self { Self { alt:   true, ..Default::default() } }
    pub fn shift()           -> Self { Self { shift: true, ..Default::default() } }
    pub fn ctrl_and_shift()  -> Self { Self { ctrl:  true, shift: true, ..Default::default() } }
    pub fn ctrl_and_alt()    -> Self { Self { ctrl:  true, alt:   true, ..Default::default() } }
    pub fn alt_and_shift()   -> Self { Self { alt:   true, shift: true, ..Default::default() } }
    pub fn ctrl_alt_shift()  -> Self { Self { ctrl:  true, alt:   true, shift: true, ..Default::default() } }
}

impl KeyCombo {
    //
    // Char variants
    //
    pub fn char(c: char)               -> Self { Self::Char(c, Mods::default()) }
    pub fn char_mods(c: char, mods: Mods) -> Self { Self::Char(c, mods) }
    pub fn ctrl(c: char)               -> Self { Self::Char(c, Mods::ctrl()) }
    pub fn alt(c: char)                -> Self { Self::Char(c, Mods::alt()) }
    pub fn shift(c: char)              -> Self { Self::Char(c, Mods::shift()) }
    pub fn ctrl_shift(c: char)         -> Self { Self::Char(c, Mods::ctrl_and_shift()) }
    pub fn ctrl_alt(c: char)           -> Self { Self::Char(c, Mods::ctrl_and_alt()) }
    pub fn alt_shift(c: char)          -> Self { Self::Char(c, Mods::alt_and_shift()) }
    pub fn ctrl_alt_shift(c: char)     -> Self { Self::Char(c, Mods::ctrl_alt_shift()) }

    //
    // Named variants
    //
    pub fn named(k: NamedKey)                    -> Self { Self::Named(k, Mods::default()) }
    pub fn named_mods(k: NamedKey, mods: Mods)  -> Self { Self::Named(k, mods) }
    pub fn named_ctrl(k: NamedKey)               -> Self { Self::Named(k, Mods::ctrl()) }
    pub fn named_alt(k: NamedKey)                -> Self { Self::Named(k, Mods::alt()) }
    pub fn named_shift(k: NamedKey)              -> Self { Self::Named(k, Mods::shift()) }
    pub fn named_ctrl_shift(k: NamedKey)         -> Self { Self::Named(k, Mods::ctrl_and_shift()) }
    pub fn named_ctrl_alt(k: NamedKey)           -> Self { Self::Named(k, Mods::ctrl_and_alt()) }
    pub fn named_alt_shift(k: NamedKey)          -> Self { Self::Named(k, Mods::alt_and_shift()) }
    pub fn named_ctrl_alt_shift(k: NamedKey)     -> Self { Self::Named(k, Mods::ctrl_alt_shift()) }

    //
    // Physical variants
    //
    pub fn physical(k: KeyCode)                   -> Self { Self::Physical(k, Mods::default()) }
    pub fn physical_mods(k: KeyCode, mods: Mods) -> Self { Self::Physical(k, mods) }
    pub fn physical_ctrl(k: KeyCode)              -> Self { Self::Physical(k, Mods::ctrl()) }
    pub fn physical_alt(k: KeyCode)               -> Self { Self::Physical(k, Mods::alt()) }
    pub fn physical_shift(k: KeyCode)             -> Self { Self::Physical(k, Mods::shift()) }
    pub fn physical_ctrl_shift(k: KeyCode)        -> Self { Self::Physical(k, Mods::ctrl_and_shift()) }
    pub fn physical_ctrl_alt(k: KeyCode)          -> Self { Self::Physical(k, Mods::ctrl_and_alt()) }
    pub fn physical_alt_shift(k: KeyCode)         -> Self { Self::Physical(k, Mods::alt_and_shift()) }
    pub fn physical_ctrl_alt_shift(k: KeyCode)    -> Self { Self::Physical(k, Mods::ctrl_alt_shift()) }
}

pub struct Keymap {
    pub bindings: FastHashMap<KeyCombo, CommandAtom>,

    pub     basic_character_atom: CommandAtom,
    pub       switch_buffer_atom: CommandAtom,
    pub  cycle_buffers_left_atom: CommandAtom,
    pub cycle_buffers_right_atom: CommandAtom,
}

impl Keymap {
    pub fn empty(table: &mut CommandTable) -> Self {
        Self {
            basic_character_atom:     table.intern("basic_character"),
            switch_buffer_atom:       table.intern("switch_buffer"),
            cycle_buffers_left_atom:  table.intern("cycle_buffers_left"),
            cycle_buffers_right_atom: table.intern("cycle_buffers_right"),
            bindings: Default::default()
        }
    }

    pub fn default_keymap(table: &mut CommandTable) -> Self {
        use NamedKey::*;

        let mut km = Self::empty(table);

        // Movement
        km.bind(KeyCombo::named(ArrowLeft),  table.intern("move_left"));
        km.bind(KeyCombo::named(ArrowRight), table.intern("move_right"));
        km.bind(KeyCombo::named(ArrowUp),    table.intern("move_up"));
        km.bind(KeyCombo::named(ArrowDown),  table.intern("move_down"));
        km.bind(KeyCombo::named(Home),       table.intern("move_line_start"));
        km.bind(KeyCombo::named(End),        table.intern("move_line_end"));
        km.bind(KeyCombo::named(Tab),        table.intern("tab"));
        km.bind(KeyCombo::named(Escape),     table.intern("unset_anchor"));

        km.bind(KeyCombo::named_ctrl(Home), table.intern("move_file_start"));
        km.bind(KeyCombo::named_ctrl(End), table.intern("move_file_end"));
        km.bind(KeyCombo::alt('<'), table.intern("move_file_start"));
        km.bind(KeyCombo::alt('>'), table.intern("move_file_end"));

        // Editing
        km.bind(KeyCombo::named(Backspace), table.intern("delete_backward"));
        km.bind(KeyCombo::named(Delete),    table.intern("delete_forward"));
        km.bind(KeyCombo::named(Enter),     table.intern("insert_newline"));
        km.bind(KeyCombo::alt('f'),         table.intern("move_word_forward"));
        km.bind(KeyCombo::alt('b'),         table.intern("move_word_backward"));
        km.bind(KeyCombo::alt('d'),         table.intern("delete_word_forward"));
        km.bind(KeyCombo::named_mods(NamedKey::Backspace, Mods::alt()),   table.intern("delete_word_backward"));  // M-DEL
        km.bind(KeyCombo::named_mods(NamedKey::Backspace, Mods::ctrl()),  table.intern("delete_word_backward"));  // common alternative

        km.bind(KeyCombo::ctrl('a'), table.intern("move_line_start"));
        km.bind(KeyCombo::ctrl('e'), table.intern("move_line_end"));
        km.bind(KeyCombo::ctrl('o'), table.intern("insert_newline_after"));
        km.bind(KeyCombo::ctrl('f'), table.intern("move_right"));
        km.bind(KeyCombo::ctrl('b'), table.intern("move_left"));
        km.bind(KeyCombo::ctrl('n'), table.intern("move_down"));
        km.bind(KeyCombo::ctrl('p'), table.intern("move_up"));
        km.bind(KeyCombo::ctrl('k'), table.intern("delete_forward_until_newline"));
        km.bind(KeyCombo::ctrl('d'), table.intern("delete_forward"));
        km.bind(KeyCombo::ctrl('v'), table.intern("move_page_down"));
        km.bind(KeyCombo::ctrl('y'), table.intern("paste"));
        km.bind(KeyCombo::ctrl('w'), table.intern("delete_selection_and_copy"));
        km.bind(KeyCombo::alt ('w'), table.intern("copy"));
        km.bind(KeyCombo::named_mods(Space, Mods::ctrl()), table.intern("set_anchor"));
        km.bind(KeyCombo::ctrl('g'), table.intern("unset_anchor"));
        km.bind(KeyCombo::alt('v'),  table.intern("move_page_up"));
        km.bind(KeyCombo::alt('q'),  table.intern("open_file"));
        km.bind(KeyCombo::alt('m'),  table.intern("move_to_first_character_in_current_line"));

        // Splits - physical keys so they're layout-independent
        km.bind(KeyCombo::ctrl('3'), table.intern("split_vertically"));
        km.bind(KeyCombo::ctrl('2'), table.intern("split_horizontally"));
        km.bind(KeyCombo::alt('0'),  table.intern("close_focused_split"));
        km.bind(KeyCombo::alt('2'),  table.intern("toggle_focused_split"));

        // Scale
        km.bind(KeyCombo::ctrl('='), table.intern("scale_up"));
        km.bind(KeyCombo::ctrl('-'), table.intern("scale_down"));
        km.bind(KeyCombo::ctrl('0'), table.intern("scale_reset"));

        // Buffers
        km.bind(KeyCombo::ctrl(';'), table.intern("open_new_buffer"));
        km.bind(KeyCombo::alt ('1'), table.intern("cycle_buffers_left"));
        km.bind(KeyCombo::alt ('3'), table.intern("cycle_buffers_right"));
        km.bind(KeyCombo::alt ('`'), table.intern("switch_buffer"));
        km.bind(KeyCombo::alt ('x'), table.intern("open_command_lister"));

        km
    }
}

impl Keymap {
    pub fn bind(&mut self, key: KeyCombo, cmd: CommandAtom) {
        self.bindings.insert(key, cmd.into());
    }

    pub fn lookup(&self, event: &KeyEvent, mods: Mods) -> Option<CommandAtom> {
        let (combo, mods) = match &event.logical_key {
            Key::Named(k) => (KeyCombo::Named(k.clone(), mods), mods),
            Key::Character(s) => {
                let c = s.chars().next()?;

                //
                // Shift is baked into the character already; strip it from mods
                // so Alt+Shift+, matches a binding on Alt+'<'
                //
                let char_mods = Mods { shift: false, ..mods };

                (KeyCombo::Char(c, char_mods), char_mods)

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
            Key::Character(_) | Key::Named(NamedKey::Space) => Some(self.basic_character_atom),
            _ => None,
        }
    }
}
