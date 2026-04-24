#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

mod gpu;
mod util;
mod color;
mod buffer;
mod command;
mod tracy;
mod director;
mod lexer;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use std::fmt::Write as _;
use std::collections::VecDeque;

use buffer::{Buffer, Cursor};
use color::Color;
use command::{CommandAtom, CommandContext, CommandTable, Keymap, Mods};
use cranelift_entity::{EntityRef, PrimaryMap};
use director::Director;
use gpu::{ATLAS_SIZE, Gpu, GpuGlyph, INITIAL_VERTEX_BUFFER_CAPACITY, draw_text_for_editor, prewarm_glyphs, reset_atlas};
use lexer::token_color;

use smallstr::SmallString;
use smallvec::SmallVec;
use util::format_bytes;
use wgpu::naga::FastHashMap;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};

#[cfg(debug_assertions)]
macro_rules! checked_push {
    ($vec:expr, $val:expr, $name:expr) => {
        {
            let cap_before = $vec.capacity();
            $vec.push($val);
            if $vec.capacity() != cap_before {
                eprintln!(
                    "[scratch] {} reallocated: {} -> {} (len={})",
                    $name, cap_before, $vec.capacity(), $vec.len()
                );
            }
        }
    };

    ($vec:expr, $val:expr) => {
        checked_push!($vec, $val, stringify!($vec))
    };
}

#[cfg(not(debug_assertions))]
macro_rules! checked_push {
    ($vec:expr, $val:expr, $name:expr) => { $vec.push($val); };
    ($vec:expr, $val:expr) => { $vec.push($val); };
}

fn prewarm_glyphs_and_print_preallocation_memory_usage(editor: &Editor, gpu: &mut Gpu) {
    println!("[Prewarming glyphs...]");
    for scale in [
        editor.scale,
        editor.scale - SCALE_STEP,
        editor.scale + SCALE_STEP,
        editor.scale - 2.0 * SCALE_STEP,
        editor.scale + 2.0 * SCALE_STEP,
        editor.scale - 3.0 * SCALE_STEP,
        editor.scale + 3.0 * SCALE_STEP,
    ] {
        let font_size = scale_base_font_size(scale);
        prewarm_glyphs(gpu, font_size);
    }

    let vertex_batch_pool_allocation = gpu.batch_pool.iter()
        .map(|b| b.verts.capacity())
        .sum::<usize>();

    println!("[Vertex batch pool preallocation]: {}", format_bytes(vertex_batch_pool_allocation));
    println!("[Vertex buffer size]:              {}", format_bytes(gpu.vertex_buffer.size() as _));
    println!("[Glyph memory usage]:              {}", format_bytes(gpu.glyphs.allocation_size()));

    let used_pixels = gpu.atlas_cur_y as u32 * ATLAS_SIZE + gpu.atlas_cur_x as u32;
    let bytes_per_pixel = 4;
    let used_bytes = used_pixels * bytes_per_pixel;
    let total_bytes = ATLAS_SIZE * ATLAS_SIZE * bytes_per_pixel;

    println!(
        "[Atlas] used={} / {} bytes ({:.2}%)",
        format_bytes(used_bytes as _),
        format_bytes(total_bytes as _),
        (used_bytes as f32 / total_bytes as f32) * 100.0
    );
}

fn draw_metrics(editor: &Editor, gpu: &mut Gpu) {
    const BUILD_GOOD: f32  = 200.0;
    const BUILD_SLOW: f32  = 800.0;

    const RENDER_GOOD: f32 = 300.0;
    const RENDER_SLOW: f32 = 1500.0;

    const RELEX_GOOD: f32  = 100.0;
    const RELEX_SLOW: f32  = 500.0;

    const FRAME_GOOD: f32  = 500.0;
    const FRAME_SLOW: f32  = 2000.0;

    const HUD_Y: f32 = 14.0;
    const HUD_LINE_H: f32 = 14.0;
    const PAD_RIGHT: f32 = 180.0;

    fn heat(v: f32, good: f32, slow: f32) -> Color {
        let t = ((v - good) / (slow - good)).clamp(0.0, 1.0);

        // Base16-ish muted ramp:
        // green -> yellow -> orange -> red (all desaturated)
        let (r, g, b) = if t < 0.33 {
            // Muted green
            (
                (90.0 * t / 0.33 + 80.0) as u8,
                (140.0 - 40.0 * t / 0.33) as u8,
                (90.0) as u8,
            )
        } else if t < 0.66 {
            // Muted yellow/orange
            let tt = (t - 0.33) / 0.33;
            (
                (160.0 + 40.0 * tt) as u8,
                (140.0 - 30.0 * tt) as u8,
                (70.0 - 20.0 * tt) as u8,
            )
        } else {
            // Muted red
            let tt = (t - 0.66) / 0.34;
            (
                (200.0 + 20.0 * tt) as u8,
                (90.0 - 20.0 * tt) as u8,
                (70.0 - 10.0 * tt) as u8,
            )
        };

        Color::rgba(r, g, b, 255)
    }

    let fps    = editor.fps;
    let build  = editor.build_us;
    let relex  = editor.relex_us;
    let render = editor.render_us;
    let frame  = build + relex + render;

    let x_right = gpu.win_w - PAD_RIGHT;

    let mut buf = SmallString::<[u8; 64]>::new();

    buf.clear();
    _ = write!(&mut buf, "fps: {:.0}", fps);
    gpu::draw_text(
        gpu,
        &buf,
        x_right,
        HUD_Y + 0.0 * HUD_LINE_H,
        12.0,
        Color::rgba(180, 180, 180, 255),
    );

    buf.clear();
    _ = write!(&mut buf, "build: {:.2}us", build);
    gpu::draw_text(
        gpu,
        &buf,
        x_right,
        HUD_Y + 1.0 * HUD_LINE_H,
        12.0,
        heat(build, BUILD_GOOD, BUILD_SLOW),
    );

    buf.clear();
    _ = write!(&mut buf, "relex: {:.2}us", relex);
    gpu::draw_text(
        gpu,
        &buf,
        x_right,
        HUD_Y + 2.0 * HUD_LINE_H,
        12.0,
        heat(relex, RELEX_GOOD, RELEX_SLOW),
    );

    buf.clear();
    _ = write!(&mut buf, "render: {:.2}us", render);
    gpu::draw_text(
        gpu,
        &buf,
        x_right,
        HUD_Y + 3.0 * HUD_LINE_H,
        12.0,
        heat(render, RENDER_GOOD, RENDER_SLOW),
    );

    buf.clear();
    _ = write!(&mut buf, "frame: {:.2}us", frame);
    gpu::draw_text(
        gpu,
        &buf,
        x_right,
        HUD_Y + 4.0 * HUD_LINE_H,
        12.0,
        heat(frame, FRAME_GOOD, FRAME_SLOW),
    );
}

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

const LISTER_ITEMS_PADDING: f32 = 0.0;

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

const BLINK_START_DELAY_MS: u128 = 500;  // Start blinking after 500ms idle
const BLINK_STOP_IDLE_MS:   u128 = 5000; // Stop blinking after 5s idle

fn cursor_visible(epoch: &Instant, last_input: &Instant) -> bool {
    let since_input = last_input.elapsed().as_millis();

    // Typing: show solid cursor
    if since_input < BLINK_START_DELAY_MS {
        return true;
    }

    // Idle too long: show solid cursor
    if since_input > BLINK_STOP_IDLE_MS {
        return true;
    }

    // In between: blink
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
            if local <= g.x + g.advance() {
                col = i;
                break;
            }
        }

        col as _
    }
}

#[derive(Clone, Debug)]
pub struct TextLayout {
    pub buffer_id: BufferId,

