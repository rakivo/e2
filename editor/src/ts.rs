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
    FullPath([Atom; 4], Atom),   // foo::...::baz::bar()
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
    pub trees:      Arc<DashMap<BufferId, Tree>>,

    pub func_table: FunctionTable,
}

impl TreeSitter {
    #[inline]
    pub fn query_cursor_overlay_without_sending(&self, buffer_id: BufferId, cursor_byte: usize, rope: &Rope) -> Option<OverlayResult> {
        let tree = self.trees.get(&buffer_id)?;
        let node = tree.root_node().descendant_for_byte_range(cursor_byte, cursor_byte)?;
        find_call_overlay(node, cursor_byte, rope, &self.atom_table)
    }

    #[inline]
    pub fn query_cursor_overlay(&self, buffer_id: BufferId, cursor_byte: usize, rope: &Rope) -> Option<OverlayResult> {
        if let Some(o) = self.query_cursor_overlay_without_sending(buffer_id, cursor_byte, rope) {
            return Some(o);
        }

        self.send_cursor_query(buffer_id, cursor_byte, rope.clone());

        None
    }

    #[inline]
    pub fn send_edit(&self, buffer_id: BufferId, edit: e2_InputEdit, rope: Rope) {
        _ = self.message_tx.send(ParserMessage::Edit { buffer_id, edit, rope });
    }

    #[inline]
    pub fn send_init(&self, buffer_id: BufferId, rope: Rope) {
        _ = self.message_tx.send(ParserMessage::Initialize { buffer_id, rope });
    }

    #[inline]
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
                    let old_tree = trees.get(&buffer_id).map(|m| m.value().clone());

                    let mut tree_to_parse = None;
                    if let Some(mut cloned_tree) = old_tree {
                        cloned_tree.edit(&edit.into());
                        tree_to_parse = Some(cloned_tree);
                    }

                    let mut callback = |byte: usize, _point: Point| {
                        // Guard against Tree-Sitter probing the end of the document
                        if byte >= rope.len_bytes() {
                            return &[][..];
                        }

                        let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
                        &chunk.as_bytes()[(byte - chunk_start)..]
                    };

                    let Some(new_tree) = parser.parse_with_options(
                        &mut callback,
                        tree_to_parse.as_ref(),
                        None,
                    ) else { continue };

                    let functions = collect_functions(new_tree.root_node(), &rope, buffer_id, &table);
                    trees.insert(buffer_id, new_tree);

