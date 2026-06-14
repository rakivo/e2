use crate::buffer::{Buffer, e2_Point, e2_InputEdit};
use crate::{BufferId, CompletionItem};
use crate::atum::{Atom, AtomTable};

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::thread;
use std::fmt::Write;

use arrayvec::ArrayString;
use cranelift_entity::PrimaryMap;
use crossbeam_channel::{Receiver, Sender};
use dashmap::DashMap;
use editor_helpers::tprint;
use piece_tree::PieceTree;
use rustc_hash::FxHashMap;
use smallstr::SmallString;
use smallvec::SmallVec;
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

pub const fn point_to_ts(e2: e2_Point) -> Point {
    Point {
        column: e2.column as _,
        row: e2.row as _,
    }
}

pub const fn edit_to_ts(e2: e2_InputEdit) -> InputEdit {
    InputEdit {
        start_byte:       e2.start_byte as _,
        old_end_byte:     e2.old_end_byte as _,
        new_end_byte:     e2.new_end_byte as _,
        start_position:   point_to_ts(e2.start_position),
        old_end_position: point_to_ts(e2.old_end_position),
        new_end_position: point_to_ts(e2.new_end_position),
    }
}

pub enum ParserMessage {
    Initialize {
        buffer_id: BufferId,
        tree: PieceTree,
        force: bool,
        buffer_last_edit_generation: u64,
    },

    Reparse {
        buffer_id: BufferId,
        tree: PieceTree,
        old_tree: Tree,
        buffer_last_edit_generation: u64,
    },
}

pub enum ParserQuery {
    FuncCallOverlay {
        buffer_id: BufferId,
        cursor_byte: usize,
        tree: PieceTree,
    },
}

pub enum ParseResultKind {
    SymbolsUpdate {
        functions: Vec<FunctionInfo>,
        symbols: Vec<RawSymbol>,
        words: WordTable,
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

pub type WordTables = HashMap<BufferId, WordTable, nohash_hasher::BuildNoHashHasher<BufferId>>;

pub struct TreeSitter {
    pub message_tx: Sender<ParserMessage>,
    pub query_tx:   Sender<ParserQuery>,

    pub result: Receiver<ParseResult>,

    // Shared tables for main-thread lookups
    pub atom_table: Arc<AtomTable>,
    pub trees:      Arc<DashMap<BufferId, VersionedTree>>,

    pub func_table: FunctionTable,
    pub symbol_table: SymbolTable,
    pub word_tables: WordTables,
}

impl TreeSitter {
    #[inline]
    pub fn query_cursor_overlay_without_sending(&self, buffer_id: BufferId, cursor_byte: usize, piece_tree: &PieceTree) -> Option<Overlay> {
        let tree = self.trees.get(&buffer_id)?;
        let node = tree.root_node().descendant_for_byte_range(cursor_byte, cursor_byte)?;
        find_call_overlay(node, cursor_byte, piece_tree, &self.atom_table)
    }

    #[inline]
    pub fn query_cursor_overlay(&self, buffer_id: BufferId, cursor_byte: usize, tree: &PieceTree) -> Option<Overlay> {
        if let Some(o) = self.query_cursor_overlay_without_sending(buffer_id, cursor_byte, tree) {
            return Some(o);
        }

        self.send_cursor_query(buffer_id, cursor_byte, tree.clone());

        None
    }

    #[inline]
    pub fn send_reparse(&self, buffer_id: BufferId, tree: PieceTree, old_tree: Tree, buffer_last_edit_generation: u64) {
        _ = self.message_tx.send(ParserMessage::Reparse { buffer_id, tree, old_tree, buffer_last_edit_generation });
    }

    #[inline]
    pub fn send_force_reparse(&self, buffer_id: BufferId, tree: PieceTree, buffer_last_edit_generation: u64) {
        _ = self.message_tx.send(ParserMessage::Initialize { buffer_id, tree, buffer_last_edit_generation, force: true });
    }

    #[inline]
    pub fn send_init(&self, buffer_id: BufferId, tree: PieceTree, buffer_last_edit_generation: u64) {
        _ = self.message_tx.send(ParserMessage::Initialize { buffer_id, tree, buffer_last_edit_generation, force: false });
    }

    #[inline]
    pub fn send_cursor_query(&self, buffer_id: BufferId, cursor_byte: usize, tree: PieceTree) {
        _ = self.query_tx.send(ParserQuery::FuncCallOverlay { buffer_id, cursor_byte, tree });
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
        func_table: Default::default(), symbol_table: SymbolTable::new(),
        word_tables: Default::default()
    }
}

fn bg_thread(query_rx: Receiver<ParserQuery>, edit_rx: Receiver<ParserMessage>, result_tx: Sender<ParseResult>, atom_table: Arc<AtomTable>, trees: Arc<DashMap<BufferId, VersionedTree>>) {
    fn chunk_callback<'a>(tree: &'a PieceTree, byte: usize) -> &'a [u8] {
        if byte >= tree.len_bytes() as usize { return &[] }
        let (chunk, _chunk_start) = tree.chunk_at_byte(byte as _);
        chunk
    }

