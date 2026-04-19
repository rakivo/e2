#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod gpu;
mod util;
mod color;
mod buffer;
mod command;
mod tracy;
mod lexer;

use std::sync::Arc;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use buffer::{Buffer, Cursor};
use color::Color;
use command::{CommandContext, CommandTable, Keymap, Mods};
use cranelift_entity::{EntityRef, PrimaryMap};
use gpu::{Gpu, GpuGlyph, reset_atlas};
use lexer::token_color;

use smallstr::SmallString;
use smallvec::SmallVec;
use winit::window::{Window, WindowId};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};

pub struct Palette {
    pub bg:           Color,
    pub selection:    Color,
    pub current_line: Color,
    pub cursor:       Color,
    pub cursor_text:  Color,
    pub paren_match:  Color
}

#[inline]
pub const fn palette() -> Palette {
    Palette {
        bg:           Color::hex(0x0f0b05),
        selection:    Color::hex(0x112c4f),
        cursor:       Color::hex(0xc3a983),
        current_line: Color::hex(0x231b0e),
        cursor_text:  Color::rgba(13, 13, 13, 255),
        paren_match:  Color::rgba(190, 128, 133, 200)
    }
}

const MIN_SCALE:  f32 = 0.75;
const MAX_SCALE:  f32 = 5.00;
const SCALE_STEP: f32 = 0.25;

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
    const BASE_CURSOR_OUTLINE_THICKNESS: f32 = 1.5;
}

const BLINK_ON_MS:  u128 = 530;
const BLINK_OFF_MS: u128 = 370;