                    _ = result_tx.send(ParseResult { buffer_id, functions, overlay: None });
                }

                Ok(ParserMessage::Initialize { buffer_id, rope }) => {
                    if trees.contains_key(&buffer_id) { continue; }

                    let mut callback = |byte: usize, _point: Point| {
                        // Guard against Tree-Sitter probing the end of the document
                        if byte >= rope.len_bytes() {
                            return &[][..];
                        }

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
    #[inline]
    pub fn get(&self, r: FuncRef) -> Option<&FunctionInfo> {
        let info = self.fns.get(r.id)?;
        if info.generation == r.generation && info.generation != 0 {
            Some(info)
        } else {
            None
        }
    }

    #[inline]
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

    // Convenience: remove old buffer entries then insert all new ones atomically
    #[inline]
    pub fn replace_buffer(&mut self, buffer_id: BufferId, buffer_filename: Atom, new_fns: Vec<FunctionInfo>) {
        self.remove_buffer(buffer_id);
        for info in new_fns {
            self.insert(info, buffer_filename);
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

#[derive(Debug, Clone, Default)]
pub struct OverlayState {
    pub current: Option<OverlayResult>,
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

pub fn overlay_needs_reset(o: &OverlayResult, edit: &e2_InputEdit) -> bool {
                                              edit.start_byte < o.opening_paren_byte ||
    o.closing_paren_byte.map_or(false, |byte| edit.start_byte > byte.get())
}

pub fn overlay_needs_reset_cursor_byte(o: &OverlayResult, cursor_byte: u32) -> bool {
                                              cursor_byte < o.opening_paren_byte ||
    o.closing_paren_byte.map_or(false, |byte| cursor_byte > byte.get())
}

//
//
// Incomplete tree lookups!
//
//

const LOOKBACK_BYTES: usize = 1024;

fn find_incomplete_call_overlay(
    cursor_byte: usize,
    source: &Rope,
    table: &AtomTable,
) -> Option<OverlayResult> {
    let cursor_byte = cursor_byte.min(source.len_bytes());

    let open_paren_byte = find_opening_paren_before_cursor(source, cursor_byte, LOOKBACK_BYTES)?;
    let callee = extract_callee_before_paren(source, open_paren_byte, LOOKBACK_BYTES)?;
    let call_kind = parse_incomplete_call_kind(&callee.text, callee.start_byte, table)?;

    let arg_index = count_incomplete_arg_index(source, open_paren_byte, cursor_byte)?;

    Some(OverlayResult {
        call_kind,
        arg_index: arg_index as u16,
        opening_paren_byte: open_paren_byte as u32,
        closing_paren_byte: None,
    })
}

struct CalleeSpan {
    text: Box<str>,
    start_byte: usize,
}

fn find_opening_paren_before_cursor( // @Memory
    source: &Rope,
    cursor_byte: usize,
    lookback_bytes: usize,
) -> Option<usize> {
    let start = cursor_byte.saturating_sub(lookback_bytes);
    let window = source.byte_slice(start..cursor_byte).to_string();

    let mut depth = 0i32;
    for (rel_byte, ch) in window.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => {
                if depth == 0 {
                    return Some(start + rel_byte);
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

fn is_escaped(chars: &[(usize, char)], idx: usize) -> bool {
    let mut backslashes = 0usize;
    let mut i = idx;
    while i > 0 {
        if chars[i - 1].1 == '\\' {
            backslashes += 1;
            i -= 1;
        } else {
            break;
        }
    }
    backslashes % 2 == 1
}

fn looks_like_char_literal_start(chars: &[(usize, char)], idx: usize) -> bool {
    // Very small heuristic: `'x'`, `'\n'`, etc.
    if idx + 1 >= chars.len() {
        return false;
    }

    let next = chars[idx + 1].1;
    next != '\'' && next != '\n'
}

fn parse_incomplete_call_kind(expr: &str, expr_start_byte: usize, table: &AtomTable) -> Option<CallKind> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }

    if expr.contains("::") {
        let parts: Vec<&str> = expr.split("::").map(str::trim).filter(|s| !s.is_empty()).collect();
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

fn extract_callee_before_paren( // @Memory @Refactor
    source: &Rope,
    open_paren_byte: usize,
    lookback_bytes: usize,
) -> Option<CalleeSpan> {
    let start = open_paren_byte.saturating_sub(lookback_bytes);
    let window = source.byte_slice(start..open_paren_byte).to_string();

    let chars: Vec<(usize, char)> = window.char_indices().collect();
    if chars.is_empty() {
        return None;
    }

    let mut end = chars.len();
    while end > 0 && chars[end - 1].1.is_whitespace() {
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

        let (_, ch) = chars[i - 1];
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
                if i >= 2 && chars[i - 2].1 == ':' {
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

    let start_char = i;
    let start_byte_rel = chars.get(start_char).map(|(b, _)| *b).unwrap_or(window.len());

    let text: Box<str> = window[start_byte_rel..end].trim().into();
    if text.is_empty() {
        return None;
    }

    Some(CalleeSpan {
        text,
        start_byte: start + start_byte_rel,
    })
}

fn count_incomplete_arg_index(  // @Memory @Refactor
    source: &Rope,
    open_paren_byte: usize,
    cursor_byte: usize,
) -> Option<usize> {
    let cursor_byte = cursor_byte.min(source.len_bytes());
    if cursor_byte <= open_paren_byte {
        return Some(0);
    }

    let text = source.byte_slice(open_paren_byte + 1..cursor_byte).to_string();
    let chars: Vec<(usize, char)> = text.char_indices().collect();

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

        let (_, ch) = chars[i];
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
    chars: &[(usize, char)],
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

    let ch = chars[*i - 1].1;

    if *in_line_comment {
        if ch == '\n' {
            *in_line_comment = false;
        }

        *i -= 1;
        return true;
    }

    if *block_comment_depth > 0 {
        if ch == '/' && *i >= 2 && chars[*i - 2].1 == '*' {
            *block_comment_depth -= 1;
            *i -= 2;
            return true;
        }

        if ch == '*' && *i >= 2 && chars[*i - 2].1 == '/' {
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
                if *i + k >= chars.len() || chars[*i + k].1 != '#' {
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
        let prev = chars[*i - 2].1;

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

        while j > 0 && chars[j - 1].1 == '#' {
            hashes += 1;
            j -= 1;
        }

        if j > 0 && chars[j - 1].1 == 'r' {
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
    chars: &[(usize, char)],
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

    let ch = chars[*i].1;

    if *in_line_comment {
        if ch == '\n' {
            *in_line_comment = false;
        }
        *i += 1;
        return true;
    }

    if *block_comment_depth > 0 {
        if ch == '/' && *i + 1 < chars.len() && chars[*i + 1].1 == '*' {
            *block_comment_depth += 1;
            *i += 2;
            return true;
        }

        if ch == '*' && *i + 1 < chars.len() && chars[*i + 1].1 == '/' {
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
                if *i + 1 + k >= chars.len() || chars[*i + 1 + k].1 != '#' {
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
        let next = chars[*i + 1].1;

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

        while j > 0 && chars[j - 1].1 == '#' {
            hashes += 1;
            j -= 1;
        }

        if j > 0 && chars[j - 1].1 == 'r' {
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