    pub view_scroll:       f32, // view.scroll
    pub line_h:            f32,
    pub font_size:         f32,
    pub first_buffer_line: u32,

    pub rect: Rect,

    pub visible_glyph_count: u32,

    // @Memory @Speed: Reuse these allocations.
    // Because currently they're being reallocated each frame.
    pub lines:  Vec<LineLayout>,
    pub glyphs: Vec<Glyph>
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
fn rebuild_text_layout(
    editor:    &mut Editor,
    gpu:       &mut Gpu,

    view_id:   ViewId,

    rect: Rect, font_size: f32, line_h: f32,
) {
    let view      = &editor.views[view_id];
    let buffer_id = view.buffer_id;

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

        let total      = editor.buffers[buffer_id].text.len_lines();
        let scroll     = editor.views[view_id].scroll;
        let first_vis  = (scroll / line_h) as u32;
        let last_vis = (((scroll + rect.h) / line_h) as usize)
            .saturating_sub(1)
            .min(total.saturating_sub(1)) as u32;

        editor.buffers[buffer_id].lex_visible(first_vis as _, last_vis as _);

        editor.relex_us_acc += t0.elapsed().as_micros() as f32;
    }

    let should_snap = editor.views[view_id].layout.as_ref()
        .map(|l| {
            (l.rect.w - rect.w).abs() > 0.5
                || (l.rect.h - rect.h).abs() > 0.5
        }).unwrap_or(true); // true = first build

    let t0 = Instant::now();
    let layout = build_text_layout(
        gpu,
        &editor.buffers[buffer_id],
        &editor.views[view_id],
        rect, font_size, line_h,
    );
    editor.build_us_acc += t0.elapsed().as_micros() as f32;

    editor.views[view_id].layout    = Some(layout);
    editor.buffers[buffer_id].dirty = false;

    if should_snap {
        let (cl, cc) = (
            editor.views[view_id].cursor_target_line,
            editor.views[view_id].cursor_target_col,
        );

        editor.snap_cursor_to_target(view_id, cl, cc, rect);
    }
}

