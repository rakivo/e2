//
// Format:
//
//   u32  magic
//   u8   version
//   str  cwd
//
//   u32  buffer count
//   [buffer count] x {
//       str  path          (empty = scratch)
//   }
//
//   u32  leaf count
//   [leaf count] x {
//       str  path
//       u32  cursor line
//       u32  cursor col
//       f32  scroll_anim
//   }
//
//   u32 active_leaf_index
//   panel tree (recursive):
//       u8 tag (0 = Leaf, 1 = Split)
//       Leaf:  u32 leaf_index
//       Split: u8 vertical, f32 ratio, left_panel, right_panel
//
//   u32  custom chunk count
//   [custom chunk count] x {
//       u16  ID
//       u32  byte_len
//       [byte_len] x u8
//   }
//

use std::path::Path;
use std::time::Instant;

use smallvec::SmallVec;
use wgpu::naga::FastHashMap;

use crate::buffer::Buffer;
use crate::{BufferId, Editor, Panel, PanelId, PanelKind, PanelSplit, Rect, VIEW_MAIN, View, ViewId};

pub const MAGIC:   u32 = 0x4E455353; // "SSEN"
pub const VERSION: u8  = 1;

pub type CustomChunkId = u16;

const PANEL_KIND_LEAF:  u8 = 0;
const PANEL_KIND_SPLIT: u8 = 1;

#[inline] pub fn write_u8 (out: &mut Vec<u8>, v: u8)   { out.push(v); }
#[inline] pub fn write_u16(out: &mut Vec<u8>, v: u16)  { out.extend_from_slice(&v.to_le_bytes()); }
#[inline] pub fn write_u32(out: &mut Vec<u8>, v: u32)  { out.extend_from_slice(&v.to_le_bytes()); }
#[inline] pub fn write_f32(out: &mut Vec<u8>, v: f32)  { out.extend_from_slice(&v.to_le_bytes()); }
#[inline] pub fn write_str(out: &mut Vec<u8>, s: &str) {
    write_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos:  usize,
}

impl<'a> Reader<'a> {
    #[inline]
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[inline]
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos + n;
        if end > self.data.len() { return None; }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Some(s)
    }

    #[inline] pub fn u8 (&mut self) -> Option<u8>  { Some( u8::from_le_bytes(self.bytes(1)?.try_into().ok()?)) }
    #[inline] pub fn u16(&mut self) -> Option<u16> { Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?)) }
    #[inline] pub fn u32(&mut self) -> Option<u32> { Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?)) }
    #[inline] pub fn f32(&mut self) -> Option<f32> { Some(f32::from_le_bytes(self.bytes(4)?.try_into().ok()?)) }
    #[inline] pub fn str(&mut self) -> Option<&'a str> {
        let len   = self.u32()? as usize;
        let bytes = self.bytes(len)?;
        std::str::from_utf8(bytes).ok()
    }

    #[inline]
    pub fn read_panel(&mut self, leaf_count: u32) -> Option<SessionPanel> {
        match self.u8()? {
            0 => {
                let index = self.u32()?.min(leaf_count.saturating_sub(1));
                Some(SessionPanel::Leaf { leaf_index: index })
            }

            1 => {
                let vertical = self.u8()? != 0;
                let ratio    = self.f32()?;
                let left     = Box::new(self.read_panel(leaf_count)?);
                let right    = Box::new(self.read_panel(leaf_count)?);
                Some(SessionPanel::Split { vertical, ratio, left, right })
            }

            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct SessionBuffer<'a> {
    pub path: &'a str,   // empty = scratch
}

#[derive(Debug)]
pub struct SessionView<'a> {
    pub buffer_index: u32,
    pub line:         u32,
    pub col:          u32,
    pub scroll_anim:  f32,
    // Back-reference filled in during load, not stored on disk
    pub path:         &'a str,
}

/// A named blob written by an extension hook, read back during restore.
pub struct SessionChunk<'a> {
    pub id:   CustomChunkId,
    pub data: &'a [u8],
}

// :Configuration: Custom Data in SessionPanel
pub enum SessionPanel {
    Leaf  { leaf_index: u32 },
    Split { vertical: bool, ratio: f32, left: Box<SessionPanel>, right: Box<SessionPanel> }, // @Memory @Speed
}

pub struct Session<'a> {
    pub cwd:          &'a str,
    pub buffers:      Vec<SessionBuffer<'a>>,
    pub views:        Vec<SessionView<'a>>,
    /// view_index for each leaf in root-panel-tree order
    pub leaves:       Vec<u32>,
    pub active_index: u32,
    pub root:         SessionPanel,
    pub chunks:       Vec<SessionChunk<'a>>,
}

