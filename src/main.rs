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
use std::time::{Duration, Instant};

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
    pub current_line: Color,
    pub cursor:      Color,
    pub cursor_text: Color,
}

#[inline]
pub const fn palette() -> Palette {
    Palette {
        bg:          Color::hex(0x0f0b05),
        selection:   Color::hex(0x112c4f),
        cursor:      Color::hex(0xc3a983),
        current_line: Color::hex(0x231b0e),
        cursor_text: Color::rgba(13, 13, 13, 255),
    }
}

const MIN_SCALE:    f32 = 0.75;
const MAX_SCALE:    f32 = 5.00;
const SCALE_STEP:   f32 = 0.10;

const SCROLL_ANIM_RATE: f32 = 46.67;
const CURSOR_ANIM_RATE: f32 = 99.420;

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
    const BASE_LINE_HEIGHT:   f32 = 16.35;
    const BASE_FONT_SIZE:     f32 = 15.0;
    const BASE_CURSOR_HEIGHT: f32 = 2.0;
    const BASE_CURSOR_WIDTH:  f32 = 2.0;
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
    #[inline]
    pub fn full(win_w: f32, win_h: f32) -> Self { Self { x: 0.0, y: 0.0, w: win_w, h: win_h } }

    #[inline] pub fn x1(&self) -> f32 { self.x + self.w }
    #[inline] pub fn y1(&self) -> f32 { self.y + self.h }

    #[inline]
    pub fn contains(&self, px: f32, py: f32) -> bool {
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

#[derive(Clone, Debug)]
pub struct TextLayout {
    pub buffer_id:   BufferId,
    pub rect:        Rect,
    pub first_visible_line: usize,
    pub visible_line_count: usize,
    pub font_size:   f32,
    pub line_h:      f32,
    pub view_scroll: f32,

    // One Color per visible *char* across all visible lines,
    // in top-to-bottom, left-to-right order.
    // Index with `char_color_index`.
    pub char_colors:  Vec<Color>,

    // [x0,y0,x1,y1] per visible char - used for click hit-testing.
    pub char_rects:   Vec<[f32; 4]>,

    // Line start indices into char_colors/char_rects.
    // line_offsets[i] = index of first char of visible line i.
    pub line_offsets: Vec<usize>,
}

impl TextLayout {
    pub fn new(buffer_id: BufferId, rect: Rect, first_visible_line: usize, visible_line_count: usize,
               font_size: f32, line_h: f32, view_scroll: f32) -> Self {
        Self {
            buffer_id, rect, first_visible_line, visible_line_count, font_size, line_h, view_scroll,
            char_colors:  Vec::new(),
            char_rects:   Vec::new(),
            line_offsets: Vec::new(),
        }
    }

    /// Map (line_idx, col) relative to first_visible_line -> flat index.
    #[inline]
    pub fn flat_index(&self, vis_line: usize, col: usize) -> Option<usize> {
        let base = *self.line_offsets.get(vis_line)?;
        let end  = self.line_offsets.get(vis_line + 1)
            .copied()
            .unwrap_or(self.char_colors.len());

        let idx = base + col;
        if idx < end { Some(idx) } else { None }
    }

    /// Screen X of the left edge of (line, col).
    /// Returns None if outside the visible range.
    #[inline]
    pub fn cursor_x(&self, abs_line: usize, col: usize) -> Option<f32> {
        let vis_line = abs_line.checked_sub(self.first_visible_line)?;

        let base = *self.line_offsets.get(vis_line)?;
        let end  = self.line_offsets.get(vis_line + 1)
            .copied()
            .unwrap_or(self.char_rects.len());

        let line_char_count = end - base;

        if line_char_count == 0 {
            // Empty line - x is just the left edge
            return Some(self.rect.x + PADDING_LEFT);
        }

        if col >= line_char_count {
            // Past end of line - right edge of last char
            return Some(self.char_rects[end - 1][2]);
        }

        Some(self.char_rects[base + col][0])
    }
}

//
// Panel system
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

#[derive(Copy, Clone, Debug)]
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

    pub scroll:        f32,  // target scroll (set instantly on any scroll event)
    pub scroll_anim:   f32,  // animated scroll (what actually gets rendered)

    pub cursor_anim_x: f32,  // animated cursor screen position
    pub cursor_anim_y: f32,  // animated cursor screen position

    pub cursor_target_line: usize,
    pub cursor_target_col:  usize,

    pub cursor:      Cursor,
    pub layout:      Option<TextLayout>
}

impl View {
    pub fn new_with_scroll(id: ViewId, buffer_id: BufferId, scroll: f32) -> Self {
        Self {
            id, buffer_id, scroll, cursor: Cursor::new(), layout: None,
            cursor_anim_x: 0.0, cursor_anim_y: 0.0, scroll_anim: 0.0,
            cursor_target_line: 0,
            cursor_target_col: 0
        }
    }

    pub fn new(id: ViewId, buffer_id: BufferId) -> Self {
        Self::new_with_scroll(id, buffer_id, 0.0)
    }

    #[inline]
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

