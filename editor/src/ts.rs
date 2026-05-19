use crate::buffer::Buffer;
use crate::{BufferId, CompletionItem, CompletionItemKind};
use crate::atum::{Atom, AtomTable};

use std::borrow::Cow;
use std::cell::UnsafeCell;
use std::num::NonZeroU32;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::thread;

use cranelift_entity::PrimaryMap;
use crossbeam_channel::{Receiver, Sender};
use dashmap::DashMap;
use ropey::Rope;
use rustc_hash::FxHashMap;
use smallstr::SmallString;
use smallvec::SmallVec;
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

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

impl Into<Point> for e2_Point {
    fn into(self) -> Point {
        Point {
            column: self.column as _,
            row: self.row as _,
        }
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
}

impl Into<InputEdit> for e2_InputEdit {
    fn into(self) -> InputEdit {
        InputEdit {
            start_byte:       self.start_byte as _,
            old_end_byte:     self.old_end_byte as _,
            new_end_byte:     self.new_end_byte as _,
            start_position:   self.start_position.into(),
            old_end_position: self.old_end_position.into(),
            new_end_position: self.new_end_position.into(),
        }
    }
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

pub enum ParserMessage {
    Initialize {
        buffer_id: BufferId,
        rope: Rope,
        buffer_last_edit_generation: u64,
    },

    Reparse {
        buffer_id: BufferId,
        rope: Rope,
        buffer_last_edit_generation: u64,
    },
}

pub enum ParserQuery {
    FuncCallOverlay {
        buffer_id: BufferId,
        cursor_byte: usize,
        rope: Rope,
    },
}

pub enum ParseResultKind {
    SymbolsUpdate {
        functions: Vec<FunctionInfo>,
        symbols: Vec<RawSymbol>,
    },

    FuncCallOverlayUpdate {
        overlay: Option<Overlay>,
    },
}

pub struct ParseResult {
    pub buffer_id: BufferId,

    pub kind: ParseResultKind,
}

#[derive(Debug, Clone)]
pub enum CallKind {
    Bare(Atom),                  // bar()
    AssocOrPath(Atom, Atom),     // Foo::bar() or foo::bar() - check by_impl first, then by_module
    FullPath([Atom; 4], Atom),   // foo::...::baz::bar() @Memory
    Method(Atom),                // .bar() - name only, ambiguous
}

impl CallKind {
    pub fn function_name(&self) -> Atom {
        match self {
            Self::FullPath(..,    atom) |
            Self::AssocOrPath(.., atom) |
            Self::Bare(atom) | Self::Method(atom) => *atom,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Overlay {
    pub call_kind: CallKind,
    pub arg_index: u16,
    pub opening_paren_byte: u32,
    pub closing_paren_byte: Option<NonZeroU32>,
}

#[derive(Debug, Clone)]
pub struct VersionedTree {
    pub tree: Tree,
    pub buffer_last_edit_generation: u64
}

impl Deref for VersionedTree {
    type Target = Tree;
    fn deref(&self) -> &Self::Target { &self.tree }
}

impl DerefMut for VersionedTree {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.tree }
}

pub struct TreeSitter {
    pub message_tx: Sender<ParserMessage>,
    pub query_tx:   Sender<ParserQuery>,

    pub result: Receiver<ParseResult>,

    // Shared tables for main-thread lookups
    pub atom_table: Arc<AtomTable>,
    pub trees:      Arc<DashMap<BufferId, VersionedTree>>,

    pub func_table: FunctionTable,
    pub symbol_table: SymbolTable
}

impl TreeSitter {
    #[inline]
    pub fn query_cursor_overlay_without_sending(&self, buffer_id: BufferId, cursor_byte: usize, rope: &Rope) -> Option<Overlay> {
        let tree = self.trees.get(&buffer_id)?;
        let node = tree.root_node().descendant_for_byte_range(cursor_byte, cursor_byte)?;
        find_call_overlay(node, cursor_byte, rope, &self.atom_table)
    }

    #[inline]
    pub fn query_cursor_overlay(&self, buffer_id: BufferId, cursor_byte: usize, rope: &Rope) -> Option<Overlay> {
        if let Some(o) = self.query_cursor_overlay_without_sending(buffer_id, cursor_byte, rope) {
            return Some(o);
        }

        self.send_cursor_query(buffer_id, cursor_byte, rope.clone());

        None
    }

    #[inline]
    pub fn send_reparse(&self, buffer_id: BufferId, rope: Rope, buffer_last_edit_generation: u64) {
        _ = self.message_tx.send(ParserMessage::Reparse { buffer_id, rope, buffer_last_edit_generation });
    }

    #[inline]
    pub fn send_init(&self, buffer_id: BufferId, rope: Rope, buffer_last_edit_generation: u64) {
        _ = self.message_tx.send(ParserMessage::Initialize { buffer_id, rope, buffer_last_edit_generation });
    }

    #[inline]
    pub fn send_cursor_query(&self, buffer_id: BufferId, cursor_byte: usize, rope: Rope) {
        _ = self.query_tx.send(ParserQuery::FuncCallOverlay { buffer_id, cursor_byte, rope });
    }
}

pub fn spawn() -> TreeSitter {
    let (message_tx, edit_rx) = crossbeam_channel::unbounded();
    let (query_tx, query_rx) = crossbeam_channel::unbounded();
    let (result_tx, result) = crossbeam_channel::unbounded();

    let atom_table = Arc::new(AtomTable::new());
    let trees = Arc::default();

    let table_clone = Arc::clone(&atom_table);
    let trees_clone = Arc::clone(&trees);

    thread::spawn(move || bg_thread(query_rx, edit_rx, result_tx, table_clone, trees_clone));

    TreeSitter {
        message_tx, result, query_tx, atom_table, trees,
        func_table: Default::default(), symbol_table: SymbolTable::new()
    }
}

fn bg_thread(query_rx: Receiver<ParserQuery>, edit_rx: Receiver<ParserMessage>, result_tx: Sender<ParseResult>, atom_table: Arc<AtomTable>, trees: Arc<DashMap<BufferId, VersionedTree>>) {
    let mut parser  = Parser::new();

    parser.set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("failed to load Rust grammar");

    fn rope_chunk_callback<'a>(rope: &'a Rope, byte: usize) -> &'a [u8] {
        if byte >= rope.len_bytes() { return &[]; }
        let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
        &chunk.as_bytes()[(byte - chunk_start)..]
    }