pub fn save_session(editor: &Editor, path: &Path) -> std::io::Result<f32> {
    let t0 = Instant::now();

    let mut out    = Vec::with_capacity(4096);                         // @Memory @Speed: Reuse that memory?
    let mut root_leaves = Vec::with_capacity(editor.panels.len() * 4 / 6);  // @Memory @Tune

    write_u32(&mut out, MAGIC);
    write_u8 (&mut out, VERSION);
    write_str(&mut out, editor.canonicalized_current_working_directory.as_str());

    //
    // Assign a stable serial index to every BufferId.
    //
    let mut buffer_index: FastHashMap<BufferId, u32> =
        FastHashMap::with_capacity_and_hasher(editor.buffers.len(), Default::default());

    write_u32(&mut out, editor.buffers.len() as u32);
    for (buf_id, buf) in editor.buffers.iter() {
        buffer_index.insert(buf_id, buffer_index.len() as u32);
        let file_path = buf.path.as_deref().and_then(|p| p.to_str()).unwrap_or("");
        write_str(&mut out, file_path);
    }

    //
    // Assign a stable serial index to every ViewId.
    //
    let mut view_index: FastHashMap<ViewId, u32> =
        FastHashMap::with_capacity_and_hasher(editor.views.len(), Default::default());

    write_u32(&mut out, editor.views.len() as u32);
    for (view_id, view) in editor.views.iter() {
        view_index.insert(view_id, view_index.len() as u32);
        let bidx = buffer_index.get(&view.buffer_id).copied().unwrap_or(0);
        write_u32(&mut out, bidx);
        write_u32(&mut out, view.cursor_target_line);
        write_u32(&mut out, view.cursor_target_col);
        write_f32(&mut out, view.scroll_anim);
    }

    collect_leaves(editor, editor.root_panel, &mut root_leaves);

    write_u32(&mut out, root_leaves.len() as u32);
    for &(_panel_id, vid) in &root_leaves {
        let vidx = view_index.get(&vid).copied().unwrap_or(0);
        write_u32(&mut out, vidx);
    }

    // Active leaf index
    let active_view = editor.active_view();
    let active_index = root_leaves.iter().enumerate()
        .find(|&(_, &(pid, vid))| {
            vid == active_view.id
                && editor.views[vid].panel_id() == Some(pid)
        })
        .map(|(i, _)| i as u32)
        .unwrap_or(u32::MAX);

    write_u32(&mut out, active_index);
    write_panel_tree(&mut out, editor, editor.root_panel, &root_leaves, &view_index);

    //
    // Custom chunks
    //
    // Ask each registered hook to serialise its state into a sub-buffer; we
    // collect them all first so we know the count before we write it.
    //
    let mut chunks: Vec<(CustomChunkId, Box<[u8]>)> = Vec::new();
    if let Some(hooks) = &editor.hooks.session_save_chunks {
        for hook in hooks {
            if let Some((id, data)) = hook(editor, &view_index, &buffer_index) {
                chunks.push((id, data.into()));
            }
        }
    }

    write_u32(&mut out, chunks.len() as u32);
    for (id, data) in &chunks {
        write_u16(&mut out, *id);
        write_u32(&mut out, data.len() as u32);
        out.extend_from_slice(data);
    }

    let result = std::fs::write(path, &out);
    let time   = t0.elapsed().as_micros() as f32;
    if result.is_ok() {
        println!("[Saved session in {time}us]");
    }
    result.map(|_| time)
}

// Walk the panel tree and collect (panel_id, view_id) for every leaf
// that has a real file buffer. Order matches the write_panel traversal.
fn collect_leaves(editor: &Editor, root: PanelId, out: &mut Vec<(PanelId, ViewId)>) {
    let mut stack = SmallVec::<[_; 12]>::with_capacity((editor.panels.len() as f32 * 1.5) as usize);
    stack.push(root);

    while let Some(id) = stack.pop() {
        match editor.panels[id].kind {
            PanelKind::Leaf { view_id } => {
                // nocheckin @Incomplete
                // let buffer_id = editor.views[view_id].buffer_id;
                // if buffer_id != editor.lister_query_buffer {
                //     out.push((id, view_id));
                // }
                out.push((id, view_id));
            }

            PanelKind::Split(s) => {
                stack.push(s.right_id);
                stack.push(s.left_id);
            }

            PanelKind::Custom(c) => {
                if let Some(collect_leaf_panels_hook) = editor.hooks.collect_leaf_panels_for_session_saving {
                    let leaves = collect_leaf_panels_hook(editor, id, c, &mut stack);
                    out.extend(leaves);
                }
            }
        }
    }
}

