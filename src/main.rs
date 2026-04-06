#![allow(unused, unused_imports, dead_code)]

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod gpu;
mod util;
mod color;
mod buffer;
mod command;
mod lexer;

use std::sync::Arc;
use std::time::Instant;

use buffer::{Buffer, Cursor};
use color::Color;
use command::EditorCommand;
use gpu::{Gpu, reset_atlas};
use lexer::token_color;

use winit::window::{Window, WindowId};
use winit::application::ApplicationHandler;
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};

pub struct Palette {
    pub bg:          Color,
    pub selection:   Color,
    pub cursor:      Color,
    pub cursor_text: Color,
}

#[inline]
pub const fn palette() -> Palette {
    Palette {
        bg:          Color::hex(0x0f0b05),
        selection:   Color::hex(0x2a2012),
        cursor:      Color::hex(0xc3a983),
        cursor_text: Color::rgba(13, 13, 13, 255),
    }
}

const MIN_SCALE:    f32 = 0.75;
const MAX_SCALE:    f32 = 5.00;
const SCALE_STEP:   f32 = 0.25;
const PADDING_LEFT: f32 = 8.0;

macro_rules! define_base_and_scale {
    ($(const $name:ident: f32 = $value:expr;)*) => {
        paste::paste! {
            $(
                const $name: f32 = $value;
                #[inline] fn [<scale_ $name:lower>](scale: f32) -> f32 { $name * scale }
            )*
        }
    };
}

define_base_and_scale! {
    const BASE_LINE_HEIGHT: f32 = 16.35;
    const BASE_FONT_SIZE:   f32 = 15.0;
}

const BLINK_ON_MS:  u128 = 530;
const BLINK_OFF_MS: u128 = 370;

fn cursor_visible(epoch: &Instant) -> bool {
    let elapsed = epoch.elapsed().as_millis() % (BLINK_ON_MS + BLINK_OFF_MS);
    elapsed < BLINK_ON_MS
}


#[derive(Clone, Copy, Default, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    #[inline] pub fn full(win_w: f32, win_h: f32) -> Self { Self { x: 0.0, y: 0.0, w: win_w, h: win_h } }
    #[inline] pub fn x1(&self) -> f32 { self.x + self.w }
    #[inline] pub fn y1(&self) -> f32 { self.y + self.h }
    #[inline] pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x1() && py >= self.y && py < self.y1()
    }

    /// Split horizontally: left gets `ratio` of the width, right gets the rest.
    #[inline]
    pub fn split_x(self, ratio: f32) -> (Rect, Rect) {
        let mid = (self.x + self.w * ratio).round();
        (
            Rect { x: self.x, y: self.y, w: mid - self.x,        h: self.h },
            Rect { x: mid,    y: self.y, w: self.x1() - mid,     h: self.h },
        )
    }

    /// Split vertically: top gets `ratio` of the height, bottom gets the rest.
    #[inline]
    pub fn split_y(self, ratio: f32) -> (Rect, Rect) {
        let mid = (self.y + self.h * ratio).round();
        (
            Rect { x: self.x, y: self.y,  w: self.w, h: mid - self.y       },
            Rect { x: self.x, y: mid,     w: self.w, h: self.y1() - mid    },
        )
    }
}

//
// TextLayout  -  one per visible panel per frame
//
// Stores per-character color for the visible region so that
// syntax highlighting writes here once and the render loop
// just reads it, rather than recomputing colors every frame.
//

pub struct TextLayout {
    pub buffer_id:  BufferId,
    pub rect:       Rect,
    pub first_line: usize,
    pub line_count: usize,
    pub font_size:  f32,
    pub line_h:     f32,

    // Flat: one Color per visible *char* across all visible lines,
    // in top-to-bottom, left-to-right order.
    // Index with `char_color_index`.
    pub char_colors: Vec<Color>,