    loop {
        crossbeam_channel::select! {
            recv(query_rx) -> q => match q {
                Ok(ParserQuery::FuncCallOverlay { buffer_id, cursor_byte, rope }) => {
                    let Some(tree) = trees.get(&buffer_id) else { continue };

                    let node = tree.root_node().descendant_for_byte_range(cursor_byte, cursor_byte);
                    let overlay = node.and_then(|n| find_call_overlay(n, cursor_byte, &rope, &atom_table));

                    _ = result_tx.send(ParseResult {
                        buffer_id,
                        kind: ParseResultKind::FuncCallOverlayUpdate { overlay }
                    });
                }

                _ => {}
            },

            recv(edit_rx) -> msg => match msg {
                Ok(ParserMessage::Reparse { buffer_id, rope, buffer_last_edit_generation }) => {
                    //
                    // Snapshot the old tree for the incremental parse hint,
                    // clone it immediately so we hold the lock for the minimum time.
                    //
                    let old_tree = trees.get(&buffer_id).map(|e| e.tree.clone());

                    //
                    // Parse entirely outside any lock.
                    //
                    let mut callback = |byte: usize, _point: Point| rope_chunk_callback(&rope, byte);
                    let Some(new_tree) = parser.parse_with_options(
                        &mut callback,
                        old_tree.as_ref(),
                        None,
                    ) else {
                        continue
                    };

                    //
                    // Collect functions from the new tree before acquiring any lock
                    //
                    let root_node = new_tree.root_node();
                    let functions = collect_functions(root_node, &rope, buffer_id, &atom_table);
                    let symbols   = collect_symbols(root_node, &rope, buffer_id, &atom_table);

                    //
                    // Atomically decide whether to commit,
                    // only commit if our parse is at least as fresh as whatever is in the map.
                    //
                    let committed = match trees.get_mut(&buffer_id) {
                        Some(mut shared) => {
                            if buffer_last_edit_generation >= shared.buffer_last_edit_generation {
                                shared.tree = new_tree;
                                shared.buffer_last_edit_generation = buffer_last_edit_generation;
                                true
                            } else {
                                //
                                // Stale: main thread has already applied newer edits on top of
                                // a newer parse. Discarding this result preserves the main
                                // thread's tree.edit() shifts which are more up-to-date.
                                //
                                false
                            }
                        }
                        None => {
                            //
                            // Buffer was closed between when we started parsing and now.
                            //
                            false
                        }
                    };

                    if !committed { continue; }

                    if functions.is_empty() && symbols.is_empty() { continue; }

                    //
                    // Only send results if we actually committed
                    //
                    _ = result_tx.send(ParseResult {
                        buffer_id,
                        kind: ParseResultKind::SymbolsUpdate { functions, symbols },
                    });
                }

                Ok(ParserMessage::Initialize { buffer_id, rope, buffer_last_edit_generation }) => {
                    if trees.contains_key(&buffer_id) { continue; }

                    let mut callback = |byte: usize, _point: Point| rope_chunk_callback(&rope, byte);

                    let Some(tree) = parser.parse_with_options(&mut callback, None, None) else { continue };

                    let tree_copy = tree.clone();
                    let root_node = tree_copy.root_node();
                    trees.insert(buffer_id, VersionedTree { tree, buffer_last_edit_generation });

                    let functions = collect_functions(root_node, &rope, buffer_id, &atom_table);
                    let symbols   = collect_symbols(root_node, &rope, buffer_id, &atom_table);
                    if functions.is_empty() && symbols.is_empty() { continue; }

                    _ = result_tx.send(ParseResult {
                        buffer_id,
                        kind: ParseResultKind::SymbolsUpdate { functions, symbols }
                    });
                }

                _ => {}
            },
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Func(pub u32);
cranelift_entity::entity_impl!(Func);

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct FuncRef {
    pub id:         Func,
    pub generation: u32,
}

#[derive(Debug, Clone)]
pub struct ParamInfo {
    pub name:      Atom,
    pub type_str:  Atom,
}

pub struct FunctionInfo {
    pub generation: u32, // 0 -> free slot

    pub name:      Atom,
    pub params:    Box<[ParamInfo]>,
    pub buffer_id: BufferId,
    pub impl_name: Option<Atom>,
    pub module:    Box<[Atom]>,
}

#[derive(Default)]
pub struct FunctionTable {
    pub fns:       PrimaryMap<Func, FunctionInfo>,
    pub free_list: Vec<Func>,

    // name -> all FuncRefs with that name
    pub by_name:   FxHashMap<Atom, SmallVec<[FuncRef; 2]>>,

    // (impl_name, fn_name) -> FuncRef, for Foo::bar() / impl Foo { fn bar }
    pub by_impl:   FxHashMap<(Atom, Atom), FuncRef>,

    // (module_hash, fn_name) -> FuncRef, for foo::bar()
    pub by_module: FxHashMap<(u64, Atom), FuncRef>,

    // buffer -> all Funcs it owns, for invalidation
    pub by_buffer: FxHashMap<BufferId, Vec<Func>>,
}

impl FunctionTable {
    #[inline]
    pub fn get(&self, r: FuncRef) -> Option<&FunctionInfo> {
        let info = self.fns.get(r.id)?;
        if info.generation == r.generation && info.generation != 0 {
            Some(info)
        } else {
            None
        }
    }

    pub fn insert_batch(&mut self, infos: Vec<FunctionInfo>, buffer_stem: Atom) {
        // Pre-reserve capacity in all maps
        let n = infos.len();
        self.by_name.reserve(n);
        self.by_impl.reserve(n);
        self.by_module.reserve(n);

        for info in infos {
            let name      = info.name;
            let impl_name = info.impl_name;
            let module    = info.module.clone();
            let buffer_id = info.buffer_id;

            let effective_module: &[Atom] = if module.is_empty() {
                std::slice::from_ref(&buffer_stem)
            } else {
                &module
            };
            let module_hash = hash_module(effective_module);

            let func = if let Some(slot) = self.free_list.pop() {
                let new_gen = self.fns[slot].generation.wrapping_add(1).max(1);
                self.fns[slot] = FunctionInfo { generation: new_gen, ..info };
                slot
            } else {
                self.fns.push(info)
            };

            let generation = self.fns[func].generation;
            let r = FuncRef { id: func, generation };

            self.by_name.entry(name).or_insert_with(|| SmallVec::new()).push(r);

            if let Some(impl_atom) = impl_name {
                self.by_impl.insert((impl_atom, name), r);
            }

            self.by_module.insert((module_hash, name), r);
            self.by_buffer.entry(buffer_id).or_insert_with(Vec::new).push(func);
        }
    }

    pub fn replace_buffer_batch(&mut self, buffer_id: BufferId, buffer_stem: Atom, new_fns: Vec<FunctionInfo>) {
        self.remove_buffer(buffer_id);
        self.insert_batch(new_fns, buffer_stem);
    }

    #[inline]
    pub fn remove_buffer(&mut self, buffer_id: BufferId) {
        let Some(funcs) = self.by_buffer.remove(&buffer_id) else { return };

        for func in funcs {
            let info = &mut self.fns[func];
            if info.generation == 0 { continue; }  // Already dead

            let name      = info.name;
            let impl_name = info.impl_name;
            let module    = info.module.clone();
            let generation = info.generation;
            info.generation = 0; // mark dead

            // Remove from by_name
            if let Some(refs) = self.by_name.get_mut(&name) {
                refs.retain(|r| !(r.id == func && r.generation == generation));
                if refs.is_empty() { self.by_name.remove(&name); }
            }

            // Remove from by_impl
            if let Some(impl_atom) = impl_name {
                let key = (impl_atom, name);
                if self.by_impl.get(&key).map_or(false, |r| r.id == func) {
                    self.by_impl.remove(&key);
                }
            }

            // Remove from by_module
            let key = (hash_module(&module), name);
            if self.by_module.get(&key).map_or(false, |r| r.id == func) {
                self.by_module.remove(&key);
            }

            self.free_list.push(func);
        }
    }

    #[inline]
    pub fn resolve_bare(&self, name: Atom) -> Option<FuncRef> {
        let refs = self.by_name.get(&name)?;

        // Prefer a free function (no impl_name) over an impl method
        refs.iter()
            .copied()
            .find(|r| self.get(*r).map_or(false, |f| f.impl_name.is_none()))
            .or_else(|| refs.first().copied())
    }

    #[inline]
    pub fn resolve_method(&self, name: Atom) -> Option<FuncRef> {
        let refs = self.by_name.get(&name)?;

        // Prefer a method function (with impl_name) over a free function
        refs.iter()
            .copied()
            .find(|r| self.get(*r).map_or(false, |f| f.impl_name.is_some()))
            .or_else(|| refs.first().copied())
    }

    #[inline]
    pub fn resolve_assoc(&self, impl_name: Atom, fn_name: Atom) -> Option<FuncRef> {
        self.by_impl.get(&(impl_name, fn_name))
            .or_else(|| self.by_module.get(&(hash_module(&[impl_name]), fn_name)))
            .copied()
    }

    #[inline]
    pub fn resolve_path(&self, module: &[Atom], fn_name: Atom) -> Option<FuncRef> {
        self.by_module.get(&(hash_module(module), fn_name))
            .copied()
            .or_else(|| module.first().and_then(|impl_name| self.resolve_assoc(*impl_name, fn_name)))
    }
}

fn hash_module(module: &[Atom]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    module.hash(&mut h);
    h.finish()
}

#[derive(Default, Clone)]
struct FuncContext {
    pub impl_name: Option<Atom>,
    pub module:    Box<[Atom]>,
}

pub fn collect_functions(root: Node, source: &Rope, buffer_id: BufferId, table: &AtomTable) -> Vec<FunctionInfo> {
    let mut out = Vec::new();
    visit_node(root, source, buffer_id, &FuncContext::default(), &mut out, table);
    out
}

fn visit_node(
    node:      Node,
    source:    &Rope,
    buffer_id: BufferId,
    ctx:       &FuncContext,
    out:       &mut Vec<FunctionInfo>,
    table:     &AtomTable,
) {
    match node.kind() {
        "function_item" => if let Some(name_node) = node.child_by_field_name("name") {
            let name   = rope_slice_to_atom(source, name_node.start_byte(), name_node.end_byte(), table);
            let params = collect_params(node, source, table);
            out.push(FunctionInfo {
                generation: u32::MAX,
                name,
                params: params.into(),
                buffer_id,
                impl_name: ctx.impl_name,
                module:    ctx.module.clone().into(),
            });
        }

        "impl_item" => {
            let impl_name = node.child_by_field_name("type")
                .map(|n| rope_slice_to_atom(source, n.start_byte(), n.end_byte(), table));

            let new_ctx = FuncContext { impl_name, module: ctx.module.clone() };
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, source, buffer_id, &new_ctx, out, table);
            }

            return;
        }

        "mod_item" => if let Some(name_node) = node.child_by_field_name("name") {
            let mod_atom = rope_slice_to_atom(source, name_node.start_byte(), name_node.end_byte(), table);
            let mut new_module = Vec::from(ctx.module.clone());
            new_module.push(mod_atom);

            let new_ctx = FuncContext { impl_name: None, module: new_module.into() };
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, source, buffer_id, &new_ctx, out, table);
            }

            return;
        }

        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_node(child, source, buffer_id, ctx, out, table);
    }
}

fn collect_params(fn_node: Node, source: &Rope, table: &AtomTable) -> Vec<ParamInfo> {
    let mut params = Vec::new();
    let Some(param_list) = fn_node.child_by_field_name("parameters") else { return params; };

    let mut cursor = param_list.walk();
    for child in param_list.children(&mut cursor) {
        if child.kind() != "parameter" { continue; }

        let (name, type_str) = match (child.child_by_field_name("pattern"), child.child_by_field_name("type")) {
            (Some(n), Some(t)) => (
                rope_slice_to_atom(source, n.start_byte(), n.end_byte(), table),
                rope_slice_to_atom(source, t.start_byte(), t.end_byte(), table),
            ),
            _ => continue,
        };

        params.push(ParamInfo { name, type_str });
    }
    params
}

/// Helper that slices the rope and interns the resulting string immediately.
fn rope_slice_to_atom(source: &Rope, start_byte: usize, end_byte: usize, table: &AtomTable) -> Atom {
    let start = source.byte_to_char(start_byte);
    let end   = source.byte_to_char(end_byte);

    source.get_slice(start..end)
        .map(|s| if let Some(in_memory) = s.as_str() {
            table.intern(in_memory)
        } else {
            let mut flat = SmallString::<[_; 128]>::with_capacity(s.len_bytes());
            for c in s.chunks() { flat.push_str(c); }
            table.intern(&flat)
        }).unwrap_or_default()
}

/// Helper that slices the rope and interns the resulting string immediately.
fn rope_slice_to_string<'a>(source: &'a Rope, start_byte: usize, end_byte: usize) -> Cow<'a, str> {
    let start = source.byte_to_char(start_byte);
    let end   = source.byte_to_char(end_byte);

    source.get_slice(start..end)
        .map(|s| if let Some(in_memory) = s.as_str() {
            Cow::Borrowed(in_memory)
        } else {
            let mut flat = String::with_capacity(s.len_bytes());
            for c in s.chunks() { flat.push_str(c); }
            Cow::Owned(flat)
        }).unwrap_or_default()
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Struct,
    Enum,
    EnumVariant,
    Trait,
    TypeAlias,
    Const,
    Static,
    Mod,
    Local
}