pub fn load_session<'a>(data: &'a [u8]) -> Option<Session<'a>> {
    let t0 = Instant::now();

    let mut r = Reader::new(&data);

    if r.u32()? != MAGIC   { return None }
    if r.u8()?  != VERSION { return None }

    let cwd = r.str()?;

    // buffers
    let buf_count = r.u32()? as usize;
    let mut buffers = Vec::with_capacity(buf_count);
    for _ in 0..buf_count {
        buffers.push(SessionBuffer { path: r.str()? });
    }

    // Views
    let view_count = r.u32()? as usize;
    let mut views = Vec::with_capacity(view_count);
    for _ in 0..view_count {
        let buffer_index = r.u32()?;
        let line         = r.u32()?;
        let col          = r.u32()?;
        let scroll_anim  = r.f32()?;
        let path = buffers.get(buffer_index as usize)
            .map(|b| b.path)
            .unwrap_or("");
        views.push(SessionView { buffer_index, line, col, scroll_anim, path });
    }

    // Leaf table
    let leaf_count = r.u32()?;
    let mut leaves = Vec::with_capacity(leaf_count as usize);
    for _ in 0..leaf_count {
        leaves.push(r.u32()?);
    }

    let active_index = r.u32()?;
    let root         = r.read_panel(leaf_count)?;

    // Custom chunks
    let chunk_count = r.u32()? as usize;
    let mut chunks  = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        let id    = r.u16()?;
        let byte_len = r.u32()? as usize;
        let data    = r.bytes(byte_len)?;
        chunks.push(SessionChunk { id, data });
    }

    println!("[Loaded session in {time}us]", time = t0.elapsed().as_micros() as f32);

    Some(Session { cwd, leaves, active_index, root, buffers, chunks, views })
}

pub fn apply_session(editor: &mut Editor, session: Session) -> f32 {
    let t0 = Instant::now();

    editor.canonicalized_current_working_directory = session.cwd.into();

    let line_h = editor.line_h();

    //
    // Materialise every buffer
    //
    // Serial buffer index -> real BufferId
    //
    let mut buf_ids: Vec<BufferId> = Vec::with_capacity(session.buffers.len());

    for (i, sb) in session.buffers.iter().enumerate() {
        let file_path = Path::new(&sb.path);
        let canon     = file_path.canonicalize().ok();

        //
        // If this file is already open, reuse its buffer id
        //
        let existing_buffer_id = canon.as_deref()
            .and_then(|c| editor.canonicalized_path_to_buffer_id.get(c))
            .copied();

        let buffer_id = if let Some(buffer_id) = existing_buffer_id {
            buffer_id
        } else {
            let buffer = Buffer::from_file(file_path).unwrap_or_else(|_| {
                let mut b = Buffer::new();
                b.path = Some(file_path.into());
                b
            });

            let buffer_id = if i == 0 {
                // Reuse the slot Editor::new already allocated so we don't
                // accumulate a phantom buffer every startup.
                editor.buffers[editor.root_buffer] = buffer;
                editor.root_buffer
            } else {
                editor.push_buffer(buffer)
            };

            buffer_id
        };

        buf_ids.push(buffer_id);
    }

    //
    // Materialise every view
    //
    // serial view index -> real ViewId
    //
    let mut view_ids: Vec<ViewId> = Vec::with_capacity(session.views.len());

    for (i, sv) in session.views.iter().enumerate() {
        let buf_id = buf_ids
            .get(sv.buffer_index as usize)
            .copied()
            .unwrap_or_else(|| editor.buffers.keys().next().unwrap());

        let view_id = if i == 0 {
            // Reuse the canonical main view slot
            editor.views[VIEW_MAIN].buffer_id = buf_id;
            VIEW_MAIN
        } else {
            let vid = editor.views.next_key();
            editor.views.push(View::new(vid, buf_id));
            vid
        };

        let total_lines = editor.buffers[buf_id].text.len_lines() as u32;

        editor.views[view_id].cursor_anim_x      = f32::NAN;
        editor.views[view_id].cursor_anim_y      = f32::NAN;
        editor.buffers[buf_id].is_dirty          = true;

        {
            let rect       = editor.panels[editor.active_panel].rect;
            let max_scroll = ((total_lines as f32 * line_h) - rect.h).max(0.0);
            let scroll     = (sv.scroll_anim / line_h).round() * line_h;
            let scroll     = scroll.clamp(0.0, max_scroll);

            editor.views[view_id].scroll      = scroll;
            editor.views[view_id].scroll_anim = scroll;
            editor.views[view_id].scroll_vel  = 0.0;
        }

        editor.buffers[buf_id].set_cursor_line_col(
            sv.line, sv.col,
            &mut editor.views[view_id].cursor,
        );
        editor.views[view_id].cursor_target_line = sv.line.clamp(0, total_lines.saturating_sub(15));
        editor.views[view_id].cursor_target_col  = sv.col;

        view_ids.push(view_id);
    }

    //
    // Rebuild the root panel tree
    //
    // The leaf table maps leaf_index -> view serial index -> real ViewId
    //
    let leaf_view_ids: Vec<ViewId> = session.leaves.iter()
        .map(|&vi| view_ids.get(vi as usize).copied()
             .unwrap_or_else(|| editor.views.keys().next().unwrap()))
        .collect();

    let new_root = apply_panel(editor, &session.root, &leaf_view_ids);
    editor.root_panel = new_root;

    //
    // Restore active panel/buffer
    //
    if let Some(&view_id) = leaf_view_ids.get(session.active_index as usize) {
        if let Some(panel_id) = editor.views[view_id].panel_id() {
            editor.active_panel = panel_id;
        }
        editor.mru_focus(editor.views[view_id].buffer_id);
    }

    //
    // Extension chunk restore hooks
    //
    let session_restore_chunks = editor.hooks.session_restore_chunks.clone(); // @Clone
    for chunk in &session.chunks {
        if let Some(hooks) = &session_restore_chunks {
            for hook in hooks {
                hook(editor, chunk.id, chunk.data, &view_ids, &buf_ids);
            }
        }
    }

    let time = t0.elapsed().as_micros() as f32;
    println!("[Applied session in {time}us]");
    editor.did_we_apply_any_sessions = true;

    time
}