    #[inline]
    pub fn clamp_scroll(&mut self, total_lines: usize, line_h: f32, rect: Rect) {
        let max = (total_lines as f32 * line_h - rect.h).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max);
    }

    #[inline]
    pub fn first_visible_line(&self, line_h: f32) -> usize {
        (self.scroll_anim / line_h) as usize
    }

    #[inline]
    pub fn visible_line_count(&self, rect: Rect, line_h: f32) -> usize {
        (rect.h / line_h) as usize + 2
    }

    #[inline]
    pub fn line_to_screen_y(&self, line: usize, rect: Rect, line_h: f32) -> f32 {
        rect.y + line as f32 * line_h - self.scroll_anim
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

    scratch_byte_color: Vec<Color>,
    scratch_line:       String,

    pub frame_count:    u32,
    pub last_fps_time:  Instant,
    pub last_frame_time: Instant,
    pub fps:            f32,
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
            rect: Rect::default(),  // Set on first resize / resumed
            kind: PanelKind::Leaf { view_id },
        }];

        Self {
            buffers,
            views,
            panels,
            scratch_byte_color: Vec::new(),
            scratch_line: String::new(),
            active_panel: panel_id,
            root_panel:   panel_id,
            scale:        1.0,
            blink_epoch:  Instant::now(),
            last_frame_time:  Instant::now(),
            mouse_pos:    (0.0, 0.0),
            mouse_left_pressed: false,
            frame_count:   0,
            last_fps_time: Instant::now(),
            fps:           0.0,
        }
    }

    pub fn view_and_buffer_mut(&mut self, view_id: ViewId) -> (&mut View, &mut Buffer) {
        let buf_id = self.views[view_id].buffer_id;
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

        // Collect split info without holding borrow
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
            _ => return, // Already split
        };

        let old_view = self.views[old_view_id].clone(); // @Memory

        //
        // New view for the right/bottom child with the same buffer AND scroll
        //
        let new_view_id = self.views.len();
        self.views.push(View {
            id: new_view_id,
            ..old_view
        });

        // Two new leaf panels
        let left_id  = self.panels.len();
        let right_id = left_id + 1;

        self.panels.push(Panel { id: left_id,  kind: PanelKind::Leaf { view_id: old_view_id }, rect: Rect::default() });
        self.panels.push(Panel { id: right_id, kind: PanelKind::Leaf { view_id: new_view_id }, rect: Rect::default() });

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

    pub fn close_active(&mut self) {
        let active_id = self.active_panel;

        let parent_and_child = self.panels.iter().find_map(|p| {
            Some(if let PanelKind::Split(s) = p.kind {
                if s.left_id == active_id {
                    (p.id, s.right_id)
                } else if s.right_id == active_id {
                    (p.id, s.left_id)

                } else {
                    return None
                }
            } else {
                return None
            })
        });

        let Some((parent, to_keep)) = parent_and_child else {
            // No parent, nothing to close
            return;
        };

        let PanelKind::Split(_split) = self.panels[parent].kind else { return };

        let to_keep_kind = self.panels[to_keep].kind;
        self.panels[parent].kind = to_keep_kind;

        self.active_panel = parent;
    }

    pub fn toggle_active_panel(&mut self) {
        let active_id = self.active_panel;

        let parent_and_child = self.panels.iter().find_map(|p| {
            Some(if let PanelKind::Split(s) = p.kind {
                if s.left_id == active_id {
                    (p.id, s.right_id)
                } else if s.right_id == active_id {
                    (p.id, s.left_id)
                } else {
                    return None
                }
            } else {
                return None
            })
        });

        let Some((parent, to_switch_to)) = parent_and_child else {
            return;
        };

        let PanelKind::Split(_split) = self.panels[parent].kind else { return };

        let (from_x, from_y) = {
            let view = self.active_view();
            (view.cursor_anim_x, view.cursor_anim_y)
        };

        self.active_panel = to_switch_to;

        let active_view = self.active_view_mut();
        active_view.cursor_anim_x = from_x;
        active_view.cursor_anim_y = from_y;
    }

    pub fn snap_cursor_to_target(&mut self, view_id: ViewId, target_line: usize, target_col: usize, panel_rect: Rect) {
        if let Some(layout) = &self.views[view_id].layout {
            if let Some(x) = layout.cursor_x(target_line, target_col) {
                let line_h = self.line_h();
                let y = self.views[view_id].line_to_screen_y(target_line, panel_rect, line_h);
                self.views[view_id].cursor_anim_x = x;
                self.views[view_id].cursor_anim_y = y;
            }
        }
    }

    // ---- scale ----

    pub fn line_h(&self)    -> f32 { scale_base_line_height(self.scale) }
    pub fn font_size(&self) -> f32 { scale_base_font_size(self.scale) }
    pub fn cursor_w(&self) -> f32 { scale_base_cursor_width(self.scale) }
    pub fn cursor_h(&self) -> f32 { scale_base_cursor_height(self.scale) }

    // ---- blink ----

    pub fn cursor_visible(&self) -> bool { cursor_visible(&self.blink_epoch) }
    pub fn reset_blink(&mut self) { self.blink_epoch = Instant::now(); }

    // ---- hit-test: which leaf panel contains a screen point? ----

    pub fn panel_at(&self, px: f32, py: f32) -> Option<PanelId> {
        self.panel_at_recursive(self.root_panel, px, py)
    }

    fn panel_at_recursive(&self, id: PanelId, px: f32, py: f32) -> Option<PanelId> {
        match self.panels[id].kind {
            PanelKind::Leaf { .. } => {
                if self.panels[id].rect.contains(px, py) { Some(id) } else { None }
            }
            PanelKind::Split(s) => {
                self.panel_at_recursive(s.left_id,  px, py)
                    .or_else(|| self.panel_at_recursive(s.right_id, px, py))
            }
        }
    }
}