impl SymbolKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Struct      => "struct",
            Self::Enum        => "enum",
            Self::EnumVariant => "variant",
            Self::Trait       => "trait",
            Self::TypeAlias   => "type",
            Self::Const       => "const",
            Self::Static      => "static",
            Self::Mod         => "mod",
            Self::Local       => "local",
        }
    }
}

pub struct SymbolTable {
    pub strings:      String,
    pub names:        Vec<Atom>,
    pub detail_start: Vec<u32>,
    pub detail_len:   Vec<u16>,
    pub kind:         Vec<SymbolKind>,
    pub buffer_id:    Vec<BufferId>,
    pub range_start:  Vec<u32>,
    pub range_end:    Vec<u32>,
    pub generation:   Vec<u32>,
    pub global_gen:   u32,

    pub by_name:   FxHashMap<Atom, SmallVec<[u32; 2]>>,
    pub by_buffer: FxHashMap<BufferId, Vec<u32>>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self {
            strings:      String::new(),
            detail_start: Vec::new(),
            detail_len:   Vec::new(),
            kind:         Vec::new(),
            buffer_id:    Vec::new(),
            range_start:  Vec::new(),
            range_end:    Vec::new(),
            generation:   Vec::new(),
            global_gen:   1,
            names:        Vec::new(),
            by_name:      FxHashMap::default(),
            by_buffer:    FxHashMap::default(),
        }
    }

    pub fn len(&self) -> usize { self.kind.len() }

    pub fn name(&self, i: usize) -> Atom {
        self.names[i]
    }

    pub fn detail(&self, i: usize) -> &str {
        let s = self.detail_start[i] as usize;
        &self.strings[s..s + self.detail_len[i] as usize]
    }

    pub fn is_alive(&self, i: usize) -> bool {
        self.generation[i] == self.global_gen
    }

    fn push_symbol(
        &mut self,
        name: Atom, detail: &str, kind: SymbolKind,
        buffer_id: BufferId, start: u32, end: u32,
    ) -> u32 {
        let ds = self.strings.len() as u32;
        self.strings.push_str(detail);

        let idx = self.kind.len() as u32;
        self.detail_start.push(ds);
        self.detail_len.push(detail.len() as u16);
        self.names.push(name);
        self.kind.push(kind);
        self.buffer_id.push(buffer_id);
        self.range_start.push(start);
        self.range_end.push(end);
        self.generation.push(self.global_gen);

        self.by_name.entry(name).or_default().push(idx);
        self.by_buffer.entry(buffer_id).or_default().push(idx);

        idx
    }

    pub fn remove_buffer(&mut self, buffer_id: BufferId) {
        let Some(indices) = self.by_buffer.remove(&buffer_id) else { return };
        for i in indices {
            self.generation[i as usize] = 0; // mark dead
            let name = self.name(i as usize);
            if let Some(refs) = self.by_name.get_mut(&name) {
                refs.retain(|r| *r != i);
                if refs.is_empty() { self.by_name.remove(&name); }
            }
        }
    }

    pub fn replace_buffer(
        &mut self,
        buffer_id: BufferId,
        new_symbols: impl IntoIterator<Item = RawSymbol>,
    ) {
        self.remove_buffer(buffer_id);
        for s in new_symbols {
            self.push_symbol(s.name, &s.detail, s.kind, buffer_id, s.range_start, s.range_end);
        }
    }

    // Prefix search - returns indices of matching live symbols
    pub fn query_prefix<'a>(&'a self, prefix: &str, atom_table: &'a AtomTable) -> impl Iterator<Item = usize> + 'a {
        let prefix_lower = prefix.to_ascii_lowercase();
        (0..self.len()).filter(move |&i| {
            self.is_alive(i) &&
            atom_table.lookup_ref(self.name(i)).starts_with(&prefix_lower)
        })
    }
}

// Intermediate type used by bg thread before inserting into table
pub struct RawSymbol {
    pub name:        Atom,
    pub detail:      String,
    pub kind:        SymbolKind,
    pub range_start: u32,
    pub range_end:   u32,
}

pub fn collect_symbols(root: Node, source: &Rope, _buffer_id: BufferId, atom_table: &AtomTable) -> Vec<RawSymbol> {
    let mut out = Vec::new();
    collect_symbols_inner(root, source, &mut out, None, atom_table);
    out
}