fn cursor_visible(epoch: &Instant) -> bool {
    let elapsed = epoch.elapsed().as_millis() % (BLINK_ON_MS + BLINK_OFF_MS);
    elapsed < BLINK_ON_MS
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Rect {
    pub x: f32, pub y: f32,
    pub w: f32, pub h: f32,
}

impl Rect {
    #[inline]
    pub fn full(win_w: f32, win_h: f32) -> Self {
        Self { x: 0.0, y: 0.0, w: win_w, h: win_h }
    }

    #[inline] pub fn x1(&self) -> f32 { self.x + self.w }
    #[inline] pub fn y1(&self) -> f32 { self.y + self.h }

    #[inline]
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x1() && py >= self.y && py < self.y1()
    }

    /// Left gets `ratio` of the width, right gets the rest.
    #[inline]
    pub fn split_horizontally(self, ratio: f32) -> (Rect, Rect) {
        let mid = (self.x + self.w * ratio).round();
        (
            Rect { x: self.x, y: self.y, w: mid - self.x,        h: self.h },
            Rect { x: mid,    y: self.y, w: self.x1() - mid,     h: self.h },
        )
    }

    /// Top gets `ratio` of the height, bottom gets the rest.
    #[inline]
    pub fn split_vertically(self, ratio: f32) -> (Rect, Rect) {
        let mid = (self.y + self.h * ratio).round();
        (
            Rect { x: self.x, y: self.y,  w: self.w, h: mid - self.y       },
            Rect { x: self.x, y: mid,     w: self.w, h: self.y1() - mid    },
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Glyph {
    /// X offset from the line's left content edge (rect.x + PADDING_LEFT)
    pub x:         f32,

    pub color:     Color,
    pub char:      char,

    /// Byte offset within the buffer line's bytes
    pub line_byte: u32,

    pub gpu_glyph: GpuGlyph
}

impl Glyph {
    #[inline]
    fn advance(&self) -> f32 {
        self.gpu_glyph.advance
    }
}

#[derive(Clone, Debug)]
pub struct LineLayout {
    pub buffer_line:     u32,
    pub wrap_index:      u8,       // Always 0 for now (@Incomplete) :WordWrapping

    pub glyph_start:     u32,      // index into TextLayout::glyphs
    pub glyph_count:     u32,

    pub width:           f32,      // Total advance (all glyphs)
    pub line_byte_start: usize,    // Rope byte offset where this line begins
}

impl LineLayout {
    #[inline]
    pub fn glyphs<'a>(&self, all: &'a [Glyph]) -> &'a [Glyph] {
        &all[self.glyph_start as usize..self.glyph_start as usize + self.glyph_count as usize]
    }

    /// Screen X of the left edge of column `col`.
    /// col == glyphs.len() -> right edge of the last glyph (EOL cursor)
    #[inline]
    pub fn x_for_col(&self, origin_x: f32, col: u32, all_glyphs: &[Glyph]) -> f32 {
        let col = col as usize;

        let glyphs = self.glyphs(all_glyphs);

        if glyphs.is_empty() {
            return origin_x;
        }

        if col >= glyphs.len() {
            let g = &glyphs[glyphs.len() - 1];
            return origin_x + g.x + g.advance();
        }

        origin_x + glyphs[col].x
    }

    /// Width of the glyph at `col`; `fallback` when at/past EOL
    #[inline]
    pub fn glyph_width_at_col(&self, col: u32, fallback: f32, all_glyphs: &[Glyph]) -> f32 {
        let col = col as usize;

        let glyphs = self.glyphs(all_glyphs);

        if glyphs.is_empty() {
            return fallback; // Caller should pass space width, not cursor_w
        }

        if col >= glyphs.len() {
            // At or past EOL - use last glyph's width to mirror Emacs
            return glyphs.last().map(|g| g.advance()).unwrap_or(fallback);
        }

        glyphs[col].advance().max(fallback)
    }

    /// Hit-test a screen X coordinate to a column index (mid-point snap).
    #[inline]
    pub fn col_for_screen_x(&self, origin_x: f32, screen_x: f32, all_glyphs: &[Glyph]) -> u32 {
        let glyphs = self.glyphs(all_glyphs);

        let local = screen_x - origin_x;
        let mut col = glyphs.len();
        for (i, g) in glyphs.iter().enumerate() {
            if local <= g.x + g.advance() * 0.5 {
                col = i;
                break;
            }
        }

        col as _
    }
}

#[derive(Clone, Debug)]
pub struct TextLayout {
    pub buffer_id:         BufferId,

    pub view_scroll:       f32, // view.scroll
    pub line_h:            f32,
    pub font_size:         f32,
    pub first_buffer_line: u32,

    pub rect:              Rect,

    // @Memory @Speed: Reuse these allocations.
    // Because currently they're being reallocated each frame.
    pub lines:             Vec<LineLayout>,
    pub glyphs:            Vec<Glyph>
}

impl TextLayout {
    #[inline]
    pub fn first_visible_buffer_line(&self) -> u32 {
        self.first_buffer_line
    }

    /// Find the LineLayout for a buffer line if visible.
    #[inline]
    pub fn line_for_buffer_line(&self, buffer_line: u32) -> Option<&LineLayout> {
        let offset = buffer_line.checked_sub(self.first_buffer_line)?;

        let ll = self.lines.get(offset as usize)?;
        if ll.buffer_line == buffer_line && ll.wrap_index == 0 {
            // :WordWrapping
            return Some(ll);
        }

        // Slow path for future word-wrap
        self.lines.iter().find(|ll| ll.buffer_line == buffer_line && ll.wrap_index == 0)
    }

    /// Screen X of the cursor at (buffer_line, col).  Kept for animate_views compat.
    #[inline]
    pub fn cursor_x(&self, buffer_line: u32, col: u32) -> Option<f32> {
        let ll = self.line_for_buffer_line(buffer_line)?;
        Some(ll.x_for_col(self.rect.x + PADDING_LEFT, col, &self.glyphs))
    }

    /// Full glyph screen rect at (buffer_line, col): [x0, y0, x1, y1].
    #[inline]
    pub fn glyph_rect(&self, buffer_line: u32, col: u32, fallback_w: f32, scroll_anim: f32) -> Option<[f32; 4]> {
        let ll = self.line_for_buffer_line(buffer_line)?;
        let y = self.rect.y
            + (ll.buffer_line - self.first_buffer_line) as f32 * self.line_h
            - (scroll_anim % self.line_h);

        let x0 = ll.x_for_col(self.rect.x + PADDING_LEFT, col, &self.glyphs);
        let x1 = x0 + ll.glyph_width_at_col(col, fallback_w, &self.glyphs);
        Some([x0, y, x1, y + self.line_h])
    }

    /// Hit-test (mx, my) -> (buffer_line, col).
    #[inline]
    pub fn hit_test(&self, mx: f32, my: f32, scroll_anim: f32) -> (u32, u32) {
        if self.lines.is_empty() {
            return (self.first_buffer_line, 0);
        }

        let line_f   = (my - self.rect.y + scroll_anim) / self.line_h;
        let line_idx = (line_f as u32).clamp(
            self.first_buffer_line,
            self.first_buffer_line + self.lines.len() as u32 - 1,
        );
        let vis_idx  = (line_idx - self.first_buffer_line) as usize;
        let ll       = &self.lines[vis_idx];
        let col      = ll.col_for_screen_x(self.rect.x + PADDING_LEFT, mx, &self.glyphs);
        (ll.buffer_line, col)
    }

    /// Screen X of the left edge of column `col`.
    /// col == glyphs.len()  ->  right edge of the last glyph (EOL cursor).
    #[inline]
    pub fn x_for_col(&self, origin_x: f32, col: u32, line_layout: &LineLayout) -> f32 {
        line_layout.x_for_col(origin_x, col, &self.glyphs)
    }

    /// Width of the glyph at `col`; `fallback` when at/past EOL.
    #[inline]
    pub fn glyph_width_at_col(&self, col: u32, fallback: f32, line_layout: &LineLayout) -> f32 {
        line_layout.glyph_width_at_col(col, fallback, &self.glyphs)
    }

    /// Hit-test a screen X coordinate to a column index (mid-point snap).
    #[inline]
    pub fn col_for_screen_x(&self, origin_x: f32, screen_x: f32, line_layout: &LineLayout) -> u32 {
        line_layout.col_for_screen_x(origin_x, screen_x, &self.glyphs)
    }
}

/// Build the TextLayout for a single leaf panel

fn build_text_layout(
    gpu:       &mut Gpu,
    buffer:    &Buffer,
    view:      &View,
    rect:      Rect,
    font_size: f32,
    line_h:    f32,
) -> TextLayout {
    let _tracy = tracy::span!("build_text_layout");

    let first_line = (view.scroll_anim / line_h) as u32;
    let line_count = (rect.h / line_h) as u32 + 4;

    let default_color = token_color(lexer::TokenKind::Default);
    let tokens        = &buffer.visible_tokens;

    let first_visible_byte = buffer.text.line_to_byte(first_line as usize);

    let mut current_token = tokens.partition_point(|t| (t.start + t.len()) as usize <= first_visible_byte);

    // @Memory
    let mut lines  = Vec::with_capacity(line_count as usize);
    let mut glyphs = Vec::with_capacity(80);

    for vis_i in 0..line_count {
        let line_index = first_line + vis_i;
        let Some(rope_line) = buffer.text.get_line(line_index as usize) else { break };

        let has_nl   = rope_line.len_chars() > 0 && rope_line.char(rope_line.len_chars() - 1) == '\n';
        let line_len = rope_line.len_bytes().saturating_sub(if has_nl { 1 } else { 0 });

        let line_byte_start = buffer.text.line_to_byte(line_index as usize);

        let mut ll = LineLayout {
            buffer_line:     line_index,
            wrap_index:      0,
            width:           0.0,
            glyph_count:     0,
            glyph_start:     0,
            line_byte_start,
        };

        if line_len > 0 {
            let mut local_x  = 0.0f32;
            let mut byte_off = 0usize;

            let glyph_start = glyphs.len() as u32;

            for char in rope_line.chars() {
                if char == '\n' { break; }

                // Advance token cursor past tokens that end before this byte
                while current_token < tokens.len() {
                    let t = &tokens[current_token];
                    if (t.start + t.len()) as usize <= line_byte_start + byte_off {
                        current_token += 1;
                    } else {
                        break;
                    }
                }

                // Color is from current token if it covers this byte, else default
                let abs_byte = line_byte_start + byte_off;
                let color = if current_token < tokens.len() {
                    let t = &tokens[current_token];
                    if abs_byte >= t.start as usize && abs_byte < (t.start + t.len()) as usize {
                        token_color(t.kind())
                    } else {
                        default_color
                    }
                } else {
                    default_color
                };

                let gpu_glyph = gpu::get_glyph(gpu, char, font_size)
                    .unwrap_or_else(|| gpu::get_glyph(gpu, 'A', font_size).unwrap());

                let advance = gpu_glyph.advance;

                glyphs.push(Glyph {
                    x:         local_x,
                    color,
                    char,
                    line_byte: byte_off as u32,
                    gpu_glyph,
                });

                local_x  += advance;
                byte_off += char.len_utf8();
            }

            ll.glyph_start = glyph_start;
            ll.glyph_count = glyphs.len() as u32 - glyph_start;
            ll.width = local_x;
        }

        lines.push(ll);
    }

    TextLayout {
        buffer_id:         view.buffer_id,
        rect,
        view_scroll:       view.scroll,
        line_h,
        font_size,
        first_buffer_line: first_line,
        glyphs,
        lines,
    }
}

fn render_text_layout(
    gpu:         &mut Gpu,
    layout:      &TextLayout,
    buffer:      &Buffer,
    view:        &View,
    active_view_id: ViewId,
    scale:       f32,
    show_cursor: bool,
    scratch_paren: &mut Vec<char>,
) {
    let _tracy = tracy::span!("render_text_layout");

    let line_y = |buffer_line: u32| -> f32 {
        layout.rect.y + buffer_line as f32 * layout.line_h - view.scroll_anim
    };

    let rect         = layout.rect;
    let line_h       = layout.line_h;
    let font_size    = layout.font_size;
    let min_cursor_w = scale_base_cursor_width(scale);
    let cursor_h     = scale_base_cursor_height(scale);
    let cursor_outline_thickness = scale_base_cursor_outline_thickness(scale);
    let origin_x     = rect.x + PADDING_LEFT;

    let (cursor_line, cursor_col) = buffer.cursor_line_col(&view.cursor);

    let vis_start = layout.first_buffer_line;
    let vis_end   = vis_start + layout.lines.len() as u32;

    let space_width = gpu::get_glyph(gpu, ' ', font_size)
        .map(|g| g.advance)
        .unwrap_or(min_cursor_w * 4.0);

    let min_cursor_w = min_cursor_w.max(space_width);

    let is_this_view_focused = active_view_id == view.id;

    //
    //
    // Selection
    //
    //
    if let Some(anchor) = view.cursor.anchor_char_index {
        let _tracy = tracy::span!("render_text_layout::selection");

        let c = view.cursor.char_index;
        let (start_index, end_index) = if anchor <= c { (anchor, c) } else { (c, anchor) };

        if start_index != end_index {
            let (start_line, start_col) = buffer.char_to_line_col(start_index);
            let (end_line,   end_col)   = buffer.char_to_line_col(end_index);

            let draw_start = start_line.max(vis_start);
            let draw_end   = end_line.min(vis_end.saturating_sub(1));

            for line_index in draw_start..=draw_end {
                let Some(ll) = layout.line_for_buffer_line(line_index) else { continue };
                let y = line_y(line_index) + cursor_h;

                let (x0, x1) = if start_line == end_line {
                    (layout.x_for_col(origin_x, start_col, ll), layout.x_for_col(origin_x, end_col, ll))
                } else if line_index == start_line {
                    (layout.x_for_col(origin_x, start_col, ll), rect.x + rect.w)
                } else if line_index == end_line {
                    (rect.x, layout.x_for_col(origin_x, end_col, ll))
                } else {
                    (rect.x, rect.x + rect.w)
                };

                if x1 > x0 {
                    gpu::draw_rect(gpu, x0, y, x1 - x0, line_h, palette().selection);
                } else {
                    gpu::draw_rect(gpu, rect.x, y, 8.0, line_h, palette().selection);
                }
            }
        }
    }

    //
    //
    // Current-line highlight
    //
    //
    if let Some(ll) = layout.line_for_buffer_line(cursor_line) {
        let _tracy = tracy::span!("render_text_layout::current_line");

        let y = view.cursor_anim_y + cursor_h;

        let has_selection = view.cursor.anchor_char_index
            .map(|a| a != view.cursor.char_index)
            .unwrap_or(false);

        if has_selection {
            let (start_index, end_index) = {
                let a = view.cursor.anchor_char_index.unwrap();
                let c = view.cursor.char_index;
                if a <= c { (a, c) } else { (c, a) }
            };
            let (start_line, start_col) = buffer.char_to_line_col(start_index);
            let (end_line,   end_col)   = buffer.char_to_line_col(end_index);

            if start_line == cursor_line && end_line == cursor_line {
                let x0 = layout.x_for_col(origin_x, start_col, ll);
                let x1 = layout.x_for_col(origin_x, end_col, ll);
                if x0 > rect.x {
                    gpu::draw_rect(gpu, rect.x, y, x0 - rect.x, line_h, palette().current_line);
                }
                if x1 < rect.x + rect.w {
                    gpu::draw_rect(gpu, x1, y, (rect.x + rect.w) - x1, line_h, palette().current_line);
                }
            } else if start_line < cursor_line && end_line > cursor_line {
                // fully covered by selection - no current-line bg
            } else if end_line == cursor_line {
                let x1 = layout.x_for_col(origin_x, end_col, ll);
                gpu::draw_rect(gpu, x1, y, (rect.x + rect.w) - x1, line_h, palette().current_line);
            } else if start_line == cursor_line {
                let x0 = layout.x_for_col(origin_x, start_col, ll);
                if x0 > rect.x {
                    gpu::draw_rect(gpu, rect.x, y, x0 - rect.x, line_h, palette().current_line);
                }
            } else {
                gpu::draw_rect(gpu, rect.x, y, rect.w, line_h, palette().current_line);
            }
        } else {
            gpu::draw_rect(gpu, rect.x, y, rect.w, line_h, palette().current_line);
        }
    }

    //
    //
    // Matching paren
    //
    //
    if let Some((m_line, m_col)) = find_matching_paren(buffer, cursor_line, cursor_col, scratch_paren) {
        // Cursor paren
        if cursor_line >= vis_start && cursor_line < vis_end {
            if let Some(ll) = layout.line_for_buffer_line(cursor_line) {
                let x = layout.x_for_col(origin_x, cursor_col, ll);
                let w = layout.glyph_width_at_col(cursor_col, min_cursor_w, ll);
                let y = line_y(cursor_line) + cursor_h;
                gpu::draw_rect(gpu, x, y + cursor_h, w, line_h + cursor_h, palette().paren_match);
            }
        }

        // Matching paren
        if m_line >= vis_start && m_line < vis_end {
            if let Some(ll) = layout.line_for_buffer_line(m_line) {
                let x = layout.x_for_col(origin_x, m_col, ll);
                let w = layout.glyph_width_at_col(m_col, min_cursor_w, ll);
                let y = line_y(m_line) + cursor_h;
                gpu::draw_rect(gpu, x, y + cursor_h, w, line_h + cursor_h, palette().paren_match);
            }
        }
    }

    //
    //
    // Cursor (on the focused view (filled in rectangle))
    //
    //
    if show_cursor && is_this_view_focused
        && let Some(ll) = layout.line_for_buffer_line(cursor_line)
    {
        let cursor_glyph_w = layout.glyph_width_at_col(cursor_col, min_cursor_w, ll).max(min_cursor_w);
        gpu::draw_rect(
            gpu,
            view.cursor_anim_x,
            view.cursor_anim_y + cursor_h,
            cursor_glyph_w,
            line_h + cursor_h,
            palette().cursor,
        );
    }

    //
    //
    // Text
    //
    //
    let default_color = token_color(lexer::TokenKind::Default);

    for ll in &layout.lines {
        let glyphs = ll.glyphs(&layout.glyphs);
        if glyphs.is_empty() { continue; }

        let y = line_y(ll.buffer_line) + line_h;
        let is_cursor_line = ll.buffer_line == cursor_line;

        gpu::draw_text_colored_with_precomputed_glyphs(gpu, glyphs, origin_x, y, |char_index| {
            if is_this_view_focused && show_cursor && is_cursor_line && char_index as u32 == cursor_col {
                palette().cursor_text
            } else {
                glyphs.get(char_index).map(|g| g.color).unwrap_or(default_color)
            }
        });
    }

    //
    //
    // Cursor (on the UNfocused view (outlined rectangle))
    //
    //
    if show_cursor && !is_this_view_focused
        && let Some(ll) = layout.line_for_buffer_line(cursor_line)
    {
        let cursor_glyph_w = layout.glyph_width_at_col(cursor_col, min_cursor_w, ll).max(min_cursor_w);
        gpu::draw_rect_outline(
            gpu,
            view.cursor_anim_x,
            view.cursor_anim_y + cursor_h,
            cursor_glyph_w,
            line_h + cursor_h,
            cursor_outline_thickness,
            palette().cursor,
        );
    }
}

#[derive(Eq, PartialEq, PartialOrd, Clone, Copy, Debug)]
pub struct PanelId(pub u32);
cranelift_entity::entity_impl!(PanelId);

#[derive(Eq, PartialEq, PartialOrd, Clone, Copy, Debug)]
pub struct BufferId(pub u32);
cranelift_entity::entity_impl!(BufferId);

#[derive(Eq, PartialEq, PartialOrd, Clone, Copy, Debug)]
pub struct ViewId(pub u32);
cranelift_entity::entity_impl!(ViewId);

pub const PANEL_NONE: PanelId = PanelId(u32::MAX-1);
pub const  VIEW_MAIN: ViewId  = ViewId(0);

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

    pub cursor_target_line: u32,
    pub cursor_target_col:  u32,

    pub cursor:      Cursor,
    pub layout:      Option<TextLayout>
}