    // Flat: [x0,y0,x1,y1] per visible char - used for click hit-testing.
    pub char_rects:  Vec<[f32; 4]>,

    // Line start indices into char_colors/char_rects.
    // line_offsets[i] = index of first char of visible line i.
    pub line_offsets: Vec<usize>,
}

impl TextLayout {
    pub fn new(buffer_id: BufferId, rect: Rect, first_line: usize, line_count: usize,
               font_size: f32, line_h: f32) -> Self {
        Self {
            buffer_id, rect, first_line, line_count, font_size, line_h,
            char_colors:  Vec::new(),
            char_rects:   Vec::new(),
            line_offsets: Vec::new(),
        }
    }

    /// Map (line_idx, col) relative to first_line -> flat index.
    #[inline]
    pub fn flat_index(&self, vis_line: usize, col: usize) -> Option<usize> {
        let base = *self.line_offsets.get(vis_line)?;
        let end  = self.line_offsets.get(vis_line + 1).copied()
            .unwrap_or(self.char_colors.len());
        let idx = base + col;
        if idx < end { Some(idx) } else { None }
    }
}

//
// Panel system  -  2-split max for now
//

pub type PanelId   = usize;
pub type BufferId  = usize;
pub type ViewId    = usize;

pub const PANEL_NONE: PanelId = usize::MAX;

#[derive(Clone, Copy, Debug)]
pub struct PanelSplit {
    pub vertical: bool,   // true = left/right, false = top/bottom
    pub ratio:    f32,    // 0.0..1.0  (left or top fraction)
    pub left_id:  PanelId,
    pub right_id: PanelId,
}

#[derive(Clone, Debug)]
pub enum PanelKind {
    Leaf  { view_id: ViewId },
    Split(PanelSplit),
}

#[derive(Clone, Debug)]
pub struct Panel {
    pub id:     PanelId,
    pub rect:   Rect,
    pub kind:   PanelKind,
}

#[derive(Clone, Debug)]
pub struct View {
    pub id:          ViewId,
    pub buffer_id:   BufferId,
    pub scroll:      f32,
    pub cursor:      Cursor
}

impl View {
    pub fn new(id: ViewId, buffer_id: BufferId) -> Self {
        Self { id, buffer_id, scroll: 0.0, cursor: Cursor::new() }
    }

    pub fn scroll_to_cursor(&mut self, line: usize, line_h: f32, rect: Rect) {
        let cursor_top    = line as f32 * line_h;
        let cursor_bottom = cursor_top + line_h;
        let margin        = line_h * 3.0;

        if cursor_top - margin < self.scroll {
            self.scroll = (cursor_top - margin).max(0.0);
        }
        let view_bottom = self.scroll + rect.h;
        if cursor_bottom + margin > view_bottom {
            self.scroll = cursor_bottom + margin - rect.h;
        }
    }

    pub fn clamp_scroll(&mut self, total_lines: usize, line_h: f32, rect: Rect) {
        let max = (total_lines as f32 * line_h - rect.h).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max);
    }

    #[inline] pub fn first_visible_line(&self, line_h: f32) -> usize { (self.scroll / line_h) as usize }
    #[inline] pub fn visible_line_count(&self, rect: Rect, line_h: f32) -> usize { (rect.h / line_h) as usize + 2 }
    #[inline] pub fn line_to_screen_y(&self, line: usize, rect: Rect, line_h: f32) -> f32 {
        rect.y + line as f32 * line_h - self.scroll
    }
}

//
// EditorState  -  owns everything, lives on the main thread
//

pub struct EditorState {
    // Storage
    pub buffers:      Vec<Buffer>,
    pub views:        Vec<View>,
    pub panels:       Vec<Panel>,

    // Which panel is active (receives keyboard input)
    pub active_panel: PanelId,

    // Root panel id - its rect always equals the window
    pub root_panel:   PanelId,

    // Scale for font/line-height
    pub scale:        f32,

    // Cursor blink
    pub blink_epoch:  Instant,