fn collect_symbols_inner(node: Node, source: &Rope, out: &mut Vec<RawSymbol>, parent_enum: Option<Atom>, atom_table: &AtomTable) {
    match node.kind() {
        "struct_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(RawSymbol {
                    name:        rope_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail:      "struct".into(),
                    kind:        SymbolKind::Struct,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "enum_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let enum_name = rope_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table);
                out.push(RawSymbol {
                    name:        enum_name,
                    detail:      "enum".into(),
                    kind:        SymbolKind::Enum,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });

                // Recurse into variants with parent enum name
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    collect_symbols_inner(child, source, out, Some(enum_name), atom_table);
                }

                return;
            }
        }

        "enum_variant" => {
            if let Some(name) = node.child_by_field_name("name") {
                let variant_name = rope_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table);
                let detail = parent_enum.map_or("variant".into(), |e| format!("{}::", atom_table.lookup_ref(e).as_ref()));
                out.push(RawSymbol {
                    name:        variant_name,
                    detail,
                    kind:        SymbolKind::EnumVariant,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "trait_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(RawSymbol {
                    name:        rope_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail:      "trait".into(),
                    kind:        SymbolKind::Trait,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "type_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(RawSymbol {
                    name:        rope_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail:      "type".into(),
                    kind:        SymbolKind::TypeAlias,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "const_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let detail = node.child_by_field_name("type")
                    .map(|t| rope_slice_to_string(source, t.start_byte(), t.end_byte()))
                    .unwrap_or_else(|| "const".into());
                out.push(RawSymbol {
                    name:        rope_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail: detail.into(),
                    kind:        SymbolKind::Const,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "static_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let detail = node.child_by_field_name("type")
                    .map(|t| rope_slice_to_string(source, t.start_byte(), t.end_byte()))
                    .unwrap_or_else(|| "static".into());

                out.push(RawSymbol {
                    name:        rope_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail: detail.into(),
                    kind:        SymbolKind::Static,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "mod_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(RawSymbol {
                    name:        rope_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail:      "mod".into(),
                    kind:        SymbolKind::Mod,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols_inner(child, source, out, None, atom_table);
    }
}

#[derive(Debug, Clone, Default)]
pub struct OverlayState {
    pub current: Option<Overlay>,
}

impl OverlayState {
    // Just invalidates if edit precedes current call site
    #[inline]
    pub fn on_edit(&mut self, edit: &e2_InputEdit) {
        let needs_reset = self.needs_reset(edit);

        if needs_reset {
            self.current = None;
        }
    }

    #[inline]
    pub fn needs_reset(&mut self, edit: &e2_InputEdit) -> bool {
        self.current.as_ref().map_or(
            false,
            |o| overlay_needs_reset(o, edit)
        )
    }

    #[inline]
    pub fn needs_reset_cursor_byte(&mut self, cursor_byte: u32) -> bool {
        self.current.as_ref().map_or(
            false,
            |o| overlay_needs_reset_cursor_byte(o, cursor_byte)
        )
    }
}

fn find_call_overlay(mut node: Node, cursor_byte: usize, source: &Rope, table: &AtomTable) -> Option<Overlay> {
    loop {
        if node.kind() == "call_expression" {
            let fn_node = node.child_by_field_name("function")?;
            let args    = node.child_by_field_name("arguments")?;

            let opening_paren_byte = args.start_byte() as u32;

            let mut cursor = args.walk();
            let closing_paren_byte = args
                .children(&mut cursor)
                .find(|n| n.kind() == ")")
                .and_then(|n| NonZeroU32::new(n.start_byte() as u32));

            if cursor_byte <= opening_paren_byte as usize ||
                closing_paren_byte.map_or(false, |b| cursor_byte > b.get() as usize)
            {
                node = node.parent()?;
                continue;
            }

            let call_kind = parse_call_kind(fn_node, source, table);
            let arg_index = count_arg_index(args, cursor_byte) as u16;

            return Some(Overlay { call_kind, arg_index, opening_paren_byte, closing_paren_byte });
        }

        if let Some(parent) = node.parent() {
            node = parent;
        } else {
            break;
        }
    }

    find_incomplete_call_overlay(cursor_byte, source, table)
}

fn parse_call_kind(fn_node: Node, source: &Rope, table: &AtomTable) -> CallKind {
    match fn_node.kind() {
        // bar()
        "identifier" => {
            let name = rope_slice_to_atom(source, fn_node.start_byte(), fn_node.end_byte(), table);
            CallKind::Bare(name)
        }

        // foo.bar() - field_expression
        "field_expression" => {
            let field = fn_node.child_by_field_name("field");
            let name  = field.map(|n| rope_slice_to_atom(source, n.start_byte(), n.end_byte(), table))
                .unwrap_or_default();
            CallKind::Method(name)
        }

        // Foo::bar() or foo::bar() or foo::baz::bar() - scoped_identifier
        "scoped_identifier" => {
            let name_node = fn_node.child_by_field_name("name");
            let path_node = fn_node.child_by_field_name("path");

            let name = name_node
                .map(|n| rope_slice_to_atom(source, n.start_byte(), n.end_byte(), table))
                .unwrap_or_default();

            match path_node {
                None => CallKind::Bare(name),
                Some(path) if path.kind() == "identifier" => {
                    let prefix = rope_slice_to_atom(source, path.start_byte(), path.end_byte(), table);
                    CallKind::AssocOrPath(prefix, name)
                }
                Some(path) => {
                    // multi-segment: foo::baz::bar
                    let mut segments = collect_path_segments(path, source, table);
                    // Prepend zeroes until len == 4
                    while segments.len() < 4 {
                        segments.insert(0, Atom(0));
                    }
                    let last4 = segments[segments.len() - 4..]
                        .try_into()
                        .unwrap();
                    CallKind::FullPath(last4, name)
                }
            }
        }

        _ => {
            // fallback: grab the last identifier in whatever expression this is
            let name = rope_slice_to_atom(source, fn_node.start_byte(), fn_node.end_byte(), table);
            CallKind::Method(name)
        }
    }
}

fn collect_path_segments(node: Node, source: &Rope, table: &AtomTable) -> Vec<Atom> {
    let mut segments = Vec::new();
    collect_path_segments_inner(node, source, table, &mut segments);
    segments
}

fn collect_path_segments_inner(node: Node, source: &Rope, table: &AtomTable, out: &mut Vec<Atom>) {
    match node.kind() {
        "scoped_identifier" => {
            if let Some(path) = node.child_by_field_name("path") {
                collect_path_segments_inner(path, source, table, out);
            }
            if let Some(name) = node.child_by_field_name("name") {
                out.push(rope_slice_to_atom(source, name.start_byte(), name.end_byte(), table));
            }
        }

        "identifier" => {
            out.push(rope_slice_to_atom(source, node.start_byte(), node.end_byte(), table));
        }

        _ => {}
    }
}

fn count_arg_index(args_node: Node, cursor_byte: usize) -> usize {
    let mut cursor = args_node.walk();
    let mut index  = 0;
    for child in args_node.children(&mut cursor) {
        if child.kind() == "," && child.start_byte() < cursor_byte {
            index += 1;
        }
    }
    index
}

pub fn lookup_overlay<'a>(
    result: &Overlay,
    func_table:  &'a FunctionTable,
    _atom_table: &AtomTable
) -> Option<&'a FunctionInfo> {
    let func_ref = match &result.call_kind {
        CallKind::Bare(name)              => func_table.resolve_bare(*name),
        CallKind::AssocOrPath(prefix, name) => func_table.resolve_assoc(*prefix, *name),
        CallKind::FullPath(path, name)    => func_table.resolve_path(path, *name),
        CallKind::Method(name)            => func_table.resolve_method(*name),
    };

    let func_ref = func_ref.or_else(|| {
        func_table.resolve_bare(result.call_kind.function_name())
    });

    let func_ref = func_ref?;

    func_table.get(func_ref)
}

pub fn overlay_needs_reset(o: &Overlay, edit: &e2_InputEdit) -> bool {
                                              edit.start_byte < o.opening_paren_byte ||
    o.closing_paren_byte.map_or(false, |byte| edit.start_byte > byte.get())
}

pub fn overlay_needs_reset_cursor_byte(o: &Overlay, cursor_byte: u32) -> bool {
                                              cursor_byte < o.opening_paren_byte ||
    o.closing_paren_byte.map_or(false, |byte| cursor_byte > byte.get())
}

//
//
// Incomplete tree lookups!
//
//

fn find_incomplete_call_overlay(
    cursor_byte: usize,
    source: &Rope,
    table: &AtomTable,
) -> Option<Overlay> {
    let cursor_byte = cursor_byte.min(source.len_bytes());

    let open_paren_byte = find_opening_paren_before_cursor(source, cursor_byte)?;
    let callee = extract_callee_before_paren(source, open_paren_byte)?;

    if is_definition_context(source, callee.start_byte) {
        return None;
    }

    let call_kind = parse_incomplete_call_kind(&callee.text, callee.start_byte, table)?;
    let arg_index = count_incomplete_arg_index(source, open_paren_byte, cursor_byte)?;

    Some(Overlay {
        call_kind,
        arg_index: arg_index as u16,
        opening_paren_byte: open_paren_byte as u32,
        closing_paren_byte: None,
    })
}

fn is_definition_context(source: &Rope, callee_start_byte: usize) -> bool {
    let start = callee_start_byte.saturating_sub(32);
    let window = source.byte_slice(start..callee_start_byte).to_string();

    let trimmed = window.trim_end();

    //
    // Check if it ends with a definition keyword
    //
    for keyword in &["fn", "struct", "enum", "trait", "impl", "macro_rules!", "type", "mod"] { // @Speed
        if trimmed == *keyword || trimmed.ends_with(&format!(" {}", keyword))
            || trimmed.ends_with(&format!("\t{}", keyword))
            || trimmed.ends_with(&format!("\n{}", keyword))
        {
            return true;
        }
    }

    false
}

struct CalleeSpan {
    text: Box<str>,
    start_byte: usize,
}

const LOOKBACK_CHARS: usize = 1024;

thread_local! {
    static SCRATCH_CHARS: UnsafeCell<SmallVec<[char; LOOKBACK_CHARS]>> = Default::default();
    static SCRATCH_BYTES: UnsafeCell<SmallVec<[u8;   LOOKBACK_CHARS]>> = Default::default();
}
macro_rules! with_scratch_chars {
    (let $chars:ident; $($tt:tt)*) => {
        SCRATCH_CHARS.with(|$chars| {
            let $chars: &mut SmallVec<[char; LOOKBACK_CHARS]> = unsafe { &mut *($chars.get() as *mut _) };
            $($tt)*
        })
    };
}
macro_rules! with_scratch_bytes {
    (let $chars:ident; $($tt:tt)*) => {
        SCRATCH_BYTES.with(|$chars| {
            let $chars: &mut SmallVec<[u8; LOOKBACK_CHARS]> = unsafe { &mut *($chars.get() as *mut _) };
            $($tt)*
        })
    };
}

fn find_opening_paren_before_cursor( // @Memory
    source: &Rope,
    cursor_byte: usize,
) -> Option<usize> {
    with_scratch_bytes! {
        let scratch_bytes;
        find_opening_paren_before_cursor_impl(source, cursor_byte, scratch_bytes)
    }
}

fn find_opening_paren_before_cursor_impl(
    source: &Rope,
    cursor_byte: usize,
    scratch_bytes: &mut SmallVec<[u8; LOOKBACK_CHARS]>,
) -> Option<usize> {
    let start = cursor_byte.saturating_sub(LOOKBACK_CHARS);

    scratch_bytes.clear();
    scratch_bytes.extend(source.byte_slice(start..cursor_byte).bytes());

    let mut depth = 0isize;

    for i in (0..scratch_bytes.len()).rev() {
        match scratch_bytes[i] {
            b')' => depth += 1,
            b'(' => {
                if depth == 0 {
                    return Some(start + i);
                }
                depth -= 1;
            }

            _ => {}
        }
    }

    None
}

fn top_level_top_boundary(a: i32, b: i32, c: i32) -> bool {
    a == 0 && b == 0 && c == 0
}

fn is_escaped(chars: &[char], idx: usize) -> bool {
    let mut backslashes = 0usize;
    let mut i = idx;
    while i > 0 {
        if chars[i - 1] == '\\' {
            backslashes += 1;
            i -= 1;
        } else {
            break;
        }
    }
    backslashes % 2 == 1
}

fn looks_like_char_literal_start(chars: &[char], idx: usize) -> bool {
    // Very small heuristic: `'x'`, `'\n'`, etc.
    if idx + 1 >= chars.len() {
        return false;
    }

    let next = chars[idx + 1];
    next != '\'' && next != '\n'
}

fn parse_incomplete_call_kind(expr: &str, expr_start_byte: usize, table: &AtomTable) -> Option<CallKind> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }

    if expr.contains("::") {
        let parts: SmallVec<[&str; 4]> = expr.split("::")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();

        if parts.is_empty() {
            return None;
        }

        let name = parts.last().copied()?;
        let name_start = expr.rfind(name)?;
        let name_atom = rope_slice_to_atom_from_str(
            expr, name_start, name_start + name.len(),
            expr_start_byte,
            table
        );

        match parts.len() {
            1 => Some(CallKind::Bare(name_atom)),

            2 => {
                let prefix = parts[0];
                let prefix_start = expr.find(prefix)?;
                let prefix_atom = rope_slice_to_atom_from_str(
                    expr, prefix_start, prefix_start + prefix.len(), expr_start_byte,
                    table
                );
                Some(CallKind::AssocOrPath(prefix_atom, name_atom))
            }

            _ => {
                let mut segs = Vec::with_capacity(4);
                let mut pos = 0usize;
                for part in &parts[..parts.len() - 1] {
                    let rel = expr[pos..].find(part)?;
                    let s = pos + rel;
                    let e = s + part.len();
                    segs.push(rope_slice_to_atom_from_str(expr, s, e, expr_start_byte, table));
                    pos = e + 2;
                }

                while segs.len() < 4 {
                    segs.insert(0, Atom(0));
                }

                let last4: [Atom; 4] = segs[segs.len() - 4..].try_into().ok()?;
                Some(CallKind::FullPath(last4, name_atom))
            }
        }

    } else if expr.contains('.') {
        let name = expr.split('.').last()?.trim();
        if name.is_empty() {
            return None;
        }

        let name_start = expr.rfind(name)?;
        let atom = rope_slice_to_atom_from_str(expr, name_start, name_start + name.len(), expr_start_byte, table);
        Some(CallKind::Method(atom))

    } else {
        let name = expr.split_whitespace().last()?.trim();
        if name.is_empty() {
            return None;
        }

        let name_start = expr.rfind(name)?;
        let atom = rope_slice_to_atom_from_str(expr, name_start, name_start + name.len(), expr_start_byte, table);
        Some(CallKind::Bare(atom))
    }
}

fn rope_slice_to_atom_from_str(
    expr: &str,
    start: usize,
    end: usize,
    _expr_start_byte: usize,
    table: &AtomTable,
) -> Atom {
    let s = &expr[start..end];
    table.intern(s)
}

fn extract_callee_before_paren(
    source: &Rope,
    open_paren_byte: usize,
) -> Option<CalleeSpan> {
    with_scratch_chars! {
        let scratch_chars;
        extract_callee_before_paren_impl(source, open_paren_byte, scratch_chars)
    }
}

fn extract_callee_before_paren_impl(
    source: &Rope,
    open_paren_byte: usize,
    scratch_chars: &mut SmallVec<[char; LOOKBACK_CHARS]>
) -> Option<CalleeSpan> {
    let start = open_paren_byte.saturating_sub(LOOKBACK_CHARS);
    let rope_slice = source.byte_slice(start..open_paren_byte);

    let chars = scratch_chars;
    chars.clear();
    chars.extend(rope_slice.chars());
    if chars.is_empty() {
        return None;
    }

    let mut end = chars.len();
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return None;
    }

    let mut paren_depth   = 0i32;
    let mut bracket_depth = 0i32;
    let mut brace_depth   = 0i32;
    let mut angle_depth   = 0i32;

    let mut in_line_comment     = false;
    let mut block_comment_depth = 0usize;
    let mut in_string           = false;
    let mut in_char             = false;
    let mut raw_string_hashes   = None;
    let mut in_pipe_params      = false;

    let mut i = end;

    while i > 0 {
        if consume_rust_syntax_backward(
            &chars,
            &mut i,
            &mut in_line_comment,
            &mut block_comment_depth,
            &mut in_string,
            &mut in_char,
            &mut raw_string_hashes,
        ) {
            continue;
        }

        let ch = chars[i - 1];
        match ch {
            ')' => paren_depth += 1,
            ']' => bracket_depth += 1,
            '}' => brace_depth += 1,
            '>' => angle_depth += 1,

            '(' => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                } else if top_level_top_boundary(bracket_depth, brace_depth, angle_depth) {
                    break;
                }
            }
            '[' => {
                if bracket_depth > 0 {
                    bracket_depth -= 1;
                } else if top_level_top_boundary(paren_depth, brace_depth, angle_depth) {
                    break;
                }
            }
            '{' => {
                if brace_depth > 0 {
                    brace_depth -= 1;
                } else if top_level_top_boundary(paren_depth, bracket_depth, angle_depth) {
                    break;
                }
            }
            '<' => {
                if angle_depth > 0 {
                    angle_depth -= 1;
                } else if top_level_top_boundary(paren_depth, bracket_depth, brace_depth) {
                    break;
                }
            }

            '|' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && brace_depth == 0 && angle_depth == 0 => {
                // Heuristic: treat `|...|` closure params as one unit.
                in_pipe_params = !in_pipe_params;
            }

            ',' | ';' | '=' | '+' | '-' | '*' | '/' | '%' | '&' | '^' | '?'
                if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 && !in_pipe_params =>
            {
                break;
            }

            ':' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 && !in_pipe_params => {
                if i >= 2 && chars[i - 2] == ':' {
                    i -= 2;
                    continue;
                }
                break;
            }

            '.' | '!' | '#' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 => {
                // allowed in `foo.bar`, `foo!`, `r#foo`
            }

            c if c.is_whitespace() && paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 && !in_pipe_params => {
                break;
            }

            _ => {}
        }

        i -= 1;
    }

    let start_byte_rel = i;

    let text: Box<str> = rope_slice.byte_slice(start_byte_rel..end).to_string().trim().into();
    if text.is_empty() {
        return None;
    }

    Some(CalleeSpan {
        text,
        start_byte: start + start_byte_rel,
    })
}