/// Build the TextLayout for a single leaf panel
fn build_text_layout(
    gpu:       &mut Gpu,

    buffer:    &Buffer,
    view:      &View,

    rect: Rect, font_size: f32, line_h: f32,
) -> TextLayout {
    let _tracy = tracy::span!("build_text_layout");

    //
    // Calculate the base visible range based on the animation (what we see now)
    //
    let mut first_line = (view.scroll_anim / line_h).floor() as u32;
    let mut last_line  = ((view.scroll_anim + rect.h) / line_h).ceil() as u32;

    //
    // Add a tiny bit of padding so lines don't pop at the very edges
    //
    first_line = first_line.saturating_sub(2);
    last_line  = last_line.saturating_add(2);

    //
    // If the animation is moving DOWN, pad the BOTTOM more.
    // If the animation is moving UP,   pad the TOP    more.
    //
    let diff = view.scroll - view.scroll_anim;
    if diff > 0.0 {
        // We are scrolling DOWN (target is below current anim)
        // Add 40 lines of lookahead to the bottom
        last_line = last_line.saturating_add(40);
    } else if diff < 0.0 {
        // We are scrolling UP   (target is above current anim)
        // Add 40 lines of lookahead to the top
        first_line = first_line.saturating_sub(40);
    }

    let line_count = last_line - first_line;

    let default_color = token_color(lexer::TokenKind::Default);
    let tokens        = &buffer.visible_tokens;

    let first_visible_byte = buffer.text.line_to_byte(first_line as usize);

    let mut visible_glyph_count = 0u32;

    let mut current_token = tokens.partition_point(|t| (t.start + t.len()) as usize <= first_visible_byte);

    // @Memory @Speed: Reuse these allocations!!!!!!!
    let mut lines  = Vec::with_capacity(line_count as usize);
    let mut glyphs = Vec::with_capacity(line_count as usize * 80);

    for vis_i in 0..line_count {
        let line_index = first_line + vis_i;
        let Some(rope_line) = buffer.text.get_line(line_index as usize) else { continue };

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

        if line_len == 0 {
            checked_push!(lines, ll);
            continue;
        }

        let mut local_x  = 0.0f32;
        let mut abs_byte = line_byte_start;

        let mut token_index = current_token;

        let glyph_start = glyphs.len() as u32;

        for char in rope_line.chars() {
            if char == '\n' { break; }

            // Advance token cursor past tokens that end before this byte
            while token_index < tokens.len() {
                let t = &tokens[token_index];
                if (t.start + t.len()) as usize <= abs_byte {
                    token_index += 1;
                } else {
                    break;
                }
            }

            // Color is from current token if it covers this byte, else default
            let color = if token_index < tokens.len() {
                let t = &tokens[token_index];
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

            checked_push!(glyphs, Glyph {
                x:         local_x,
                color,
                char,
                gpu_glyph,
            });

            local_x  += advance;
            abs_byte += char.len_utf8();
        }

        ll.glyph_start = glyph_start;
        ll.glyph_count = glyphs.len() as u32 - glyph_start;
        ll.width = local_x;
        visible_glyph_count += ll.glyph_count;

        checked_push!(lines, ll);

        current_token = token_index;
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
        visible_glyph_count
    }
}

fn render_text_layout(
    gpu:         &mut Gpu,
    layout:      &TextLayout,
    buffer:      &Buffer,
    view:        &View,
    active_view_id: ViewId,
    lister_query_view_id: ViewId,
    scale:       f32,
    show_cursor: bool,
    is_our_window_focused: bool,
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

    let is_this_view_focused = is_our_window_focused && active_view_id == view.id;
    let is_this_view_into_query_buffer = lister_query_view_id == view.id;

    let default_color = token_color(lexer::TokenKind::Default);

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
    if !is_this_view_into_query_buffer && let Some(ll) = layout.line_for_buffer_line(cursor_line) {
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
        let _tracy = tracy::span!("render_text_layout::matching_paren_render");

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

    let cursor_rect = |cursor_glyph_w: f32| {
        let cursor_width = if is_this_view_into_query_buffer {
            scale_base_cursor_width(scale)
        } else {
            cursor_glyph_w
        };

        Rect {
            x: view.cursor_anim_x,
            y: view.cursor_anim_y + cursor_h,
            w: cursor_width,
            h: line_h + cursor_h,
        }
    };

    if show_cursor && (is_this_view_into_query_buffer || is_this_view_focused)
        && let Some(ll) = layout.line_for_buffer_line(cursor_line)
    {
        let cursor_glyph_w = layout.glyph_width_at_col(cursor_col, min_cursor_w, ll).max(min_cursor_w);
        let rect = cursor_rect(cursor_glyph_w);
        gpu::draw_rect(gpu, rect.x, rect.y, rect.w, rect.h, palette().cursor);
    }

    //
    //
    // Text
    //
    //
    {
        let _tracy = tracy::span!("render_text_layout::text");

        let cursor_color = palette().cursor_text;

        for ll in &layout.lines {
            let glyphs = ll.glyphs(&layout.glyphs);
            if glyphs.is_empty() { continue; }

            let y = line_y(ll.buffer_line) + line_h;
            let is_cursor_line = ll.buffer_line == cursor_line;

            draw_text_for_editor(
                gpu,
                glyphs,
                origin_x,
                y,
                is_cursor_line && is_this_view_focused && !is_this_view_into_query_buffer && show_cursor,
                cursor_col,
                cursor_color,
                default_color,
            );
        }
    }

    //
    //
    // Cursor (on the UNfocused view (outlined rectangle))
    //
    //
    if show_cursor && !is_this_view_focused && !is_this_view_into_query_buffer
        && let Some(ll) = layout.line_for_buffer_line(cursor_line)
    {
        let cursor_glyph_w = layout.glyph_width_at_col(cursor_col, min_cursor_w, ll).max(min_cursor_w);
        let rect = cursor_rect(cursor_glyph_w);
        gpu::draw_rect_outline(gpu, rect.x, rect.y, rect.w, rect.h, cursor_outline_thickness, palette().cursor);
    }
}

// Frosted glass approximation - layered semi-transparent rects
// with slight size variations to fake depth
fn render_lister_background_frosted(gpu: &mut Gpu, lister: Rect, scale: f32) {
    // Base dark fill
    gpu::draw_rect(gpu, lister.x, lister.y, lister.w, lister.h,
        Color::rgba(12, 9, 4, 200));

    // Warm tint layer
    gpu::draw_rect(gpu, lister.x, lister.y, lister.w, lister.h,
        Color::rgba(40, 25, 8, 40));

    // Slightly inset lighter layer - gives illusion of depth/glass
    let i = scale * 1.0;
    gpu::draw_rect(gpu, lister.x + i, lister.y + i, lister.w - i*2.0, lister.h - i*2.0,
        Color::rgba(255, 200, 120, 12));

    // Top edge highlight - light catches the glass rim
    gpu::draw_rect(gpu, lister.x, lister.y, lister.w, scale,
        Color::rgba(255, 210, 140, 60));

    // Left edge highlight
    gpu::draw_rect(gpu, lister.x, lister.y, scale, lister.h,
        Color::rgba(255, 210, 140, 30));

    // Bottom edge shadow
    gpu::draw_rect(gpu, lister.x, lister.y + lister.h - scale, lister.w, scale,
        Color::rgba(0, 0, 0, 80));
}

fn render_lister_background(gpu: &mut Gpu, editor: &Editor) {
    if editor.active_panel != editor.lister_split_panel { return; }

    if !editor.lister.is_open { return; }

    // Dim the whole screen
    gpu::draw_rect(gpu, 0.0, 0.0, gpu.win_w, gpu.win_h, Color::rgba(0, 0, 0, 100));
}

fn render_lister_foreground(gpu: &mut Gpu, editor: &mut Editor) {
    if !editor.lister.is_open { return; }

    let scale     = editor.scale;
    let font_size = editor.font_size();
    let line_h    = editor.line_h();

    let lister = lister_rect(gpu.win_w, gpu.win_h);
    let Rect { x: px, y: py, w: pw, h: ph } = lister;

    let pad     = (8.0 * scale).round();
    let item_h  = (line_h + pad).round();
    let input_h = (line_h + pad).round();
    let sep     = scale.max(1.0);
    let list_y  = py + input_h + sep;
    let list_h  = ph - input_h - sep;

    editor.lister.item_h = item_h;
    editor.lister.list_h = list_h;

    // Outer border
    gpu::draw_rect_outline(gpu, px, py, pw, ph, sep,
                           Color::rgba(180, 140, 80, 200));

    // Inner border
    gpu::draw_rect_outline(gpu, px + sep, py + sep, pw - sep*2.0, ph - sep*2.0, sep,
                           Color::rgba(80, 60, 30, 80));

    // Separator
    gpu::draw_rect(gpu, px, py + input_h, pw, sep,
                   Color::rgba(180, 140, 80, 160));

    // Item count
    editor.lister.scratch_str.clear();
    _ = write!(&mut editor.lister.scratch_str, "{} results", editor.lister.filtered.len());
    let count_w = gpu::measure_str(gpu, &editor.lister.scratch_str, font_size * 0.80);
    gpu::draw_text(gpu, &editor.lister.scratch_str,
                   px + pw - pad - count_w,
                   py + input_h * 0.44 + line_h * 0.35,
                   font_size * 0.80,
                   Color::rgba(160, 120, 60, 150));

    // Items
    let first   = (editor.lister.scroll_anim / item_h) as usize;
    let visible = (list_h / item_h) as usize + 2;
    let frac    = editor.lister.scroll_anim % item_h;

    gpu::push_clip(gpu, px, list_y, pw, list_h);

    for slot in 0..visible {
        let idx      = first + slot;
        let Some(&item_idx) = editor.lister.filtered.get(idx) else { break };
        let item     = &editor.lister.items[item_idx];
        let iy       = list_y + slot as f32 * item_h - frac;

        let is_selected = idx == editor.lister.selected_index;
        let is_hovered  = editor.lister.hovered_index == Some(idx);

        if iy > list_y + list_h { break; }

        // Alternating row tint  very subtle, just enough to separate rows
        if idx % 2 == 0 {
            gpu::draw_rect(gpu, px, iy, pw, item_h,
                           Color::rgba(255, 200, 100, 8));
        }


        if is_hovered && !is_selected {
            gpu::draw_rect(gpu, px + sep*2.0, iy, pw - sep*4.0, item_h,
                           Color::rgba(60, 45, 15, 120));
        }

        if is_selected {
            gpu::draw_rect(gpu, px + sep*2.0, iy, pw - sep*4.0, item_h,
                           Color::rgba(80, 55, 20, 180));
            gpu::draw_rect(gpu, px + sep, iy, sep * 3.0, item_h,
                           Color::hex(0xc3a983));
            gpu::draw_rect(gpu, px, iy, pw, sep, Color::rgba(180, 140, 80, 60));
            gpu::draw_rect(gpu, px, iy + item_h - sep, pw, sep, Color::rgba(180, 140, 80, 60));
        }

        let label_x = px + pad + sep * 5.0;
        let label_y = iy + item_h * 0.5 + line_h * 0.35;

        gpu::draw_text(gpu, &item.label, label_x, label_y, font_size,
                       if is_selected { Color::hex(0xf0d090) } else { Color::rgba(200, 190, 165, 220) });

        if !item.sublabel.is_empty() {
            let sub_w = gpu::measure_str(gpu, &item.sublabel, font_size * 0.82);
            gpu::draw_text(gpu, &item.sublabel,
                           px + pw - pad - sub_w,
                           label_y,
                           font_size * 0.82,
                           if is_selected { Color::rgba(180, 140, 80, 200) }
                           else        { Color::rgba(120, 100, 60, 120) });
        }
    }

    gpu::pop_clip(gpu);

    // Scrollbar
    let total_items = editor.lister.filtered.len();
    if total_items > 0 {
        let total_h = total_items as f32 * item_h + item_h * LISTER_ITEMS_PADDING;
        let bar_h    = (list_h * (list_h / total_h).min(1.0)).max(sep * 4.0);
        let bar_frac = (editor.lister.scroll_anim / (total_h - list_h).max(1.0)).clamp(0.0, 1.0);
        let bar_y    = list_y + bar_frac * (list_h - bar_h);

        // Scrollbar track - very faint
        gpu::draw_rect(gpu, px + pw - sep*3.0 - sep, list_y, sep*3.0, list_h,
                       Color::rgba(255, 200, 100, 15));

        // Scrollbar thumb
        gpu::draw_rect(gpu, px + pw - sep*3.0 - sep, bar_y, sep*3.0, bar_h,
                       Color::rgba(180, 140, 80, 140));
    }
}

#[derive(Hash, Ord, Eq, PartialEq, PartialOrd, Clone, Copy, Debug)]
pub struct PanelId(pub u32);
cranelift_entity::entity_impl!(PanelId);

#[derive(Hash, Ord, Eq, PartialEq, PartialOrd, Clone, Copy, Debug)]
pub struct BufferId(pub u32);
cranelift_entity::entity_impl!(BufferId);

#[derive(Hash, Ord, Eq, PartialEq, PartialOrd, Clone, Copy, Debug)]
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
    Leaf { view_id: ViewId },
    ListerSplit,
    Split(PanelSplit),
}

#[derive(Copy, Clone, Debug)]
pub struct Panel {
    pub id:   PanelId,
    pub rect: Rect,
    pub kind: PanelKind,
}

#[derive(Debug, Clone, Copy)]
pub struct ViewState {
    pub cursor: Cursor,
    pub scroll: f32,
    pub scroll_anim: f32,
}

#[derive(Clone, Debug)]
pub struct View {
    pub id:        ViewId,
    pub buffer_id: BufferId,

    pub scroll:        f32,  // Target scroll (set instantly on any scroll event)
    pub scroll_anim:   f32,  // Animated scroll (what actually gets rendered)

    pub cursor_anim_x: f32,  // Animated cursor screen position @Redundant (We currently only animate cursor's Y movements)
    pub cursor_anim_y: f32,  // Animated cursor screen position

    pub cursor_target_line: u32,
    pub cursor_target_col:  u32,

    pub cursor:      Cursor,
    pub layout:      Option<TextLayout>,

    pub per_buffer:  FastHashMap<BufferId, ViewState>,
}

impl View {
    pub fn new_with_scroll(id: ViewId, buffer_id: BufferId, scroll: f32) -> Self {
        Self {
            id, buffer_id, scroll, cursor: Cursor::new(), layout: None,
            cursor_anim_x: 0.0, cursor_anim_y: 0.0, scroll_anim: 0.0,
            cursor_target_line: 0,
            cursor_target_col: 0,
            per_buffer: Default::default()
        }
    }

    pub fn new(id: ViewId, buffer_id: BufferId) -> Self {
        Self::new_with_scroll(id, buffer_id, 0.0)
    }

    #[inline]
    pub fn switch_buffer(&mut self, new: BufferId) {
        let old = self.buffer_id;
        if old == new { return; }

        // Save old state
        self.per_buffer.insert(old, ViewState {
            cursor: self.cursor,
            scroll: self.scroll,
            scroll_anim: self.scroll_anim,
            // @Incomplete ...
        });

        // Switch
        self.buffer_id = new;
        self.layout    = None;

        // Restore if exists
        if let Some(state) = self.per_buffer.get(&new) {
            self.cursor = state.cursor;
            self.scroll = state.scroll;
            self.scroll_anim = state.scroll_anim;
        } else {
            self.cursor = Cursor::new();
            self.scroll = 0.0;
            self.scroll_anim = 0.0;
        }
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

pub enum ListerKeyDispatch {
    Selected,
    Close,
    Other,
    None
}

pub struct ListerItem {
    pub label:    SmallString<[u8; 32]>,
    pub sublabel: SmallString<[u8; 64]>,
    pub data:     u64,
}

pub type ListerFrameUpdateCallback = fn(&mut CommandContext) -> bool;
pub type ListerSelectFn = fn(&mut CommandContext, u64);

pub struct Lister {
    pub is_open:       bool,
    pub is_listing_file_entries: bool,

    pub query:         SmallString<[u8; 128]>,

    pub selected_index: usize,
    pub  hovered_index: Option<usize>,

    pub set_selected_index_to_1_instead_of_0: bool,

    pub on_confirm:     Vec<ListerSelectFn>,
    pub pending_datas:  Vec<u64>,
    pub items_update_frame_update_callback: Vec<Option<ListerFrameUpdateCallback>>,

    pub scroll:        f32,
    pub scroll_anim:   f32,
    pub item_h:        f32,
    pub list_h:        f32,

    // Storage - cleared and refilled when lister opens
    pub items:         Vec<ListerItem>,

    // Scratch - rebuilt only when query changes (dirty flag)
    pub filtered:      Vec<usize>,   // Indices into items
    pub query_dirty:   bool,
    pub scratch_str:   String,       // For formatting, reused
}

impl Lister {
    pub fn new() -> Self {
        Self {
            is_open: false,
            query: SmallString::new(),
            filtered: Default::default(),
            items_update_frame_update_callback: Default::default(),
            hovered_index: None,
            is_listing_file_entries: false,
            items: Default::default(),
            on_confirm: Default::default(),
            pending_datas: Default::default(),
            query_dirty: false,
            scratch_str: String::with_capacity(512),
            scroll: 0.0,
            set_selected_index_to_1_instead_of_0: false,
            scroll_anim: 0.0,
            selected_index: 0,
            list_h: 0.0,
            item_h: 0.0
        }
    }

    pub fn rebuild_filtered(&mut self) {
        if !self.query_dirty { return; }
        self.filtered.clear();

        let filter_str = if self.is_listing_file_entries {
            // For filtering entries, only use the filename part of the query
            // (the part after the last /)

            let after_last_slash = self.query.as_str()
                .rsplit('/')
                .next()
                .unwrap_or(self.query.as_str());

            // If query ends with / or the part before the last slash is a dir,
            // show everything - user navigated into a directory
            if after_last_slash.is_empty() {
                ""
            } else {
                after_last_slash
            }
        } else {
            &self.query
        };

        if filter_str.is_empty() {
            self.filtered.extend(0..self.items.len());
            self.query_dirty = false;
            return;
        }

        // Filter by subsequence
        // Then score by edit distance on matched items only for sorting
        let mut scored: Vec<(usize, usize)> = self.items.iter()
            .enumerate()
            .filter(|(_, item)| Self::fuzzy_match(&item.label, filter_str))
            .map(|(i, item)| {
                // Score: edit distance between query and best substring of label
                // Use a large limit since we already know it's a subsequence match
                let score = rustc_edit_distance::edit_distance_with_substrings(
                    filter_str,
                    &item.label,
                    filter_str.len() * 3, // nocheckin
                ).unwrap_or(usize::MAX);
                (i, score)
            }).collect();

        scored.sort_unstable_by_key(|&(_, score)| score);
        self.filtered.extend(scored.iter().map(|&(i, _)| i));
        self.query_dirty = false;
    }

    fn fuzzy_match(text: &str, query: &str) -> bool {
        let mut qchars = query.chars();
        let mut qc = match qchars.next() { Some(c) => c, None => return true };
        for tc in text.chars() {
            if tc.to_ascii_lowercase() == qc.to_ascii_lowercase() {
                qc = match qchars.next() { Some(c) => c, None => return true };
            }
        }
        false
    }

    pub fn lister_key(&mut self, event: &KeyEvent, mods: Mods) -> ListerKeyDispatch {
        if !self.is_open { return ListerKeyDispatch::None; }

        let Mods { ctrl, alt, shift: _ } = mods;

        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                ListerKeyDispatch::Close
            }

            Key::Named(NamedKey::Enter) => {
                ListerKeyDispatch::Selected
            }

            Key::Named(NamedKey::ArrowDown) => {
                if self.selected_index + 1 < self.filtered.len() {
                    self.selected_index += 1;
                    self.scroll_to_selected();
                }
                ListerKeyDispatch::Other
            }

            Key::Named(NamedKey::ArrowUp) => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                    self.scroll_to_selected();
                }
                ListerKeyDispatch::Other
            }

            Key::Character(s) if ctrl => match s.chars().next() {
                Some('n') => {
                    if self.selected_index + 1 < self.filtered.len() {
                        self.selected_index += 1;
                        self.scroll_to_selected();
                    }

                    ListerKeyDispatch::Other
                }

                Some('p') => {
                    if self.selected_index > 0 {
                        self.selected_index -= 1;
                        self.scroll_to_selected();
                    }

                    ListerKeyDispatch::Other
                }

                Some('v') => {
                    let page = (self.list_h / self.item_h) as usize;
                    self.selected_index = (self.selected_index + page).min(self.filtered.len().saturating_sub(1));
                    self.scroll_to_selected();
                    ListerKeyDispatch::Other
                }

                Some('g') => ListerKeyDispatch::Close,

                _ => ListerKeyDispatch::None
            }

            Key::Character(s) if alt => match s.chars().next() {
                Some('v') => {
                    let page = (self.list_h / self.item_h) as usize;
                    self.selected_index = self.selected_index.saturating_sub(page);
                    self.scroll_to_selected();
                    ListerKeyDispatch::Other
                }
                _ => ListerKeyDispatch::None
            }

            _ => ListerKeyDispatch::None
        }
    }

    fn scroll_to_selected(&mut self) {
        let top    = self.selected_index as f32 * self.item_h;
        let bottom = top + self.item_h;

        // Only scroll when item is fully outside the visible area
        if top < self.scroll {
            self.scroll = top;
        }
        if bottom > self.scroll + self.list_h {
            self.scroll = bottom - self.list_h;
        }
    }
}