    loop {
        crossbeam_channel::select! {
            recv(query_rx) -> q => match q {
                Ok(ParserQuery::FuncCallOverlay { buffer_id, cursor_byte, tree: piece_tree }) => {
                    let Some(tree) = trees.get(&buffer_id) else { continue };

                    let node = tree.root_node().descendant_for_byte_range(cursor_byte, cursor_byte);
                    let overlay = node.and_then(|n| find_call_overlay(n, cursor_byte, &piece_tree, &atom_table));

                    _ = result_tx.send(ParseResult {
                        buffer_id,
                        kind: ParseResultKind::FuncCallOverlayUpdate { overlay }
                    });
                }

                _ => {}
            },

            recv(edit_rx) -> msg => match msg {
                Ok(ParserMessage::Reparse { buffer_id, tree, old_tree, buffer_last_edit_generation }) => {
                    let mut parser = Parser::new();

                    parser.set_language(&tree_sitter_rust::LANGUAGE.into()).expect("failed to load Rust grammar");


                    //
                    // Parse entirely outside any lock
                    //
                    let mut callback = |byte: usize, _point: Point| chunk_callback(&tree, byte);
                    let Some(new_tree) = parser.parse_with_options(
                        &mut callback,
                        Some(&old_tree),
                        None,
                    ) else {
                        continue
                    };

                    //
                    // Collect functions from the new tree before acquiring any lock
                    //
                    let root_node = new_tree.root_node();
                    let functions = collect_functions(root_node, &tree, buffer_id, &atom_table);
                    let symbols   = collect_symbols(root_node, &tree, buffer_id, &atom_table);
                    let words = WordTable::build(root_node, &tree, &atom_table);

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

                    if !committed { continue }

                    if functions.is_empty() && symbols.is_empty() && words.is_empty() { continue }

                    //
                    // Only send results if we actually committed
                    //
                    _ = result_tx.send(ParseResult {
                        buffer_id,
                        kind: ParseResultKind::SymbolsUpdate { functions, symbols, words },
                    });
                }

                Ok(ParserMessage::Initialize { buffer_id, tree: piece_tree, buffer_last_edit_generation, force }) => {
                    if !force && trees.contains_key(&buffer_id) { continue; }

                    let mut callback = |byte: usize, _point: Point| chunk_callback(&piece_tree, byte);

                    let mut parser = Parser::new();

                    parser.set_language(&tree_sitter_rust::LANGUAGE.into()).expect("failed to load Rust grammar");

                    let Some(tree) = parser.parse_with_options(&mut callback, None, None) else { continue };

                    let tree_copy = tree.clone();
                    let root_node = tree_copy.root_node();
                    trees.insert(buffer_id, VersionedTree { tree, buffer_last_edit_generation });

                    let functions = collect_functions(root_node, &piece_tree, buffer_id, &atom_table);
                    let symbols   = collect_symbols(root_node, &piece_tree, buffer_id, &atom_table);
                    let words     = WordTable::build(root_node, &piece_tree, &atom_table);

                    println!(
                        "[Collected {} functions, {} symbols and {} words from [{}] buffer]",
                        functions.len(), symbols.len(), words.len(),
                        buffer_id.0
                    );

                    if functions.is_empty() && symbols.is_empty() && words.is_empty() { continue }

                    _ = result_tx.send(ParseResult {
                        buffer_id,
                        kind: ParseResultKind::SymbolsUpdate { functions, symbols, words }
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

pub fn collect_functions(root: Node, source: &PieceTree, buffer_id: BufferId, table: &AtomTable) -> Vec<FunctionInfo> {
    let mut out = Vec::new();
    visit_node(root, source, buffer_id, &FuncContext::default(), &mut out, table);
    out
}

fn visit_node(
    node:      Node,
    source:    &PieceTree,
    buffer_id: BufferId,
    ctx:       &FuncContext,
    out:       &mut Vec<FunctionInfo>,
    table:     &AtomTable,
) {
    match node.kind() {
        "function_item" => if let Some(name_node) = node.child_by_field_name("name") {
            let name   = tree_slice_to_atom(source, name_node.start_byte(), name_node.end_byte(), table);
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
                .map(|n| tree_slice_to_atom(source, n.start_byte(), n.end_byte(), table));

            let new_ctx = FuncContext { impl_name, module: ctx.module.clone() };
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                visit_node(child, source, buffer_id, &new_ctx, out, table);
            }

            return;
        }

        "mod_item" => if let Some(name_node) = node.child_by_field_name("name") {
            let mod_atom = tree_slice_to_atom(source, name_node.start_byte(), name_node.end_byte(), table);
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

fn collect_params(fn_node: Node, source: &PieceTree, table: &AtomTable) -> Vec<ParamInfo> {
    let mut params = Vec::new();
    let Some(param_list) = fn_node.child_by_field_name("parameters") else { return params; };

    let mut cursor = param_list.walk();
    for child in param_list.children(&mut cursor) {
        if child.kind() != "parameter" { continue; }

        let (name, type_str) = match (child.child_by_field_name("pattern"), child.child_by_field_name("type")) {
            (Some(n), Some(t)) => (
                tree_slice_to_atom(source, n.start_byte(), n.end_byte(), table),
                tree_slice_to_atom(source, t.start_byte(), t.end_byte(), table),
            ),
            _ => continue,
        };

        params.push(ParamInfo { name, type_str });
    }
    params
}

/// Helper that slices the tree and interns the resulting string immediately.
fn tree_slice_to_atom(source: &PieceTree, start_byte: usize, end_byte: usize, table: &AtomTable) -> Atom {
    let slice = source.slice(start_byte as u32..end_byte as u32);
    let mut flat = SmallString::<[_; 128]>::with_capacity(slice.len_bytes() as usize);
    _ = write!(&mut flat, "{slice}");
    table.intern(&flat)
}

/// Helper that slices the tree and interns the resulting string immediately.
fn tree_slice_to_string<'a>(source: &'a PieceTree, start_byte: usize, end_byte: usize) -> SmallString<[u8; 128]> {
    let slice = source.slice(start_byte as u32..end_byte as u32);
    let mut flat = SmallString::with_capacity(slice.len_bytes() as usize);
    _ = write!(&mut flat, "{}", slice);
    flat
}

pub struct WordTable {
    strings:    String,           // slab
    starts:     Vec<u32>,         // into strings
    lens:       Vec<u16>,
    atoms:      Vec<Atom>,
    sorted_index: Vec<u32>,         // indices sorted by string value for binary search
}

impl WordTable {
    pub fn build(root: Node, source: &PieceTree, atom_table: &AtomTable) -> Self {
        let mut strings = String::new();
        let mut starts  = Vec::new();
        let mut lens    = Vec::new();
        let mut atoms   = Vec::new();

        let mut scratch = String::new();

        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "identifier" | "type_identifier" | "field_identifier" => {
                    let s     = node.start_byte() as u32;
                    let e     = node.end_byte() as u32;
                    let slice = source.slice(s..e);

                    let start = strings.len() as u32;
                    let len_before = strings.len();
                    _ = write!(&mut strings, "{slice}");
                    let len = (strings.len() - len_before) as u16;
                    let atom = atom_table.intern(&strings[start as usize..start as usize + len as usize]);

                    starts.push(start);
                    lens.push(len);
                    atoms.push(atom);
                }

                "line_comment" | "block_comment" => {
                    let s     = node.start_byte() as u32;
                    let e     = node.end_byte() as u32;
                    let slice = source.slice(s..e);

                    tprint!(&mut scratch, "{slice}");       // @Memory

                    let bytes = scratch.as_bytes();
                    let mut i = 0;
                    while i < bytes.len() {
                        if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
                            let word_start = i;
                            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                                i += 1;
                            }

                            let word = &scratch[word_start..i];
                            if word.len() >= 3 {
                                let start = strings.len() as u32;
                                strings.push_str(word);
                                let len = (i - word_start) as u16;
                                let atom = atom_table.intern(&strings[start as usize..start as usize + len as usize]);
                                starts.push(start);
                                lens.push(len);
                                atoms.push(atom);
                            }
                        } else {
                            i += 1;
                        }
                    }
                }

                _ => {
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        stack.push(child);
                    }
                }
            }
        }

        let mut sorted_index = (0..starts.len() as u32).collect::<Vec<_>>();
        sorted_index.sort_unstable_by(|&a, &b| {
            Self::get_str_raw(&strings, &starts, &lens, a)
                .cmp(Self::get_str_raw(&strings, &starts, &lens, b))
        });
        sorted_index.dedup_by(|&mut a, &mut b| {
            Self::get_str_raw(&strings, &starts, &lens, a) ==
                Self::get_str_raw(&strings, &starts, &lens, b)
        });

        Self { strings, starts, lens, atoms, sorted_index }
    }

    #[inline]
    pub fn is_empty(&self) -> bool { self.sorted_index.is_empty() }

    #[inline]
    pub fn len(&self) -> usize { self.sorted_index.len() }

    #[inline]
    fn get_str_raw<'a>(strings: &'a str, starts: &[u32], lens: &[u16], i: u32) -> &'a str {
        let s = starts[i as usize] as usize;
        &strings[s..s + lens[i as usize] as usize]
    }

    #[inline]
    fn get_str(&self, i: u32) -> &str {
        Self::get_str_raw(&self.strings, &self.starts, &self.lens, i)
    }

    #[inline]
    pub fn query_prefix<'a>(&'a self, prefix: &str) -> impl Iterator<Item = Atom> + 'a {
        let lo = self.sorted_index.partition_point(|&i| self.get_str(i) < prefix);
        let hi = match prefix.as_bytes().last() {
            None => self.sorted_index.len(),
            Some(&b) => {
                let mut end = prefix.to_owned();
                end.pop();
                end.push((b + 1) as char);
                self.sorted_index.partition_point(|&i| self.get_str(i) < end.as_str())
            }
        };
        self.sorted_index[lo..hi].iter().map(move |&i| self.atoms[i as usize])
    }
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

    pub fn len(&self) -> usize { self.detail_len.len() }

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
        name: Atom, detail: &str,
        buffer_id: BufferId, start: u32, end: u32,
    ) -> u32 {
        let ds = self.strings.len() as u32;
        self.strings.push_str(detail);

        let index = self.detail_len.len() as u32;
        self.detail_start.push(ds);
        self.detail_len.push(detail.len() as u16);
        self.names.push(name);
        self.buffer_id.push(buffer_id);
        self.range_start.push(start);
        self.range_end.push(end);
        self.generation.push(self.global_gen);

        self.by_name.entry(name).or_default().push(index);
        self.by_buffer.entry(buffer_id).or_default().push(index);

        index
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
            self.push_symbol(s.name, &s.detail, buffer_id, s.range_start, s.range_end);
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
    pub detail:      ArrayString<64>,
    pub kind:        SymbolKind,
    pub range_start: u32,
    pub range_end:   u32,
}