    // Mouse
    pub mouse_pos:            (f32, f32),
    pub mouse_left_pressed:   bool,
}

impl EditorState {
    pub fn new(buffer: Buffer) -> Self {
        let buf_id  = 0;
        let view_id = 0;
        let panel_id = 0;

        let buffers = vec![buffer];
        let views   = vec![View::new(view_id, buf_id)];
        let panels  = vec![Panel {
            id:   panel_id,
            rect: Rect::default(), // set on first resize / resumed
            kind: PanelKind::Leaf { view_id },
        }];

        Self {
            buffers,
            views,
            panels,
            active_panel: panel_id,
            root_panel:   panel_id,
            scale:        1.0,
            blink_epoch:  Instant::now(),
            mouse_pos:    (0.0, 0.0),
            mouse_left_pressed: false,
        }
    }

    pub fn view_and_buffer_mut(&mut self, view_id: ViewId) -> (&mut View, &mut Buffer) {
        let buf_id = self.views[view_id].buffer_id;
        // Split borrows manually
        // This is safe because view and buffer are different Vecs
        let view   = &mut self.views[view_id];
        let buffer = &mut self.buffers[buf_id];
        (view, buffer)
    }

    pub fn active_view_and_buffer_mut(&mut self) -> (&mut View, &mut Buffer) {
        let view_id = self.active_view_id();
        self.view_and_buffer_mut(view_id)
    }

    // ---- panel helpers ----

    pub fn panel(&self, id: PanelId) -> &Panel        { &self.panels[id] }
    pub fn panel_mut(&mut self, id: PanelId) -> &mut Panel { &mut self.panels[id] }

    pub fn active_view_id(&self) -> ViewId {
        match self.panels[self.active_panel].kind {
            PanelKind::Leaf { view_id } => view_id,
            _ => 0,
        }
    }

    pub fn active_view(&self) -> &View             { &self.views[self.active_view_id()] }
    pub fn active_view_mut(&mut self) -> &mut View { let id = self.active_view_id(); &mut self.views[id] }

    pub fn active_buffer(&self) -> &Buffer {
        let buf_id = self.active_view().buffer_id;
        &self.buffers[buf_id]
    }
    pub fn active_buffer_mut(&mut self) -> &mut Buffer {
        let buf_id = self.active_view().buffer_id;
        &mut self.buffers[buf_id]
    }

    // ---- layout ----

    /// Re-layout the panel tree from the root given the window rect.
    /// For now: root can either be a Leaf (single view) or a Split
    /// (exactly two children, both Leaf).  N+2 panels max.
    pub fn layout_panels(&mut self, win_rect: Rect) {
        self.layout_panel(self.root_panel, win_rect);
    }

    fn layout_panel(&mut self, id: PanelId, rect: Rect) {
        self.panels[id].rect = rect;
        // collect split info without holding borrow
        let split = match self.panels[id].kind {
            PanelKind::Split(s) => s,
            PanelKind::Leaf { .. } => return,
        };
        let (r_left, r_right) = if split.vertical {
            rect.split_x(split.ratio)
        } else {
            rect.split_y(split.ratio)
        };
        self.layout_panel(split.left_id,  r_left);
        self.layout_panel(split.right_id, r_right);
    }

    /// Split the active panel.  Creates two new panels + one new view
    /// (the new panel gets a new view into the same buffer).
    pub fn split_active(&mut self, vertical: bool, ratio: f32) {
        let active_id   = self.active_panel;
        let old_view_id = match self.panels[active_id].kind {
            PanelKind::Leaf { view_id } => view_id,
            _ => return, // already split
        };
        let buf_id = self.views[old_view_id].buffer_id;

        // New view for the right/bottom child - same buffer, fresh scroll
        let new_view_id = self.views.len();
        self.views.push(View::new(new_view_id, buf_id));

        // Two new leaf panels
        let left_id  = self.panels.len();
        let right_id = left_id + 1;

        self.panels.push(Panel { id: left_id,  rect: Rect::default(), kind: PanelKind::Leaf { view_id: old_view_id } });
        self.panels.push(Panel { id: right_id, rect: Rect::default(), kind: PanelKind::Leaf { view_id: new_view_id } });

        // Turn the old panel into a split node
        self.panels[active_id].kind = PanelKind::Split(PanelSplit {
            vertical,
            ratio,
            left_id,
            right_id,
        });

        // Active panel becomes the left child
        self.active_panel = left_id;
    }