fn count_incomplete_arg_index(
    source: &Rope,
    open_paren_byte: usize,
    cursor_byte: usize,
) -> Option<usize> {
    with_scratch_chars! {
        let scratch_chars;
        count_incomplete_arg_index_impl(source, open_paren_byte, cursor_byte, scratch_chars)
    }
}

fn count_incomplete_arg_index_impl(
    source: &Rope,
    open_paren_byte: usize,
    cursor_byte: usize,
    scratch_chars: &mut SmallVec<[char; LOOKBACK_CHARS]>
) -> Option<usize> {
    let cursor_byte = cursor_byte.min(source.len_bytes());
    if cursor_byte <= open_paren_byte {
        return Some(0);
    }

    let rope_slice = source.byte_slice(open_paren_byte + 1..cursor_byte);
    let chars = scratch_chars;

    chars.clear();
    chars.extend(rope_slice.chars());

    let mut paren_depth   = 0i32;
    let mut bracket_depth = 0i32;
    let mut brace_depth   = 0i32;
    let mut angle_depth   = 0i32;

    let mut in_line_comment     = false;
    let mut block_comment_depth = 0usize;
    let mut in_string           = false;
    let mut in_char             = false;
    let mut raw_string_hashes   = None;
    let mut in_pipe_params      = false;

    let mut commas = 0usize;
    let mut i      = 0usize;

    while i < chars.len() {
        if consume_rust_syntax_forward(
            &chars,
            &mut i,
            &mut in_line_comment,
            &mut block_comment_depth,
            &mut in_string,
            &mut in_char,
            &mut raw_string_hashes,
        ) {
            continue;
        }

        let ch = chars[i];
        match ch {
            '(' => paren_depth += 1,
            ')' => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                }
            }
            '[' => bracket_depth += 1,
            ']' => {
                if bracket_depth > 0 {
                    bracket_depth -= 1;
                }
            }
            '{' => brace_depth += 1,
            '}' => {
                if brace_depth > 0 {
                    brace_depth -= 1;
                }
            }
            '<' => angle_depth += 1,
            '>' => {
                if angle_depth > 0 {
                    angle_depth -= 1;
                }
            }

            '|' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 => {
                in_pipe_params = !in_pipe_params;
            }

            ',' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 && angle_depth == 0 && !in_pipe_params => {
                commas += 1;
            }

            _ => {}
        }

        i += 1;
    }

    Some(commas)
}

