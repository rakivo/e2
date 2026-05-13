use crate::BufferId;
use crate::atum::{Atom, AtomTable};

use std::num::NonZeroU32;
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

pub enum ParserMessage {
    Initialize {
        buffer_id: BufferId,
        rope: Rope,
    },

    Edit {
        buffer_id: BufferId,
        edit: e2_InputEdit,
        rope: Rope,
    },
}

pub enum ParserQuery {
    Cursor {
        buffer_id: BufferId,
        cursor_byte: usize,
        rope: Rope,
    },
}

pub struct ParseResult {
    pub buffer_id: BufferId,
    pub functions: Vec<FunctionInfo>,
    pub overlay:   Option<OverlayResult>,
}

#[derive(Debug, Clone)]
pub enum CallKind {
    Bare(Atom),                  // bar()
    AssocOrPath(Atom, Atom),     // Foo::bar() or foo::bar() - check by_impl first, then by_module
    FullPath(Box<[Atom]>, Atom), // foo::baz::bar()
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
pub struct OverlayResult {
    pub call_kind: CallKind,
    pub arg_index: u16,
    pub opening_paren_byte: u32,
    pub closing_paren_byte: Option<NonZeroU32>,
}

pub struct TreeSitter {
    pub message_tx: Sender<ParserMessage>,
    pub query_tx:   Sender<ParserQuery>,

    pub result: Receiver<ParseResult>,

    // Shared tables for main-thread lookups
    pub atom_table: Arc<AtomTable>,
    pub trees:  Arc<DashMap<BufferId, Tree>>,

    pub func_table: FunctionTable,
}

impl TreeSitter {
    pub fn query_cursor_overlay_without_sending(&self, buffer_id: BufferId, cursor_byte: usize, rope: &Rope) -> Option<OverlayResult> {
        let tree = self.trees.get(&buffer_id)?;
        let node = tree.root_node().descendant_for_byte_range(cursor_byte, cursor_byte)?;
        find_call_overlay(node, cursor_byte, rope, &self.atom_table)
    }

    pub fn query_cursor_overlay(&self, buffer_id: BufferId, cursor_byte: usize, rope: &Rope) -> Option<OverlayResult> {
        if let Some(o) = self.query_cursor_overlay_without_sending(buffer_id, cursor_byte, rope) {
            return Some(o);
        }

        self.send_cursor_query(buffer_id, cursor_byte, rope.clone());

        None
    }

    pub fn send_edit(&self, buffer_id: BufferId, edit: e2_InputEdit, rope: Rope) {
        _ = self.message_tx.send(ParserMessage::Edit { buffer_id, edit, rope });
    }

    pub fn send_init(&self, buffer_id: BufferId, rope: Rope) {
        _ = self.message_tx.send(ParserMessage::Initialize { buffer_id, rope });
    }

