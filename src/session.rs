//
// Format:
//
//   u32  magic
//   u8   version
//   str  cwd
//   u32  leaf count
//   [leaf count] x {
//       str  path
//       u32  cursor line
//       u32  cursor col
//       f32  scroll
//   }
//   u32 active_leaf_index
//   panel tree (recursive):
//       u8 tag (0 = Leaf, 1 = Split)
//       Leaf:  u32 leaf_index
//       Split: u8 vertical, f32 ratio, left_panel, right_panel
//

use std::path::Path;
use std::time::Instant;

use crate::buffer::Buffer;
use crate::{BufferId, PanelId, ViewId, Panel, Editor, PanelKind, PanelSplit, Rect, View};

const MAGIC:   u32 = 0x4E455353; // "SSEN"
const VERSION: u8  = 1;

const PANEL_KIND_LEAF:  u8 = 0;
const PANEL_KIND_SPLIT: u8 = 1;

#[inline] fn write_u8 (out: &mut Vec<u8>, v: u8)   { out.push(v); }
#[inline] fn write_u32(out: &mut Vec<u8>, v: u32)  { out.extend_from_slice(&v.to_le_bytes()); }
#[inline] fn write_f32(out: &mut Vec<u8>, v: f32)  { out.extend_from_slice(&v.to_le_bytes()); }
#[inline] fn write_str(out: &mut Vec<u8>, s: &str) {
    write_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

#[inline]
fn write_panel(out: &mut Vec<u8>, editor: &Editor, panel_id: PanelId, leaves: &[(PanelId, ViewId)]) {
    match editor.panels[panel_id].kind {
        PanelKind::Leaf { .. } => {
            let index = leaves.iter()
                .position(|&(pid, _)| pid == panel_id)
                .map(|i| i as u32)
                .unwrap_or(0);

            write_u8(out,  PANEL_KIND_LEAF);
            write_u32(out, index);
        }

        PanelKind::Split(s) => {
            write_u8(out, PANEL_KIND_SPLIT);
            write_u8(out, s.vertical as u8);
            write_f32(out, s.ratio);
            write_panel(out, editor, s.left_id,  leaves);
            write_panel(out, editor, s.right_id, leaves);
        }

        PanelKind::ListerSplit => {}
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos:  usize,
}

impl<'a> Reader<'a> {
    #[inline]
    const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[inline]
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos + n;
        if end > self.data.len() { return None; }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Some(s)
    }

    #[inline] fn u8 (&mut self) -> Option<u8>  { Some( u8::from_le_bytes(self.bytes(1)?.try_into().ok()?)) }
    #[inline] fn u32(&mut self) -> Option<u32> { Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?)) }
    #[inline] fn f32(&mut self) -> Option<f32> { Some(f32::from_le_bytes(self.bytes(4)?.try_into().ok()?)) }
    #[inline] fn str(&mut self) -> Option<&'a str> {
        let len   = self.u32()? as usize;
        let bytes = self.bytes(len)?;
        std::str::from_utf8(bytes).ok()
    }

    #[inline]
    fn read_panel(&mut self, leaf_count: u32) -> Option<SessionPanel> {
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
pub struct SessionLeaf<'a> {
    pub path:   &'a str,
    pub line:   u32,
    pub col:    u32,
    pub scroll_anim: f32,
}

pub enum SessionPanel {
    Leaf  { leaf_index: u32 },
    Split { vertical: bool, ratio: f32, left: Box<SessionPanel>, right: Box<SessionPanel> }, // @Memory @Speed
}

pub struct Session<'file> {
    pub cwd:        &'file str,
    pub leaves:     Vec<SessionLeaf<'file>>,
    pub active_index: u32,
    pub root:       SessionPanel,
}

pub fn save_session(editor: &Editor, path: &Path) -> std::io::Result<()> {
    let t0 = Instant::now();

    let mut out    = Vec::with_capacity(4096); // @Memory @Speed: Reuse that memory?
    let mut leaves = Vec::with_capacity(editor.panels.len() * 4 / 6);  // @Memory @Tune

    write_u32(&mut out, MAGIC);
    write_u8 (&mut out, VERSION);
    write_str(&mut out, editor.canonicalized_current_working_directory.as_str());

    collect_leaves(editor, editor.root_panel, &mut leaves);

    write_u32(&mut out, leaves.len() as u32);

    let active_buf = editor.active_view().buffer_id;
    let mut active_index = u32::MAX;

    for (i, &(_, view_id)) in leaves.iter().enumerate() {
        let view        = &editor.views[view_id];
        let buf         = &editor.buffers[view.buffer_id];
        let file_path   = buf.path.as_deref().and_then(|p| p.to_str()).unwrap_or("");
        let (line, col) = (view.cursor_target_line, view.cursor_target_col);

        if view.buffer_id == active_buf { active_index = i as u32; }

        write_str(&mut out, file_path);
        write_u32(&mut out, line);
        write_u32(&mut out, col);
        write_f32(&mut out, view.scroll_anim);
    }

    write_u32(&mut out, active_index);
    write_panel(&mut out, editor, editor.root_panel, &leaves);

    let result = std::fs::write(path, &out);

    if result.is_ok() {
        println!("[Saved session in {time}us]", time = t0.elapsed().as_micros() as f32);
    }

    result
}