impl View {
    pub fn new_with_scroll(id: ViewId, buffer_id: BufferId, scroll: f32) -> Self {
        Self {
            id, buffer_id, scroll, cursor: Cursor::new(), layout: None,
            cursor_anim_x: 0.0, cursor_anim_y: 0.0, scroll_anim: 0.0,
            cursor_target_line: 0,
            cursor_target_col: 0,
        }
    }

    pub fn new(id: ViewId, buffer_id: BufferId) -> Self {
        Self::new_with_scroll(id, buffer_id, 0.0)
    }

    #[inline]
    pub fn scroll_to_cursor(&mut self, line: u32, line_h: f32, rect: Rect) {
        let cursor_top    = line as f32 * line_h;
        let cursor_bottom = cursor_top + line_h;

        // Only scroll when cursor is fully off screen - no margin
        if cursor_top < self.scroll {
            self.scroll = cursor_top;
        }

        let view_bottom = self.scroll + rect.h;
        if cursor_bottom > view_bottom {
            self.scroll = cursor_bottom - rect.h;
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
    pub fn line_to_screen_y(&self, line: u32, rect: Rect, line_h: f32) -> f32 {
        rect.y + line as f32 * line_h - self.scroll
    }
}

//
// EditorState  -  owns everything, lives on the main thread
//

pub struct EditorState {
    // Storage
    pub buffers:         PrimaryMap<BufferId, Buffer>,
    pub views:           PrimaryMap<ViewId,   View>,
    pub panels:          PrimaryMap<PanelId,  Panel>,

    // Which panel is active (receives keyboard input)
    pub active_panel:    PanelId,

    // Root panel id - its rect always equals the window
    pub root_panel:      PanelId,

    // Scale for font/line-height
    pub scale:           f32,

    // Cursor blink
    pub blink_epoch:     Instant,

    // Mouse
    pub mouse_pos:          (f32, f32),
    pub mouse_left_pressed: bool,

    scratch_paren:       Vec<char>,

    pub frame_count:     u32,
    pub last_fps_time:   Instant,
    pub last_frame_time: Instant,
    pub fps:             f32,

    pub relex_us_acc:    f32,
    pub build_us_acc:    f32,
    pub render_us_acc:   f32,

    pub relex_us:        f32,
    pub build_us:        f32,
    pub render_us:       f32,
}

impl EditorState {
    pub fn new(buffer: Buffer) -> Self {
        let mut buffers = PrimaryMap::default();
        let mut views   = PrimaryMap::default();
        let mut panels  = PrimaryMap::default();

        let buf_id   = buffers.push(buffer);
        let view_id  = ViewId::new(0);  views.push(View::new(view_id, buf_id));
        let panel_id = PanelId::new(0); panels.push(Panel {
            id:   panel_id,
            rect: Rect::default(),  // Set on first resize / resumed
            kind: PanelKind::Leaf { view_id },
        });

        Self {
            buffers,
            views,
            panels,
            scratch_paren: Default::default(),
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
            relex_us_acc:  0.0,
            build_us_acc:  0.0,
            relex_us:  0.0,
            build_us:  0.0,
            render_us_acc:  0.0,
            render_us:  0.0,
        }
    }

    pub fn view_and_buffer(&mut self, view_id: ViewId) -> (&View, &Buffer) {
        let buf_id = self.views[view_id].buffer_id;
        let view   = &self.views[view_id];
        let buffer = &self.buffers[buf_id];
        (view, buffer)
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

    pub fn active_view_and_buffer(&mut self) -> (&View, &Buffer) {
        let view_id = self.active_view_id();
        self.view_and_buffer(view_id)
    }

    pub fn panel(&self, id: PanelId) -> &Panel { &self.panels[id] }
    pub fn panel_mut(&mut self, id: PanelId) -> &mut Panel { &mut self.panels[id] }

    pub fn active_view_id(&self) -> ViewId {
        match self.panels[self.active_panel].kind {
            PanelKind::Leaf { view_id } => view_id,
            _ => VIEW_MAIN,
        }
    }

    pub fn active_view(&self) -> &View { &self.views[self.active_view_id()] }
    pub fn active_view_mut(&mut self) -> &mut View { let id = self.active_view_id(); &mut self.views[id] }

    pub fn active_buffer(&self) -> &Buffer {
        let buf_id = self.active_view().buffer_id;
        &self.buffers[buf_id]
    }
    pub fn active_buffer_mut(&mut self) -> &mut Buffer {
        let buf_id = self.active_view().buffer_id;
        &mut self.buffers[buf_id]
    }

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
            rect.split_horizontally(split.ratio)
        } else {
            rect.split_vertically(split.ratio)
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
        let new_view_id = ViewId::new(self.views.len());
        self.views.push(View {
            id: new_view_id,
            ..old_view
        });

        // Two new leaf panels
        let left_id  = PanelId::new(self.panels.len());
        let right_id = PanelId::new(left_id.0 as usize + 1);

        self.panels.push(Panel { id: left_id,  kind: PanelKind::Leaf { view_id: old_view_id }, rect: Rect::default() });
        self.panels.push(Panel { id: right_id, kind: PanelKind::Leaf { view_id: new_view_id }, rect: Rect::default() });

        // Turn the old panel into a split node
        self.panels[active_id].kind = PanelKind::Split(PanelSplit {
            vertical,
            ratio,
            left_id,
            right_id,
        });

        // Reset layouts
        self.views[new_view_id].layout = None;
        self.views[old_view_id].layout = None;

        // Active panel becomes the left child
        self.active_panel = left_id;
    }

    pub fn close_active(&mut self) {
        let active_id = self.active_panel;

        let parent_and_child = self.panels.values().find_map(|p| {
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

        let parent_and_child = self.panels.values().find_map(|p| {
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

    pub fn snap_cursor_to_target(&mut self, view_id: ViewId, target_line: u32, target_col: u32, panel_rect: Rect) {
        if let Some(layout) = &self.views[view_id].layout {
            if let Some(x) = layout.cursor_x(target_line, target_col) {
                let line_h = self.line_h();
                let y = self.views[view_id].line_to_screen_y(target_line, panel_rect, line_h);
                self.views[view_id].cursor_anim_x = x;
                self.views[view_id].cursor_anim_y = y;
            }
        }
    }

    pub fn line_h(&self)    -> f32 { scale_base_line_height(self.scale) }
    pub fn font_size(&self) -> f32 { scale_base_font_size(self.scale) }
    pub fn cursor_w(&self) -> f32 { scale_base_cursor_width(self.scale) }
    pub fn cursor_h(&self) -> f32 { scale_base_cursor_height(self.scale) }

    pub fn cursor_visible(&self) -> bool { cursor_visible(&self.blink_epoch) }
    pub fn reset_blink(&mut self) { self.blink_epoch = Instant::now(); }

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
    let _tracy = tracy::span!("animate_views");

    let epsilon       = 0.5f32; // Stop animating when close enough
    let mut animating = false;

    let line_h    = editor.line_h();

    for view in editor.views.values_mut() {
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

        let (cursor_line, cursor_col) = (view.cursor_target_line, view.cursor_target_col);

        if let Some(target_x) = layout.cursor_x(cursor_line, cursor_col) {
            // Compute target Y from scroll_anim so cursor tracks the animated scroll,
            // not the settled scroll position
            let target_y = layout.rect.y + cursor_line as f32 * line_h - view.scroll_anim;

            let dy = target_y - view.cursor_anim_y;

            if dy.abs() > layout.rect.h {
                view.cursor_anim_y = target_y;
            } else if dy.abs() > epsilon {
                view.cursor_anim_y += dy * (1.0 - (-CURSOR_ANIM_RATE * dt).exp());
                animating = true;
            } else {
                view.cursor_anim_y = target_y;
            }

            view.cursor_anim_x = target_x;
        }
    }

    animating
}

fn find_matching_paren(
    buffer: &Buffer,
    start_line: u32, start_col: u32,
    scratch_paren: &mut Vec<char>,
) -> Option<(u32, u32)> {
    let _tracy = tracy::span!("find_matching_paren");

    let (open, close, dir) = {
        let ch = char_at_line_col(buffer, start_line, start_col)?;

        match ch {
            '(' => ('(', ')',  1),
            '[' => ('[', ']',  1),
            '{' => ('{', '}',  1),
            ')' => ('(', ')', -1),
            ']' => ('[', ']', -1),
            '}' => ('{', '}', -1),
            _ => return None,
        }
    };

    let mut depth = 0;

    const MAX_SCAN_LINES: u32 = 2000;

    if dir > 0 {
        let mut line = start_line;

        while line < buffer.text.len_lines() as u32 && line < start_line + MAX_SCAN_LINES {
            let text = buffer.text.line(line as usize);

            let start_col_in_line = if line == start_line { start_col } else { 0 };

            for (col, ch) in text.chars().enumerate().skip(start_col_in_line as usize) {
                if ch == open {
                    depth += 1;
                } else if ch == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some((line, col as u32));
                    }
                }
            }

            line += 1;
        }
    } else {
        let mut line = start_line;
        loop {
            let text = buffer.text.line(line as usize);
            let end_col = if line == start_line { start_col + 1 } else { text.chars().count() as u32 };

            scratch_paren.clear();
            scratch_paren.extend(text.chars().take(end_col as usize));

            scratch_paren.clear();
            scratch_paren.extend(text.chars().take(end_col as usize));

            for col in (0..scratch_paren.len()).rev() {
                let ch = scratch_paren[col];
                if ch == close      { depth += 1; }
                else if ch == open  { depth -= 1; if depth == 0 { return Some((line, col as u32)); } }
            }

            if line == 0 { break; }
            line -= 1;

            if start_line - line > MAX_SCAN_LINES { break; }
        }
    }

    None
}

fn char_at_line_col(buffer: &Buffer, line: u32, col: u32) -> Option<char> {
    let line_text = buffer.text.line(line as usize);
    line_text.chars().nth(col as usize)
}

fn collect_leaves(panels: &[Panel], id: PanelId, out: &mut SmallVec<[(PanelId, ViewId, Rect); 16]>) {
    match panels[id.index()].kind {
        PanelKind::Leaf { view_id } => out.push((id, view_id, panels[id.index()].rect)),
        PanelKind::Split(s) => {
            collect_leaves(panels, s.left_id,  out);
            collect_leaves(panels, s.right_id, out);
        }
    }
}

fn apply_scale(editor: &mut EditorState, new_scale: f32, anchor_my: Option<f32>) {
    let old_line_h = editor.line_h();
    editor.scale = new_scale.clamp(MIN_SCALE, MAX_SCALE);
    let new_line_h = editor.line_h();

    // For each view, preserve the line that was at the anchor screen Y.
    // If no anchor (keyboard zoom), preserve the line at the top of the view.
    for view in editor.views.values_mut() {
        let anchor_y = anchor_my.unwrap_or(0.0);
        // Which line was at anchor_y before the scale change?
        let line_at_anchor = (view.scroll + anchor_y) / old_line_h;
        // Recompute scroll so that same line stays at anchor_y
        view.scroll = (line_at_anchor * new_line_h - anchor_y).max(0.0);
        view.scroll_anim = view.scroll;
        view.layout = None;
    }
}

fn rescale(editor: &mut EditorState, gpu: &mut Gpu, new_scale: f32) {
    let new = new_scale.clamp(MIN_SCALE, MAX_SCALE);
    if new != editor.scale {
        reset_atlas(gpu);
        apply_scale(editor, new, None);
        force_layouts_from_all_views_to_rebuild(editor);
    }
}

fn force_layouts_from_all_views_to_rebuild(editor: &mut EditorState) {
    for view in editor.views.values_mut() {
        view.layout = None;
    }
}

fn scroll_page(editor: &mut EditorState, _gpu: &Gpu, direction: i32) {
    let line_h   = editor.line_h();
    let view_id  = editor.active_view_id();
    let panel_id = editor.active_panel;
    let rect     = editor.panels[panel_id].rect;
    let buf_id   = editor.views[view_id].buffer_id;

    let page_lines = ((rect.h / line_h) as isize - 2).max(1);
    let delta      = direction as isize * page_lines;
    let total      = editor.buffers[buf_id].text.len_lines();

    let (cur_line, cur_col) = editor.buffers[buf_id]
        .cursor_line_col(&editor.views[view_id].cursor);

    let new_line = ((cur_line as isize + delta).max(0) as usize)
        .min(total.saturating_sub(1)) as u32;

    let max_scroll = ((total as f32 * line_h) - rect.h).max(0.0);
    let new_scroll = (editor.views[view_id].scroll + delta as f32 * line_h)
        .clamp(0.0, max_scroll);

    editor.views[view_id].scroll = new_scroll;

    editor.buffers[buf_id].set_cursor_line_col(
        new_line, cur_col, &mut editor.views[view_id].cursor
    );
    editor.views[view_id].cursor_target_line = new_line;
    editor.views[view_id].cursor_target_col  = cur_col;

    editor.reset_blink();
}

pub fn snap_cursor_to_target_point_in_active_view(editor: &mut EditorState) {
    let view_id  = editor.active_view_id();
    let panel_id = editor.active_panel;
    let rect     = editor.panels[panel_id].rect;

    let (view, buf) = editor.active_view_and_buffer();
    let (line, col) = buf.cursor_line_col(&view.cursor);

    editor.snap_cursor_to_target(view_id, line, col, rect);
}

pub fn scroll_to_cursor(editor: &mut EditorState) {
    let view_id = editor.active_view_id();
    let (view, buf) = editor.active_view_and_buffer_mut();

    let (line, col) = buf.cursor_line_col(&view.cursor);

    let line_h   = editor.line_h();
    let panel_id = editor.active_panel;
    let rect     = editor.panels[panel_id].rect;

    editor.views[view_id].scroll_to_cursor(line, line_h, rect);
    editor.views[view_id].cursor_target_col  = col;
    editor.views[view_id].cursor_target_line = line;
}

#[derive(Default)]
struct App {
    gpu:    Option<Gpu>,
    editor: Option<EditorState>,
    window: Option<Arc<Window>>,
    mods:   winit::event::Modifiers,
    command_table: CommandTable,
    keymap: Keymap
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
        let buffer = Buffer::from_file(path.as_ref()).expect("failed to open file");

        let mut editor = EditorState::new(buffer);
        editor.layout_panels(Rect::full(w as f32, h as f32));
        self.editor = Some(editor);
        self.window = Some(win);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let Some(editor) = &self.editor else { return };
        let Some(_win)   = &self.window else { return };

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

        macro_rules! make_command_context {
            ($event: expr) => {
                CommandContext {
                    editor, gpu,
                    mods: self.mods, event: $event, command_table: &self.command_table,
                    last_executed_command: None
                }
            };
        }

        match event {
            WindowEvent::CloseRequested => el.exit(),

            WindowEvent::ModifiersChanged(m) => self.mods = m,

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed { return; }

                let mods = Mods { alt, ctrl, shift };
                if let Some(command_name) = self.keymap.lookup(&event, mods) {
                    let Some(command) = self.command_table.get(command_name) else {
                        return;
                    };

                    let mut cx = make_command_context!(&event);
                    cx.last_executed_command = Some(command);
                    (command.func)(&mut cx);
                    win.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                if ctrl {
                    let dy = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y,
                        MouseScrollDelta::PixelDelta(p)   => p.y as f32 * 0.01,
                    };
                    let new = (editor.scale + dy * 0.075).clamp(MIN_SCALE, MAX_SCALE);
                    rescale(editor, gpu, new);
                    win.request_redraw();
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
                let first_vis  = (scroll / line_h) as u32;
                let last_vis = (((scroll + rect.h) / line_h) as usize)
                    .saturating_sub(1)
                    .min(total.saturating_sub(1)) as u32;

                let new_line = if cur_line < first_vis {
                    first_vis
                } else if cur_line > last_vis {
                    last_vis.min(total.saturating_sub(1) as u32) as u32
                } else {
                    cur_line // Still visible, don't move
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
                    let rect      = editor.panels[editor.active_panel].rect;
                    let buf_id    = editor.views[view_id].buffer_id;
                    let scroll_anim = editor.views[view_id].scroll_anim;

                    let (line, col) = if let Some(layout) = &editor.views[view_id].layout {
                        layout.hit_test(mx, my, scroll_anim)
                    } else { // @Robustness
                        let line_h = editor.line_h();
                        let line = ((my - rect.y + editor.views[view_id].scroll) / line_h) as usize;
                        let line = line.min(editor.buffers[buf_id].text.len_lines().saturating_sub(1));
                        (line as u32, 0)  // col 0 - no glyph metrics without layout
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

            WindowEvent::CursorMoved { position, .. } => {
                editor.mouse_pos = (position.x as f32, position.y as f32);

                if editor.mouse_left_pressed {
                    let (mx, my) = editor.mouse_pos;
                    let pid = editor.panel_at(mx, my).unwrap_or(editor.active_panel);

                    if let PanelKind::Leaf { view_id } = editor.panels[pid].kind {
                        let rect      = editor.panels[pid].rect;
                        let buf_id    = editor.views[view_id].buffer_id;
                        let scroll_anim = editor.views[view_id].scroll_anim;

                        let (line, col) = if let Some(layout) = &editor.views[view_id].layout {
                            layout.hit_test(mx, my, scroll_anim)
                        } else { // @Robustness
                            let line_h = editor.line_h();
                            let line = ((my - rect.y + editor.views[view_id].scroll) / line_h) as usize;
                            let line = line.min(editor.buffers[buf_id].text.len_lines().saturating_sub(1));
                            (line as u32, 0)  // col 0 - no glyph metrics without layout
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

            WindowEvent::RedrawRequested => {
                tracy_client::frame_mark();

                let now = Instant::now();
                let dt = now.duration_since(editor.last_frame_time).as_secs_f32().min(0.05);
                editor.last_frame_time = now;
                editor.frame_count += 1;

                let elapsed = editor.last_fps_time.elapsed().as_secs_f32();
                if elapsed >= 0.5 {
                    editor.fps       = editor.frame_count as f32 / elapsed;
                    editor.build_us  = editor.build_us_acc       / editor.frame_count as f32;
                    editor.render_us = editor.render_us_acc      / editor.frame_count as f32;
                    editor.relex_us  = editor.relex_us_acc       / editor.frame_count as f32;

                    editor.frame_count    = 0;
                    editor.last_fps_time  = Instant::now();
                    editor.build_us_acc   = 0.0;
                    editor.relex_us_acc   = 0.0;
                    editor.render_us_acc  = 0.0;
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

                let mut leaf_panels = Default::default();
                collect_leaves(editor.panels.as_values_slice(), editor.root_panel, &mut leaf_panels);

                let mut should_request_redraw = false;

                should_request_redraw |= still_animating;

                for (panel_id, view_id, rect) in leaf_panels {
                    let buf_id = editor.views[view_id].buffer_id;

                    let screen_lines = (rect.h / line_h) as u32;
                    let anim_first = (editor.views[view_id].scroll_anim / line_h) as u32;
                    let is_dirty = editor.buffers[buf_id].dirty
                        || editor.views[view_id].layout.as_ref().map(|l| {
                            anim_first < l.first_buffer_line
                                || anim_first + screen_lines > l.first_buffer_line + l.lines.len() as u32
                                || (l.rect.w - rect.w).abs() > 0.5
                                || (l.rect.h - rect.h).abs() > 0.5
                                || (l.font_size - font_size).abs() > 0.01
                        }).unwrap_or(true);

                    should_request_redraw |= is_dirty;

                    if is_dirty {
                        //
                        // @Speed @Note:
                        //
                        // Currently we re-lex visible tokens on every dirty frame,
                        // thats fine for now, since those conditions inside is_dirty
                        // make sure that either the amount of lines we see has changed,
                        // OR we now see completely different lines that in the previous frame.
                        //
                        // BUT, in the future, we might wanna have a different flag for this re-lex step.
                        //
                        {
                            let t0 = Instant::now();

                            let total      = editor.buffers[buf_id].text.len_lines();
                            let scroll     = editor.views[view_id].scroll;
                            let first_vis  = (scroll / line_h) as u32;
                            let last_vis = (((scroll + rect.h) / line_h) as usize)
                                .saturating_sub(1)
                                .min(total.saturating_sub(1)) as u32;

                            editor.buffers[buf_id].lex_visible(first_vis as _, last_vis as _);

                            editor.relex_us_acc = t0.elapsed().as_micros() as f32;
                        }

                        let should_snap = editor.views[view_id].layout.as_ref()
                            .map(|l| {
                                (l.rect.w - rect.w).abs() > 0.5
                                    || (l.rect.h - rect.h).abs() > 0.5
                            }).unwrap_or(true); // true = first build

                        let t0 = Instant::now();
                        let layout = build_text_layout(
                            gpu,
                            &editor.buffers[buf_id],
                            &editor.views[view_id],
                            rect, font_size, line_h,
                        );
                        editor.build_us_acc = t0.elapsed().as_micros() as f32;

                        editor.views[view_id].layout = Some(layout);

                        if should_snap {
                            let (cl, cc) = (
                                editor.views[view_id].cursor_target_line,
                                editor.views[view_id].cursor_target_col,
                            );

                            editor.snap_cursor_to_target(view_id, cl, cc, rect);
                        }
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
                    let t1 = Instant::now();
                    render_text_layout(
                        gpu, layout,
                        &editor.buffers[buf_id],
                        &editor.views[view_id],
                        editor.active_view_id(),
                        editor.scale,
                        show_cursor,
                        &mut editor.scratch_paren,
                    );
                    editor.render_us += t1.elapsed().as_micros() as f32;
                }

                // Clear dirty after all views have rebuilt their layouts
                for buf in editor.buffers.values_mut() {
                    buf.dirty = false;
                }

                let mut perf = SmallString::<[u8; 128]>::new();
                _ = writeln!(&mut perf, "{:.0} fps  build:{:.2}us  relex:{:.2}us  render:{:.2}us", editor.fps, editor.build_us, editor.relex_us, editor.render_us);
                gpu::draw_text(gpu, &perf, gpu.win_w - 456.0, 14.0, 12.0, Color::rgba(120, 120, 120, 255));
                gpu::submit_frame(gpu).unwrap();

                if should_request_redraw {
                    win.request_redraw();
                }
            }

            _ => {}
        }
    }
}

fn main() {
    let _client = tracy_client::Client::start();

    let el = EventLoop::new().unwrap();
    el.set_control_flow(ControlFlow::Wait);

    let command_table = CommandTable::from_inventory();
    let keymap = Keymap::default_keymap();
    let mut app = App { command_table, keymap, ..Default::default() };
    _ = el.run_app(&mut app);
}