    pub fn send_cursor_query(&self, buffer_id: BufferId, cursor_byte: usize, rope: Rope) {
        _ = self.query_tx.send(ParserQuery::Cursor { buffer_id, cursor_byte, rope });
    }
}

pub fn spawn() -> TreeSitter {
    let (message_tx, edit_rx) = crossbeam_channel::unbounded();
    let (query_tx, query_rx) = crossbeam_channel::unbounded();
    let (result_tx, result) = crossbeam_channel::unbounded();

    let table = Arc::new(AtomTable::new());
    let trees = Arc::default();

    let table_clone = Arc::clone(&table);
    let trees_clone = Arc::clone(&trees);

    thread::spawn(move || bg_thread(query_rx, edit_rx, result_tx, table_clone, trees_clone));

    TreeSitter { message_tx, result, query_tx, atom_table: table, trees, func_table: Default::default() }
}

fn bg_thread(query_rx: Receiver<ParserQuery>, edit_rx: Receiver<ParserMessage>, result_tx: Sender<ParseResult>, table: Arc<AtomTable>, trees: Arc<DashMap<BufferId, Tree>>) {
    let mut parser  = Parser::new();

    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("failed to load Rust grammar");

    loop {
        crossbeam_channel::select! {
            recv(query_rx) -> q => match q {
                Ok(ParserQuery::Cursor { buffer_id, cursor_byte, rope }) => {
                    let Some(tree) = trees.get(&buffer_id) else { continue };
                    let node = tree.root_node().descendant_for_byte_range(cursor_byte, cursor_byte);
                    let overlay = node.and_then(|n| find_call_overlay(n, cursor_byte, &rope, &table));
                    _ = result_tx.send(ParseResult { buffer_id, functions: vec![], overlay });
                }

                _ => {}
            },

            recv(edit_rx) -> msg => match msg {
                Ok(ParserMessage::Edit { buffer_id, edit, rope }) => {
                    if let Some(mut tree) = trees.get_mut(&buffer_id) {
                        tree.edit(&edit.into());
                    }

                    let old_tree = trees.get(&buffer_id);
                    let mut callback = |byte: usize, _point: Point| {
                        let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
                        &chunk.as_bytes()[(byte - chunk_start)..]
                    };

                    let Some(new_tree) = parser.parse_with_options(
                        &mut callback,
                        old_tree.as_deref(),
                        None,
                    ) else { continue };

                    let functions = collect_functions(new_tree.root_node(), &rope, buffer_id, &table);
                    trees.insert(buffer_id, new_tree);

                    _ = result_tx.send(ParseResult { buffer_id, functions, overlay: None });
                }

                Ok(ParserMessage::Initialize { buffer_id, rope }) => {
                    if trees.contains_key(&buffer_id) { continue; }

                    let mut callback = |byte: usize, _point: Point| {
                        let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
                        &chunk.as_bytes()[(byte - chunk_start)..]
                    };

                    let Some(tree) = parser.parse_with_options(&mut callback, None, None) else { continue };

                    let functions = collect_functions(tree.root_node(), &rope, buffer_id, &table);
                    trees.insert(buffer_id, tree);

                    _ = result_tx.send(ParseResult { buffer_id, functions, overlay: None });
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
    pub fn get(&self, r: FuncRef) -> Option<&FunctionInfo> {
        let info = self.fns.get(r.id)?;
        if info.generation == r.generation && info.generation != 0 {
            Some(info)
        } else {
            None
        }
    }

    pub fn insert(&mut self, info: FunctionInfo, buffer_filename: Atom) -> FuncRef {
        let name      = info.name;
        let impl_name = info.impl_name;
        let buffer_id = info.buffer_id;

        let effective_module: &[Atom] = if info.module.is_empty() {
            std::slice::from_ref(&buffer_filename)
        } else {
            &info.module
        };
        let module_hash = hash_module(&effective_module);

        let func = if let Some(slot) = self.free_list.pop() {
            let new_gen = self.fns[slot].generation.wrapping_add(1).max(1);
            self.fns[slot] = FunctionInfo { generation: new_gen, ..info };
            slot
        } else {
            self.fns.push(info)
        };

        let generation = self.fns[func].generation;
        let r = FuncRef { id: func, generation };

        self.by_name.entry(name).or_default().push(r);

        if let Some(impl_atom) = impl_name {
            self.by_impl.insert((impl_atom, name), r);
        }

        self.by_module.insert((module_hash, name), r);

        self.by_buffer.entry(buffer_id).or_default().push(func);

        r
    }

    pub fn remove_buffer(&mut self, buffer_id: BufferId) {
        let Some(funcs) = self.by_buffer.remove(&buffer_id) else { return };

        for func in funcs {
            let info = &mut self.fns[func];
            if info.generation == 0 { continue; } // already dead

            let name      = info.name;
            let impl_name = info.impl_name;
            let module    = info.module.clone();
            let generation = info.generation;
            info.generation = 0; // mark dead

            // remove from by_name
            if let Some(refs) = self.by_name.get_mut(&name) {
                refs.retain(|r| !(r.id == func && r.generation == generation));
                if refs.is_empty() { self.by_name.remove(&name); }
            }

            // remove from by_impl
            if let Some(impl_atom) = impl_name {
                let key = (impl_atom, name);
                if self.by_impl.get(&key).map_or(false, |r| r.id == func) {
                    self.by_impl.remove(&key);
                }
            }

            // remove from by_module
            let key = (hash_module(&module), name);
            if self.by_module.get(&key).map_or(false, |r| r.id == func) {
                self.by_module.remove(&key);
            }

            self.free_list.push(func);
        }
    }

    // Convenience: remove old buffer entries then insert all new ones atomically
    pub fn replace_buffer(&mut self, buffer_id: BufferId, buffer_filename: Atom, new_fns: Vec<FunctionInfo>) {
        self.remove_buffer(buffer_id);
        for info in new_fns {
            self.insert(info, buffer_filename);
        }
    }

    pub fn resolve_bare(&self, name: Atom) -> Option<FuncRef> {
        let refs = self.by_name.get(&name)?;

        // Prefer a free function (no impl_name) over an impl method
        refs.iter()
            .copied()
            .find(|r| self.get(*r).map_or(false, |f| f.impl_name.is_none()))
            .or_else(|| refs.first().copied())
    }

    pub fn resolve_method(&self, name: Atom) -> Option<FuncRef> {
        let refs = self.by_name.get(&name)?;

        // Prefer a method function (with impl_name) over a free function
        refs.iter()
            .copied()
            .find(|r| self.get(*r).map_or(false, |f| f.impl_name.is_some()))
            .or_else(|| refs.first().copied())
    }

    pub fn resolve_assoc(&self, impl_name: Atom, fn_name: Atom) -> Option<FuncRef> {
        self.by_impl.get(&(impl_name, fn_name))
            .or_else(|| self.by_module.get(&(hash_module(&[impl_name]), fn_name)))
            .copied()
    }

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
        "function_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
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
        "mod_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
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

#[derive(Debug, Clone, Default)]
pub struct OverlayState {
    pub current: Option<OverlayResult>,
}

impl OverlayState {
    // Just invalidates if edit precedes current call site
    pub fn on_edit(&mut self, edit: &e2_InputEdit) {
        if let Some(ref o) = self.current {
            if edit.start_byte <= o.opening_paren_byte {
                self.current = None;
            }
        }
    }
}

fn find_call_overlay(mut node: Node, cursor_byte: usize, source: &Rope, table: &AtomTable) -> Option<OverlayResult> {
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

            return Some(OverlayResult { call_kind, arg_index, opening_paren_byte, closing_paren_byte });
        }

        node = node.parent()?;
    }
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
                    // multi-segment: foo::baz::bar - collect all segments
                    let segments = collect_path_segments(path, source, table);
                    CallKind::FullPath(segments.into(), name)
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
    result: &OverlayResult,
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