pub fn maybe_truncate_to_array_string<const N: usize>(s: &str) -> ArrayString<N> {
    const ELLIPSIS: &str = "…"; // 3 bytes UTF-8

    if s.len() <= N {
        // fits as-is
        let mut out = ArrayString::new();
        out.push_str(s);  // can't fail
        return out;
    }

    let mut out = ArrayString::new();
    let limit = N.saturating_sub(ELLIPSIS.len());
    // walk back to a char boundary
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(&s[..end]);
    out.push_str(ELLIPSIS);
    out
}

pub fn collect_symbols(root: Node, source: &PieceTree, _buffer_id: BufferId, atom_table: &AtomTable) -> Vec<RawSymbol> {
    let mut out = Vec::new();
    collect_symbols_inner(root, source, &mut out, None, atom_table);
    out
}

fn collect_symbols_inner(node: Node, source: &PieceTree, out: &mut Vec<RawSymbol>, parent_enum: Option<Atom>, atom_table: &AtomTable) {
    match node.kind() {
        "struct_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(RawSymbol {
                    name:        tree_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail:      maybe_truncate_to_array_string("struct"),
                    kind:        SymbolKind::Struct,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "enum_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let enum_name = tree_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table);
                out.push(RawSymbol {
                    name:        enum_name,
                    detail:      maybe_truncate_to_array_string("enum"),
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
                let variant_name = tree_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table);
                let detail = parent_enum.map_or("variant".into(), |e| format!("{}::", atom_table.lookup_ref(e).as_ref()));
                out.push(RawSymbol {
                    name:        variant_name,
                    detail:      maybe_truncate_to_array_string(&detail),
                    kind:        SymbolKind::EnumVariant,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "trait_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(RawSymbol {
                    name:        tree_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail:      maybe_truncate_to_array_string("trait"),
                    kind:        SymbolKind::Trait,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "type_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(RawSymbol {
                    name:        tree_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail:      maybe_truncate_to_array_string("type"),
                    kind:        SymbolKind::TypeAlias,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "const_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let detail = node.child_by_field_name("type")
                    .map(|t| tree_slice_to_string(source, t.start_byte(), t.end_byte()))
                    .unwrap_or_else(|| "const".into());
                out.push(RawSymbol {
                    name:        tree_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail:      maybe_truncate_to_array_string(&detail),
                    kind:        SymbolKind::Const,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "static_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let detail = node.child_by_field_name("type")
                    .map(|t| tree_slice_to_string(source, t.start_byte(), t.end_byte()))
                    .unwrap_or_else(|| "static".into());

                out.push(RawSymbol {
                    name:        tree_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail:      maybe_truncate_to_array_string(&detail),
                    kind:        SymbolKind::Static,
                    range_start: node.start_byte() as u32,
                    range_end:   node.end_byte() as u32,
                });
            }
        }

        "mod_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(RawSymbol {
                    name:        tree_slice_to_atom(source, name.start_byte(), name.end_byte(), atom_table),
                    detail:      maybe_truncate_to_array_string("mod"),
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

fn find_call_overlay(mut node: Node, cursor_byte: usize, source: &PieceTree, table: &AtomTable) -> Option<Overlay> {
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

fn parse_call_kind(fn_node: Node, source: &PieceTree, table: &AtomTable) -> CallKind {
    match fn_node.kind() {
        // bar()
        "identifier" => {
            let name = tree_slice_to_atom(source, fn_node.start_byte(), fn_node.end_byte(), table);
            CallKind::Bare(name)
        }

        // foo.bar() - field_expression
        "field_expression" => {
            let field = fn_node.child_by_field_name("field");
            let name  = field.map(|n| tree_slice_to_atom(source, n.start_byte(), n.end_byte(), table))
                .unwrap_or_default();
            CallKind::Method(name)
        }

        // Foo::bar() or foo::bar() or foo::baz::bar() - scoped_identifier
        "scoped_identifier" => {
            let name_node = fn_node.child_by_field_name("name");
            let path_node = fn_node.child_by_field_name("path");

            let name = name_node
                .map(|n| tree_slice_to_atom(source, n.start_byte(), n.end_byte(), table))
                .unwrap_or_default();

            match path_node {
                None => CallKind::Bare(name),
                Some(path) if path.kind() == "identifier" => {
                    let prefix = tree_slice_to_atom(source, path.start_byte(), path.end_byte(), table);
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
            let name = tree_slice_to_atom(source, fn_node.start_byte(), fn_node.end_byte(), table);
            CallKind::Method(name)
        }
    }
}

fn collect_path_segments(node: Node, source: &PieceTree, table: &AtomTable) -> Vec<Atom> {
    let mut segments = Vec::new();
    collect_path_segments_inner(node, source, table, &mut segments);
    segments
}

fn collect_path_segments_inner(node: Node, source: &PieceTree, table: &AtomTable, out: &mut Vec<Atom>) {
    match node.kind() {
        "scoped_identifier" => {
            if let Some(path) = node.child_by_field_name("path") {
                collect_path_segments_inner(path, source, table, out);
            }
            if let Some(name) = node.child_by_field_name("name") {
                out.push(tree_slice_to_atom(source, name.start_byte(), name.end_byte(), table));
            }
        }

        "identifier" => {
            out.push(tree_slice_to_atom(source, node.start_byte(), node.end_byte(), table));
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
    source: &PieceTree,
    table: &AtomTable,
) -> Option<Overlay> {
    let cursor_byte = cursor_byte.min(source.len_bytes() as _);

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

fn is_definition_context(source: &PieceTree, callee_start_byte: usize) -> bool {
    let start = callee_start_byte.saturating_sub(32);
    let window = source.slice(start as u32..callee_start_byte as u32).to_string();

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
    source: &PieceTree,
    cursor_byte: usize,
) -> Option<usize> {
    with_scratch_bytes! {
        let scratch_bytes;
        find_opening_paren_before_cursor_impl(source, cursor_byte, scratch_bytes)
    }
}

fn find_opening_paren_before_cursor_impl(
    source: &PieceTree,
    cursor_byte: usize,
    scratch_bytes: &mut SmallVec<[u8; LOOKBACK_CHARS]>,
) -> Option<usize> {
    let start = cursor_byte.saturating_sub(LOOKBACK_CHARS);

    scratch_bytes.clear();
    scratch_bytes.extend(source.slice(start as u32..cursor_byte as u32).bytes());

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
        let name_atom = tree_slice_to_atom_from_str(
            expr, name_start, name_start + name.len(),
            expr_start_byte,
            table
        );

        match parts.len() {
            1 => Some(CallKind::Bare(name_atom)),

            2 => {
                let prefix = parts[0];
                let prefix_start = expr.find(prefix)?;
                let prefix_atom = tree_slice_to_atom_from_str(
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
                    segs.push(tree_slice_to_atom_from_str(expr, s, e, expr_start_byte, table));
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
        let atom = tree_slice_to_atom_from_str(expr, name_start, name_start + name.len(), expr_start_byte, table);
        Some(CallKind::Method(atom))

    } else {
        let name = expr.split_whitespace().last()?.trim();
        if name.is_empty() {
            return None;
        }

        let name_start = expr.rfind(name)?;
        let atom = tree_slice_to_atom_from_str(expr, name_start, name_start + name.len(), expr_start_byte, table);
        Some(CallKind::Bare(atom))
    }
}

fn tree_slice_to_atom_from_str(
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
    source: &PieceTree,
    open_paren_byte: usize,
) -> Option<CalleeSpan> {
    with_scratch_chars! {
        let scratch_chars;
        extract_callee_before_paren_impl(source, open_paren_byte, scratch_chars)
    }
}

fn extract_callee_before_paren_impl(
    source: &PieceTree,
    open_paren_byte: usize,
    scratch_chars: &mut SmallVec<[char; LOOKBACK_CHARS]>
) -> Option<CalleeSpan> {
    let start = open_paren_byte.saturating_sub(LOOKBACK_CHARS);
    let tree_slice = source.slice(start as u32..open_paren_byte as u32);

    let chars = scratch_chars;
    chars.clear();
    chars.extend(tree_slice.chars());
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
    let end_byte = tree_slice.char_to_byte(end as u32);

    let text: Box<str> = tree_slice.slice(start_byte_rel as u32..end_byte).to_string().trim().into();
    if text.is_empty() {
        return None;
    }

    Some(CalleeSpan {
        text,
        start_byte: start + start_byte_rel,
    })
}

fn count_incomplete_arg_index(
    source: &PieceTree,
    open_paren_byte: usize,
    cursor_byte: usize,
) -> Option<usize> {
    with_scratch_chars! {
        let scratch_chars;
        count_incomplete_arg_index_impl(source, open_paren_byte, cursor_byte, scratch_chars)
    }
}

fn count_incomplete_arg_index_impl(
    source: &PieceTree,
    open_paren_byte: usize,
    cursor_byte: usize,
    scratch_chars: &mut SmallVec<[char; LOOKBACK_CHARS]>
) -> Option<usize> {
    let cursor_byte = cursor_byte.min(source.len_bytes() as usize);
    if cursor_byte <= open_paren_byte {
        return Some(0);
    }

    let tree_slice = source.slice(open_paren_byte as u32 + 1..cursor_byte as u32);
    let chars = scratch_chars;

    chars.clear();
    chars.extend(tree_slice.chars());

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
        // Rightmost leaf of the previous sibling
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

pub fn jump_definition_prev(root: Node, cursor_byte: usize) -> Option<usize> {
    let current = root.descendant_for_byte_range(cursor_byte, cursor_byte)?;

    //
    // Find the enclosing top-level definition
    //
    let enclosing = find_enclosing(current, is_definition_node);

    if let Some(enc) = enclosing {
        //
        // If cursor is already at the start, jump to previous definition's start
        //
        if cursor_byte <= enc.start_byte() {
            return prev_named_matching(enc, is_definition_node).map(|n| n.start_byte());
        }

        //
        // Otherwise jump to start of enclosing definition
        //
        return Some(enc.start_byte());
    }

    //
    // Not inside any definition,  jump to previous definition from here
    //
    let node_at = root.descendant_for_byte_range(cursor_byte, cursor_byte)?;
    prev_named_matching(node_at, is_definition_node).map(|n| n.start_byte())
}

pub fn jump_definition_next(root: Node, cursor_byte: usize) -> Option<usize> {
    let current = root.descendant_for_byte_range(cursor_byte, cursor_byte)?;

    let enclosing = find_enclosing(current, is_definition_node);

    if let Some(enc) = enclosing {
        //
        // If cursor is already at or past the end, jump to next definition's end
        //
        if cursor_byte >= enc.end_byte().saturating_sub(1) {
            return next_named_matching(enc, is_definition_node).map(|n| n.end_byte().saturating_sub(1));
        }

        //
        // Otherwise jump to end of enclosing definition
        //
        return Some(enc.end_byte().saturating_sub(1));
    }

    let node_at = root.descendant_for_byte_range(cursor_byte, cursor_byte)?;
    next_named_matching(node_at, is_definition_node).map(|n| n.end_byte().saturating_sub(1))
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
    source:      &PieceTree,
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

fn collect_pattern_bindings(pat: Node, source: &PieceTree, out: &mut Vec<CompletionItem>, atom_table: &AtomTable) {
    match pat.kind() {
        "identifier" => {
            let name = tree_slice_to_atom(source, pat.start_byte(), pat.end_byte(), atom_table);
            if atom_table.lookup_ref(name).as_ref() == "_" { return; }
            out.push(CompletionItem {
                name: name,
                detail: maybe_truncate_to_array_string("let"),
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

pub fn extract_prefix_at_cursor(buf: &Buffer, cursor_char: usize) -> (u32, SmallString<[u8; 32]>) {
    let mut start = cursor_char;
    while start > 0 {
        let c = buf.text.char(start as u32 - 1);
        if c.is_alphanumeric() || c == '_' {
            start -= 1;
        } else {
            break;
        }
    }

    let mut s = SmallString::new();
    tprint!(&mut s, "{}", buf.text.slice_chars(start as _, cursor_char as _));

    (start as _, s)
}

pub fn query_completions(
    prefix:       &str,
    sym_table:    &SymbolTable,
    func_table:   &FunctionTable,
    atom_table:   &AtomTable,
    word_tables:  &WordTables,
    buffer_id:    BufferId,
    cursor_byte:  usize,
    tree:         Option<&Tree>,
    source:       &PieceTree,
    limit:        usize,
) -> Vec<CompletionItem> {
    if prefix.is_empty() { return Vec::new(); }

    let prefix_lower = prefix.to_ascii_lowercase();
    let mut items: Vec<CompletionItem> = Vec::new();    // @Memory @Memory @Memory

    // Symbols
    for i in sym_table.query_prefix(prefix, atom_table) {
        let name   = sym_table.name(i);
        let detail = maybe_truncate_to_array_string(sym_table.detail(i)); // @Memory @Memory
        let score  = score_completion(&atom_table.lookup_ref(name), prefix);
        items.push(CompletionItem {
            name,
            detail,
            score,
        });

        if items.len() >= limit * 2 { break; }
    }

    let mut scratch_name = SmallString::<[u8; 256]>::new();

    // Functions from func_table
    for (_, refs) in &func_table.by_name {
        for &r in refs.as_slice() {
            let Some(info) = func_table.get(r) else { continue };

            tprint!(&mut scratch_name, "{}", atom_table.lookup_ref(info.name).as_ref());
            let name = &scratch_name;

            if !name.starts_with(&prefix_lower) { continue }

            let detail = maybe_truncate_to_array_string(&format_fn_detail(info, atom_table));
            let score  = score_completion(&name, prefix);
            items.push(CompletionItem {
                name: info.name,
                detail,
                score,
            });

            if items.len() >= limit * 2 { break }
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
            tprint!(&mut scratch_name, "{}", atom_table.lookup_ref(item.name).as_ref());
            let name = &scratch_name;

            if !name.to_ascii_lowercase().starts_with(&prefix_lower) { continue }
            item.score = score_completion(&name, prefix) + 5; // + 5 for locals
            items.push(item);
        }
    }

    // Words from current buffer
    if let Some(word_table) = word_tables.get(&buffer_id) {
        for atom in word_table.query_prefix(prefix) {
            tprint!(&mut scratch_name, "{}", atom_table.lookup_ref(atom).as_ref());
            let name = &scratch_name;

            if name == prefix { continue }  // @Hack?

            if !name.to_ascii_lowercase().starts_with(&prefix_lower) { continue }

            let score = score_completion(&name, prefix).saturating_sub(2); // Below symbols/locals
            items.push(CompletionItem {
                name:   atom,
                detail: maybe_truncate_to_array_string("word"),
                score,
            });

            if items.len() >= limit * 2 { break; }
        }
    }

    // Sort by score, deduplicate by name
    items.sort_unstable_by(|a, b| a.name.cmp(&b.name).then_with(|| b.score.cmp(&a.score)));
    items.dedup_by(|a, b| a.name == b.name);  // keeps first = highest score per name
    items.sort_unstable_by_key(|i| core::cmp::Reverse(i.score));
    items.truncate(limit);
    items
}

fn score_completion(name: &str, prefix: &str) -> u32 {
    // Exact prefix match scores 0 (best)
    // Then by length difference
    // Then alphabetical
    if name == prefix { return 0; }
    let len_diff = (name.len() as i32 - prefix.len() as i32).unsigned_abs();
    len_diff + 1
}

fn format_fn_detail(info: &FunctionInfo, atom_table: &AtomTable) -> SmallString<[u8; 64]> {
    let mut s = SmallString::from("(");
    for (i, param) in info.params.iter().enumerate() {
        if i > 0 { s.push_str(", "); }
        s.push_str(atom_table.lookup_ref(param.name).as_str());
        s.push_str(": ");
        s.push_str(atom_table.lookup_ref(param.type_str).as_str());
    }
    s.push(')');
    s
}