fn animate_views(editor: &mut EditorState, dt: f32) -> bool {
    let epsilon       = 0.5f32; // Stop animating when close enough
    let mut animating = false;

    let line_h    = editor.line_h();
    let font_size = editor.font_size();

    for view in &mut editor.views {
        //
        // Scroll
        //
        let ds = view.scroll - view.scroll_anim;
        if ds.abs() > epsilon {
            view.scroll_anim += ds * (1.0 - (-SCROLL_ANIM_RATE * dt).exp());
            animating = true;
        } else {
            view.scroll_anim = view.scroll;
        }

        //
        // Cursor target comes from layout if available
        //
        let Some(layout) = &view.layout else { continue };

        let (cursor_line, cursor_col) = {
            (view.cursor_target_line, view.cursor_target_col)
        };

        if let Some(target_x) = layout.cursor_x(cursor_line, cursor_col) {
            let target_y = view.line_to_screen_y(cursor_line, layout.rect, line_h);

            let dx = target_x - view.cursor_anim_x;
            let dy = target_y - view.cursor_anim_y;

            if dx.abs() > epsilon {
                view.cursor_anim_x += dx * (1.0 - (-CURSOR_ANIM_RATE * dt).exp());
                animating = true;
            } else {
                view.cursor_anim_x = target_x;
            }

            if dy.abs() > epsilon {
                view.cursor_anim_y += dy * (1.0 - (-CURSOR_ANIM_RATE * dt).exp());
                animating = true;
            } else {
                view.cursor_anim_y = target_y;
            }
        }
    }

    animating
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
    scratch_byte_color: &mut Vec<Color>,
) -> TextLayout {
    let buf_id     = view.buffer_id;
    let first_line = view.first_visible_line(line_h);
    let line_count = view.visible_line_count(rect, line_h);

    let mut layout = TextLayout::new(buf_id, rect, first_line, line_count, font_size, line_h, view.scroll_anim);

    let default_color = token_color(lexer::TokenKind::Default);
    let tokens        = &buffer.tokens;

    let mut flat_idx = 0usize;

    // Cache the first token touching the visible region
    let first_visible_byte = buffer.text.line_to_byte(first_line);
    let mut tok_i = tokens.partition_point(|t| (t.start + t.len) as usize <= first_visible_byte);

    for vis_i in 0..line_count {
        let line_idx = first_line + vis_i;
        let Some(line) = buffer.text.get_line(line_idx) else { break };

        // len_bytes includes the \n, subtract it
        let line_len = line.len_bytes()
            .saturating_sub(if line.len_chars() > 0 && line.char(line.len_chars()-1) == '\n' { 1 } else { 0 });

        layout.line_offsets.push(flat_idx);

        if line_len == 0 { continue; }

        let line_byte_start = buffer.text.line_to_byte(line_idx);

        //
        // Build byte -> color map for this line
        //

        scratch_byte_color.clear();
        scratch_byte_color.resize(line_len, default_color);

        // tok_i is already positioned - don't binary search again
        // save tok_i at line start so next line continues from here
        let mut t = tok_i;
        while t < tokens.len() {
            let tok       = &tokens[t];
            let tok_start = tok.start as usize;
            let tok_end   = tok_start + tok.len as usize;
            if tok_start >= line_byte_start + line_len { break; }

            let color = token_color(tok.kind);
            let lo    = tok_start.saturating_sub(line_byte_start).min(line_len);
            let hi    = tok_end.saturating_sub(line_byte_start).min(line_len);
            for b in lo..hi { scratch_byte_color[b] = color; }

            t += 1;
        }
        // Advance tok_i to first token that starts on or after next line
        // (tokens can span lines so we only skip tokens fully behind us)
        while tok_i < tokens.len() {
            let tok_end = (tokens[tok_i].start + tokens[tok_i].len) as usize;
            if tok_end <= line_byte_start + line_len { tok_i += 1; }
            else { break; }
        }

        // Walk chars: emit color + rect
        let screen_y = view.line_to_screen_y(layout.first_visible_line + vis_i, rect, line_h);
        let mut x    = rect.x + PADDING_LEFT;

        let mut byte_off = 0usize;
        for ch in line.chars() {
            if ch == '\n' { break }

            let char_width = gpu::get_glyph(gpu, ch, font_size)
                .map(|g| g.advance)
                .unwrap_or(8.0);

            let color = scratch_byte_color.get(byte_off).copied().unwrap_or(default_color);
            layout.char_colors.push(color);

            layout.char_rects.push([x, screen_y, x + char_width, screen_y + line_h]);

            x        += char_width;
            flat_idx += 1;
            byte_off += ch.len_utf8();
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
    line_scratch: &mut String,
    scale: f32,
    show_cursor: bool,
) {
    let rect      = layout.rect;
    let line_h    = layout.line_h;
    let font_size = layout.font_size;

    let cursor_w = scale_base_cursor_width(scale);
    let cursor_h = scale_base_cursor_height(scale);

    // -- Selection --
    if let Some(anchor) = view.cursor.anchor_char_idx {
        let cursor = view.cursor.char_idx;
        let (start_idx, end_idx) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };

        if start_idx != end_idx {
            let (start_line, start_col) = buffer.char_to_line_col(start_idx);
            let (end_line,   end_col)   = buffer.char_to_line_col(end_idx);

            let draw_start = start_line.max(layout.first_visible_line);
            let draw_end   = end_line.min(layout.first_visible_line + layout.visible_line_count - 1);

            for line_idx in draw_start..=draw_end {
                let y = view.line_to_screen_y(line_idx, rect, line_h) + cursor_h;

                // x0: left edge, x1: right edge for this line's highlight
                let (x0, x1) = if start_line == end_line {
                    // single line selection
                    let x0 = layout.cursor_x(line_idx, start_col).unwrap_or(rect.x + PADDING_LEFT);
                    let x1 = layout.cursor_x(line_idx, end_col  ).unwrap_or(x0);
                    (x0, x1)
                } else if line_idx == start_line {
                    // first line: from start_col to end of line
                    let x0 = layout.cursor_x(line_idx, start_col).unwrap_or(rect.x + PADDING_LEFT);
                    let x1 = rect.x + rect.w;
                    (x0, x1)
                } else if line_idx == end_line {
                    // last line: from left edge to end_col
                    let x0 = rect.x;
                    let x1 = layout.cursor_x(line_idx, end_col).unwrap_or(rect.x + PADDING_LEFT);
                    (x0, x1)
                } else {
                    // middle lines: full width
                    (rect.x, rect.x + rect.w)
                };

                if x1 > x0 {
                    gpu::draw_rect(gpu, x0, y, x1 - x0, line_h, palette().selection);
                } else {
                    // empty line or zero-width: draw a thin placeholder so the
                    // selection gap is visible
                    gpu::draw_rect(gpu, rect.x, y, 8.0, line_h, palette().selection);
                }
            }
        }
    }

    // -- Cursor line --
    let (cursor_line, cursor_col) = buffer.cursor_line_col(&view.cursor);

    let cursor_x = view.cursor_anim_x;
    let cursor_y = view.cursor_anim_y;

    let cursor_target_y = view.line_to_screen_y(cursor_line, rect, line_h);

    let has_selection = view.cursor.anchor_char_idx
        .map(|a| a != view.cursor.char_idx)
        .unwrap_or(false);

    if has_selection {
        let (start_idx, end_idx) = {
            let a = view.cursor.anchor_char_idx.unwrap();
            let c = view.cursor.char_idx;
            if a <= c { (a, c) } else { (c, a) }
        };
        let (start_line, start_col) = buffer.char_to_line_col(start_idx);
        let (end_line,   end_col)   = buffer.char_to_line_col(end_idx);

        if start_line == cursor_line && end_line == cursor_line {
            // selection on same line: draw left strip and right strip
            let sel_x0 = layout.cursor_x(cursor_line, start_col).unwrap_or(rect.x + PADDING_LEFT);
            let sel_x1 = layout.cursor_x(cursor_line, end_col  ).unwrap_or(sel_x0);
            // left of selection
            if sel_x0 > rect.x {
                gpu::draw_rect(gpu, rect.x, cursor_target_y + cursor_h, sel_x0 - rect.x, line_h, palette().current_line);
            }
            // right of selection
            if sel_x1 < rect.x + rect.w {
                gpu::draw_rect(gpu, sel_x1, cursor_target_y + cursor_h, (rect.x + rect.w) - sel_x1, line_h, palette().current_line);
            }
        } else if start_line < cursor_line && end_line > cursor_line {
            // cursor line fully inside selection, no bg
        } else if end_line == cursor_line {
            // selection ends here: right strip only
            let sel_x1 = layout.cursor_x(cursor_line, end_col).unwrap_or(rect.x);
            gpu::draw_rect(gpu, sel_x1, cursor_target_y + cursor_h, (rect.x + rect.w) - sel_x1, line_h, palette().current_line);
        } else if start_line == cursor_line {
            // selection starts on cursor line and extends down
            // left of selection start gets cursor line bg
            let sel_x0 = layout.cursor_x(cursor_line, start_col).unwrap_or(rect.x + PADDING_LEFT);
            if sel_x0 > rect.x {
                gpu::draw_rect(gpu, rect.x, cursor_target_y + cursor_h, sel_x0 - rect.x, line_h, palette().current_line);
            }
        } else {
            // selection entirely above or below: full line bg
            gpu::draw_rect(gpu, rect.x, cursor_target_y + cursor_h, rect.w, line_h, palette().current_line);
        }
    } else {
        gpu::draw_rect(gpu, rect.x, cursor_target_y + cursor_h, rect.w, line_h, palette().current_line);
    }

    // -- Cursor --
    if show_cursor {
        if cursor_line >= layout.first_visible_line
        && cursor_line <  layout.first_visible_line + layout.visible_line_count
        {
            gpu::draw_rect(gpu, cursor_x, cursor_y+cursor_h, cursor_w, line_h+cursor_h, palette().cursor);
        }
    }

    // -- Text --
    let default_color = token_color(lexer::TokenKind::Default);

    for (vis_i, &line_base) in layout.line_offsets.iter().enumerate() {
        let line_idx = layout.first_visible_line + vis_i;

        line_scratch.clear();
        for ch in buffer.text.line(line_idx).chars() {
            if ch == '\n' { break; }
            line_scratch.push(ch);
        }

        let line_str = line_scratch.trim_end_matches('\n');
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
    line_scratch: &mut String,
) -> (usize, usize) {
    let line_idx = (((my - rect.y) + view.scroll) / line_h) as usize;
    let line_idx = line_idx.min(buffer.text.len_lines().saturating_sub(1));

    line_scratch.clear();
    for ch in buffer.text.line(line_idx).chars() {
        if ch == '\n' { break; }
        line_scratch.push(ch);
    }
    let line_str = line_scratch.trim_end_matches('\n');

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

fn screen_pos_to_line_col_fast(layout: &TextLayout, view_scroll: f32, mx: f32, my: f32) -> (usize, usize) {
    let n        = layout.line_offsets.len();
    if n == 0 { return (layout.first_visible_line, 0); }

    let scroll_offset = layout.view_scroll % layout.line_h;
    let vis_line = n.min(((my - layout.rect.y + scroll_offset) / layout.line_h) as usize);
    let vis_line = vis_line.min(n - 1);
    let abs_line = layout.first_visible_line + vis_line;

    let base = layout.line_offsets[vis_line];
    let end  = layout.line_offsets.get(vis_line + 1)
        .copied()
        .unwrap_or(layout.char_rects.len());

    if base == end {
        return (abs_line, 0);
    }

    // Scan chars on this line, snap to nearest boundary
    let mut col = end - base; // default: end of line
    for i in base..end {
        let r = layout.char_rects[i];
        let mid = (r[0] + r[2]) * 0.5;
        if mx <= mid {
            col = i - base;
            break;
        }
    }

    (abs_line, col)
}

fn collect_leaves(panels: &[Panel], id: PanelId, out: &mut Vec<(PanelId, ViewId, Rect)>) {
    match panels[id].kind {
        PanelKind::Leaf { view_id } => out.push((id, view_id, panels[id].rect)),
        PanelKind::Split(s) => {
            collect_leaves(panels, s.left_id,  out);
            collect_leaves(panels, s.right_id, out);
        }
    }
}

fn apply_scale(editor: &mut EditorState, gpu: &Gpu, new_scale: f32, anchor_my: Option<f32>) {
    let old_line_h = editor.line_h();
    editor.scale = new_scale.clamp(MIN_SCALE, MAX_SCALE);
    let new_line_h = editor.line_h();

    // For each view, preserve the line that was at the anchor screen Y.
    // If no anchor (keyboard zoom), preserve the line at the top of the view.
    for view in &mut editor.views {
        let anchor_y = anchor_my.unwrap_or(0.0);
        // Which line was at anchor_y before the scale change?
        let line_at_anchor = (view.scroll + anchor_y) / old_line_h;
        // Recompute scroll so that same line stays at anchor_y
        view.scroll = (line_at_anchor * new_line_h - anchor_y).max(0.0);
        view.scroll_anim = view.scroll;
        view.layout = None;
    }
}

fn scroll_page(editor: &mut EditorState, gpu: &Gpu, direction: i32) {
    let line_h   = editor.line_h();
    let view_id  = editor.active_view_id();
    let panel_id = editor.active_panel;
    let rect     = editor.panels[panel_id].rect;
    let buf_id   = editor.views[view_id].buffer_id;

    // How many lines fit on screen (minus a little overlap like emacs)
    let page_lines = ((rect.h / line_h) as isize - 2).max(1);
    let delta      = direction as isize * page_lines;

    // Move scroll first
    let total     = editor.buffers[buf_id].text.len_lines();
    let old_scroll = editor.views[view_id].scroll;
    let new_scroll = (old_scroll + delta as f32 * line_h).max(0.0);
    let max_scroll = ((total as f32 * line_h) - rect.h).max(0.0);
    editor.views[view_id].scroll = new_scroll.min(max_scroll);

    // Drag cursor by the same number of lines
    let (cur_line, cur_col) = editor.buffers[buf_id].cursor_line_col(&editor.views[view_id].cursor);
    let new_line = ((cur_line as isize + delta).max(0) as usize).min(total.saturating_sub(1));

    let mut cursor = &mut editor.views[view_id].cursor;
    editor.buffers[buf_id].set_cursor_line_col(new_line, cur_col, cursor);
    editor.views[view_id].cursor_target_line = new_line;
    editor.views[view_id].cursor_target_col  = cur_col;
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

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let Some(editor) = &self.editor else { return };
        let Some(win)    = &self.window else { return };

        // Schedule next wakeup for cursor blink
        let elapsed = editor.blink_epoch.elapsed().as_millis();
        let cycle   = BLINK_ON_MS + BLINK_OFF_MS;
        let phase   = elapsed % cycle;
        let ms_until = if phase < BLINK_ON_MS {
            BLINK_ON_MS - phase
        } else {
            cycle - phase
        };

        el.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + std::time::Duration::from_millis(ms_until as u64)
        ));
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let WindowEvent::ModifiersChanged(m) = &event {
            self.mods = *m;
            return;
        }

        let (Some(gpu), Some(editor), Some(win)) =
            (&mut self.gpu, &mut self.editor, &self.window) else { return };

        let ctrl  = self.mods.state().control_key();
        let shift = self.mods.state().shift_key();
        let alt   = self.mods.state().alt_key();

        match event {
            WindowEvent::CloseRequested => el.exit(),

            WindowEvent::ModifiersChanged(m) => self.mods = m,

            // ---- Keyboard ----
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed { return; }

                // Scale
                if ctrl {
                    match &event.logical_key {
                        Key::Character(s) => match s.as_str() {
                            "=" | "+" => {
                                let new = (editor.scale + SCALE_STEP).clamp(MIN_SCALE, MAX_SCALE);
                                if new != editor.scale {
                                    reset_atlas(gpu);
                                    apply_scale(editor, gpu, new, None);
                                    win.request_redraw();
                                }
                                return;
                            }
                            "-" => {
                                let new = (editor.scale - SCALE_STEP).clamp(MIN_SCALE, MAX_SCALE);
                                if new != editor.scale {
                                    reset_atlas(gpu);
                                    apply_scale(editor, gpu, new, None);
                                    win.request_redraw();
                                }
                                return;
                            }
                            "0" => {
                                reset_atlas(gpu);
                                apply_scale(editor, gpu, 1.0, None);
                                win.request_redraw();
                                return;
                            }
                            _ => {}
                        }
                        _ => {}
                    }
                }

                // Split: Ctrl-2 = vertical split, Ctrl-1
                if ctrl {
                    if let PhysicalKey::Code(KeyCode::Digit2) = event.physical_key {
                        let win_rect = Rect::full(gpu.win_w, gpu.win_h);
                        editor.split_active(true, 0.5);
                        editor.layout_panels(win_rect);
                        win.request_redraw();
                        return;
                    }

                    if let PhysicalKey::Code(KeyCode::Digit1) = event.physical_key {
                        let win_rect = Rect::full(gpu.win_w, gpu.win_h);
                        editor.close_active();
                        editor.layout_panels(win_rect);
                        // Invalidate layouts
                        for view in &mut editor.views {
                            view.layout = None;
                        }
                        win.request_redraw();
                        return;
                    }
                }

                if alt {
                    if let PhysicalKey::Code(KeyCode::Digit2) = event.physical_key {
                        editor.toggle_active_panel();
                        return;
                    }
                }

                let view_id  = editor.active_view_id();
                let (view, buf) = editor.active_view_and_buffer_mut();

                let cmd = match &event.logical_key {
                    Key::Named(NamedKey::ArrowLeft)  => Some(EditorCommand::MoveLeft),
                    Key::Named(NamedKey::ArrowRight) => Some(EditorCommand::MoveRight),
                    Key::Named(NamedKey::ArrowUp)    => Some(EditorCommand::MoveUp),
                    Key::Named(NamedKey::ArrowDown)  => Some(EditorCommand::MoveDown),

                    Key::Named(NamedKey::Home) => Some(if ctrl { EditorCommand::MoveFileStart } else { EditorCommand::MoveLineStart }),
                    Key::Named(NamedKey::End)  => Some(if ctrl { EditorCommand::MoveFileEnd   } else { EditorCommand::MoveLineEnd   }),

                    Key::Named(NamedKey::Backspace) => { view.cursor.unset_anchor(); Some(EditorCommand::DeleteBackward) }
                    Key::Named(NamedKey::Delete)    => { view.cursor.unset_anchor(); Some(EditorCommand::DeleteForward)  }
                    Key::Named(NamedKey::Enter)     => { view.cursor.unset_anchor(); Some(EditorCommand::InsertNewline)  }
                    Key::Named(NamedKey::Tab)       => { view.cursor.unset_anchor(); Some(EditorCommand::InsertLiteral("    ")) }

                    Key::Character(s) if ctrl => match s.as_str() {
                        "a" => Some(EditorCommand::MoveLineStart),
                        "e" => Some(EditorCommand::MoveLineEnd),
                        "f" => Some(EditorCommand::MoveRight),
                        "b" => Some(EditorCommand::MoveLeft),
                        "n" => Some(EditorCommand::MoveDown),
                        "p" => Some(EditorCommand::MoveUp),
                        "o" => { view.cursor.unset_anchor(); Some(EditorCommand::InsertNewlineAfter) }
                        "d" => { view.cursor.unset_anchor(); Some(EditorCommand::DeleteForward) }
                        "k" => { view.cursor.unset_anchor(); Some(EditorCommand::DeleteForwardUntilNewline) },
                        "g" => { view.cursor.unset_anchor(); None }
                        "v" => { scroll_page(editor, gpu, 1);  editor.reset_blink(); win.request_redraw(); return; }
                        _   => None,
                    }

                    Key::Character(s) if alt && shift => match s.as_str() {
                        "<" => Some(EditorCommand::MoveFileStart),
                        ">" => Some(EditorCommand::MoveFileEnd),
                        _ => None
                    }

                    Key::Named(NamedKey::Space) => if ctrl {
                        view.cursor.set_anchor();
                        None
                    } else {
                        view.cursor.unset_anchor();
                        Some(EditorCommand::InsertChar(' '))
                    }

                    Key::Character(s) if alt => match s.as_str() {
                        "v" => { scroll_page(editor, gpu, -1); editor.reset_blink(); win.request_redraw(); return; }
                        _ => None
                    }

                    //
                    // Basic insert character
                    //
                    Key::Character(s) if !ctrl => {
                        view.cursor.unset_anchor();
                        s.chars().next().map(EditorCommand::InsertChar)
                    }

                    _ => None,
                };

                if let Some(cmd) = cmd {
                    let is_cmd_insert     = cmd.is_insert();
                    let is_cmd_big_scroll = cmd.is_big_scroll();

                    buf.apply(cmd, &mut view.cursor);

                    let (line, col) = buf.cursor_line_col(&view.cursor);

                    let line_h   = editor.line_h();
                    let panel_id = editor.active_panel;
                    let rect     = editor.panels[panel_id].rect;

                    editor.views[view_id].scroll_to_cursor(line, line_h, rect);
                    editor.views[view_id].cursor_target_col  = col;
                    editor.views[view_id].cursor_target_line = line;

                    if is_cmd_insert || is_cmd_big_scroll {
                        editor.snap_cursor_to_target(view_id, line, col, rect);
                    }

                    editor.reset_blink();

                    win.request_redraw();
                }
            }

            // ---- Mouse wheel ----
            WindowEvent::MouseWheel { delta, .. } => {
                if ctrl {
                    let dy = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y,
                        MouseScrollDelta::PixelDelta(p)   => p.y as f32 * 0.01,
                    };
                    let new = (editor.scale + dy * 0.055).clamp(MIN_SCALE, MAX_SCALE);
                    if new != editor.scale {
                        reset_atlas(gpu);
                        let (_, my) = editor.mouse_pos;
                        apply_scale(editor, gpu, new, Some(my));
                        win.request_redraw();
                    }
                    return;
                }

                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * editor.line_h(),
                    MouseScrollDelta::PixelDelta(p)   => p.y as f32,
                };

                let (mx, my) = editor.mouse_pos;
                let Some(panel_id) = editor.panel_at(mx, my) else { return };
                let PanelKind::Leaf { view_id } = editor.panels[panel_id].kind else { return };

                let rect    = editor.panels[panel_id].rect;
                let buf_id  = editor.views[view_id].buffer_id;
                let total   = editor.buffers[buf_id].text.len_lines();
                let line_h  = editor.line_h();

                let old_scroll = editor.views[view_id].scroll;
                let new_scroll = (old_scroll - dy * 4.55).max(0.0);
                let max_scroll = ((total as f32 * line_h) - rect.h).max(0.0);
                editor.views[view_id].scroll = new_scroll.min(max_scroll);

                // Drag cursor if it went off screen
                let (cur_line, cur_col) = editor.buffers[buf_id]
                    .cursor_line_col(&editor.views[view_id].cursor);

                let scroll     = editor.views[view_id].scroll;
                let first_vis  = (scroll / line_h) as usize;
                let last_vis = (((scroll + rect.h) / line_h) as usize)
                    .saturating_sub(1)
                    .min(total.saturating_sub(1));

                let new_line = if cur_line < first_vis {
                    first_vis
                } else if cur_line > last_vis {
                    last_vis.min(total.saturating_sub(1))
                } else {
                    cur_line // still visible, don't move
                };

                if new_line != cur_line {
                    let mut cursor = editor.views[view_id].cursor.clone();
                    editor.buffers[buf_id].set_cursor_line_col(new_line, cur_col, &mut cursor);

                    editor.views[view_id].cursor_target_line = new_line;
                    editor.views[view_id].cursor_target_col  = cur_col;
                    editor.views[view_id].cursor             = cursor;

                    editor.snap_cursor_to_target(view_id, new_line, cur_col, rect);
                }

                win.request_redraw();
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

                    let (line, col) = if let Some(layout) = &editor.views[view_id].layout {
                        screen_pos_to_line_col_fast(layout, editor.views[view_id].scroll, mx, my)
                    } else {
                        screen_pos_to_line_col(
                            gpu, &editor.buffers[buf_id], &editor.views[view_id],
                            rect, mx, my, font_size, line_h, &mut editor.scratch_line
                        )
                    };

                    let view = &mut editor.views[view_id];
                    editor.buffers[buf_id].set_cursor_line_col(line, col, &mut view.cursor);
                    view.cursor_target_line = line;
                    view.cursor_target_col  = col;

                    view.cursor.unset_anchor();
                    editor.reset_blink();

                    win.request_redraw();
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

                        let (line, col) = if let Some(layout) = &editor.views[view_id].layout {
                            screen_pos_to_line_col_fast(layout, editor.views[view_id].scroll, mx, my)
                        } else {
                            screen_pos_to_line_col(
                                gpu, &editor.buffers[buf_id], &editor.views[view_id],
                                rect, mx, my, font_size, line_h, &mut editor.scratch_line
                            )
                        };

                        let view = &mut editor.views[view_id];
                        editor.buffers[buf_id].set_cursor_line_col(line, col, &mut view.cursor);
                        view.cursor_target_line = line;
                        view.cursor_target_col  = col;

                        if !view.cursor.is_anchor_set() {
                            view.cursor.set_anchor();
                        }

                        editor.reset_blink();

                        win.request_redraw();
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

                    win.request_redraw();
                }
            }

            // ---- Redraw ----
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let dt = now.duration_since(editor.last_frame_time).as_secs_f32().min(0.05);
                editor.last_frame_time = now;
                editor.frame_count += 1;

                let elapsed = editor.last_fps_time.elapsed().as_secs_f32();
                if elapsed >= 0.5 {
                    editor.fps = editor.frame_count as f32 / elapsed;
                    editor.frame_count   = 0;
                    editor.last_fps_time = Instant::now();
                }

                //
                // Schedule next wakeup for blink
                //
                {
                    let elapsed = editor.blink_epoch.elapsed().as_millis();
                    let cycle   = BLINK_ON_MS + BLINK_OFF_MS;
                    let phase   = elapsed % cycle;
                    let ms_until_transition = if phase < BLINK_ON_MS {
                        BLINK_ON_MS - phase
                    } else {
                        cycle - phase
                    };

                    el.set_control_flow(ControlFlow::WaitUntil(
                        Instant::now() + Duration::from_millis(ms_until_transition as u64)
                    ));
                }

                let still_animating = animate_views(editor, dt);

                let font_size    = editor.font_size();
                let line_h       = editor.line_h();
                let show_cursor  = editor.cursor_visible();
                let active_panel = editor.active_panel;

                let mut leaf_panels = Vec::new();
                collect_leaves(&editor.panels, editor.root_panel, &mut leaf_panels);

                for (panel_id, view_id, rect) in leaf_panels {
                    let buf_id  = editor.views[view_id].buffer_id;
                    let dirty = editor.buffers[buf_id].dirty
                        || editor.views[view_id].layout.is_none()
                        || editor.views[view_id].layout.as_ref().map(|l| {
                            l.first_visible_line != editor.views[view_id].first_visible_line(line_h)
                                || l.view_scroll != editor.views[view_id].scroll_anim
                                || l.rect.w != rect.w
                                || l.rect.h != rect.h
                        }).unwrap_or(true);

                    if dirty {
                        let layout = build_text_layout(
                            gpu,
                            &editor.buffers[buf_id],
                            &editor.views[view_id],
                            rect, font_size, line_h,
                            &mut editor.scratch_byte_color
                        );

                        // Initialize anim position if this is the first layout
                        if editor.views[view_id].layout.is_none() {
                            let (cl, cc) = (editor.views[view_id].cursor_target_line,
                                            editor.views[view_id].cursor_target_col);

                            editor.snap_cursor_to_target(view_id, cl, cc, rect);
                        }

                        editor.views[view_id].layout = Some(layout);
                    }

                    let show_cursor = if panel_id == active_panel {
                        //
                        // Only make cursor blink on the active panel.
                        //
                        show_cursor
                    } else {
                        true
                    };

                    let layout = editor.views[view_id].layout.as_ref().unwrap();
                    render_text_layout(
                        gpu, layout,
                        &editor.buffers[buf_id],
                        &editor.views[view_id],
                        &mut editor.scratch_line,
                        editor.scale,
                        show_cursor
                    );
                }

                // Clear dirty after all views have rebuilt their layouts
                for buf in &mut editor.buffers {
                    buf.dirty = false;
                }

                let fps_str = format!("{:.0} fps", editor.fps);
                gpu::draw_text(gpu, &fps_str, gpu.win_w - 64.0, 14.0, 12.0, Color::rgba(120, 120, 120, 255));

                gpu::submit_frame(gpu).unwrap();
                win.request_redraw();
            }

            _ => {}
        }
    }
}

fn main() {
    let el = EventLoop::new().unwrap();
    el.set_control_flow(ControlFlow::Wait);
    el.run_app(&mut App::default()).unwrap();
}