fn write_panel_tree(
    out:         &mut Vec<u8>,
    editor:      &Editor,
    panel_id:    PanelId,
    root_leaves: &[(PanelId, ViewId)],
    _view_index: &FastHashMap<ViewId, u32>,
) {
    match editor.panels[panel_id].kind {
        PanelKind::Leaf { .. } => {
            let index = root_leaves.iter()
                .position(|&(pid, _)| pid == panel_id)
                .map(|i| i as u32)
                .unwrap_or(0);
            write_u8(out, PANEL_KIND_LEAF);
            write_u32(out, index);
        }

        PanelKind::Split(s) => {
            write_u8(out, PANEL_KIND_SPLIT);
            write_u8(out, s.vertical as u8);
            write_f32(out, s.ratio);
            write_panel_tree(out, editor, s.left_id,  root_leaves, _view_index);
            write_panel_tree(out, editor, s.right_id, root_leaves, _view_index);
        }

        // :Hook @Incomplete: write_panel_tree hook
        PanelKind::Custom(_) => {}
    }
}

fn apply_panel(editor: &mut Editor, node: &SessionPanel, leaf_views: &[ViewId]) -> PanelId {
    match node {
        SessionPanel::Leaf { leaf_index } => {
            let view_id = leaf_views
                .get(*leaf_index as usize)
                .copied()
                .unwrap_or_else(|| editor.views.keys().next().unwrap());

            // Reuse the existing panel for this view if it already has one,
            // otherwise push a new one.
            if let Some(existing_panel) = editor.views[view_id].panel_id() {
                // Update kind in case it changed
                editor.panels[existing_panel].kind = PanelKind::Leaf { view_id };
                existing_panel
            } else {
                let panel_id = editor.panels.next_key();
                editor.panels.push(Panel {
                    id:   panel_id,
                    rect: Rect::default(),
                    rect_including_panel_bar: Rect::default(),
                    kind: PanelKind::Leaf { view_id },
                });
                editor.views[view_id].panel_id = panel_id;
                panel_id
            }
        }

        SessionPanel::Split { vertical, ratio, left, right } => {
            let left_id  = apply_panel(editor, left,  leaf_views);
            let right_id = apply_panel(editor, right, leaf_views);
            let panel_id = editor.panels.next_key();
            editor.panels.push(Panel {
                id:   panel_id,
                rect: Rect::default(),
                rect_including_panel_bar: Rect::default(),
                kind: PanelKind::Split(PanelSplit {
                    vertical: *vertical,
                    ratio:    *ratio,
                    left_id,
                    right_id,
                }),
            });

            panel_id
        }

        // :Hook @Incomplete: apply_panel hook
    }
}

pub fn default_session_path() -> Box<Path> {
    // ~/.local/share/naysayer/session.bin                 on Linux
    // ~/Library/Application Support/naysayer/session.bin  on Mac
    // %APPDATA%\naysayer\session.bin                      on Windows
    let base = dirs::data_dir().unwrap_or_else(|| ".".into());

    let dir = base.join("naysayer");
    _ = std::fs::create_dir_all(&dir);
    dir.join("session.bin").into()
}

pub fn pretty_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }

    path.display().to_string()
}