pub struct Editor {
    // Storage
    pub buffers: PrimaryMap<BufferId, Buffer>,
    pub views:   PrimaryMap<ViewId,   View>,
    pub panels:  PrimaryMap<PanelId,  Panel>,

    // Which panel is active (receives keyboard input)
    pub active_panel:  PanelId,

    // Root panel id - its rect always equals the window
    pub root_panel:    PanelId,

    pub scratch_paren: Vec<char>,

    pub most_recently_used_buffers:  VecDeque<BufferId>,
    pub buffer_cycle_index:          Option<usize>,

    pub panel_before_opening_lister: Option<PanelId>,

    // Scale for font/line-height
    pub scale: f32,

    // Cursor blink
    pub blink_epoch:         Instant,
    pub last_cursor_visible: bool,

    // Mouse
    pub mouse_pos:          (f32, f32),
    pub mouse_left_pressed: bool,
    pub is_cursor_visible:  bool,

    lister_query_buffer: BufferId,
    lister_query_view:   ViewId,
    lister_query_panel:  PanelId, // @Redundant?
    lister_split_panel:  PanelId,

    pub canonicalized_current_working_directory: SmallString<[u8; 64]>,
    pub canonicalized_last_scanned_directory:    SmallString<[u8; 64]>,

    pub last_input_time: Instant,

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