    // ---- scale ----

    pub fn line_h(&self)    -> f32 { scale_base_line_height(self.scale) }
    pub fn font_size(&self) -> f32 { scale_base_font_size(self.scale) }

    // ---- blink ----

    pub fn cursor_visible(&self) -> bool { cursor_visible(&self.blink_epoch) }
    pub fn reset_blink(&mut self) { self.blink_epoch = Instant::now(); }

    // ---- hit-test: which leaf panel contains a screen point? ----

    pub fn panel_at(&self, px: f32, py: f32) -> Option<PanelId> {
        self.panels.iter()
            .filter(|p| matches!(p.kind, PanelKind::Leaf { .. }))
            .find(|p| p.rect.contains(px, py))
            .map(|p| p.id)
    }
}

/// Build the TextLayout for a single leaf panel.
/// Called once per frame per visible panel when the buffer has changed,
/// or fully every frame (cheap - it's just color assignment).
fn build_text_layout(
    gpu:    &mut Gpu,
    buffer: &Buffer,
    view:   &View,
    rect:   Rect,
    font_size: f32,
    line_h:    f32,
) -> TextLayout {
    let buf_id     = view.buffer_id;
    let first_line = view.first_visible_line(line_h);
    let line_count = view.visible_line_count(rect, line_h);

    let mut layout = TextLayout::new(buf_id, rect, first_line, line_count, font_size, line_h);

    let default_color = token_color(lexer::TokenKind::Default);
    let tokens        = &buffer.tokens;

    let mut flat_idx = 0usize;

    for vis_i in 0..line_count {
        let line_idx = first_line + vis_i;
        let Some(line) = buffer.text.get_line(line_idx) else { break };

        let line_str      = line.to_string();
        let line_str_trim = line_str.trim_end_matches('\n');
        let line_len      = line_str_trim.len(); // byte len

        layout.line_offsets.push(flat_idx);

        if line_str_trim.is_empty() { continue; }

        // Byte offset of this line in the whole file
        let line_byte_start = buffer.text.line_to_byte(line_idx);

        // Binary search: first token touching this line
        let first_tok = tokens.partition_point(|t| (t.start + t.len) as usize <= line_byte_start);

        // Build byte -> color map for this line
        let mut byte_color: Vec<Color> = vec![default_color; line_len];

        let mut tok_i = first_tok;
        while tok_i < tokens.len() {
            let tok       = &tokens[tok_i];
            let tok_start = tok.start as usize;
            let tok_end   = tok_start + tok.len as usize;
            if tok_start >= line_byte_start + line_len { break; }

            let color = token_color(tok.kind);
            let lo    = tok_start.saturating_sub(line_byte_start).min(line_len);
            let hi    = (tok_end.saturating_sub(line_byte_start)).min(line_len);
            for b in lo..hi { byte_color[b] = color; }

            tok_i += 1;
        }

        // Walk chars: emit color + rect
        let screen_y = view.line_to_screen_y(layout.first_line + vis_i, rect, line_h);
        let mut x    = rect.x + PADDING_LEFT;

        for (byte_off, ch) in line_str_trim.char_indices() {
            let advance = gpu::get_glyph(gpu, ch, font_size)
                .map(|g| g.advance)
                .unwrap_or(8.0);

            let color = byte_color.get(byte_off).copied().unwrap_or(default_color);
            layout.char_colors.push(color);
            layout.char_rects.push([x, screen_y, x + advance, screen_y + line_h]);

            x        += advance;
            flat_idx += 1;
        }
    }

    layout
}