// Walk the panel tree and collect (panel_id, view_id) for every leaf
// that has a real file buffer. Order matches the write_panel traversal.
fn collect_leaves(editor: &Editor, panel_id: PanelId, out: &mut Vec<(PanelId, ViewId)>) {
    match editor.panels[panel_id].kind {
        PanelKind::Leaf { view_id } => {
            let buf_id = editor.views[view_id].buffer_id;
            if buf_id != editor.lister_query_buffer
                && editor.buffers[buf_id].path.is_some()
            {
                out.push((panel_id, view_id));
            }
        }

        PanelKind::Split(s) => {
            collect_leaves(editor, s.left_id,  out);
            collect_leaves(editor, s.right_id, out);
        }

        PanelKind::ListerSplit => {}
    }
}

pub fn load_session<'a>(data: &'a [u8]) -> Option<Session<'a>> {
    let t0 = Instant::now();

    let mut r = Reader::new(&data);

    if r.u32()? != MAGIC   { return None; }
    if r.u8()?  != VERSION { return None; }

    let cwd   = r.str()?;
    let count = r.u32()? as usize;

    let mut leaves = Vec::with_capacity(count);
    for _ in 0..count {
        let path   = r.str()?;
        let line   = r.u32()?;
        let col    = r.u32()?;
        let scroll_anim = r.f32()?;
        leaves.push(SessionLeaf { path, line, col, scroll_anim });
    }

    let active_index = r.u32()?;
    let root         = r.read_panel(count as u32)?;

    println!("[Loaded session in {time}us]", time = t0.elapsed().as_micros() as f32);

    Some(Session { cwd, leaves, active_index, root })
}

pub fn apply_session(editor: &mut Editor, session: Session) {
    let t0 = Instant::now();

    editor.canonicalized_current_working_directory = session.cwd.into();

    let mut leaf_buffers = Vec::<BufferId>::with_capacity(session.leaves.len());
    let mut leaf_views   = Vec::<ViewId>  ::with_capacity(session.leaves.len());

    for sl in &session.leaves {
        let file_path = Path::new(&sl.path);

        let buf = Buffer::from_file(file_path).unwrap_or_else(|_| {
            //
            // We couldn't find the file, make an empty buffer instead.
            //
            let mut b = Buffer::new();
            b.path = Some(file_path.into());
            b
        });

        let total_lines_count = buf.text.len_lines();

        let buffer_id = editor.buffers.push(buf);
        let view_id   = editor.views.next_key();
        editor.views.push(View::new(view_id, buffer_id));

        editor.buffers[buffer_id].set_cursor_line_col(
            sl.line, sl.col,
            &mut editor.views[view_id].cursor,
        );

        let line = sl.line.clamp(0, total_lines_count as _);
        editor.views[view_id].cursor_target_line = line;
        editor.views[view_id].cursor_target_col  = sl.col;
        editor.views[view_id].scroll             = sl.scroll_anim;
        editor.views[view_id].scroll_anim        = sl.scroll_anim;

        editor.mru_register_new_buffer(buffer_id);

        if let Ok(canon) = file_path.canonicalize() {
            editor.canonicalized_path_to_buffer_id.insert(canon.into(), buffer_id);
        }

        leaf_buffers.push(buffer_id);
        leaf_views.push(view_id);
    }

    let new_root = apply_panel(editor, &session.root, &leaf_views);
    editor.root_panel = new_root;

    //
    // Restore active buffer.
    //
    if let Some(&view_id) = leaf_views.get(session.active_index as usize) {
        let buf_id = editor.views[view_id].buffer_id;
        if let Some(panel_id) = editor.views[view_id].panel_id() {
            editor.active_panel = panel_id;
        }
        editor.mru_focus(buf_id);
    }

    println!("[Applied session in {time}us]", time = t0.elapsed().as_micros() as f32);
}

fn apply_panel(editor: &mut Editor, node: &SessionPanel, leaf_views: &[ViewId]) -> PanelId {
    match node {
        SessionPanel::Leaf { leaf_index } => {
            let view_id = leaf_views
                .get(*leaf_index as usize)
                .copied()
                .unwrap_or_else(|| editor.views.keys().next().unwrap());

            let panel_id = editor.panels.next_key();
            editor.panels.push(Panel {
                id:   panel_id,
                rect: Rect::default(),
                kind: PanelKind::Leaf { view_id },
            });
            editor.views[view_id].panel_id = panel_id;

            panel_id
        }

        SessionPanel::Split { vertical, ratio, left, right } => {
            let left_id  = apply_panel(editor, left,  leaf_views);
            let right_id = apply_panel(editor, right, leaf_views);
            let panel_id = editor.panels.next_key();
            editor.panels.push(Panel {
                id:   panel_id,
                rect: Rect::default(),
                kind: PanelKind::Split(PanelSplit {
                    vertical: *vertical,
                    ratio:    *ratio,
                    left_id,
                    right_id,
                }),
            });

            panel_id
        }
    }
}

pub fn default_session_path() -> std::path::PathBuf {
    // ~/.local/share/naysayer/session.bin                 on Linux
    // ~/Library/Application Support/naysayer/session.bin  on Mac
    // %APPDATA%\naysayer\session.bin                      on Windows
    let base = dirs::data_dir().unwrap_or_else(|| ".".into());

    let dir = base.join("naysayer");
    _ = std::fs::create_dir_all(&dir);
    dir.join("session.bin")
}