    pub lister:          Lister,
    pub director:        Director
}

impl Editor {
    pub fn new(buffer: Buffer) -> Self {
        let mut buffers = PrimaryMap::with_capacity(32);
        let mut views   = PrimaryMap::with_capacity(32);
        let mut panels  = PrimaryMap::with_capacity(32);

        let root_buffer = buffers.push(buffer);
        let root_view   = views.next_key();  views.push(View::new(root_view, root_buffer));
        let root_panel  = panels.next_key(); panels.push(Panel {
            id:   root_panel,
            rect: Rect::default(),  // Set on first resize / resumed
            kind: PanelKind::Leaf { view_id: root_view },
        });

        let lister_query_buffer = buffers.push(Buffer::default());
        let lister_query_view   = views.next_key();  views.push(View::new(lister_query_view, lister_query_buffer));
        let lister_query_panel  = panels.next_key(); panels.push(Panel {
            id:   lister_query_panel,
            rect: Rect::default(),  // Set on first resize / resumed
            kind: PanelKind::Leaf { view_id: lister_query_view },
        });

        let lister_split_panel = panels.next_key();
        panels.push(Panel {
            id: lister_split_panel,
            rect: Rect::default(),  // Set on first resize / resumed
            kind: PanelKind::ListerSplit
        });

        let canonicalized_current_working_directory: SmallString<[_; _]> = std::env::args().nth(1)
            .and_then(|p| Path::new(&p).parent().map(|p| p.to_path_buf()))
            .and_then(|p| std::fs::canonicalize(p).ok())
            .unwrap_or_else(|| std::fs::canonicalize(".").unwrap_or_default())
            .into_os_string()
            .into_string()
            .unwrap()
            .into();

        let mut editor = Self {
            buffers,
            views,
            panels,
            lister_split_panel,
            lister: Lister::new(),
            last_input_time: Instant::now(),
            last_cursor_visible: false,
            is_cursor_visible: true,
            buffer_cycle_index: None,
            scratch_paren: Vec::with_capacity(256),
            active_panel: root_panel,
            root_panel,
            panel_before_opening_lister: None,
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
            canonicalized_last_scanned_directory: "".into(),
            canonicalized_current_working_directory,
            most_recently_used_buffers: VecDeque::with_capacity(32),
            lister_query_panel,
            lister_query_view,
            lister_query_buffer,
            director: Director::new()
        };

        editor.mru_register_new_buffer(root_buffer);
        editor
    }

    pub fn hide_cursor(&mut self, win: &Window) {
        if !self.is_cursor_visible { return }

        self.is_cursor_visible = false;
        win.set_cursor_visible(false);
    }

    pub fn show_cursor(&mut self, win: &Window) {
        if self.is_cursor_visible { return }

        self.is_cursor_visible = true;
        win.set_cursor_visible(true);
    }

    pub fn is_lister_open_and_is_it_listing_file_entries(&self) -> bool {
        self.lister.is_open && self.lister.is_listing_file_entries
    }

    pub fn is_lister_buffer_dirty(&self) -> bool {
        self.buffers[self.lister_query_buffer].dirty
    }

    pub fn open_lister(&mut self, items: Vec<ListerItem>, on_confirm: ListerSelectFn) {
        self.open_lister_impl(items, on_confirm, None)
    }

    pub fn open_lister_with_frame_callback(&mut self, items: Vec<ListerItem>, on_confirm: ListerSelectFn, frame_callback: ListerFrameUpdateCallback) {
        self.open_lister_impl(items, on_confirm, Some(frame_callback))
    }