//
// Render a single TextLayout  -  selection, cursor, text
//

fn render_text_layout(
    gpu:    &mut Gpu,
    layout: &TextLayout,
    buffer: &Buffer,
    view:   &View,
    show_cursor: bool,
) {
    let rect      = layout.rect;
    let line_h    = layout.line_h;
    let font_size = layout.font_size;

    // -- Selection --
    if let Some(anchor) = view.cursor.anchor_char_idx {
        let cursor = view.cursor.char_idx;
        let (start_idx, end_idx) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };

        if start_idx != end_idx {
            let (start_line, start_col) = buffer.char_to_line_col(start_idx);
            let (end_line,   end_col)   = buffer.char_to_line_col(end_idx);

            let draw_start = start_line.max(layout.first_line);
            let draw_end   = end_line.min(layout.first_line + layout.line_count);

            for line_idx in draw_start..=draw_end {
                let vis   = line_idx - layout.first_line;
                let Some(line) = buffer.text.get_line(line_idx) else { break };
                let line_str   = {
                    let s = line.to_string();
                    s.trim_end_matches('\n').to_string()
                };
                let line_char_len = line_str.chars().count();

                let (col_start, col_end) = if start_line == end_line {
                    (start_col, end_col)
                } else if line_idx == start_line {
                    (start_col, line_char_len)
                } else if line_idx == end_line {
                    (0, end_col)
                } else {
                    (0, line_char_len)
                };

                if col_start == col_end { continue; }

                let byte_at = |s: &str, col: usize| s.char_indices().nth(col).map(|(b,_)| b).unwrap_or(s.len());
                let sb = byte_at(&line_str, col_start);
                let eb = byte_at(&line_str, col_end);

                let x0 = rect.x + PADDING_LEFT + measure_text_width(gpu, &line_str[..sb], font_size);
                let x1 = rect.x + PADDING_LEFT + measure_text_width(gpu, &line_str[..eb], font_size);
                let y = view.line_to_screen_y(line_idx, rect, line_h);

                gpu::draw_rect(gpu, x0, y, x1 - x0, line_h, palette().selection);
            }
        }
    }

    // -- Cursor --
    if show_cursor {
        let (cursor_line, cursor_col) = buffer.cursor_line_col(&view.cursor);
        if cursor_line >= layout.first_line
        && cursor_line <  layout.first_line + layout.line_count
        {
            let vis     = cursor_line - layout.first_line;
            let line    = buffer.text.line(cursor_line).to_string();
            let up_to   = &line[..line.char_indices().nth(cursor_col).map(|(b,_)| b).unwrap_or(line.len())];
            let x_off   = measure_text_width(gpu, up_to, font_size);
            let cursor_x = rect.x + PADDING_LEFT + x_off;
            let cursor_y = view.line_to_screen_y(cursor_line, rect, line_h);
            gpu::draw_rect(gpu, cursor_x, cursor_y, 2.0, line_h, palette().cursor);
        }
    }

    // -- Text --
    let default_color = token_color(lexer::TokenKind::Default);

    for (vis_i, &line_base) in layout.line_offsets.iter().enumerate() {
        let line_idx = layout.first_line + vis_i;
        let Some(line) = buffer.text.get_line(line_idx) else { break };
        let line_str   = line.to_string();
        let line_str   = line_str.trim_end_matches('\n');
        if line_str.is_empty() { continue; }

        let line_end = layout.line_offsets.get(vis_i + 1).copied()
            .unwrap_or(layout.char_colors.len());

        let char_colors_slice = &layout.char_colors[line_base..line_end];

        let y = view.line_to_screen_y(line_idx, rect, line_h) + line_h;
        let x = rect.x + PADDING_LEFT;

        gpu::draw_text_colored(gpu, line_str, x, y, font_size, |char_idx| {
            char_colors_slice.get(char_idx).copied().unwrap_or(default_color)
        });
    }
}