fn consume_rust_syntax_backward(
    chars: &[char],
    i: &mut usize,

    in_line_comment: &mut bool,
    block_comment_depth: &mut usize,
    in_string: &mut bool,
    in_char: &mut bool,
    raw_string_hashes: &mut Option<usize>,
) -> bool {
    if *i < 1 {
        return false;
    }

    let ch = chars[*i - 1];

    if *in_line_comment {
        if ch == '\n' {
            *in_line_comment = false;
        }

        *i -= 1;
        return true;
    }

    if *block_comment_depth > 0 {
        if ch == '/' && *i >= 2 && chars[*i - 2] == '*' {
            *block_comment_depth -= 1;
            *i -= 2;
            return true;
        }

        if ch == '*' && *i >= 2 && chars[*i - 2] == '/' {
            *block_comment_depth += 1;
            *i -= 2;
            return true;
        }

        *i -= 1;
        return true;
    }

    if let Some(hashes) = *raw_string_hashes {
        if ch == '"' {
            let mut ok = true;

            for k in 0..hashes {
                if *i + k >= chars.len() || chars[*i + k] != '#' {
                    ok = false;
                    break;
                }
            }

            if ok {
                *raw_string_hashes = None;
                *i -= 1;
                return true;
            }
        }

        *i -= 1;
        return true;
    }

    if *in_string {
        if ch == '"' && !is_escaped(chars, *i - 1) {
            *in_string = false;
        }

        *i -= 1;
        return true;
    }

    if *in_char {
        if ch == '\'' && !is_escaped(chars, *i - 1) {
            *in_char = false;
        }

        *i -= 1;
        return true;
    }

    if ch == '/' && *i >= 2 {
        let prev = chars[*i - 2];

        if prev == '/' {
            *in_line_comment = true;
            *i -= 2;
            return true;
        }

        if prev == '*' {
            *block_comment_depth += 1;
            *i -= 2;
            return true;
        }
    }

    if ch == '"' {
        let mut hashes = 0usize;
        let mut j = *i - 1;

        while j > 0 && chars[j - 1] == '#' {
            hashes += 1;
            j -= 1;
        }

        if j > 0 && chars[j - 1] == 'r' {
            *raw_string_hashes = Some(hashes);
            *i -= 1;
            return true;
        } else {
            *in_string = true;
            *i -= 1;
            return true;
        }
    }

    if ch == '\'' && looks_like_char_literal_start(chars, *i - 1) {
        *in_char = true;
        *i -= 1;
        return true;
    }

    false
}

fn consume_rust_syntax_forward(
    chars: &[char],
    i: &mut usize,

    in_line_comment: &mut bool,
    block_comment_depth: &mut usize,
    in_string: &mut bool,
    in_char: &mut bool,
    raw_string_hashes: &mut Option<usize>,
) -> bool {
    if *i >= chars.len() {
        return false;
    }

    let ch = chars[*i];

    if *in_line_comment {
        if ch == '\n' {
            *in_line_comment = false;
        }
        *i += 1;
        return true;
    }

    if *block_comment_depth > 0 {
        if ch == '/' && *i + 1 < chars.len() && chars[*i + 1] == '*' {
            *block_comment_depth += 1;
            *i += 2;
            return true;
        }

        if ch == '*' && *i + 1 < chars.len() && chars[*i + 1] == '/' {
            *block_comment_depth -= 1;
            *i += 2;
            return true;
        }

        *i += 1;
        return true;
    }

    if let Some(hashes) = *raw_string_hashes {
        if ch == '"' {
            let mut ok = true;

            for k in 0..hashes {
                if *i + 1 + k >= chars.len() || chars[*i + 1 + k] != '#' {
                    ok = false;
                    break;
                }
            }

            if ok {
                *raw_string_hashes = None;
                *i += 1 + hashes;
                return true;
            }
        }

        *i += 1;
        return true;
    }

    if *in_string {
        if ch == '"' && !is_escaped(chars, *i) {
            *in_string = false;
        }

        *i += 1;
        return true;
    }

    if *in_char {
        if ch == '\'' && !is_escaped(chars, *i) {
            *in_char = false;
        }

        *i += 1;
        return true;
    }

    if ch == '/' && *i + 1 < chars.len() {
        let next = chars[*i + 1];

        if next == '/' {
            *in_line_comment = true;
            *i += 2;
            return true;
        }

        if next == '*' {
            *block_comment_depth += 1;
            *i += 2;
            return true;
        }
    }

    if ch == '"' {
        let mut hashes = 0usize;
        let mut j = *i;

        while j > 0 && chars[j - 1] == '#' {
            hashes += 1;
            j -= 1;
        }

        if j > 0 && chars[j - 1] == 'r' {
            *raw_string_hashes = Some(hashes);
            *i += 1;
            return true;
        }

        *in_string = true;
        *i += 1;
        return true;
    }

    if ch == '\'' && looks_like_char_literal_start(chars, *i) {
        *in_char = true;
        *i += 1;
        return true;
    }

    false
}

