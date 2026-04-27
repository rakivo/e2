#![feature(likely_unlikely)]

// TODO: Paste animation continuation threshold

// TODO: Talk to system clipboard

// TODO: Mouse double left click should select the word

// TODO: Multi-cursors

// TODO: Undo+redo
// TODO: mark-sexp
// TODO: move-line
// TODO: backward-list/forward-list
// TODO: backward-list/forward-list
// TODO: beginning-of-defun/end-of-defun
// TODO: align-rexegp

// TODO: Auto-indentation (minor)
// TODO: Automatic session save

// TODO: [messages] buffer

// TODO: Lexer support for HERE strings
// TODO: Lexer support for raw  strings

// TODO: Lexing is buggy with large strings
// For instance: if we only see the closing quote and the opening quote is off the screen,
// currently it just highlights as if the closing quote was an opening one.
// `
// But the actual reliable solution to this would involve going back/forward into the file,
// searching for a matching quote with a "state machine".

// TODO: Make Buffer have distinct CANONICALIZED/RELATIVE path fields

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod gpu;
mod util;
mod color;
mod buffer;
mod command;
mod tracy;
mod director;
mod lexer;
mod session;
mod audioer;
mod messager;

use audioer::Audioer;
use lexer::token_color;
use messager::{MAX_MESSAGE_COUNT, MESSAGE_DURATION_IN_MILLISECONDS, MESSAGER_FONT_SIZE, Messager};
use util::format_bytes;
use session::{apply_session, default_session_path, load_session, pretty_path, save_session};
use buffer::{AnimatedInsertion, Buffer, Cursor};
use color::{Color, GpuColor};
use command::{CommandAtom, CommandContext, CommandTable, Keymap, Mods};
use director::Director;

use std::io::{BufWriter, Write};
use std::num::NonZero;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::fmt::Write as _;
use std::collections::VecDeque;

use cranelift_entity::packed_option::ReservedValue;
use cranelift_entity::{EntityRef, PrimaryMap};
use memmap2::MmapOptions;
use smallstr::SmallString;
use smallvec::SmallVec;
use wgpu::naga::FastHashMap;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use gpu::{ATLAS_SIZE, Gpu, GpuGlyph, INITIAL_VERTEX_BUFFER_CAPACITY, draw_text_for_editor, prewarm_glyphs, reset_atlas};

#[cfg(debug_assertions)]
fn vec_element_size<T>(_: &Vec<T>) -> usize {
    size_of::<T>()
}

#[cfg(debug_assertions)]
macro_rules! checked_reserve {
    ($vec:expr, $n:expr, $name:expr) => {
        {
            let _old_cap = $vec.capacity();
            $vec.reserve($n);
            let _new_cap = $vec.capacity();
            if _new_cap != _old_cap {
                let _elem = vec_element_size(&$vec);
                eprintln!(
                    "[{} reallocated]: {} -> {}",
                    $name,
                    util::format_bytes(_old_cap * _elem),
                    util::format_bytes(_new_cap * _elem),
                );
            }
        }
    };
    ($vec:expr, $n:expr) => {
        checked_reserve!($vec, $n, stringify!($vec))
    };
}
#[cfg(not(debug_assertions))]
macro_rules! checked_reserve {
    ($vec:expr, $n:expr, $name:expr) => { $vec.reserve($n); };
    ($vec:expr, $n:expr) => { $vec.reserve($n); };
}