//
// Click -> (line, col)  -  shared by MouseInput and CursorMoved
//

fn screen_pos_to_line_col(
    gpu:    &mut Gpu,
    buffer: &Buffer,
    view:   &View,
    rect:   Rect,
    mx: f32, my: f32,
    font_size: f32,
    line_h:    f32,
) -> (usize, usize) {
    let line_idx = (((my - rect.y) + view.scroll) / line_h) as usize;
    let line_idx = line_idx.min(buffer.text.len_lines().saturating_sub(1));

    let line     = buffer.text.line(line_idx);
    let line_str = line.to_string();
    let line_str = line_str.trim_end_matches('\n');

    let click_x = mx - rect.x - PADDING_LEFT;
    let mut col = 0usize;
    let mut x   = 0.0f32;

    for (i, c) in line_str.chars().enumerate() {
        let advance = gpu::get_glyph(gpu, c, font_size)
            .map(|g| g.advance)
            .unwrap_or(8.0);
        if click_x < x + advance * 0.5 {
            col = i;
            break;
        }
        x   += advance;
        col  = i + 1;
    }

    (line_idx, col)
}

//
// App - winit plumbing
//

#[derive(Default)]
struct App {
    gpu:    Option<Gpu>,
    editor: Option<EditorState>,
    window: Option<Arc<Window>>,
    mods:   winit::event::Modifiers,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let win: Arc<_> = el.create_window(
            Window::default_attributes()
                .with_title("naysayer")
                .with_decorations(false)
        ).unwrap().into();