//
// Shared predicates
//

pub fn is_definition_node(node: Node) -> bool {
    if !node.is_named() { return false; }
    matches!(node.kind(),
        | "function_item"
        | "impl_item"
        | "mod_item"
        | "trait_item"
        | "struct_item"
        | "enum_item"
        | "static_item"
        | "const_item"
        | "macro_definition"
        | "type_item"
    )
}

pub fn is_scope_node(node: Node) -> bool {
    if !node.is_named() { return false; }
    matches!(node.kind(),
        | "function_item"
        | "impl_item"
        | "mod_item"
        | "trait_item"
        | "struct_item"
        | "enum_item"
        | "static_item"
        | "const_item"
        | "macro_definition"
        | "macro_invocation"
        | "block"
        | "unsafe_block"
        | "declaration_list"
        | "for_expression"
        | "while_expression"
        | "loop_expression"
        | "if_expression"
        | "else_clause"
        | "match_expression"
        | "match_arm"
        | "closure_expression"
    )
}

pub fn scope_end_byte(node: Node) -> usize {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(),
            | "block"
            | "declaration_list"
            | "enum_variant_list"
            | "field_declaration_list"
            | "token_tree"
        ) {
            return child.end_byte().saturating_sub(1);
        }
    }
    node.end_byte().saturating_sub(1)
}

//
// Core traversal primitives - O(depth + distance), no allocation
//

// Walk forward in document order from `node`, skipping `node` itself.
// Visits children before siblings (pre-order).
pub fn next_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    // descend into first child if any
    if node.child_count() > 0 {
        return node.child(0);
    }
    // otherwise walk up until we find a next sibling
    ascend_to_next_sibling(node)
}

pub fn ascend_to_next_sibling(mut node: Node) -> Option<Node> {
    loop {
        if let Some(sib) = node.next_sibling() {
            return Some(sib);
        }
        node = node.parent()?;
    }
}

// Walk backward in document order from `node`, skipping `node` itself.
// This is the reverse pre-order: prev sibling's rightmost leaf, then parent.
pub fn prev_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    if let Some(sib) = node.prev_sibling() {
        // rightmost leaf of the previous sibling
        return Some(rightmost_leaf(sib));
    }
    node.parent()
}

pub fn rightmost_leaf(mut node: Node) -> Node {
    loop {
        let c = node.child_count();
        if c == 0 { return node; }
        node = node.child((c - 1) as _).unwrap();
    }
}

pub fn next_named_matching<'a, F>(start: Node<'a>, pred: F) -> Option<Node<'a>>
where F: Fn(Node<'a>) -> bool {
    let mut cur = next_node(start);
    while let Some(n) = cur {
        if n.is_named() && pred(n) { return Some(n); }
        cur = next_node(n);
    }
    None
}

pub fn prev_named_matching<'a, F>(start: Node<'a>, pred: F) -> Option<Node<'a>>
where F: Fn(Node<'a>) -> bool {
    let mut cur = prev_node(start);
    while let Some(n) = cur {
        if n.is_named() && pred(n) { return Some(n); }
        cur = prev_node(n);
    }
    None
}

pub fn find_enclosing<'a, F>(mut node: Node<'a>, pred: F) -> Option<Node<'a>>
where F: Fn(Node<'a>) -> bool {
    loop {
        node = node.parent()?;
        if pred(node) { return Some(node); }
    }
}

//
// jump_definition_prev / next
//

pub fn jump_definition_next(root: Node, cursor_byte: usize) -> Option<usize> {
    let leaf = root.descendant_for_byte_range(cursor_byte, cursor_byte)?;

    // Start search from the node whose start is strictly after cursor
    let start = if leaf.start_byte() > cursor_byte { leaf } else {
        // Find first node that starts after cursor
        let mut n = leaf;
        loop {
            match next_node(n) {
                Some(next) if next.start_byte() > cursor_byte => { n = next; break; }
                Some(next) => n = next,
                None => return None,
            }
        }
        n
    };

    if is_definition_node(start) {
        return Some(start.start_byte());
    }

    next_named_matching(start, is_definition_node).map(|n| n.start_byte())
}

pub fn jump_definition_prev(root: Node, cursor_byte: usize) -> Option<usize> {
    let leaf = root.descendant_for_byte_range(cursor_byte, cursor_byte)?;
    prev_named_matching(leaf, |n| is_definition_node(n) && n.start_byte() < cursor_byte)
        .map(|n| n.start_byte())
}

//
// jump_sexp_prev / next
//

pub fn jump_scope_next(root: Node, cursor_byte: usize) -> Option<usize> {
    let leaf = root.descendant_for_byte_range(cursor_byte, cursor_byte)?;
    if let Some(n) = next_named_matching(leaf, |n| is_scope_node(n) && n.start_byte() > cursor_byte) {
        return Some(n.start_byte());
    }
    // no next scope - jump to end of enclosing scope
    find_enclosing(leaf, is_scope_node).map(scope_end_byte)
}

pub fn jump_scope_prev(root: Node, cursor_byte: usize) -> Option<usize> {
    let leaf = root.descendant_for_byte_range(cursor_byte, cursor_byte)?;
    if let Some(n) = prev_named_matching(leaf, |n| is_scope_node(n) && n.start_byte() < cursor_byte) {
        return Some(n.start_byte());
    }
    // no prev scope - jump to start of enclosing scope
    find_enclosing(leaf, is_scope_node).map(|n| n.start_byte())
}

//
// jump_matching_delim_backward / forward O(depth)
//

pub fn jump_matching_delim_forward(root: Node, cursor_byte: usize) -> Option<usize> {
    let node = root.descendant_for_byte_range(cursor_byte, cursor_byte)?;
    if let Some(close) = matching_close_from(node, cursor_byte) {
        return Some(close);
    }
    find_enclosing_delim_end(node)
}

pub fn jump_matching_delim_backward(root: Node, cursor_byte: usize) -> Option<usize> {
    let node = root.descendant_for_byte_range(
        cursor_byte.saturating_sub(1),
        cursor_byte.saturating_sub(1),
    )?;
    if let Some(open) = matching_open_from(node, cursor_byte) {
        return Some(open);
    }
    find_enclosing_delim_start(node)
}

fn matching_close_from(mut node: Node, cursor_byte: usize) -> Option<usize> {
    loop {
        if let Some(end) = child_open_delim(node, cursor_byte) { return Some(end); }
        node = node.parent()?;
    }
}

fn matching_open_from(mut node: Node, cursor_byte: usize) -> Option<usize> {
    loop {
        if let Some(start) = child_close_delim(node, cursor_byte) { return Some(start); }
        node = node.parent()?;
    }
}

fn find_enclosing_delim_end(mut node: Node) -> Option<usize> {
    loop {
        if let Some(end) = last_close_child(node) { return Some(end); }
        node = node.parent()?;
    }
}

fn find_enclosing_delim_start(mut node: Node) -> Option<usize> {
    loop {
        if let Some(start) = first_open_child(node) { return Some(start); }
        node = node.parent()?;
    }
}

fn child_open_delim(node: Node, at_byte: usize) -> Option<usize> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() { return None; }
    loop {
        let c = cursor.node();
        if matches!(c.kind(), "(" | "[" | "{") && c.start_byte() == at_byte {
            // found open - now find the matching close by walking rest of children
            while cursor.goto_next_sibling() {
                let c = cursor.node();
                if matches!(c.kind(), ")" | "]" | "}") {
                    return Some(c.end_byte());
                }
            }
            return None;
        }
        if !cursor.goto_next_sibling() { break; }
    }
    None
}