    pub fn open_lister_impl(&mut self, items: Vec<ListerItem>, on_confirm: ListerSelectFn, frame_callback: Option<ListerFrameUpdateCallback>) {
        clear_buffer(self, self.lister_query_buffer);

        self.set_active_panel(self.lister_split_panel);

        self.lister.items_update_frame_update_callback.push(frame_callback);
        self.lister.on_confirm.push(on_confirm);

        self.lister.query.clear();
        self.lister.filtered.clear();
        self.lister.query_dirty     = true;
        self.canonicalized_last_scanned_directory = SmallString::new();
        self.lister.selected_index  = if self.lister.set_selected_index_to_1_instead_of_0 {
            (items.len() > 1) as usize
        } else {
            0
        };
        self.lister.scroll          = 0.0;
        self.lister.scroll_anim     = 0.0;
        self.lister.is_open         = true;
        self.lister.items           = items;

        self.lister.rebuild_filtered();
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
            PanelKind::ListerSplit => self.lister_query_view,
            PanelKind::Split(_) => VIEW_MAIN,
        }
    }

    pub fn active_view(&self) -> &View { &self.views[self.active_view_id()] }
    pub fn active_view_mut(&mut self) -> &mut View { let id = self.active_view_id(); &mut self.views[id] }

    pub fn panel_of_view(&self, view_id: ViewId) -> PanelId { // @Speed
        let mut leaf_panels = Default::default();
        collect_leaves(self, self.root_panel, &mut leaf_panels);

        for (panel, view, _) in leaf_panels {
            if view == view_id {
                return panel;
            }
        }

        unreachable!()
    }

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
        self.layout_panel(self.lister_query_panel, lister_rect(win_rect.w, win_rect.h));
        self.layout_panel(self.lister_split_panel, lister_rect(win_rect.w, win_rect.h));
    }

    fn layout_panel(&mut self, id: PanelId, rect: Rect) {
        self.panels[id].rect = rect;

        // Collect split info without holding borrow
        let split = match self.panels[id].kind {
            PanelKind::Split(s) => s,
            PanelKind::ListerSplit => return,
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
    pub fn split_active(&mut self, vertical: bool, ratio: f32, win_rect: Rect) {
        self.split_active_no_layout(vertical, ratio);
        self.layout_panels(win_rect);
    }

    /// Split the active panel.  Creates two new panels + one new view
    /// (the new panel gets a new view into the same buffer).
    pub fn split_active_no_layout(&mut self, vertical: bool, ratio: f32) {
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
        self.set_active_panel(left_id);
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

        self.set_active_panel(parent);
    }

    pub fn toggle_active_panel(&mut self) {
        let mut leaves = Default::default();
        collect_leaves(self, self.root_panel, &mut leaves);

        if leaves.len() <= 1 {
            return;
        }

        let current_pos = leaves.iter().position(|(id, _, _)| *id == self.active_panel).unwrap_or(0);
        let next = (current_pos + 1) % leaves.len();
        let (to_switch_to, _, _) = leaves[next];

        let (from_x, from_y) = {
            let view = self.active_view();
            (view.cursor_anim_x, view.cursor_anim_y)
        };

        self.set_active_panel(to_switch_to);

        let active_view = self.active_view_mut();
        active_view.cursor_anim_x = from_x;
        active_view.cursor_anim_y = from_y;
    }

    pub fn next_buffer(&mut self) -> BufferId {
        let len = self.most_recently_used_buffers.len();
        if len <= 1 { return self.active_view().buffer_id; }

        let idx = self.buffer_cycle_index.get_or_insert(0);
        *idx = (*idx + 1) % len;

        self.most_recently_used_buffers[*idx]
    }

    pub fn previous_buffer(&mut self) -> BufferId {
        let len = self.most_recently_used_buffers.len();
        if len <= 1 { return self.active_view().buffer_id; }

        let idx = self.buffer_cycle_index.get_or_insert(0);
        *idx = if *idx == 0 { len - 1 } else { *idx - 1 };

        self.most_recently_used_buffers[*idx]
    }

    pub fn commit_buffer_cycle(&mut self) {
        let Some(idx) = self.buffer_cycle_index.take() else { return };

        let buf = self.most_recently_used_buffers[idx];
        self.most_recently_used_buffers.retain(|&b| b != buf);
        self.most_recently_used_buffers.insert(0, buf);
    }

    // @Refactor
    pub fn mru_register_new_buffer(&mut self, buffer_id: BufferId) {
        if let Some(pos) = self.most_recently_used_buffers.iter().position(|&b| b == buffer_id) {
            self.most_recently_used_buffers.remove(pos);
        }
        // Insert at 1, not 0 - current buffer stays at front, new buffer is "next"
        let insert_at = 1.min(self.most_recently_used_buffers.len());
        self.most_recently_used_buffers.insert(insert_at, buffer_id);
    }

    pub fn mru_focus(&mut self, id: BufferId) {
        if self.buffer_cycle_index.is_some() { return; }
        if let Some(pos) = self.most_recently_used_buffers.iter().position(|&x| x == id) {
            self.most_recently_used_buffers.remove(pos);
        }
        self.most_recently_used_buffers.insert(0, id);
    }

    pub fn set_active_panel(&mut self, panel_id: PanelId) {
        if panel_id == self.lister_split_panel {
            self.panel_before_opening_lister = Some(self.active_panel);
        }
        self.active_panel = panel_id;
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

    pub fn cursor_visible(&self) -> bool {
        cursor_visible(&self.blink_epoch, &self.last_input_time)
    }

    pub fn reset_blink(&mut self) {
        self.blink_epoch     = Instant::now();
        self.last_input_time = Instant::now();
    }

    pub fn panel_at(&self, px: f32, py: f32) -> Option<PanelId> {
        let mut leaves = Default::default();
        collect_leaves(self, self.root_panel, &mut leaves);

        for (panel_id, ..) in leaves {
            if self.panels[panel_id].rect.contains(px, py) {
                return Some(panel_id)
            }
        }

        None
    }
}

fn animate(editor: &mut Editor, dt: f32) -> bool {
    let _tracy = tracy::span!("animate");

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

    let ds = editor.lister.scroll - editor.lister.scroll_anim;
    if ds.abs() > epsilon {
        editor.lister.scroll_anim += ds * (1.0 - (-SCROLL_ANIM_RATE * dt).exp());
        animating = true;
    } else {
        editor.lister.scroll_anim = editor.lister.scroll;
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

pub fn char_at_line_col(buffer: &Buffer, line: u32, col: u32) -> Option<char> {
    let line_text = buffer.text.line(line as usize);
    line_text.chars().nth(col as usize)
}

pub fn collect_leaves(editor: &Editor, id: PanelId, out: &mut SmallVec<[(PanelId, ViewId, Rect); 16]>) {
    let panels = &editor.panels;

    let mut stack = SmallVec::<[_; 48]>::with_capacity((panels.len() as f32 * 1.5) as usize);
    if editor.lister.is_open {
        stack.push(editor.lister_split_panel);
    }

    stack.push(id);

    while let Some(id) = stack.pop() {
        match panels[id].kind {
            PanelKind::Leaf { view_id } => out.push((id, view_id, panels[id].rect)),
            PanelKind::ListerSplit  => out.push((id, editor.lister_query_view, panels[editor.lister_query_panel].rect)),
            PanelKind::Split(split) => {
                stack.push(split.right_id);
                stack.push(split.left_id);
            }
        }
    }
}

fn apply_scale(editor: &mut Editor, new_scale: f32, anchor_my: Option<f32>) {
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

fn rescale(editor: &mut Editor, gpu: &mut Gpu, new_scale: f32) {
    let new = new_scale.clamp(MIN_SCALE, MAX_SCALE);
    if new != editor.scale {
        reset_atlas(gpu);
        apply_scale(editor, new, None);
        force_layouts_from_all_views_to_rebuild(editor);
    }
}

fn force_layouts_from_all_views_to_rebuild(editor: &mut Editor) {
    for view in editor.views.values_mut() {
        view.layout = None;
    }
}

fn scroll_page(editor: &mut Editor, _gpu: &Gpu, direction: i32) {
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

const fn lister_rect(win_w: f32, win_h: f32) -> Rect {
    let panel_w = (win_w * 0.45).clamp(320.0, 720.0);
    let panel_h = (win_h * 0.65).clamp(200.0, 600.0);

    Rect {
        x: ((win_w - panel_w) * 0.50).round(),
        y: ((win_h - panel_h) * 0.40).round(),
        w: panel_w,
        h: panel_h,
    }
}

fn editor_dispatch_lister_confirm(cx: &mut CommandContext) {
    let index = cx.editor.lister.selected_index;
    let index = cx.editor.lister.filtered[index];
    let item_data = cx.editor.lister.items[index].data;

    let on_confirm = cx.editor.lister.on_confirm.pop().unwrap();
    cx.editor.lister.pending_datas.push(item_data);
    _ = cx.editor.lister.items_update_frame_update_callback.pop();
    cx.editor.lister.set_selected_index_to_1_instead_of_0 = false;
    on_confirm(cx, item_data);
}

fn handle_left_mouse_click(editor: &mut Editor, gpu: &mut Gpu, command_table: &CommandTable) -> bool {
    if editor.lister.is_open {
        let lister = lister_rect(gpu.win_w, gpu.win_h);
        let (mx, my) = editor.mouse_pos;
        if lister.contains(mx, my) {
            let line_h   = editor.line_h();
            let scale    = editor.scale;
            let pad      = (8.0 * scale).round();
            let item_h   = editor.lister.item_h;
            let input_h  = (line_h + pad).round();
            let sep      = scale.max(1.0);
            let list_y   = lister.y + input_h + sep;

            if my >= list_y {
                let local_y = my - list_y + editor.lister.scroll_anim;
                let clicked = (local_y / item_h) as usize;
                if clicked < editor.lister.filtered.len() {
                    editor.lister.selected_index = clicked;
                    editor.lister.is_open = false;
                    editor.lister.is_listing_file_entries = true;

                    let panel = editor.panel_before_opening_lister.take().unwrap();
                    editor.set_active_panel(panel);
                    editor.reset_blink();

                    editor_dispatch_lister_confirm(&mut CommandContext { editor, gpu, command_table, event: None });
                }
            }

            return true;
        } else {
            // Click outside lister closes it
            editor.lister.is_open = false;
            editor.lister.is_listing_file_entries = true;

            let panel = editor.panel_before_opening_lister.take().unwrap();
            editor.set_active_panel(panel);
            return true;
        }
    }

    let (mx, my) = editor.mouse_pos;
    let pid = editor.panel_at(mx, my).unwrap_or(editor.active_panel);

    let PanelKind::Leaf { view_id } = editor.panels[pid].kind else {
        return false;
    };

    let rect        = editor.panels[pid].rect;
    let buf_id      = editor.views[view_id].buffer_id;
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

    if editor.mouse_left_pressed {
        if !view.cursor.is_anchor_set() {
            view.cursor.set_anchor();
        }
    } else {
        view.cursor.unset_anchor();
    }

    editor.set_active_panel(pid);
    editor.reset_blink();

    true
}

pub fn snap_cursor_to_target_point_in_active_view(editor: &mut Editor) {
    let view_id  = editor.active_view_id();
    let panel_id = editor.active_panel;
    let rect     = editor.panels[panel_id].rect;

    let (view, buf) = editor.active_view_and_buffer();
    let (line, col) = buf.cursor_line_col(&view.cursor);

    editor.snap_cursor_to_target(view_id, line, col, rect);
}

pub fn scroll_to_cursor(editor: &mut Editor) {
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

pub fn clear_buffer(editor: &mut Editor, buffer: BufferId) {
    editor.buffers[buffer].clear();
}

pub fn open_buffer_from_path_in(editor: &mut Editor, view: ViewId, path: impl Into<Box<Path>>) {
    let Ok(buffer) = Buffer::from_file(path) else { return };

    let buffer_id = editor.buffers.push(buffer);
    editor.mru_register_new_buffer(buffer_id);

    editor.views[view].switch_buffer(buffer_id);
    editor.mru_focus(buffer_id); // @Refactor
}

pub fn does_panel_need_rebuild(
    editor: &Editor,

    view: ViewId, buffer: BufferId,

    rect: Rect,

    font_size: f32, line_h: f32
) -> bool {
    let screen_lines = (rect.h / line_h) as u32;
    let anim_first = (editor.views[view].scroll_anim / line_h) as u32;
    editor.buffers[buffer].dirty || editor.views[view].layout.as_ref().map(|l| {
        anim_first < l.first_buffer_line
            || (
                anim_first + screen_lines > l.first_buffer_line + l.lines.len() as u32
                    && screen_lines < l.lines.len() as u32
            )
            || (l.rect.w - rect.w).abs() > 0.5
            || (l.rect.h - rect.h).abs() > 0.5
            || (l.font_size - font_size).abs() > 0.01
    }).unwrap_or(true)
}

#[derive(Default)]
struct App {
    gpu:    Option<Gpu>,
    editor: Option<Editor>,
    window: Option<Arc<Window>>,
    mods:   winit::event::Modifiers,

    is_our_window_focused: bool,

    command_table: CommandTable,
    keymap: Keymap,
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

        let path   = std::env::args().nth(1).expect("usage: naysayer <file>");
        let buffer = Buffer::from_file(path.as_ref()).expect("failed to open file");

        let mut editor = Editor::new(buffer);
        editor.layout_panels(Rect::full(w as f32, h as f32));
        editor.director.kick_scan(PathBuf::from("."), false);

        let mut gpu = gpu::init(Arc::clone(&win));
        gpu.verts_mut().reserve(INITIAL_VERTEX_BUFFER_CAPACITY as _);

        prewarm_glyphs_and_print_preallocation_memory_usage(&editor, &mut gpu);

        self.gpu    = Some(gpu);
        self.editor = Some(editor);
        self.window = Some(win);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let Some(editor) = &self.editor else { return };
        let Some(win)    = &self.window else { return };

        let since_input = editor.last_input_time.elapsed().as_millis();

        if since_input < BLINK_START_DELAY_MS {
            // Waiting to start blinking - wake up when delay expires
            let ms_until = BLINK_START_DELAY_MS - since_input;
            el.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + std::time::Duration::from_millis(ms_until as u64)
            ));
        } else if since_input > BLINK_STOP_IDLE_MS {
            // Idle too long - just wait for input
            el.set_control_flow(ControlFlow::Wait);
        } else {
            // Actively blinking - wake up at next blink transition
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
            win.request_redraw();
        }
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
                    event: $event, command_table: &self.command_table,
                }
            };
        }

        match event {
            WindowEvent::CloseRequested => el.exit(),

            WindowEvent::ModifiersChanged(m) => self.mods = m,

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }

                editor.hide_cursor(win);

                let mods = Mods { alt, ctrl, shift };

                let is_active_view_query = editor.active_view_id() == editor.lister_query_view;
                if is_active_view_query {
                    let result = editor.lister.lister_key(&event, mods);
                    let is_selected = matches!(result, ListerKeyDispatch::Selected);
                    match result {
                        ListerKeyDispatch::Selected | ListerKeyDispatch::Close => {
                            editor.lister.is_open = false;
                            editor.lister.is_listing_file_entries = true;

                            let panel_before_opening_lister = editor.panel_before_opening_lister.take().unwrap();
                            editor.set_active_panel(panel_before_opening_lister);

                            if is_selected {
                                let mut cx = make_command_context!(Some(&event));
                                editor_dispatch_lister_confirm(&mut cx);
                            }

                            win.request_redraw();

                            return;
                        }

                        ListerKeyDispatch::Other => {
                            editor.reset_blink();
                            win.request_redraw();
                            return;
                        }

                        ListerKeyDispatch::None => {}
                    }
                }

                if let Some(command_name) = self.keymap.lookup(&event, mods) {
                    let Some(command) = self.command_table.get(&command_name) else {
                        return;
                    };

                    // Commit cycle if switching to a non-cycle command
                    if !matches!(command_name, CommandAtom("switch_buffer") | CommandAtom("cycle_buffers_left") | CommandAtom("cycle_buffers_right")) {
                        editor.commit_buffer_cycle();
                    }

                    {
                        let mut cx = make_command_context!(Some(&event));
                        (command.func)(&mut cx);
                    }

                    if editor.is_lister_buffer_dirty() {
                        //
                        // Keep lister query updated
                        //

                        let query = editor.buffers[editor.lister_query_buffer].text.chars();
                        editor.lister.query.clear();
                        editor.lister.query.extend(query);
                        editor.lister.scroll = 0.0;
                        editor.lister.selected_index = if editor.lister.set_selected_index_to_1_instead_of_0 {
                            (editor.lister.items.len() > 1) as usize
                        } else {
                            0
                        };
                        editor.lister.query_dirty = true;
                        editor.lister.rebuild_filtered();
                    }

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

                editor.show_cursor(win);

                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * editor.line_h(),
                    MouseScrollDelta::PixelDelta(p)   => p.y as f32,
                };

                if editor.lister.is_open { // @Refactor
                    //
                    // Lister scroll takes priority if open and mouse is over it
                    //

                    let lister = lister_rect(gpu.win_w, gpu.win_h);
                    let (mx, my) = editor.mouse_pos;
                    if lister.contains(mx, my) {
                        let max_scroll = (
                            editor.lister.filtered.len() as f32 * editor.lister.item_h
                                + editor.lister.item_h * 2.0 - editor.lister.list_h
                        ).max(0.0);

                        editor.lister.scroll = (editor.lister.scroll - dy * 2.0).clamp(0.0, max_scroll);

                        //
                        // Update hovered index for new scroll position
                        //
                        let line_h  = editor.line_h();
                        let scale   = editor.scale;
                        let pad     = (8.0 * scale).round();
                        let input_h = (line_h + pad).round();
                        let sep     = scale.max(1.0);
                        let list_y  = lister.y + input_h + sep;
                        if my >= list_y {
                            let local_y = my - list_y + editor.lister.scroll;  // Use new scroll, not anim
                            let hovered = (local_y / editor.lister.item_h) as usize;
                            editor.lister.hovered_index = if hovered < editor.lister.filtered.len() {
                                Some(hovered)
                            } else {
                                None
                            };
                        }

                        win.request_redraw();
                        return;
                    }
                }

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
                    cur_line  // Still visible, don't move
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
                editor.show_cursor(win);

                editor.mouse_left_pressed = false;
            }

            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                editor.show_cursor(win);

                if handle_left_mouse_click(editor, gpu, &self.command_table) {
                    win.request_redraw();
                }

                editor.mouse_left_pressed = true;
            }

            WindowEvent::CursorMoved { position, .. } => {
                editor.show_cursor(win);

                editor.mouse_pos = (position.x as f32, position.y as f32);

                if editor.lister.is_open { // @Refactor
                    let lister = lister_rect(gpu.win_w, gpu.win_h);
                    let (mx, my) = editor.mouse_pos;
                    let line_h  = editor.line_h();
                    let scale   = editor.scale;
                    let pad     = (8.0 * scale).round();
                    let item_h  = editor.lister.item_h;
                    let input_h = (line_h + pad).round();
                    let sep     = scale.max(1.0);
                    let list_y  = lister.y + input_h + sep;

                    if lister.contains(mx, my) && my >= list_y {
                        let local_y = my - list_y + editor.lister.scroll_anim;
                        let hovered = (local_y / item_h) as usize;
                        editor.lister.hovered_index = if hovered < editor.lister.filtered.len() {
                            Some(hovered)
                        } else {
                            None
                        };
                        win.request_redraw();
                    } else {
                        editor.lister.hovered_index = None;
                    }

                    win.request_redraw();
                }

                if editor.mouse_left_pressed {
                    if handle_left_mouse_click(editor, gpu, &self.command_table) {
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

                // Poll directory cache results
                editor.director.poll();

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
                // Ensure vertex buffer has enough capacity
                //
                {
                    let verts = gpu.verts_mut();
                    verts.clear();

                    let estimated = editor.views
                        .values()
                        .filter_map(|v| v.layout.as_ref())
                        .map(|l| l.visible_glyph_count)
                        .sum::<u32>();

                    let reserve = estimated * 6 + 4096;

                    let _old_capacity = verts.capacity();
                    verts.reserve(reserve as usize);

                    #[cfg(debug_assertions)] {
                        let _new_capacity = verts.capacity();
                        if _old_capacity != _new_capacity {
                            eprintln!(
                                "[vertex buffer grew {} -> {} new_capacity: {}]",
                                util::format_bytes(_old_capacity*size_of::<gpu::Vert>()),
                                util::format_bytes(_new_capacity*size_of::<gpu::Vert>()),
                                _new_capacity
                            );
                        }
                    }
                }

                let still_animating = animate(editor, dt);

                let font_size    = editor.font_size();
                let line_h       = editor.line_h();
                let show_cursor  = editor.cursor_visible();
                let active_panel = editor.active_panel;

                let mut leaf_panels = Default::default();
                collect_leaves(editor, editor.root_panel, &mut leaf_panels);

                let mut should_request_redraw = false;
                should_request_redraw |= still_animating;

                if let Some(Some(callback)) = editor.lister.items_update_frame_update_callback.last().copied() {
                    let mut cx = make_command_context!(None);
                    should_request_redraw |= callback(&mut cx);
                }

                for &(panel_id, view_id, rect) in &leaf_panels {
                    if view_id == editor.lister_query_view {
                        // Lister buffer is drawn below
                        continue;
                    }

                    let buffer_id = editor.views[view_id].buffer_id;

                    let is_dirty = does_panel_need_rebuild(editor, view_id, buffer_id, rect, font_size, line_h);

                    should_request_redraw |= is_dirty;

                    if is_dirty {
                        rebuild_text_layout(editor, gpu, view_id, rect, font_size, line_h);
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
                    gpu::push_clip(gpu, rect.x, rect.y, rect.w, rect.h);
                    let t1 = Instant::now();
                    render_text_layout(
                        gpu, layout,
                        &editor.buffers[buffer_id],
                        &editor.views[view_id],
                        editor.active_view_id(),
                        editor.lister_query_view,
                        editor.scale,
                        show_cursor,
                        self.is_our_window_focused,
                        &mut editor.scratch_paren,
                    );
                    editor.render_us_acc += t1.elapsed().as_micros() as f32;
                    gpu::pop_clip(gpu);
                }

                if editor.lister.is_open {
                    //
                    // Prepare lister bg
                    //

                    if active_panel == editor.lister_split_panel {
                        let lister = lister_rect(gpu.win_w, gpu.win_h);
                        render_lister_background_frosted(gpu, lister, editor.scale);
                    }

                    render_lister_background(gpu, editor);
                }

                if editor.lister.is_open {
                    // @Cutnpaste from above

                    //
                    // Render lister query buffer
                    //

                    let view_id = editor.lister_query_view;
                    let panel_id = editor.lister_query_panel;
                    let rect = editor.panels[editor.lister_query_panel].rect;
                    let buffer_id = editor.views[view_id].buffer_id;

                    let is_dirty = does_panel_need_rebuild(editor, view_id, buffer_id, rect, font_size, line_h);

                    should_request_redraw |= is_dirty;

                    if is_dirty {
                        rebuild_text_layout(editor, gpu, view_id, rect, font_size, line_h);
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
                    gpu::push_clip(gpu, rect.x, rect.y, rect.w, rect.h);
                    let t1 = Instant::now();
                    render_text_layout(
                        gpu, layout,
                        &editor.buffers[buffer_id],
                        &editor.views[view_id],
                        editor.active_view_id(),
                        editor.lister_query_view,
                        editor.scale,
                        show_cursor,
                        self.is_our_window_focused,
                        &mut editor.scratch_paren,
                    );
                    editor.render_us_acc += t1.elapsed().as_micros() as f32;
                    gpu::pop_clip(gpu);
                }

                render_lister_foreground(gpu, editor);

                draw_metrics(editor, gpu);

                _ = gpu::submit_frame(gpu);

                let new_cursor_visible = editor.cursor_visible();
                let blink_changed = new_cursor_visible != editor.last_cursor_visible;
                editor.last_cursor_visible = new_cursor_visible;

                should_request_redraw |= blink_changed;

                if should_request_redraw {
                    win.request_redraw();
                } else {
                    self.about_to_wait(el);
                }
            }

            WindowEvent::Focused(is_focused) => {
                self.is_our_window_focused = is_focused;
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