        let size = win.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));

        self.gpu = Some(gpu::init(Arc::clone(&win)));

        let path   = std::env::args().nth(1).expect("usage: naysayer <file>");
        let buffer = Buffer::from_file(std::path::Path::new(&path)).expect("failed to open file");

        let mut editor = EditorState::new(buffer);
        editor.layout_panels(Rect::full(w as f32, h as f32));
        self.editor = Some(editor);
        self.window = Some(win);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let WindowEvent::ModifiersChanged(m) = &event {
            self.mods = *m;
            return;
        }

        let (Some(gpu), Some(editor), Some(win)) =
            (&mut self.gpu, &mut self.editor, &self.window) else { return };

        let ctrl = self.mods.state().control_key();
        let alt  = self.mods.state().alt_key();

        match event {
            WindowEvent::CloseRequested => el.exit(),

            WindowEvent::ModifiersChanged(m) => self.mods = m,

            // ---- Keyboard ----
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed { return; }

                // Scale
                if ctrl && let PhysicalKey::Code(KeyCode::Equal | KeyCode::Minus) = event.physical_key {
                    let delta = if matches!(event.physical_key, PhysicalKey::Code(KeyCode::Equal)) {
                        SCALE_STEP
                    } else {
                        -SCALE_STEP
                    };
                    let new = (editor.scale + delta).clamp(MIN_SCALE, MAX_SCALE);
                    if new != editor.scale {
                        editor.scale = new;
                        reset_atlas(gpu);
                    }
                    return;
                }

                // Split: Ctrl-2 = vertical split, Ctrl-1 = close split (future)
                if ctrl {
                    if let PhysicalKey::Code(KeyCode::Digit2) = event.physical_key {
                        let win_rect = Rect::full(gpu.win_w, gpu.win_h);
                        editor.split_active(true, 0.5);
                        editor.layout_panels(win_rect);
                        return;
                    }
                }

                let view_id  = editor.active_view_id();
                let (view, buf) = editor.active_view_and_buffer_mut();

                let cmd: Option<EditorCommand> = match &event.logical_key {
                    Key::Named(NamedKey::ArrowLeft)  => Some(EditorCommand::MoveLeft),
                    Key::Named(NamedKey::ArrowRight) => Some(EditorCommand::MoveRight),
                    Key::Named(NamedKey::ArrowUp)    => Some(EditorCommand::MoveUp),
                    Key::Named(NamedKey::ArrowDown)  => Some(EditorCommand::MoveDown),

                    Key::Named(NamedKey::Home) => Some(if ctrl { EditorCommand::MoveFileStart } else { EditorCommand::MoveLineStart }),
                    Key::Named(NamedKey::End)  => Some(if ctrl { EditorCommand::MoveFileEnd   } else { EditorCommand::MoveLineEnd   }),

                    Key::Named(NamedKey::Backspace) => { view.cursor.unset_anchor(); Some(EditorCommand::DeleteBackward) }
                    Key::Named(NamedKey::Delete)    => { view.cursor.unset_anchor(); Some(EditorCommand::DeleteForward)  }
                    Key::Named(NamedKey::Enter)     => { view.cursor.unset_anchor(); Some(EditorCommand::InsertNewline)  }

                    Key::Character(s) if ctrl => match s.as_str() {
                        "a" => Some(EditorCommand::MoveLineStart),
                        "e" => Some(EditorCommand::MoveLineEnd),
                        "f" => Some(EditorCommand::MoveRight),
                        "b" => Some(EditorCommand::MoveLeft),
                        "n" => Some(EditorCommand::MoveDown),
                        "p" => Some(EditorCommand::MoveUp),
                        "d" => { view.cursor.unset_anchor(); Some(EditorCommand::DeleteForward) }
                        "g" => { view.cursor.unset_anchor(); None }
                        _   => None,
                    },

                    Key::Named(NamedKey::Space) if ctrl => {
                        view.cursor.set_anchor();
                        None
                    }

                    Key::Character(s) if !ctrl => {
                        view.cursor.unset_anchor();
                        s.chars().next().map(EditorCommand::InsertChar)
                    }

                    _ => None,
                };

                if let Some(cmd) = cmd {
                    buf.apply(cmd, &mut view.cursor);

                    let (line, _) = buf.cursor_line_col(&view.cursor);

                    let line_h   = editor.line_h();
                    let win_h    = gpu.win_h;
                    let panel_id = editor.active_panel;
                    let rect     = editor.panels[panel_id].rect;
                    editor.views[view_id].scroll_to_cursor(line, line_h, rect);

                    editor.reset_blink();
                }
            }

            // ---- Mouse wheel ----
            WindowEvent::MouseWheel { delta, .. } => {
                if ctrl {
                    let dy = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y,
                        MouseScrollDelta::PixelDelta(p)   => p.y as f32 * 0.01,
                    };
                    editor.scale = (editor.scale + dy * 0.077).clamp(MIN_SCALE, MAX_SCALE);
                    reset_atlas(gpu);
                    return;
                }

                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * editor.line_h(),
                    MouseScrollDelta::PixelDelta(p)   => p.y as f32,
                };

                let (mx, my) = editor.mouse_pos;
                if let Some(panel_id) = editor.panel_at(mx, my) {
                    if let PanelKind::Leaf { view_id } = editor.panels[panel_id].kind {
                        let rect  = editor.panels[panel_id].rect;
                        let total = editor.buffers[editor.views[view_id].buffer_id].text.len_lines();
                        let line_h = editor.line_h();
                        editor.views[view_id].scroll = (editor.views[view_id].scroll - dy * 2.55).max(0.0);
                        editor.views[view_id].clamp_scroll(total, line_h, rect);
                    }
                }
            }

            // ---- Mouse buttons ----
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                editor.mouse_left_pressed = false;
            }

            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                let (mx, my) = editor.mouse_pos;

                // Switch active panel on click
                if let Some(pid) = editor.panel_at(mx, my) {
                    editor.active_panel = pid;
                }

                if let PanelKind::Leaf { view_id } = editor.panels[editor.active_panel].kind {
                    let font_size = editor.font_size();
                    let line_h    = editor.line_h();
                    let rect      = editor.panels[editor.active_panel].rect;
                    let buf_id    = editor.views[view_id].buffer_id;

                    let (line, col) = screen_pos_to_line_col(
                        gpu, &editor.buffers[buf_id], &editor.views[view_id],
                        rect, mx, my, font_size, line_h,
                    );
                    let view = &mut editor.views[view_id];
                    editor.buffers[buf_id].set_cursor_line_col(line, col, &mut view.cursor);
                    view.cursor.set_anchor();
                    editor.reset_blink();
                }

                editor.mouse_left_pressed = true;
            }

            // ---- Cursor moved ----
            WindowEvent::CursorMoved { position, .. } => {
                editor.mouse_pos = (position.x as f32, position.y as f32);

                if editor.mouse_left_pressed {
                    let (mx, my) = editor.mouse_pos;
                    let pid = editor.panel_at(mx, my).unwrap_or(editor.active_panel);

                    if let PanelKind::Leaf { view_id } = editor.panels[pid].kind {
                        let font_size = editor.font_size();
                        let line_h    = editor.line_h();
                        let rect      = editor.panels[pid].rect;
                        let buf_id    = editor.views[view_id].buffer_id;

                        let (line, col) = screen_pos_to_line_col(
                            gpu, &editor.buffers[buf_id], &editor.views[view_id],
                            rect, mx, my, font_size, line_h,
                        );
                        let view = &mut editor.views[view_id];
                        editor.buffers[buf_id].set_cursor_line_col(line, col, &mut view.cursor);
                        editor.reset_blink();
                    }
                }
            }

            // ---- Resize ----
            WindowEvent::Resized(sz) => {
                if sz.width > 0 && sz.height > 0 {
                    gpu.win_w = sz.width  as f32;
                    gpu.win_h = sz.height as f32;
                    gpu.surface_config.width  = sz.width;
                    gpu.surface_config.height = sz.height;
                    gpu.surface.configure(&gpu.device, &gpu.surface_config);
                    editor.layout_panels(Rect::full(gpu.win_w, gpu.win_h));
                }
            }

            // ---- Redraw ----
            WindowEvent::RedrawRequested => {
                let font_size    = editor.font_size();
                let line_h       = editor.line_h();
                let show_cursor  = editor.cursor_visible();
                let active_panel = editor.active_panel;

                // Collect leaf panels to render (avoid borrow issues)
                let leaf_panels: Vec<(PanelId, ViewId, Rect)> = editor.panels.iter()
                    .filter_map(|p| {
                        if let PanelKind::Leaf { view_id } = p.kind {
                            Some((p.id, view_id, p.rect))
                        } else {
                            None
                        }
                    })
                    .collect();

                for (panel_id, view_id, rect) in leaf_panels {
                    let buf_id = editor.views[view_id].buffer_id;
                    let layout = build_text_layout(
                        gpu,
                        &editor.buffers[buf_id],
                        &editor.views[view_id],
                        rect,
                        font_size,
                        line_h,
                    );
                    render_text_layout(
                        gpu,
                        &layout,
                        &editor.buffers[buf_id],
                        &editor.views[view_id],
                        show_cursor && panel_id == active_panel,
                    );
                }

                gpu::submit_frame(gpu).unwrap();
                win.request_redraw();
            }

            _ => {}
        }
    }
}

fn measure_text_width(gpu: &mut Gpu, text: &str, font_size: f32) -> f32 {
    text.chars()
        .map(|c| gpu::get_glyph(gpu, c, font_size).map(|g| g.advance).unwrap_or(8.0))
        .sum()
}

fn main() {
    let el = EventLoop::new().unwrap();
    el.set_control_flow(ControlFlow::Poll);
    el.run_app(&mut App::default()).unwrap();
}