fn child_close_delim(node: Node, at_byte: usize) -> Option<usize> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() { return None; }
    let mut open_byte = None;
    loop {
        let c = cursor.node();
        if matches!(c.kind(), "(" | "[" | "{") {
            open_byte = Some(c.start_byte());
        }
        if matches!(c.kind(), ")" | "]" | "}") && c.end_byte() == at_byte {
            return open_byte;
        }
        if !cursor.goto_next_sibling() { break; }
    }
    None
}

fn first_open_child(node: Node) -> Option<usize> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() { return None; }
    loop {
        let c = cursor.node();
        if matches!(c.kind(), "(" | "[" | "{") { return Some(c.start_byte()); }
        if !cursor.goto_next_sibling() { break; }
    }
    None
}

fn last_close_child(node: Node) -> Option<usize> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() { return None; }
    let mut found = None;
    loop {
        let c = cursor.node();
        if matches!(c.kind(), ")" | "]" | "}") { found = Some(c.end_byte()); }
        if !cursor.goto_next_sibling() { break; }
    }
    found
}

// @Incomplete: Skip incomplete let's that the user might be typing right now.
pub fn collect_locals_at_cursor(
    root:        Node,
    cursor_byte: usize,
    source:      &Rope,
    atom_table:  &AtomTable
) -> Vec<CompletionItem> {
    let mut out = Vec::new();

    // Find the leaf at cursor then walk up collecting bindings
    let Some(mut node) = root.descendant_for_byte_range(cursor_byte, cursor_byte) else {
        return out;
    };

    loop {
        match node.kind() {
            "block" | "source_file" => {
                //
                // Scan named children of this block that come BEFORE cursor
                //
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.start_byte() >= cursor_byte { break; }
                    match child.kind() {
                        "let_declaration" => {
                            if let Some(pat) = child.child_by_field_name("pattern") {
                                collect_pattern_bindings(pat, source, &mut out, atom_table);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "match_arm" => {
                if let Some(pat) = node.child_by_field_name("pattern") {
                    //
                    // Only collect if cursor is inside this arm's body
                    //
                    if let Some(body) = node.child_by_field_name("value") {
                        if cursor_byte >= body.start_byte() && cursor_byte <= body.end_byte() {
                            collect_pattern_bindings(pat, source, &mut out, atom_table);
                        }
                    }
                }
            }
            "function_item" => {
                //
                // Collect parameters
                //
                if let Some(params) = node.child_by_field_name("parameters") {
                    let mut cursor = params.walk();
                    for param in params.named_children(&mut cursor) {
                        if param.kind() != "parameter" { continue; }
                        if let Some(pat) = param.child_by_field_name("pattern") {
                            collect_pattern_bindings(pat, source, &mut out, atom_table);
                        }
                    }
                }
            }
            "for_expression" => {
                //
                // For x in ... - collect the pattern
                //
                if let Some(pat) = node.child_by_field_name("pattern") {
                    collect_pattern_bindings(pat, source, &mut out, atom_table);
                }
            }
            "closure_expression" => {
                if let Some(params) = node.child_by_field_name("parameters") {
                    let mut cursor = params.walk();
                    for param in params.named_children(&mut cursor) {
                        collect_pattern_bindings(param, source, &mut out, atom_table);
                    }
                }
            }
            "if_expression" | "while_expression" => {
                //
                // if let Some(x) = ... / while let Some(x) = ...
                //
                if let Some(cond) = node.child_by_field_name("condition") {
                    if cond.kind() == "let_condition" {
                        if let Some(pat) = cond.child_by_field_name("pattern") {
                            collect_pattern_bindings(pat, source, &mut out, atom_table);
                        }
                    }
                }
            }
            _ => {}
        }

        match node.parent() {
            Some(p) => node = p,
            None    => break,
        }
    }

    out
}

fn collect_pattern_bindings(pat: Node, source: &Rope, out: &mut Vec<CompletionItem>, atom_table: &AtomTable) {
    match pat.kind() {
        "identifier" => {
            let name = rope_slice_to_atom(source, pat.start_byte(), pat.end_byte(), atom_table);
            if atom_table.lookup_ref(name).as_ref() == "_" { return; }
            out.push(CompletionItem {
                name: name,
                detail: "let".into(),
                kind:   CompletionItemKind::Local,
                score:  0,
            });
        }

        "tuple_pattern" | "struct_pattern" | "tuple_struct_pattern" => {
            let mut cursor = pat.walk();
            for child in pat.named_children(&mut cursor) {
                collect_pattern_bindings(child, source, out, atom_table);
            }
        }

        "ref_pattern" | "mut_pattern" => {
            if let Some(inner) = pat.named_child(0) {
                collect_pattern_bindings(inner, source, out, atom_table);
            }
        }

        _ => {}
    }
}

pub fn extract_prefix_at_cursor(buf: &Buffer, cursor_char: usize) -> (usize, String) {
    let mut start = cursor_char;
    while start > 0 {
        let c = buf.text.char(start - 1);
        if c.is_alphanumeric() || c == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    let prefix: String = buf.text.slice(start..cursor_char).to_string();
    (start, prefix)
}

pub fn query_completions(
    prefix:       &str,
    sym_table:    &SymbolTable,
    func_table:   &FunctionTable,
    atom_table:   &AtomTable,
    _buffer_id:    BufferId,
    cursor_byte: usize,
    tree:        Option<&Tree>,
    source:      &Rope,
    limit:        usize,
) -> Vec<CompletionItem> {
    if prefix.is_empty() { return vec![]; }

    let prefix_lower = prefix.to_ascii_lowercase();
    let mut items: Vec<CompletionItem> = Vec::new();

    // symbols
    for i in sym_table.query_prefix(prefix, atom_table) {
        let name   = sym_table.name(i);
        let detail: Box<str> = sym_table.detail(i).into(); // @Memory
        let kind   = sym_table.kind[i];
        let score  = score_completion(&atom_table.lookup_ref(name), prefix);
        items.push(CompletionItem {
            name,
            detail,
            kind: kind.into(),
            score,
        });
        if items.len() >= limit * 2 { break; }
    }

    // functions from func_table
    for (_, refs) in &func_table.by_name {
        for &r in refs.as_slice() {
            let Some(info) = func_table.get(r) else { continue };
            let name = atom_table.lookup_ref(info.name);
            if !name.to_ascii_lowercase().starts_with(&prefix_lower) { continue; }
            let detail: Box<str> = format_fn_detail(info, atom_table).into();
            let score  = score_completion(&name, prefix);
            items.push(CompletionItem {
                name: info.name,
                detail,
                kind: if info.impl_name.is_some() {
                    CompletionItemKind::Function // method
                } else {
                    CompletionItemKind::Function
                },
                score,
            });
            if items.len() >= limit * 2 { break; }
        }
    }

    // Locals from scope walk
    if let Some(tree) = tree {
        let locals = collect_locals_at_cursor(
            tree.root_node(),
            cursor_byte,
            source,
            atom_table
        );
        let prefix_lower = prefix.to_ascii_lowercase();
        for mut item in locals {
            let name = atom_table.lookup_ref(item.name);
            if !name.to_ascii_lowercase().starts_with(&prefix_lower) { continue; }
            item.score = score_completion(&name, prefix) + 5; // + 5 for locals
            items.push(item);
        }
    }

    // sort by score, deduplicate by name
    items.sort_unstable_by_key(|i| i.score);
    items.dedup_by(|a, b| a.name == b.name);
    items.truncate(limit);
    items
}

fn score_completion(name: &str, prefix: &str) -> u32 {
    // exact prefix match scores 0 (best)
    // then by length difference
    // then alphabetical
    if name == prefix { return 0; }
    let len_diff = (name.len() as i32 - prefix.len() as i32).unsigned_abs();
    len_diff + 1
}

fn format_fn_detail(info: &FunctionInfo, atom_table: &AtomTable) -> String {
    let mut s = String::from("(");
    for (i, param) in info.params.iter().enumerate() {
        if i > 0 { s.push_str(", "); }
        s.push_str(atom_table.lookup_ref(param.name).as_str());
        s.push_str(": ");
        s.push_str(atom_table.lookup_ref(param.type_str).as_str());
    }
    s.push(')');
    s
}