#[cfg(debug_assertions)]
macro_rules! checked_push {
    ($vec:expr, $val:expr, $name:expr) => {
        {
            let cap_before = $vec.capacity();
            $vec.push($val);
            if $vec.capacity() != cap_before {
                eprintln!(
                    "[{} reallocated]: {} -> {}",
                    $name, cap_before, $vec.capacity()
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

    prewarm_glyphs(gpu, MESSAGER_FONT_SIZE);

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

fn draw_metrics(editor: &Editor, gpu: &mut Gpu, refresh_rate_millihertz: u32) {
    const BUILD_GOOD:  f32 = 200.0;
    const BUILD_SLOW:  f32 = 800.0;

    const RENDER_GOOD: f32 = 300.0;
    const RENDER_SLOW: f32 = 1500.0;

    const RELEX_GOOD:  f32 = 100.0;
    const RELEX_SLOW:  f32 = 500.0;

    const HUD_LINE_H: f32 = 14.0;
    const PAD_RIGHT: f32 = 150.0;

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

    let frame_budget_us = 1_000_000_000.0 / refresh_rate_millihertz as f32; // us per frame
    let frame_good = frame_budget_us * 0.5;  // good = under 50% of the budget
    let frame_slow = frame_budget_us * 0.8;  // slow = over  80% of the budget

    let fps    = editor.fps;
    let build  = editor.build_us;
    let relex  = editor.relex_us;
    let render = editor.render_us;
    let frame  = build + relex + render;

    let hud_y   = gpu.win_h - HUD_LINE_H * 5.0 - 10.0;
    let x_right = (gpu.win_w - PAD_RIGHT).clamp(0.0, f32::MAX);

    let mut buf = SmallString::<[u8; 64]>::new();

    buf.clear();
    _ = write!(&mut buf, "fps: {:.0}", fps);
    gpu::draw_text(
        gpu,
        &buf,
        x_right,
        hud_y + 0.0 * HUD_LINE_H,
        12.0,
        Color::rgba(180, 180, 180, 255),
    );

    buf.clear();
    _ = write!(&mut buf, "build: {:.2}us", build);
    gpu::draw_text(
        gpu,
        &buf,
        x_right,
        hud_y + 1.0 * HUD_LINE_H,
        12.0,
        heat(build, BUILD_GOOD, BUILD_SLOW),
    );

    buf.clear();
    _ = write!(&mut buf, "relex: {:.2}us", relex);
    gpu::draw_text(
        gpu,
        &buf,
        x_right,
        hud_y + 2.0 * HUD_LINE_H,
        12.0,
        heat(relex, RELEX_GOOD, RELEX_SLOW),
    );

    buf.clear();
    _ = write!(&mut buf, "render: {:.2}us", render);
    gpu::draw_text(
        gpu,
        &buf,
        x_right,
        hud_y + 3.0 * HUD_LINE_H,
        12.0,
        heat(render, RENDER_GOOD, RENDER_SLOW),
    );

    buf.clear();
    _ = write!(&mut buf, "frame: {:.2}us", frame);
    gpu::draw_text(
        gpu,
        &buf,
        x_right,
        hud_y + 4.0 * HUD_LINE_H,
        12.0,
        heat(frame, frame_good, frame_slow),
    );
}

pub struct Palette {
    pub bg:               Color,
    pub selection:        Color,
    pub current_line:     Color,
    pub cursor:           Color,
    pub cursor_text:      Color,
    pub paste_highlight:  Color,
    pub paren_match:      Color,
    pub delete_highlight: Color,
}

#[inline]
pub const fn palette() -> Palette {
    Palette {
        bg:               Color::hex(0x0f0b05),
        selection:        Color::hex(0x112c4f),
        cursor:           Color::hex(0xc3a983),
        current_line:     Color::hex(0x231b0e),
        cursor_text:      Color::rgba(13, 13, 13, 255),
        paren_match:      Color::rgba(190, 128, 133, 200),
        paste_highlight:  Color::hex(0xe6c86a),
        delete_highlight: Color::hex(0x8b3a1e)
    }
}

const LISTER_ITEMS_PADDING: f32 = 0.0;
const PADDING_LEFT:         f32 = 0.0;
const QUERY_BUFFER_PADDING_LEFT: f32 = 8.0;

fn padding_left(is_view_into_query_buffer: bool) -> f32 {
    if is_view_into_query_buffer {
        QUERY_BUFFER_PADDING_LEFT
    } else {
        PADDING_LEFT
    }
}

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
    pub x: f32,           // x offset from the line's left content edge (rect.x + PADDING_LEFT)

    pub byte_offset: u32, // Absolute byte offset into buffer

    pub color:     GpuColor,
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

    pub is_view_into_query_buffer: bool, // @Memory @Speed

    pub view_scroll:       f32, // view.scroll
    pub line_h:            f32,
    pub font_size:         f32,
    pub first_buffer_line: u32,

    pub rect: Rect,

    pub visible_glyph_count: u32,

    //
    // Recycled each rebuild
    //
    pub lines:        Vec<LineLayout>,
    pub glyphs:       Vec<Glyph>,
    pub line_offsets: Vec<(usize, usize)>,

    // 4 bits per glyph, packed: 0 = not animated, 1–15 = insertion index
    // fits 16 glyphs per u64, ~63 bytes per 1000 visible glyphs
    pub glyph_insertion_ids: Vec<u64>,
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
        Some(ll.x_for_col(self.rect.x + padding_left(self.is_view_into_query_buffer), col, &self.glyphs))
    }

    /// Full glyph screen rect at (buffer_line, col): [x0, y0, x1, y1].
    #[inline]
    pub fn glyph_rect(&self, buffer_line: u32, col: u32, fallback_w: f32, scroll_anim: f32) -> Option<[f32; 4]> {
        let ll = self.line_for_buffer_line(buffer_line)?;
        let y = self.rect.y
            + (ll.buffer_line - self.first_buffer_line) as f32 * self.line_h
            - (scroll_anim % self.line_h);

        let x0 = ll.x_for_col(self.rect.x + padding_left(self.is_view_into_query_buffer), col, &self.glyphs);
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
        let line_index = (line_f as u32).clamp(
            self.first_buffer_line,
            self.first_buffer_line + self.lines.len() as u32 - 1,
        );
        let vis_index  = (line_index - self.first_buffer_line) as usize;
        let ll       = &self.lines[vis_index];
        let col      = ll.col_for_screen_x(self.rect.x + padding_left(self.is_view_into_query_buffer), mx, &self.glyphs);
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
    let view = &editor.views[view_id];
    let buffer_id = view.buffer_id;

    let mut should_snap = view.layout.as_ref().map(|l| {
        (l.rect.w - rect.w).abs() > 0.5
            || (l.rect.h - rect.h).abs() > 0.5
    }).unwrap_or(true); // true = first build

    let should_snap_cursor_anim_to_buffer =
        view.cursor_anim_x.is_nan() || view.cursor_anim_y.is_nan(); // @Note: See View::new and animate()
    should_snap |= should_snap_cursor_anim_to_buffer;

    let t0 = Instant::now();
    let mut layout = build_text_layout(
        editor,
        gpu,
        view_id,
        rect, font_size, line_h,
    );
    editor.build_us_acc += t0.elapsed().as_micros() as f32;

    layout_update_currently_animated_insertions(
        &mut layout,
        &editor.buffers[buffer_id].currently_animated_insertions
    );

    editor.views[view_id].layout = Some(layout);

    if should_snap {
        let (cl, cc) = if should_snap_cursor_anim_to_buffer {
            let view = &editor.views[view_id];
            editor.buffers[view.buffer_id].cursor_line_col(&view.cursor)
        } else {
            (
                editor.views[view_id].cursor_target_line,
                editor.views[view_id].cursor_target_col,
            )
        };

        editor.snap_cursor_to_target(view_id, cl, cc, rect);

        if should_snap_cursor_anim_to_buffer {  // @Robustness
            scroll_to_cursor(editor);
        }
    }
}

/// Build the TextLayout for a single leaf panel
fn build_text_layout(
    editor: &mut Editor,
    gpu:    &mut Gpu,

    view_id: ViewId,

    rect: Rect, font_size: f32, line_h: f32,
) -> TextLayout {
    let _tracy = tracy::span!("build_text_layout");

    let old_layout = editor.views[view_id].layout.take();

    let view      = &editor.views[view_id];
    let buffer_id = view.buffer_id;

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
        last_line  = last_line.saturating_add(40);
    } else if diff < 0.0 {
        // We are scrolling UP   (target is above current anim)
        // Add 40 lines of lookahead to the top
        first_line = first_line.saturating_sub(40);
    }

    let line_count = last_line - first_line;

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
        editor.buffers[buffer_id].lex_visible(first_line as _, last_line as _); // :BufferScratch
        editor.relex_us_acc += t0.elapsed().as_micros() as f32;
    }

    let buffer = &editor.buffers[buffer_id];
    let view   = &editor.views[view_id];

    let (mut lines, mut glyphs, mut line_offsets) = if let Some(mut old) = old_layout {
        old.lines.clear();
        old.glyphs.clear();
        old.line_offsets.clear();
        (old.lines, old.glyphs, old.line_offsets)
    } else {
        Default::default()
    };

    checked_reserve!(lines,  line_count as usize);

    //
    //
    // Build line-start offset table from lex_scratch
    //
    //

    let scratch     = buffer.scratch_space_to_flatten_rope_into.as_bytes(); // :BufferScratch
    let scratch_str = &buffer.scratch_space_to_flatten_rope_into;

    let approximate_glyph_count = scratch_str.len();
    checked_reserve!(glyphs, approximate_glyph_count);  // @Tune

    //
    // line_offsets[i] = (scratch_relative_start, scratch_relative_end_excl_nl)
    //
    checked_reserve!(line_offsets, line_count as usize + 1);
    {
        let mut pos = 0usize;
        let mut collected = 0u32;

        while collected < line_count && pos <= scratch.len() {
            let remaining = &scratch[pos..];
            let (line_end_excl_nl, next_pos) = match memchr::memchr(b'\n', remaining) {
                Some(nl_rel) => (pos + nl_rel, pos + nl_rel + 1),
                None         => (scratch.len(), scratch.len()),
            };

            checked_push!(line_offsets, (pos, line_end_excl_nl));
            pos = next_pos;
            collected += 1;

            if pos >= scratch.len() { break; }
        }

        //
        // Pad with sentinel (empty) entries for lines beyond scratch content
        // (e.g. requesting past EOF). The loop below handles them gracefully.
        //
        while line_offsets.len() < line_count as usize {
            checked_push!(line_offsets, (scratch.len(), scratch.len()));
        }
    }

    //
    // @Speed:
    //
    // Instead of indirecting into the actual rope in this loop,
    // we REALLY wanna reuse the lex_scratch from Buffer,
    // since it's already pre-populated at this point because of the lex_visible call above.
    //

    let default_color = token_color(lexer::TokenKind::Default);
    let tokens        = &buffer.visible_tokens;

    // Clamp first_line to actual buffer line count before computing first_visible_byte
    let total_lines = buffer.text.len_lines() as u32;
    let first_line_clamped = first_line.min(total_lines.saturating_sub(1));
    let first_visible_byte = buffer.text.line_to_byte(first_line_clamped as usize);

    let mut visible_glyph_count = 0u32;

    let mut token_cursor = tokens.partition_point(|t| (t.start + t.len()) as usize <= first_visible_byte);

    for vis_i in 0..line_count {
        let line_index = first_line + vis_i;

        let (s_start, s_end) = line_offsets[vis_i as usize];

        // Absolute byte offset of this line's start
        let line_byte_start = first_visible_byte + s_start;

        let mut ll = LineLayout {
            buffer_line:     line_index,
            wrap_index:      0,
            width:           0.0,
            glyph_count:     0,
            glyph_start:     0,
            line_byte_start,
        };

        if s_start == s_end {
            checked_push!(lines, ll);
            continue;
        }

        // @Note: This is fine because lex_scratch is valid UTF-8 and s_start/s_end are on char boundaries
        // because memchr splits on b'\n' which is single-byte.
        let line_str = &scratch_str[s_start..s_end];

        let glyph_start = glyphs.len() as u32;

        let mut local_x  = 0.0f32;
        let mut abs_byte = line_byte_start;

        for ch in line_str.chars() {
            //
            // Advance token cursor past tokens that end before this byte.
            // Because both tokens and lines are sorted by byte offset,
            //
            while token_cursor < tokens.len()
                && (tokens[token_cursor].start + tokens[token_cursor].len()) as usize <= abs_byte
            {
                token_cursor += 1;
            }

            let color = if token_cursor < tokens.len() {
                let t = &tokens[token_cursor];
                if abs_byte >= t.start as usize && abs_byte < (t.start + t.len()) as usize {
                    token_color(t.kind())
                } else {
                    default_color
                }
            } else {
                default_color
            }.into();

            let gpu_glyph = gpu::get_glyph(gpu, ch, font_size)
                .unwrap_or_else(|| gpu::get_glyph(gpu, 'A', font_size).unwrap());

            let advance = gpu_glyph.advance;

            checked_push!(glyphs, Glyph { x: local_x, color, char: ch, gpu_glyph, byte_offset: abs_byte as _ });

            local_x  += advance;
            abs_byte += ch.len_utf8();
        }

        ll.glyph_start = glyph_start;
        ll.glyph_count = glyphs.len() as u32 - glyph_start;
        ll.width = local_x;

        visible_glyph_count += ll.glyph_count;

        checked_push!(lines, ll);
    }

    // :Metrics
    // let actual_glyph_count = glyphs.len();
    // println!("[Approximated glyph count]: {approximate_glyph_count}");
    // println!("[Actual       glyph count]: {actual_glyph_count}");

    TextLayout {
        buffer_id,
        rect,
        line_h,
        font_size,
        glyphs,
        lines,
        visible_glyph_count,
        line_offsets,
        is_view_into_query_buffer: buffer_id == editor.lister_query_buffer,
        glyph_insertion_ids: Default::default(),
        view_scroll: view.scroll,
        first_buffer_line: first_line,
    }
}

fn render_text_layout(
    gpu:         &mut Gpu,
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

    let Some(layout) = &view.layout else { return };

    let is_this_view_focused = is_our_window_focused && active_view_id == view.id;
    let is_this_view_into_query_buffer = lister_query_view_id == view.id;

    let line_y = |buffer_line: u32| -> f32 {
        layout.rect.y + buffer_line as f32 * layout.line_h - view.scroll_anim
    };

    let rect         = layout.rect;
    let line_h       = layout.line_h;
    let font_size    = layout.font_size;
    let min_cursor_w = scale_base_cursor_width(scale);
    let cursor_h     = scale_base_cursor_height(scale);
    let cursor_outline_thickness = scale_base_cursor_outline_thickness(scale);
    let padding_left = padding_left(is_this_view_into_query_buffer);
    let origin_x     = rect.x + padding_left;

    let (cursor_line, cursor_col) = buffer.cursor_line_col(&view.cursor);

    let vis_start = layout.first_buffer_line;
    let vis_end   = vis_start + layout.lines.len() as u32;

    let space_width = gpu::get_glyph(gpu, ' ', font_size)
        .map(|g| g.advance)
        .unwrap_or(min_cursor_w * 4.0);

    let min_cursor_w = min_cursor_w.max(space_width);

    //
    //
    // Selection
    //
    //

    // :FeelImprovement
    //
    // @Incomplete: Looks like selection doesn't go as much down the character,
    // as it does go up, which is really bad.
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
                    // Cover left gutter too
                    if layout.x_for_col(origin_x, start_col, ll) > rect.x {
                        gpu::draw_rect(
                            gpu,
                            rect.x, y,
                            layout.x_for_col(origin_x, start_col, ll) - rect.x,
                            line_h,
                            palette().selection
                        );
                    }

                    (layout.x_for_col(origin_x, start_col, ll), rect.x + rect.w)
                } else if line_index == end_line {
                    (rect.x,                                    layout.x_for_col(origin_x, end_col, ll))
                } else {
                    (rect.x,                                    rect.x + rect.w)
                };

                if x1 > x0 {
                    gpu::draw_rect(gpu, x0,     y, x1 - x0, line_h + cursor_h, palette().selection);
                } else {
                    gpu::draw_rect(gpu, rect.x, y, 8.0,     line_h + cursor_h, palette().selection);
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

        let y = view.cursor_anim_y + cursor_h*2.0;

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
                // Fully covered by selection, no current-line bg

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
                let y = line_y(cursor_line);
                gpu::draw_rect(gpu, x, y + cursor_h, w, line_h + cursor_h, palette().paren_match);
            }
        }

        // Matching paren
        if m_line >= vis_start && m_line < vis_end {
            if let Some(ll) = layout.line_for_buffer_line(m_line) {
                let x = layout.x_for_col(origin_x, m_col, ll);
                let w = layout.glyph_width_at_col(m_col, min_cursor_w, ll);
                let y = line_y(m_line);
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
    // Deletion animations
    //
    //
    for anim in &buffer.currently_animated_deletions {
        let alpha = ((1.0 - anim.t) * 160.0) as u8;  // Linear fade
        if alpha == 0 { continue }

        let color = Color::rgba(
            palette().delete_highlight.r,
            palette().delete_highlight.g,
            palette().delete_highlight.b,
            alpha
        );

        for line in anim.start_line..=anim.end_line {
            if line == anim.end_line && anim.end_col == 0         { continue }
            let Some(ll) = layout.line_for_buffer_line(line) else { continue };

            let full_x0 = if line == anim.start_line { layout.x_for_col(origin_x, anim.start_col, ll) } else { rect.x };
            let full_x1 = if line == anim.end_line   { layout.x_for_col(origin_x, anim.end_col,   ll) } else { rect.x + rect.w };
            let y = layout.rect.y + line as f32 * layout.line_h - view.scroll_anim;
            if full_x1 <= full_x0 { continue; }

            gpu::draw_rect(gpu, full_x0, y, full_x1 - full_x0, layout.line_h, color);

            if line == anim.start_line
                && anim.start_line != anim.end_line
                && anim.start_col > 0
                && full_x0 > rect.x + 1.0
            {
                gpu::draw_rect(gpu, rect.x, y, full_x0 - rect.x, layout.line_h, color);
            }
        }
    }

    //
    //
    // Text
    //
    //
    {
        let _tracy = tracy::span!("render_text_layout::text");

        // Hoist reciprocals - 2 divides per frame instead of 2 divides per glyph.
        let inv_sw = 1.0 / gpu.win_w;
        let inv_sh = 1.0 / gpu.win_h;

        let verts = gpu.verts_mut();

        let cursor_color = palette().cursor_text.into();
        let highlight = palette().paste_highlight.into();

        // Precompute insertion ts
        let mut insertion_ts = [1.0f32; PASTE_ANIMATION_MAX_ID + 1]; // [0] = 1.0 sentinel
        for a in buffer.currently_animated_insertions.iter() {
            insertion_ts[a.id as usize] = a.t; // a.id is 1-based, fits in [1..=PASTE_ANIMATION_MAX_ID]
        }

        for ll in &layout.lines {
            let glyphs = ll.glyphs(&layout.glyphs);
            if glyphs.is_empty() { continue; }

            let y = line_y(ll.buffer_line) + line_h;

            let cursor_col_glyph_index = if ll.buffer_line == cursor_line
                && is_this_view_focused
                && !is_this_view_into_query_buffer
                && show_cursor
            {
                Some(cursor_col as usize)
            } else {
                None
            };

            draw_text_for_editor(
                verts,
                inv_sw,
                inv_sh,
                glyphs,
                origin_x,
                y,
                cursor_col_glyph_index,
                cursor_color,
                highlight,
                &layout.glyph_insertion_ids,
                ll.glyph_start as usize,
                insertion_ts
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

fn lister_rect(win_w: f32, win_h: f32, open_anim: f32, scale: f32) -> Rect {
    let t = 1.0 - (1.0 - open_anim).powi(4);

    let panel_w = (win_w * 0.45).clamp(320.0, 720.0);
    let panel_h = (win_h * 0.65).clamp(200.0, 600.0);

    let cx = win_w * 0.50;
    let cy = (win_h - panel_h) * 0.40 + panel_h * 0.50;

    let min_w = 60.0 * scale;
    let min_h = 40.0 * scale;

    let w = (panel_w * t).max(min_w);
    let h = (panel_h * t).max(min_h);

    Rect {
        x: cx - w * 0.5,
        y: cy - h * 0.5,
        w,
        h,
    }
}

// Frosted glass approximation - layered semi-transparent rects
// with slight size variations to fake depth
fn render_lister_background_frosted(gpu: &mut Gpu, lister: Rect, scale: f32, open_anim: f32) {
    let a = |base: u8| -> u8 { ((base as f32) * open_anim) as u8 };

    // Base dark fill
    gpu::draw_rect(gpu, lister.x, lister.y, lister.w, lister.h,
        Color::rgba(12, 9, 4, a(200)));

    // Warm tint layer
    gpu::draw_rect(gpu, lister.x, lister.y, lister.w, lister.h,
        Color::rgba(40, 25, 8, a(40)));

    // Slightly inset lighter layer - gives illusion of depth/glass
    let i = scale * 1.0;
    gpu::draw_rect(gpu, lister.x + i, lister.y + i, lister.w - i*2.0, lister.h - i*2.0,
        Color::rgba(255, 200, 120, a(12)));

    // Top edge highlight - light catches the glass rim
    gpu::draw_rect(gpu, lister.x, lister.y, lister.w, scale,
        Color::rgba(255, 210, 140, a(60)));

    // Left edge highlight
    gpu::draw_rect(gpu, lister.x, lister.y, scale, lister.h,
        Color::rgba(255, 210, 140, a(30)));

    // Bottom edge shadow
    gpu::draw_rect(gpu, lister.x, lister.y + lister.h - scale, lister.w, scale,
        Color::rgba(0, 0, 0, a(80)));
}

fn render_lister_background(gpu: &mut Gpu, editor: &Editor) {
    if editor.active_panel != editor.lister_split_panel { return; }

    if !editor.lister.renderer_is_open() { return; }

    // Dim the whole screen
    gpu::draw_rect(gpu, 0.0, 0.0, gpu.win_w, gpu.win_h, Color::rgba(0, 0, 0, 100));
}

fn render_lister_foreground(gpu: &mut Gpu, editor: &mut Editor) {
    if !editor.lister.renderer_is_open() { return; }

    let open_anim = editor.lister.open_anim;
    let a = |base: u8| -> u8 { ((base as f32) * open_anim) as u8 };

    let scale     = editor.scale;
    let font_size = editor.font_size();
    let line_h    = editor.line_h();

    let lister = lister_rect(gpu.win_w, gpu.win_h, editor.lister.open_anim, editor.scale);
    let Rect { x: px, y: py, w: pw, h: ph } = lister;

    let pad     = (8.0 * scale).round();
    let item_h  = (line_h + pad).round();
    let input_h = (line_h + pad).round();
    let sep     = scale.max(1.0);
    let list_y  = py + input_h + sep;
    let list_h  = ph - input_h - sep;

    let is_mouse_cursor_hidden = editor.is_cursor_visible;

    editor.lister.item_h = item_h;
    editor.lister.list_h = list_h;

    // Outer border
    gpu::draw_rect_outline(gpu, px, py, pw, ph, sep,
                           Color::rgba(180, 140, 80, a(200)));

    // Inner border
    gpu::draw_rect_outline(gpu, px + sep, py + sep, pw - sep*2.0, ph - sep*2.0, sep,
                           Color::rgba(80, 60, 30, a(80)));

    // Separator
    gpu::draw_rect(gpu, px, py + input_h, pw, sep,
                   Color::rgba(180, 140, 80, a(160)));

    // Item count
    editor.lister.scratch_str.clear();
    _ = write!(&mut editor.lister.scratch_str, "{} results", editor.lister.filtered.len());
    let count_w = gpu::measure_str(gpu, &editor.lister.scratch_str, font_size * 0.80);
    gpu::draw_text(gpu, &editor.lister.scratch_str,
                   px + pw - pad - count_w,
                   py + input_h * 0.44 + line_h * 0.35,
                   font_size * 0.80,
                   Color::rgba(160, 120, 60, a(150)));

    // Items
    let first   = (editor.lister.scroll_anim / item_h) as usize;
    let visible = (list_h / item_h) as usize + 2;
    let frac    = editor.lister.scroll_anim % item_h;

    gpu::push_clip(gpu, px, list_y, pw, list_h);

    for slot in 0..visible {
        let index      = first + slot;
        let Some(&item_index) = editor.lister.filtered.get(index)            else { break };
        let Some(item)        = editor.lister.items.get(item_index as usize) else { break };

        let iy       = list_y + slot as f32 * item_h - frac;

        let is_selected = index == editor.lister.selected_index as usize;
        let is_hovered  = editor.lister.hovered_index == Some(index as u32);

        if iy > list_y + list_h { break; }

        // Alternating row tint  very subtle, just enough to separate rows
        if index % 2 == 0 {
            gpu::draw_rect(gpu, px, iy, pw, item_h, Color::rgba(255, 200, 100, a(8)));
        }


        if is_mouse_cursor_hidden && is_hovered && !is_selected {
            gpu::draw_rect(gpu, px + sep*2.0, iy, pw - sep*4.0, item_h, Color::rgba(60, 45, 15, a(120)));
        }

        if is_selected {
            gpu::draw_rect(gpu, px + sep*2.0, iy, pw - sep*4.0, item_h, Color::rgba(80, 55, 20, a(180)));
            gpu::draw_rect(gpu, px + sep, iy, sep * 3.0, item_h, Color::rgba(195, 169, 131, a(255)));
            gpu::draw_rect(gpu, px, iy, pw, sep, Color::rgba(180, 140, 80, a(60)));
            gpu::draw_rect(gpu, px, iy + item_h - sep, pw, sep, Color::rgba(180, 140, 80, a(60)));
        }

        let label_x = px + pad + sep * 5.0;
        let label_y = iy + item_h * 0.5 + line_h * 0.35;

        gpu::draw_text(
            gpu, &item.label, label_x, label_y, font_size,
            if is_selected { Color::rgba(240, 208, 144, a(255)) } else { Color::rgba(200, 190, 165, a(220)) }
        );

        if !item.sublabel.is_empty() {
            let sub_w = gpu::measure_str(gpu, &item.sublabel, font_size * 0.82);
            gpu::draw_text(
                gpu, &item.sublabel,
                px + pw - pad - sub_w,
                label_y,
                font_size * 0.82,
                if is_selected { Color::rgba(180, 140, 80, a(200)) }
                else           { Color::rgba(120, 100, 60, a(120)) }
            );
        }
    }

    gpu::pop_clip(gpu);

    //
    // Scrollbar
    //
    let total_items = editor.lister.filtered.len();
    if total_items > 0 {
        let total_h = total_items as f32 * item_h + item_h * LISTER_ITEMS_PADDING;
        let bar_h    = (list_h * (list_h / total_h).min(1.0)).max(sep * 4.0);
        let bar_frac = (editor.lister.scroll_anim / (total_h - list_h).max(1.0)).clamp(0.0, 1.0);
        let bar_y    = list_y + bar_frac * (list_h - bar_h);

        // Scrollbar track - very faint
        gpu::draw_rect(gpu, px + pw - sep*3.0 - sep, list_y, sep*3.0, list_h, Color::rgba(255, 200, 100, a(15)));

        // Scrollbar thumb
        gpu::draw_rect(gpu, px + pw - sep*3.0 - sep, bar_y, sep*3.0, bar_h, Color::rgba(180, 140, 80, a(140)));
    }
}

pub fn render_messager(gpu: &mut Gpu, editor: &mut Editor) {
    let tick  = editor.messager.tick;
    let head  = editor.messager.head  as usize;
    let count = editor.messager.count as usize;
    if count == 0 {
        return;
    }

    let screen_width = gpu.win_w;
    let font_size    = MESSAGER_FONT_SIZE;
    let line_height  = font_size + 4.0;
    let margin_top   = 12.0;
    let margin_right = 8.0;
    let x = screen_width - editor.messager.column_width - margin_right;

    for i in 0..count {
        let index = (head + i) % MAX_MESSAGE_COUNT;
        let message = &mut editor.messager.entries[index];

        if message.started_at.is_none() {
            message.started_at = Some(NonZero::new(tick.max(1)).unwrap());
        }

        let age    = tick.wrapping_sub(message.started_at.unwrap().get());
        let offset = message.blob_offset() as usize;
        let len    = message.len()         as usize;
        let text   = &editor.messager.blob[offset..offset + len];

        let t = (age as f32 / MESSAGE_DURATION_IN_MILLISECONDS as f32).clamp(0.0, 1.0);
        let alpha = if t < 0.08 {
            // Fade in: 0.0 - 0.08
            // Smooth step (cubic) fade in
            let x = (t / 0.08).clamp(0.0, 1.0);
            x * x * (3.0 - 2.0 * x)
        } else if t < 0.6 {
            // Full opacity hold
            1.0
        } else {
            // Smooth step (quintic) fade out: 0.6 - 1.0
            let x = ((1.0 - t) / 0.4).clamp(0.0, 1.0);
            x * x * x * (x * (x * 6.0 - 15.0) + 10.0)
        };
        let alpha = (alpha * 0.85).min(0.85);  // Cap at 85% opacity

        let stack_index = (count - 1 - i) as f32;
        let y = margin_top + stack_index * line_height;

        gpu::draw_text(gpu, text, x, y, font_size, Color::rgba(255, 255, 255, (alpha * 255.0) as u8));
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
    pub        id: ViewId,
    pub buffer_id: BufferId,
    pub  panel_id: PanelId,

    pub scroll:        f32,  // Target scroll (set instantly on any scroll event)
    pub scroll_anim:   f32,  // Animated scroll (what actually gets rendered)

    pub cursor_anim_x: f32,  // Animated cursor screen position @Redundant (We currently only animate cursor's Y movements)
    pub cursor_anim_y: f32,  // Animated cursor screen position

    pub cursor_target_line: u32,
    pub cursor_target_col:  u32,

    pub cursor:      Cursor,
    pub layout:      Option<TextLayout>,

    pub persistent_state_per_buffer: FastHashMap<BufferId, ViewState>,
}

impl View {
    pub fn new_with_scroll(id: ViewId, buffer_id: BufferId, scroll: f32) -> Self {
        Self {
            id, buffer_id, scroll, cursor: Cursor::new(), layout: None,
            cursor_anim_x: f32::NAN,
            cursor_anim_y: f32::NAN,
            cursor_target_line: 0, cursor_target_col: 0,
            scroll_anim: 0.0,
            persistent_state_per_buffer: Default::default(),
            panel_id: PanelId::reserved_value()  // Set on first layout
        }
    }

    pub fn new(id: ViewId, buffer_id: BufferId) -> Self {
        Self::new_with_scroll(id, buffer_id, 0.0)
    }

    #[inline]
    pub fn panel_id(&self) -> Option<PanelId> {
        if self.panel_id.is_reserved_value() {
            return None;
        }

        Some(self.panel_id)
    }

    #[inline]
    pub fn switch_buffer(&mut self, new: BufferId) {
        let old = self.buffer_id;
        if old == new { return; }

        //
        // Save old state
        //
        self.persistent_state_per_buffer.insert(old, ViewState {
            cursor: self.cursor,
            scroll: self.scroll,
            scroll_anim: self.scroll_anim,
            // @Incomplete ...
        });

        //
        // Switch
        //
        self.buffer_id = new;
        self.layout    = None;

        //
        // Restore if exists
        //
        if let Some(state) = self.persistent_state_per_buffer.get(&new) {
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

#[derive(Debug)]
pub struct ListerItem {
    pub label:    SmallString<[u8; 32]>,
    pub sublabel: SmallString<[u8; 64]>,
    pub data:     u64,
}

pub type ListerFrameUpdateCallback = fn(&mut CommandContext) -> bool;
pub type ListerSelectFn = fn(&mut CommandContext, u64);

pub struct Lister {
    pub is_open:        bool,
    pub is_listing_file_entries: bool,
    pub is_query_dirty: bool,

    pub last_seen_cached_dir_generation: u64, // u64::MAX if we didnt see any generations

    pub query:         SmallString<[u8; 128]>,

    pub selected_index: u32,
    pub  hovered_index: Option<u32>,

    pub set_selected_index_to_1_instead_of_0: bool,

    pub on_confirm:    Vec<ListerSelectFn>,
    pub pending_datas: Vec<u64>,
    pub items_update_frame_update_callback: Vec<Option<ListerFrameUpdateCallback>>,

    pub scroll:        f32,
    pub scroll_anim:   f32,
    pub open_anim:     f32,  // 0.0 = closed, 1.0 = fully open

    pub item_h:        f32,
    pub list_h:        f32,

    // Storage - cleared and refilled when lister opens
    pub items:           Vec<ListerItem>,

    // Scratch - rebuilt only when query changes (dirty flag)
    pub filtered:        Vec<u32>,        // Indices into items

    pub scratch_str:     String,          // Reused for formatting
    pub scoring_scratch: Vec<(u32, u32)>, // Reused rebuild_filtered
}

impl Lister {
    pub fn new() -> Self {
        Self {
            is_open: false,
            open_anim: 0.0,
            query: SmallString::new(),
            filtered: Default::default(),
            last_seen_cached_dir_generation: u64::MAX,
            items_update_frame_update_callback: Default::default(),
            hovered_index: None,
            is_listing_file_entries: false,
            items: Default::default(),
            on_confirm: Default::default(),
            pending_datas: Default::default(),
            is_query_dirty: false,
            scratch_str: String::with_capacity(512),
            scroll: 0.0,
            set_selected_index_to_1_instead_of_0: false,
            scroll_anim: 0.0,
            selected_index: 0,
            scoring_scratch: Vec::with_capacity(256),
            list_h: 0.0,
            item_h: 0.0
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    pub fn renderer_is_open(&self) -> bool {
        self.open_anim > 0.10
    }

    pub fn rebuild_filtered(&mut self) {
        if !self.is_query_dirty { return; }

        self.filtered.clear();

        let filter_str = if self.is_listing_file_entries {
            // For filtering entries, only use the filename part of the query
            // (the part after the last /)

            let after_last_slash = self.query.as_str()
                .rsplit(MAIN_SEPARATOR)
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
            self.filtered.extend(0..self.items.len() as u32);
            self.is_query_dirty = false;
            return;
        }

        //
        // Filter by subsequence,
        // then score by edit distance on matched items only for sorting
        //
        self.scoring_scratch.clear();
        self.scoring_scratch.extend(self.items.iter()
            .enumerate()
            .filter(|(_, item)| Self::fuzzy_match(&item.label, filter_str))
            .map(|(i, item)| {
                let score = rustc_edit_distance::edit_distance_with_substrings(
                    filter_str,
                    &item.label,
                    // Use a large limit since we already know it's a subsequence match
                    filter_str.len() * 3,
                ).unwrap_or(usize::MAX);

                (i as u32, score as u32)
            }));

        self.scoring_scratch.sort_unstable_by_key(|&(_, score)| score);
        self.filtered.extend(self.scoring_scratch.iter().map(|&(i, _)| i));
        self.is_query_dirty = false;
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
                if self.selected_index + 1 < self.filtered.len() as u32 {
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
                    if self.selected_index + 1 < self.filtered.len() as u32 {
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
                    let page = (self.list_h / self.item_h) as u32;
                    self.selected_index = (self.selected_index + page).min(self.filtered.len().saturating_sub(1) as u32);
                    self.scroll_to_selected();
                    ListerKeyDispatch::Other
                }

                Some('g') => ListerKeyDispatch::Close,

                _ => ListerKeyDispatch::None
            }

            Key::Character(s) if alt => match s.chars().next() {
                Some('v') => {
                    let page = (self.list_h / self.item_h) as u32;
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

    pub canonicalized_path_to_buffer_id: FastHashMap<Arc<Path>, BufferId>,

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

    pub last_is_lister_open: bool,
    pub last_messager_count: u32,

    // Mouse
    pub mouse_pos:          (f32, f32),
    pub mouse_left_pressed: bool,
    pub is_cursor_visible:  bool,

    lister_query_buffer: BufferId,
    lister_query_view:   ViewId,
    lister_query_panel:  PanelId,  // @Redundant?
    lister_split_panel:  PanelId,

    pub clipboard:       Option<arboard::Clipboard>,

    pub frame_count:     u32,
    pub fps:             f32,

    pub last_fps_time:   Instant,
    pub last_frame_time: Instant,
    pub last_input_time: Instant,

    pub relex_us_acc:    f32,
    pub build_us_acc:    f32,
    pub render_us_acc:   f32,

    pub relex_us:        f32,
    pub build_us:        f32,
    pub render_us:       f32,

    pub session_apply_time_in_milliseconds: Option<f32>,

    pub canonicalized_current_working_directory: SmallString<[u8; 256]>,
    pub canonicalized_last_scanned_directory:    SmallString<[u8; 256]>,

    pub lister:          Lister,
    pub director:        Director,
    pub messager:        Messager,
    pub audioer:         Audioer,
}

impl Editor {
    pub fn new(audioer: Audioer) -> Self {
        let mut buffers = PrimaryMap::with_capacity(32);
        let mut views   = PrimaryMap::with_capacity(32);
        let mut panels  = PrimaryMap::with_capacity(32);

        //
        // Root buffer/view/panel at index 0 is always the main editing surface,
        // Replaced by apply_session or open_initial_buffer before the first frame.
        //
        let root_buffer = buffers.push(Buffer::new());
        let root_view   = views.next_key();
        views.push(View::new(root_view, root_buffer));
        let root_panel  = panels.next_key();
        panels.push(Panel {
            id:   root_panel,
            rect: Rect::default(),
            kind: PanelKind::Leaf { view_id: root_view },
        });
        views[root_view].panel_id = root_panel;

        //
        // Lister internals
        //
        let lister_query_buffer = buffers.push(Buffer::new());
        let lister_query_view   = views.next_key();
        views.push(View::new(lister_query_view, lister_query_buffer));
        let lister_query_panel  = panels.next_key();
        panels.push(Panel {
            id:   lister_query_panel,
            rect: Rect::default(),
            kind: PanelKind::Leaf { view_id: lister_query_view },
        });
        views[lister_query_view].panel_id = lister_query_panel;

        let lister_split_panel = panels.next_key();
        panels.push(Panel {
            id:   lister_split_panel,
            rect: Rect::default(),
            kind: PanelKind::ListerSplit,
        });

        let canonicalized_current_working_directory: SmallString<[_; _]> = std::env::args().nth(1)
            .and_then(|p| Path::new(&p).parent().map(|p| p.to_path_buf()))
            .and_then(|p| std::fs::canonicalize(p).ok())
            .unwrap_or_else(|| std::fs::canonicalize(".").unwrap_or_default())
            .into_os_string()
            .into_string()
            .unwrap()
            .into();

        views[lister_query_view].panel_id = lister_query_panel;

        let canonicalized_path_to_buffer_id = FastHashMap::with_capacity_and_hasher(128, Default::default());

        let mut editor = Self {
            buffers,
            views,
            panels,
            lister_split_panel,
            canonicalized_path_to_buffer_id,
            lister: Lister::new(),
            last_input_time: Instant::now(),
            last_cursor_visible: false,
            is_cursor_visible: true,
            buffer_cycle_index: None,
            last_messager_count: u32::MAX,
            last_is_lister_open: false,
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
            clipboard: arboard::Clipboard::new().ok(),
            audioer,
            director: Director::new(),
            messager: Messager::new(),
            session_apply_time_in_milliseconds: None
        };

        //
        // Try to restore session first
        //
        let session_path = &default_session_path();

        if let Ok(file)      = std::fs::File::open(session_path)
        && let Ok(mmap)      = unsafe { MmapOptions::new().populate().map(&file) }
        && let Some(session) = load_session(&mmap[..])
        {
            let time = apply_session(&mut editor, session);
            editor.session_apply_time_in_milliseconds = Some(time);
        }

        //
        // Open the file from argv if user provided it
        //
        open_initial_buffer(&mut editor);

        editor
    }

    #[inline]
    pub fn get_clipboard(&mut self) -> Option<String> {
        self.clipboard.as_mut()?.get_text().ok()
    }

    #[inline]
    pub fn set_clipboard(clipboard: &mut Option<arboard::Clipboard>, text: &str) {
        if let Some(cb) = clipboard {
            _ = cb.set_text(text);
        }
    }

    #[inline]
    pub fn hide_cursor(&mut self, win: &Window) {
        if !self.is_cursor_visible { return }

        self.is_cursor_visible = false;
        win.set_cursor_visible(false);
    }

    #[inline]
    pub fn show_cursor(&mut self, win: &Window) {
        if self.is_cursor_visible { return }

        self.is_cursor_visible = true;
        win.set_cursor_visible(true);
    }

    #[inline]
    pub fn is_lister_open_and_is_it_listing_file_entries(&self) -> bool {
        self.lister.is_open() && self.lister.is_listing_file_entries
    }

    #[inline]
    pub fn is_lister_buffer_dirty(&self) -> bool {
        self.buffers[self.lister_query_buffer].is_dirty
    }

    #[inline]
    pub fn open_lister(&mut self, items: Vec<ListerItem>, on_confirm: ListerSelectFn) {
        self.open_lister_impl(items, on_confirm, None)
    }

    #[inline]
    pub fn open_lister_with_frame_callback(&mut self, items: Vec<ListerItem>, on_confirm: ListerSelectFn, frame_callback: ListerFrameUpdateCallback) {
        self.open_lister_impl(items, on_confirm, Some(frame_callback))
    }

    #[inline]
    pub fn open_lister_impl(&mut self, items: Vec<ListerItem>, on_confirm: ListerSelectFn, frame_callback: Option<ListerFrameUpdateCallback>) {
        clear_buffer(self, self.lister_query_buffer);

        self.set_active_panel(self.lister_split_panel);

        self.lister.items_update_frame_update_callback.push(frame_callback);
        self.lister.on_confirm.push(on_confirm);

        self.lister.query.clear();
        self.lister.filtered.clear();
        self.lister.is_query_dirty     = true;
        self.canonicalized_last_scanned_directory = SmallString::new();
        self.lister.selected_index  = if self.lister.set_selected_index_to_1_instead_of_0 {
            (items.len() > 1) as u32
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

    pub fn panel_of_view(&self, view_id: ViewId) -> PanelId {
        self.views[view_id].panel_id
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
        self.layout_panel(self.lister_query_panel, lister_rect(win_rect.w, win_rect.h, 1.0, self.scale));
        self.layout_panel(self.lister_split_panel, lister_rect(win_rect.w, win_rect.h, 1.0, self.scale));
    }

    fn layout_panel(&mut self, id: PanelId, rect: Rect) {
        self.panels[id].rect = rect;

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

        self.views[new_view_id].panel_id = right_id;
        self.views[old_view_id].panel_id = left_id;

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

        if let PanelKind::Leaf { view_id } = self.panels[active_id].kind {
            self.views[view_id].panel_id = PanelId::reserved_value();
        }

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

        let index = self.buffer_cycle_index.get_or_insert(0);
        *index = (*index + 1) % len;

        self.most_recently_used_buffers[*index]
    }

    pub fn previous_buffer(&mut self) -> BufferId {
        let len = self.most_recently_used_buffers.len();
        if len <= 1 { return self.active_view().buffer_id; }

        let index = self.buffer_cycle_index.get_or_insert(0);
        *index = if *index == 0 { len - 1 } else { *index - 1 };

        self.most_recently_used_buffers[*index]
    }

    pub fn commit_buffer_cycle(&mut self) {
        let Some(idx) = self.buffer_cycle_index.take() else { return };

        let buf = self.most_recently_used_buffers[idx];
        self.most_recently_used_buffers.retain(|&b| b != buf);
        self.most_recently_used_buffers.insert(0, buf);
    }

    // @Refactor
    pub fn mru_register_new_buffer(&mut self, buffer_id: BufferId) {
        if buffer_id == self.lister_query_buffer { return }

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

const MIN_SCALE:  f32 = 0.75;
const MAX_SCALE:  f32 = 5.00;
const SCALE_STEP: f32 = 0.25;

const SCROLL_ANIM_RATE: f32 = 46.67;
const CURSOR_ANIM_RATE: f32 = 99.420;

const BLINK_ON_MS:  u128 = 530;
const BLINK_OFF_MS: u128 = 370;

const BLINK_START_DELAY_MS: u128 = 500;  // Start blinking after 500ms idle
const BLINK_STOP_IDLE_MS:   u128 = 5000; // Stop  blinking after 5s    idle

const DELETE_ANIMATION_DURATION: f32 = 0.115; // nocheckin @Tune

const  PASTE_ANIMATION_DURATION: f32 = 2.48;  // nocheckin @Tune

const PASTE_ANIMATION_BITS:     usize = 4;
const PASTE_ANIMATION_PER_WORD: usize = 64  / PASTE_ANIMATION_BITS;        // 16
const PASTE_ANIMATION_MASK:     u64   = (1 << PASTE_ANIMATION_BITS) - 1;   // 0b1111
const PASTE_ANIMATION_MAX_ID:   usize = PASTE_ANIMATION_MASK as usize;     // 15

pub fn layout_update_currently_animated_insertions(layout: &mut TextLayout, insertions: &[AnimatedInsertion]) {
    layout.glyph_insertion_ids.clear();
    if insertions.is_empty() { return; }

    let n     = layout.glyphs.len();
    let words = (n + PASTE_ANIMATION_PER_WORD - 1) / PASTE_ANIMATION_PER_WORD;

    layout.glyph_insertion_ids.resize(words, 0u64);

    for (i, g) in layout.glyphs.iter().enumerate() {
        let byte = g.byte_offset as usize;
        for a in insertions.iter().take(PASTE_ANIMATION_MAX_ID) {
            if byte >= a.byte_start && byte < a.byte_start + a.byte_len as usize {
                let word =  i / PASTE_ANIMATION_PER_WORD;
                let bit  = (i % PASTE_ANIMATION_PER_WORD) * PASTE_ANIMATION_BITS;
                layout.glyph_insertion_ids[word] |= (a.id as u64) << bit;
                break;
            }
        }
    }
}

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

fn animate(editor: &mut Editor, dt: f32) -> bool {
    let _tracy = tracy::span!("animate");

    let mut still_animating = false;

    let epsilon = 0.5f32;  // Stop animating when close enough
    let line_h  = editor.line_h();

    for view in editor.views.values_mut() {
        //
        // Scroll
        //
        let ds = view.scroll - view.scroll_anim;
        if ds.abs() > epsilon {
            view.scroll_anim += ds * (1.0 - (-SCROLL_ANIM_RATE * dt).exp());
            still_animating = true;
        } else {
            view.scroll_anim = view.scroll;
        }

        //
        // Cursor target comes from layout if available
        //
        let Some(layout) = &view.layout else { continue };

        let (cursor_line, cursor_col) = (view.cursor_target_line, view.cursor_target_col);

        if let Some(target_x) = layout.cursor_x(cursor_line, cursor_col) {
            //
            // Compute target Y from scroll_anim so cursor tracks the animated scroll,
            // not the settled scroll position
            //
            let target_y = layout.rect.y + cursor_line as f32 * line_h - view.scroll_anim;

            let dy = target_y - view.cursor_anim_y;
            if view.cursor_anim_y.is_nan() {  // @Redundant
                // ... Fallthrough, keep it NAN for should_snap inside rebuild_text_layout

            } else if dy.abs() > layout.rect.h {
                view.cursor_anim_y = target_y;

            } else if dy.abs() > epsilon {
                view.cursor_anim_y += dy * (1.0 - (-CURSOR_ANIM_RATE * dt).exp());
                still_animating = true;

            } else {
                view.cursor_anim_y = target_y;
            }

            //
            // @Design: Don't animate cursor's horizontal movements.
            //

            view.cursor_anim_x = target_x;

            // let dx = target_x - view.cursor_anim_x;
            // if view.cursor_anim_x.is_nan() {
            //     // ... Fallthrough, keep it NAN for should_snap inside rebuild_text_layout
            //
            // } else if dx.abs() > layout.rect.w {
            //     view.cursor_anim_x = target_x;
            //
            // } else if dx.abs() > epsilon {
            //     view.cursor_anim_x += dx * (1.0 - (-CURSOR_ANIM_RATE * dt).exp());
            //     still_animating = true;
            //
            // } else {
            //     view.cursor_anim_x = target_x;
            // }
        }
    }

    //
    // Advance insertion animations per buffer
    //
    for buffer in editor.buffers.values_mut() {
        let before = buffer.currently_animated_insertions.len();
        buffer.currently_animated_insertions.retain_mut(|a| {
            a.t = (a.t + dt / PASTE_ANIMATION_DURATION).min(1.0);
            a.t < 1.0
        });
        if buffer.currently_animated_insertions.len() < before {
            buffer.is_dirty = true; // @Hack nocheckin @DocumentThis
        }
        if !buffer.currently_animated_insertions.is_empty() {
            still_animating = true;
        }
    }

    //
    // Advance deletion animations per buffer
    //
    for buffer in editor.buffers.values_mut() {
        let before = buffer.currently_animated_insertions.len();
        buffer.currently_animated_deletions.retain_mut(|a| {
            a.t = (a.t + dt / DELETE_ANIMATION_DURATION).min(1.0);
            a.t < 1.0
        });
        if buffer.currently_animated_deletions.len() < before {
            buffer.is_dirty = true; // @Hack nocheckin @DocumentThis
        }
        if !buffer.currently_animated_deletions.is_empty() {
            still_animating = true;
        }
    }

    //
    // Lister smooth scrolling
    //
    let ds = editor.lister.scroll - editor.lister.scroll_anim;
    if ds.abs() > epsilon {
        editor.lister.scroll_anim += ds * (1.0 - (-SCROLL_ANIM_RATE * dt).exp());
        still_animating = true;
    } else {
        editor.lister.scroll_anim = editor.lister.scroll;
    }

    //
    // Lister opening animation
    //
    let target = if editor.lister.is_open { 1.0_f32 } else { 0.0 };
    let speed = if editor.lister.open_anim > target { 25.0 } else { 25.0 }; // @Tune
    let remaining = target - editor.lister.open_anim;
    if remaining.abs() < 0.08 {
        editor.lister.open_anim = target;
    } else {
        let step = (remaining * speed * dt).clamp(-0.15, 0.15);
        editor.lister.open_anim += step;
        editor.lister.open_anim = editor.lister.open_anim.clamp(0.0, 1.0);
    }

    still_animating |= editor.lister.open_anim != target;

    still_animating
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

    const MAX_SCAN_LINES: u32 = 128;

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
    if editor.lister.is_open() {
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

fn editor_dispatch_lister_confirm(cx: &mut CommandContext) {
    let index = cx.editor.lister.selected_index;
    let Some(index) = cx.editor.lister.filtered.get(index as usize) else { return };
    let item_data = cx.editor.lister.items[*index as usize].data;

    let Some(on_confirm) = cx.editor.lister.on_confirm.pop()        else { return };

    cx.editor.lister.pending_datas.push(item_data);
    _ = cx.editor.lister.items_update_frame_update_callback.pop();
    cx.editor.lister.set_selected_index_to_1_instead_of_0 = false;

    on_confirm(cx, item_data);
}

//
// @Note @Speed: We might want to somehow parallelize this for very slow hard drives,
// but besides from that, saving a 100mb file on my cheap ass SSD wasn't that slow at all.
//
pub fn editor_save_buffer_onto_disk(editor: &mut Editor, buffer_id: BufferId) -> std::io::Result<()> {
    let buffer = &editor.buffers[buffer_id];
    let Some(path) = buffer.path.as_ref() else { return Ok(()) };

    let tmp_path = path.with_extension("tmp");
    let mut f = BufWriter::new(std::fs::File::create(&tmp_path)?);
    for chunk in buffer.text.chunks() {
        f.write(chunk.as_bytes())?;
    }

    f.flush()?;
    drop(f);

    std::fs::rename(&tmp_path, path)?;

    Ok(())
}

fn editor_handle_left_mouse_click(editor: &mut Editor, gpu: &mut Gpu, command_table: &CommandTable) -> bool {
    if editor.lister.is_open() {
        let lister = lister_rect(gpu.win_w, gpu.win_h, editor.lister.open_anim, editor.scale);
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
                let clicked = (local_y / item_h) as u32;
                if clicked < editor.lister.filtered.len() as u32 {
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

pub fn adjust_cursors_after_buffer_mutation(editor: &mut Editor) {
    //
    // If user has two panels looking into the same buffer,
    // and he mutated the buffer this frame, ensure that
    // all cursors pointed into this buffer visually stay at the same place.
    //

    // @Speed: In the future we kinda wanna maintain a hashmap of BufferId -> Edit,
    // but iterating all buffers isn't that slow since they're completely flat.

    let _tracy = tracy::span!("adjust_cursors_after_mutation");

    let active_view_id = editor.active_view_id();

    let mut adjusted_views: SmallVec<[ViewId; 24]> = SmallVec::new();

    for (buffer_id, buffer) in editor.buffers.iter_mut() {
        if let Some((at, len)) = buffer.last_insert.take() {
            for (vid, view) in editor.views.iter_mut() {
                if view.buffer_id != buffer_id { continue }
                if vid == active_view_id       { continue }

                if view.cursor.char_index > at {
                    view.cursor.char_index += len as usize;
                    adjusted_views.push(vid);
                }

                if let Some(a) = view.cursor.anchor_char_index {
                    if a > at { view.cursor.anchor_char_index = Some(a + len as usize); }
                }
            }
        }

        if let Some((at, len)) = buffer.last_delete.take() {
            for (vid, view) in editor.views.iter_mut() {
                if view.buffer_id != buffer_id { continue }
                if vid == active_view_id       { continue }

                if view.cursor.char_index > at {
                    view.cursor.char_index = view.cursor.char_index.saturating_sub(len as usize).max(at);
                    adjusted_views.push(vid);
                }

                if let Some(a) = view.cursor.anchor_char_index {
                    if a > at { view.cursor.anchor_char_index = Some(a.saturating_sub(len as usize).max(at)); }
                }
            }
        }
    }

    let mut leaves = Default::default();
    collect_leaves(editor, editor.root_panel, &mut leaves);

    for view_id in adjusted_views {
        let Some(panel_id) = editor.views[view_id].panel_id() else { continue };
        let rect = editor.panels[panel_id].rect;

        let (view, buf) = editor.view_and_buffer(view_id);

        let (line, col) = buf.cursor_line_col(&view.cursor);

        editor.views[view_id].cursor_target_line = line;
        editor.views[view_id].cursor_target_col  = col;
        editor.snap_cursor_to_target(view_id, line, col, rect);

        // Force anim y in case the line wasn't in the layout
        let line_h = editor.line_h();
        if let Some(layout) = &editor.views[view_id].layout {
            let target_y = layout.rect.y + line as f32 * line_h - editor.views[view_id].scroll_anim;
            editor.views[view_id].cursor_anim_y = target_y;
        }
    }
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

    editor.canonicalized_path_to_buffer_id.insert(editor.buffers[buffer_id].path.clone().unwrap().into(), buffer_id);  // @Clone

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
    editor.buffers[buffer].is_dirty || editor.views[view].layout.as_ref().map(|l| {
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

fn open_initial_buffer(editor: &mut Editor) {
    let path = std::env::args().nth(1).map(|p| PathBuf::from(p));

    let canon = path.as_deref().and_then(|p| p.canonicalize().ok());

    if let Some(canon) = &canon
    && let Some(&old_buffer_id) = editor.canonicalized_path_to_buffer_id.get(canon.as_path())
    {
        //
        // If this buffer is already opened, just switch onto it.
        //
        editor.active_view_mut().switch_buffer(old_buffer_id);
        editor.mru_focus(old_buffer_id); // @Refactor
        return;
    }

    let buffer = path.as_deref()
        .and_then(|p| Buffer::from_file(p).ok())
        .unwrap_or_else(Buffer::new);

    //
    // Reuse the root buffer slot
    //

    editor.buffers[editor.views[VIEW_MAIN].buffer_id] = buffer;

    let root_buf_id = editor.views[VIEW_MAIN].buffer_id;
    editor.mru_register_new_buffer(root_buf_id);

    if let Some(p) = canon {
        editor.canonicalized_path_to_buffer_id.insert(p.into(), root_buf_id);
    }
}

struct App {
    gpu:    Option<Gpu>,
    window: Option<Arc<Window>>,
    mods:   winit::event::Modifiers,

    editor: Editor,

    is_our_window_focused: bool,
    refresh_rate_millihertz: u32,

    command_table: CommandTable,
    keymap: Keymap,
}

impl App {
    fn new(audioer: Audioer) -> Self {
        let mut editor = Editor::new(audioer);
        editor.director.kick_scan(PathBuf::from("."), true, true, false);

        App {
            command_table: CommandTable::from_inventory(),
            keymap: Keymap::default_keymap(),

            editor,

            gpu: None,
            window: None,

            is_our_window_focused: false,
            refresh_rate_millihertz: u32::MAX,
            mods: Default::default(),
        }
    }
}

enum UserEvent {
    ExitRequested,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let win: Arc<_> = el.create_window(
            Window::default_attributes()
                .with_title("naysayer")
                .with_decorations(false)
        ).unwrap().into();

        let size = win.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));

        let mut gpu = gpu::init(Arc::clone(&win));
        gpu.verts_mut().reserve(INITIAL_VERTEX_BUFFER_CAPACITY as _);

        let editor = &mut self.editor;
        editor.layout_panels(Rect::full(w as f32, h as f32));

        prewarm_glyphs_and_print_preallocation_memory_usage(&editor, &mut gpu);

        self.refresh_rate_millihertz = win.current_monitor()
            .and_then(|m| m.refresh_rate_millihertz())
            .unwrap_or(60*1000);

        {
            if let Some(time) = editor.session_apply_time_in_milliseconds {
                let path = pretty_path(&default_session_path());
                let message = format!("Applied session in {time}us from '{path}'");
                editor.messager.push(&message, &mut gpu);
            }
        }

        self.gpu    = Some(gpu);
        self.window = Some(win);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let Some(win)    = &self.window else { return };

        let editor = &self.editor;

        if editor.lister.open_anim > 0.0 && !editor.lister.is_open {
            win.request_redraw();
            return;
        }

        let since_input = editor.last_input_time.elapsed().as_millis();

        if since_input < BLINK_START_DELAY_MS {
            //
            // Waiting to start blinking - wake up when delay expires
            //
            let ms_until = BLINK_START_DELAY_MS - since_input;
            el.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(ms_until as u64)
            ));

        } else if since_input > BLINK_STOP_IDLE_MS {
            //
            // Idle too long - just wait for input
            //
            el.set_control_flow(ControlFlow::Wait);

        } else {
            //
            // Actively blinking - wake up at next blink transition
            //
            let elapsed = editor.blink_epoch.elapsed().as_millis();
            let cycle   = BLINK_ON_MS + BLINK_OFF_MS;
            let phase   = elapsed % cycle;
            let ms_until = if phase < BLINK_ON_MS {
                BLINK_ON_MS - phase
            } else {
                cycle - phase
            };

            el.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(ms_until as u64)
            ));

            win.request_redraw();
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::ExitRequested => {
                el.exit();
            }
        }
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        _ = save_session(&self.editor, &default_session_path());
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let WindowEvent::ModifiersChanged(m) = &event {
            self.mods = *m;
            return;
        }

        let (Some(gpu), Some(win)) = (&mut self.gpu, &self.window) else { return };

        let editor = &mut self.editor;

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
                            (editor.lister.items.len() > 1) as u32
                        } else {
                            0
                        };
                        editor.lister.is_query_dirty = true;
                        editor.lister.rebuild_filtered();
                        editor.lister.is_query_dirty = true; // nocheckin @DocumentThis
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

                if editor.lister.is_open() { // @Refactor
                    //
                    // Lister scroll takes priority if open and mouse is over it
                    //

                    let lister = lister_rect(gpu.win_w, gpu.win_h, editor.lister.open_anim, editor.scale);
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
                            let hovered_index_before = editor.lister.hovered_index;
                            editor.lister.hovered_index = if hovered < editor.lister.filtered.len() {
                                if hovered_index_before != Some(hovered as u32) {
                                    editor.audioer.play_lister_item_hover_sound();
                                }

                                Some(hovered as u32)
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

                if editor_handle_left_mouse_click(editor, gpu, &self.command_table) {
                    win.request_redraw();
                }

                editor.mouse_left_pressed = true;
            }

            WindowEvent::CursorMoved { position, .. } => {
                editor.show_cursor(win);

                editor.mouse_pos = (position.x as f32, position.y as f32);

                if editor.lister.is_open() { // @Refactor
                    let lister = lister_rect(gpu.win_w, gpu.win_h, editor.lister.open_anim, editor.scale);
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
                        let hovered_index_before = editor.lister.hovered_index;
                        editor.lister.hovered_index = if hovered < editor.lister.filtered.len() {
                            if hovered_index_before != Some(hovered as u32) {
                                editor.audioer.play_lister_item_hover_sound();
                            }

                            Some(hovered as u32)
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
                    if editor_handle_left_mouse_click(editor, gpu, &self.command_table) {
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

                editor.last_is_lister_open = editor.lister.is_open();
                editor.last_messager_count = editor.messager.count;

                editor.messager.tick(dt);
                editor.messager.evict_expired(MESSAGE_DURATION_IN_MILLISECONDS);

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
                    checked_reserve!(verts, reserve as usize, "vertex buffer");
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

                    gpu::push_clip(gpu, rect.x, rect.y, rect.w, rect.h);
                    let t1 = Instant::now();
                    render_text_layout(
                        gpu,
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

                if editor.lister.is_open() {
                    //
                    // Prepare lister bg
                    //

                    let t1 = Instant::now();
                    {
                        if active_panel == editor.lister_split_panel {
                            let lister = lister_rect(gpu.win_w, gpu.win_h, editor.lister.open_anim, editor.scale);
                            let t = 1.0 - (1.0 - editor.lister.open_anim).powi(4);  // Same easing as lister_rect
                            render_lister_background_frosted(gpu, lister, editor.scale, t);
                        }
                        render_lister_background(gpu, editor);
                    }
                    editor.render_us_acc += t1.elapsed().as_micros() as f32;
                }

                if editor.lister.is_open() {
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

                    gpu::push_clip(gpu, rect.x, rect.y, rect.w, rect.h);
                    let t1 = Instant::now();
                    render_text_layout(
                        gpu,
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

                for buffer in editor.buffers.values_mut() {
                    //
                    // No buffer can be dirty now!
                    //
                    buffer.is_dirty = false;
                }

                let t1 = Instant::now();
                {
                    render_lister_foreground(gpu, editor);
                    render_messager(gpu, editor);
                    draw_metrics(editor, gpu, self.refresh_rate_millihertz);
                }
                editor.render_us_acc += t1.elapsed().as_micros() as f32;

                _ = gpu::submit_frame(gpu);

                let new_cursor_visible = editor.cursor_visible();
                let blink_changed = new_cursor_visible != editor.last_cursor_visible;
                editor.last_cursor_visible = new_cursor_visible;

                should_request_redraw |= blink_changed;

                should_request_redraw |= editor.lister.is_open() != editor.last_is_lister_open;
                should_request_redraw |= editor.messager.count != editor.last_messager_count;
                should_request_redraw |= editor.messager.count != 0;
                should_request_redraw |= editor.lister.open_anim > 0.0 && !editor.lister.is_open;

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

    // @Note: We want to start Audio initialization as soon as possible,
    // because audio servers tend to be VERY slow when trying to initialize a connection,
    // very sad ...
    let audioer = Audioer::spawn();

    let Ok(el) = EventLoop::<UserEvent>::with_user_event().build() else { return };
    el.set_control_flow(ControlFlow::Wait);

    ctrlc::set_handler({
        let proxy = el.create_proxy();
        move || _ = proxy.send_event(UserEvent::ExitRequested)
    }).unwrap();

    let mut app = App::new(audioer);
    _ = el.run_app(&mut app);
}
