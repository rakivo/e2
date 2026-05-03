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

pub mod gpu;
pub mod util;
pub mod color;
pub mod buffer;
pub mod command;
pub mod tracy;
pub mod director;
pub mod lexer;
pub mod session;
pub mod audioer;
pub mod messager;

use audioer::Audioer;
use lexer::token_color;
use messager::{MAX_MESSAGE_COUNT, MESSAGE_DURATION_IN_MILLISECONDS, MESSAGER_FONT_SIZE, Messager};
use util::format_bytes;
use session::{apply_session, default_session_path, load_session};
use buffer::{AnimatedRegion, Buffer, Cursor};
use color::{Color, GpuColor};
use command::{CommandContext, CommandAtom};
use director::Director;

use std::any::Any;
use std::io::{BufWriter, Write};
use std::num::NonZero;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use std::fmt::Write as _;
use std::collections::VecDeque;

use cranelift_entity::packed_option::ReservedValue;
use cranelift_entity::{EntityRef, PrimaryMap};
use memmap2::MmapOptions;
use smallstr::SmallString;
use smallvec::{SmallVec, smallvec};
use wgpu::naga::FastHashMap;
use winit::window::Window;
use gpu::{ATLAS_SIZE, Gpu, GpuGlyph, draw_text_for_editor, prewarm_glyphs, reset_atlas};

macro_rules! hooks {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$field_meta:meta])*
                    $hook_vis:vis $hook:ident: $ty:ty
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name {
            $(
                $(#[$field_meta])*
                $hook_vis $hook: Option<$ty>,
            )*
        }
    };
}

hooks! {
    #[derive(Default)]
    pub struct Hooks {
        pub additional_font_sizes_to_prewarm: fn (&Editor) -> SmallVec<[f32; 8]>,

        pub layout_panels:  fn (&mut Editor, win_rect: Rect),
        pub layout_panel:   fn (&mut Editor, id: PanelId, rect: Rect),

        pub animate:        fn (&mut Editor, dt: f32, still_animating: &mut bool),

        pub text_layout_render_settings: fn (&Editor, ViewId) -> TextLayoutRenderSettings,

        pub active_view_id: fn (&Editor, CustomPanel) -> ViewId,

        /// Called at the start of [`Editor::set_active_panel`].
        ///
        /// Returns `true` if should [`Editor::set_active_panel`] should short-circuit
        /// and give full control to the custom layer.
        pub set_active_panel: fn (&mut Editor, PanelId) -> bool,

        /// Called at the start of [`Editor::mru_register_new_buffer`].
        ///
        /// Returns: Same as set_active_panel
        pub register_new_buffer_in_most_recently_used_list: fn (&mut Editor, BufferId) -> bool,

        pub does_view_need_layout_rebuild: fn (&Editor, ViewId, BufferId, Rect) -> bool,

        pub inside_about_to_wait_should_request_redraw: fn (&Editor) -> bool,
        pub        inside_redraw_should_request_redraw: fn (&Editor) -> bool,

        /// You usually wanna do your ticks here.
        pub about_to_redraw_a_frame:        fn (&mut CommandContext, dt: f32) -> bool,

        /// Returns whether should request a redraw.
        pub about_to_rebuild_dirty_layouts: fn (&mut CommandContext)          -> bool,

        /// Rebuilt all dirty layouts, about to animate()!
        pub rebuilt_all_dirty_layouts:      fn (&mut CommandContext)          -> bool,

        /// Animated all animations,   about to draw!
        pub animated_all_animations:        fn (&mut CommandContext)          -> bool,

        /// Returns `true` if should skip rendering this specific panel.
        pub about_to_draw_this_panel:       fn (&mut CommandContext, PanelId, ViewId, Rect) -> bool,

        pub drew_all_leaf_panels:           fn (&mut CommandContext)          -> bool,

        /// Returns:
        ///
        /// First  bool corresponds to whether we should request a window redraw.
        ///
        /// Second bool corresponds to whether mouse click handler should short-circuit
        /// out of the function and let the custom layer have full control over the user input.
        pub left_mouse_clicked:     fn (&mut CommandContext)               -> (bool, bool),

        /// Returns: Same as left_mouse_clicked
        pub mouse_wheel_scrolled:   fn (&mut CommandContext, delta_y: f32) -> (bool, bool),

        /// Returns: Same as left_mouse_clicked
        pub mouse_moved:            fn (&mut CommandContext)               -> (bool, bool),

        /// Returns: Same as left_mouse_clicked
        pub key_pressed:            fn (&mut CommandContext)               -> (bool, bool),

        /// Returns: Same as left_mouse_clicked
        pub pre_command_execution:  fn (&mut CommandContext, CommandAtom)  -> (bool, bool),

        /// Returns: Same as left_mouse_clicked
        pub post_command_execution: fn (&mut CommandContext, CommandAtom)  -> (bool, bool),

        pub collect_leaf_panels_init_stack:         fn (&Editor, root: PanelId,              stack: &mut SmallVec<[PanelId; 48]>),
        pub collect_leaf_panels:                    fn (&Editor, root: PanelId, CustomPanel, stack: &mut SmallVec<[PanelId; 48]>) -> SmallVec<[(PanelId, ViewId, Rect); 2]>,
        pub collect_leaf_panels_for_session_saving: fn (&Editor, root: PanelId, CustomPanel, stack: &mut SmallVec<[PanelId; 48]>) -> SmallVec<[(PanelId, ViewId); 2]>,
    }
}

#[cfg(debug_assertions)]
#[inline(always)]
pub const fn vec_element_size<T>(_: &Vec<T>) -> usize {
    size_of::<T>()
}

#[cfg(debug_assertions)]
#[macro_export]
macro_rules! checked_reserve {
    ($vec:expr, $n:expr, $name:expr, $config:expr) => {
        let _old_cap = $vec.capacity();
        $vec.reserve($n);
        let _new_cap = $vec.capacity();
        if $config.log_checked_reserves {
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

    ($vec:expr, $n:expr, cfg=$config:expr) => {
        checked_reserve!($vec, $n, stringify!($vec), $config)
    };

    ($vec:expr, $n:expr, $name:expr) => {
        checked_reserve!($vec, $n, $name, EditorLoggerConfig::new())
    };

    ($vec:expr, $n:expr, $name:expr, cfg=$config:expr) => {
        checked_reserve!($vec, $n, $name, $config)
    };

    ($vec:expr, $n:expr) => {
        checked_reserve!($vec, $n, stringify!($vec))
    };
}
#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! checked_reserve {
    ($vec:expr, $n:expr, $name:expr, $config:expr) => { $vec.reserve($n); };
    ($vec:expr, $n:expr, $name:expr) => { $vec.reserve($n); };
    ($vec:expr, $n:expr) => { $vec.reserve($n); };
}

#[cfg(debug_assertions)]
#[macro_export]
macro_rules! checked_push {
    ($vec:expr, $val:expr, $name:expr, $config:expr) => {
        let cap_before = $vec.capacity();
        $vec.push($val);
        if $config.log_checked_pushes {
            if $vec.capacity() != cap_before {
                eprintln!(
                    "[{} reallocated]: {} -> {}",
                    $name, cap_before, $vec.capacity()
                );
            }
        }
    };

    ($vec:expr, $val:expr, cfg=$config:expr) => {
        checked_push!($vec, $val, stringify!($vec), $config)
    };

    ($vec:expr, $val:expr, $name:expr) => {
        checked_push!($vec, $val, $name, EditorLoggerConfig::new())
    };

    ($vec:expr, $val:expr, $name:expr, cfg=$config:expr) => {
        checked_push!($vec, $val, $name, $config)
    };

    ($vec:expr, $val:expr) => {
        checked_push!($vec, $val, stringify!($vec))
    };
}
#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! checked_push {
    ($vec:expr, $val:expr, $name:expr, $config:expr) => { $vec.push($val); };
    ($vec:expr, $val:expr, $name:expr) => { $vec.push($val); };
    ($vec:expr, $val:expr) => { $vec.push($val); };
}

pub fn prewarm_glyphs_and_print_preallocation_memory_usage(editor: &Editor, gpu: &mut Gpu) {
    let mut builtin_prewarmed_font_sizes: SmallVec<[f32; 16]> = smallvec![
        editor.scale,
        editor.scale - SCALE_STEP,
        editor.scale + SCALE_STEP,
        editor.scale - 2.0 * SCALE_STEP,
        editor.scale + 2.0 * SCALE_STEP,
        editor.scale - 3.0 * SCALE_STEP,
        editor.scale + 3.0 * SCALE_STEP,
    ];

    if let Some(additional_font_sizes_to_prewarm_hook) = editor.hooks.additional_font_sizes_to_prewarm {
        for value in additional_font_sizes_to_prewarm_hook(editor) {
            if builtin_prewarmed_font_sizes.iter().any(|&x| x == value) {
                continue;
            }

            let d = (value - editor.scale).abs();

            // Find insertion point: after last element with <= distance
            let mut insert_at = builtin_prewarmed_font_sizes.len();

            for (i, &existing) in builtin_prewarmed_font_sizes.iter().enumerate() {
                let ed = (existing - editor.scale).abs();

                if ed > d {
                    insert_at = i;
                    break;
                }
            }

            builtin_prewarmed_font_sizes.insert(insert_at, value);
        }
    }

    eprintln!("[Prewarming glyphs...]");
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

    eprintln!("[Vertex batch pool preallocation]: {}", format_bytes(vertex_batch_pool_allocation));
    eprintln!("[Vertex buffer size]:              {}", format_bytes(gpu.vertex_buffer.size() as _));
    eprintln!("[Glyph memory usage]:              {}", format_bytes(gpu.glyphs.allocation_size()));

    let used_pixels = gpu.atlas_cur_y as u32 * ATLAS_SIZE + gpu.atlas_cur_x as u32;
    let bytes_per_pixel = 4;
    let used_bytes = used_pixels * bytes_per_pixel;
    let total_bytes = ATLAS_SIZE * ATLAS_SIZE * bytes_per_pixel;

    eprintln!(
        "[Atlas] used={} / {} bytes ({:.2}%)",
        format_bytes(used_bytes as _),
        format_bytes(total_bytes as _),
        (used_bytes as f32 / total_bytes as f32) * 100.0
    );
}

pub fn draw_metrics(editor: &Editor, gpu: &mut Gpu, refresh_rate_millihertz: u32) {
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
    pub paren_match:      Color,
    pub paste_highlight:  Color,
    pub copy_highlight:   Color,
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
        delete_highlight: Color::hex(0x8b3a1e),
        copy_highlight:   Color::hex(0x3a8fb5),
    }
}

pub const PADDING_LEFT: f32 = 8.0;

pub fn padding_left(should_pad_left_when_rendering: bool) -> f32 {
    if should_pad_left_when_rendering {
        PADDING_LEFT
    } else {
        0.0
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

#[derive(Eq, PartialEq, Copy, Clone, Debug, Default)]
pub enum CursorStyle {
    #[default] Block,
    Stick
}

#[derive(Clone, Debug)]
pub struct TextLayoutRenderSettings {
    pub should_pad_left_when_rendering: bool, // @Memory @Speed
    pub should_highlight_current_line_when_rendering: bool, // @Memory @Speed

    pub cursor_style: CursorStyle, // @Memory @Speed
}

impl Default for TextLayoutRenderSettings {
    fn default() -> Self {
        Self {
            should_highlight_current_line_when_rendering: true,
            should_pad_left_when_rendering: false,
            cursor_style: CursorStyle::Block,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TextLayout {
    pub buffer_id: BufferId,

    pub render_settings: TextLayoutRenderSettings,

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

impl Deref for TextLayout {
    type Target = TextLayoutRenderSettings;
    fn deref(&self) -> &Self::Target { &self.render_settings}
}

impl DerefMut for TextLayout {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.render_settings}
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
        Some(ll.x_for_col(self.rect.x + padding_left(self.should_pad_left_when_rendering), col, &self.glyphs))
    }

    /// Full glyph screen rect at (buffer_line, col): [x0, y0, x1, y1].
    #[inline]
    pub fn glyph_rect(&self, buffer_line: u32, col: u32, fallback_w: f32, scroll_anim: f32) -> Option<[f32; 4]> {
        let ll = self.line_for_buffer_line(buffer_line)?;
        let y = self.rect.y
            + (ll.buffer_line - self.first_buffer_line) as f32 * self.line_h
            - (scroll_anim % self.line_h);

        let x0 = ll.x_for_col(self.rect.x + padding_left(self.should_pad_left_when_rendering), col, &self.glyphs);
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
        let col      = ll.col_for_screen_x(self.rect.x + padding_left(self.should_pad_left_when_rendering), mx, &self.glyphs);
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
pub fn rebuild_text_layout(
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

    layout_update_currently_animated_regions(
        &mut layout,
        &editor.buffers[buffer_id].currently_animated_pastes,
        &editor.buffers[buffer_id].currently_animated_copies,
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
pub fn build_text_layout(
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
    let speed = view.scroll_vel.abs();
    let prelex_line_count = (speed * 0.017).clamp(20.0, 200.0) as u32;
    if view.scroll_vel > 0.0 {
        // We are scrolling DOWN (target is below current anim)
        // Add prelex_line_count lines of lookahead to the bottom
        last_line  = last_line.saturating_add(prelex_line_count);
    } else if view.scroll_vel < 0.0 {
        // We are scrolling UP   (target is above current anim)
        // Add prelex_line_count lines of lookahead to the top
        first_line = first_line.saturating_sub(prelex_line_count);
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

    checked_reserve!(lines, line_count as usize, cfg=editor.logger_config);

    //
    //
    // Build line-start offset table from lex_scratch
    //
    //

    let scratch     = buffer.scratch_space_to_flatten_rope_into.as_bytes(); // :BufferScratch
    let scratch_str = &buffer.scratch_space_to_flatten_rope_into;

    let approximate_glyph_count = scratch_str.len();
    checked_reserve!(glyphs, approximate_glyph_count, cfg=editor.logger_config);  // @Tune

    //
    // line_offsets[i] = (scratch_relative_start, scratch_relative_end_excl_nl)
    //
    checked_reserve!(line_offsets, line_count as usize + 1, cfg=editor.logger_config);
    {
        let mut pos = 0usize;
        let mut collected = 0u32;

        while collected < line_count && pos <= scratch.len() {
            let remaining = &scratch[pos..];
            let (line_end_excl_nl, next_pos) = match memchr::memchr(b'\n', remaining) {
                Some(nl_rel) => (pos + nl_rel, pos + nl_rel + 1),
                None         => (scratch.len(), scratch.len()),
            };

            checked_push!(
                line_offsets,
                (pos, line_end_excl_nl),
                cfg=editor.logger_config
            );
            pos = next_pos;
            collected += 1;

            if pos >= scratch.len() { break; }
        }

        //
        // Pad with sentinel (empty) entries for lines beyond scratch content
        // (e.g. requesting past EOF). The loop below handles them gracefully.
        //
        while line_offsets.len() < line_count as usize {
            checked_push!(
                line_offsets,
                (scratch.len(), scratch.len()),
                cfg=editor.logger_config
            );
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
            checked_push!(lines, ll, cfg=editor.logger_config);
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

            checked_push!(
                glyphs,
                Glyph { x: local_x, color, char: ch, gpu_glyph, byte_offset: abs_byte as _ },
                cfg=editor.logger_config
            );

            local_x  += advance;
            abs_byte += ch.len_utf8();
        }

        ll.glyph_start = glyph_start;
        ll.glyph_count = glyphs.len() as u32 - glyph_start;
        ll.width = local_x;

        visible_glyph_count += ll.glyph_count;

        checked_push!(lines, ll, cfg=editor.logger_config);
    }

    // :Metrics
    // let actual_glyph_count = glyphs.len();
    // eprintln!("[Approximated glyph count]: {approximate_glyph_count}");
    // eprintln!("[Actual       glyph count]: {actual_glyph_count}");

    let render_settings = editor.hooks.text_layout_render_settings.map(
        |f| f(editor, view_id)
    ).unwrap_or_default();

    TextLayout {
        buffer_id,
        rect,
        line_h,
        font_size,
        glyphs,
        lines,
        visible_glyph_count,
        line_offsets,
        render_settings,
        glyph_insertion_ids: Default::default(),
        view_scroll: view.scroll,
        first_buffer_line: first_line,
    }
}

pub fn render_text_layout(
    gpu:         &mut Gpu,
    buffer:      &Buffer,
    view:        &View,
    active_view_id: ViewId,
    scale:       f32,
    show_cursor: bool,
    is_our_window_focused: bool,
    scratch_paren: &mut Vec<char>,
) {
    let _tracy = tracy::span!("render_text_layout");

    let Some(layout) = &view.layout else { return };

    let is_this_view_focused = is_our_window_focused && active_view_id == view.id;

    let line_y = |buffer_line: u32| -> f32 {
        layout.rect.y + buffer_line as f32 * layout.line_h - view.scroll_anim
    };

    let rect         = layout.rect;
    let line_h       = layout.line_h;
    let font_size    = layout.font_size;
    let min_cursor_w = scale_base_cursor_width(scale);
    let cursor_h     = scale_base_cursor_height(scale);
    let cursor_outline_thickness = scale_base_cursor_outline_thickness(scale);
    let padding_left = padding_left(layout.should_pad_left_when_rendering);
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
                let y = line_y(line_index) + cursor_h*2.0;

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
                    if end_col == 0 { continue }  // Don't draw newline characters

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
    if layout.should_highlight_current_line_when_rendering
        && let Some(ll) = layout.line_for_buffer_line(cursor_line)
    {
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
        let cursor_width = if layout.cursor_style == CursorStyle::Stick {
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
    if show_cursor && is_this_view_focused
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

        let cursor_color    = palette().cursor_text.into();
        let paste_highlight = palette().paste_highlight.into();
        let copy_highlight  = palette().copy_highlight.into();

        // Precompute insertion ts
        let mut animated_regions_ts = [1.0f32; PASTE_ANIMATION_MAX_ID * 2 + 2]; // [0] = 1.0 sentinel
        for a in buffer.currently_animated_pastes.iter().chain(buffer.currently_animated_copies.iter()) {
            animated_regions_ts[a.id as usize] = a.t;
        }

        for ll in &layout.lines {
            let glyphs = ll.glyphs(&layout.glyphs);
            if glyphs.is_empty() { continue; }

            let y = line_y(ll.buffer_line) + line_h;

            let cursor_col_glyph_index = if ll.buffer_line == cursor_line
                && is_this_view_focused
                && layout.cursor_style == CursorStyle::Block
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
                paste_highlight,
                copy_highlight,
                &layout.glyph_insertion_ids,
                ll.glyph_start as usize,
                animated_regions_ts
            );
        }
    }

    //
    //
    // Cursor (on the UNfocused view (outlined rectangle))
    //
    //
    if show_cursor && !is_this_view_focused && layout.cursor_style == CursorStyle::Block
        && let Some(ll) = layout.line_for_buffer_line(cursor_line)
    {
        let cursor_glyph_w = layout.glyph_width_at_col(cursor_col, min_cursor_w, ll).max(min_cursor_w);
        let rect = cursor_rect(cursor_glyph_w);
        gpu::draw_rect_outline(gpu, rect.x, rect.y, rect.w, rect.h, cursor_outline_thickness, palette().cursor);
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

#[derive(Eq, PartialEq, PartialOrd, Ord, Debug, Copy, Clone, Default)]
pub struct CustomPanel {
    pub extra0: u32, pub extra1: u32, pub extra2: u32,
}

impl CustomPanel {
    pub const UNIT: Self = unsafe { core::mem::zeroed() };
}

#[derive(Copy, Clone, Debug)]
pub enum PanelKind {
    Leaf { view_id: ViewId },
    Split(PanelSplit),
    Custom(CustomPanel),
}

impl PanelKind {
    #[track_caller]
    #[inline]
    pub const fn as_custom(&self) -> CustomPanel {
        match self {
            Self::Custom(c) => *c,

            _ => unreachable!()
        }
    }
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
    pub scroll_vel:    f32,

    pub cursor_anim_x: f32,  // Animated cursor screen position @Redundant (We currently only animate cursor's Y movements)
    pub cursor_anim_y: f32,  // Animated cursor screen position

    pub cursor_target_line: u32,
    pub cursor_target_col:  u32,

    pub cursor: Cursor,
    pub layout: Option<TextLayout>,

    pub persistent_state_per_buffer: FastHashMap<BufferId, ViewState>,
}

impl View {
    pub fn new_with_scroll(id: ViewId, buffer_id: BufferId, scroll: f32) -> Self {
        Self {
            id, buffer_id, scroll, cursor: Cursor::new(), layout: None,
            cursor_anim_x: f32::NAN,
            cursor_anim_y: f32::NAN,
            scroll_vel: 0.0,
            cursor_target_line: 0, cursor_target_col: 0,
            scroll_anim: scroll,
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

#[derive(Default)]
pub struct EditorLoggerConfig {
    pub log_checked_reserves: bool,
    pub log_checked_pushes:   bool,
}

impl EditorLoggerConfig {
    #[inline]
    pub fn new() -> Self {
        let debug = std::env::var("LOG_ALLOCATIONS").is_ok();
        Self {
            log_checked_reserves: debug,
            log_checked_pushes:   debug,
        }
    }
}

pub struct EditorCustomData(pub Option<Box<dyn Any>>);

impl Deref for EditorCustomData {
    type Target = Option<Box<dyn Any>>;
    fn deref(&self) -> &Self::Target { &self.0 }
}

impl DerefMut for EditorCustomData {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 }
}

impl EditorCustomData {
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn get<T: 'static>(&self) -> &T {
        #[cfg(debug_assertions)]
        {
            self.as_ref()
                .unwrap_or_else(|| panic!(
                    "EditorCustomData::get::<{}>() called but custom data was never initialized. \
                     Call editor.custom_data.set() before accessing it.",
                    std::any::type_name::<T>()
                ))
                .downcast_ref::<T>()
                .unwrap_or_else(|| panic!(
                    "EditorCustomData::get::<{}>() failed: custom data was initialized with a different type ({:?}).",
                    std::any::type_name::<T>(),
                    self.as_ref().unwrap().type_id()
                ))
        }
        #[cfg(not(debug_assertions))]
        unsafe { &*(self.as_ref().unwrap_unchecked().as_ref() as *const dyn Any as *const T) }
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn get_mut<T: 'static>(&mut self) -> &mut T {
        #[cfg(debug_assertions)]
        {
            let type_id = self.as_ref().map(|b| b.type_id());
            self.as_mut()
                .unwrap_or_else(|| panic!(
                    "EditorCustomData::get_mut::<{}>() called but custom data was never initialized. \
                     Call editor.custom_data.set() before accessing it.",
                    std::any::type_name::<T>()
                ))
                .downcast_mut::<T>()
                .unwrap_or_else(|| panic!(
                    "EditorCustomData::get_mut::<{}>() failed: custom data was initialized with a different type ({:?}).",
                    std::any::type_name::<T>(),
                    type_id
                ))
        }
        #[cfg(not(debug_assertions))]
        unsafe { &mut *(self.as_mut().unwrap_unchecked().as_mut() as *mut dyn Any as *mut T) }
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn set<T: 'static>(&mut self, value: impl Into<Box<T>>) -> Option<Box<T>> {
        #[cfg(debug_assertions)]
        { self.replace(value.into()).map(|c| c.downcast().unwrap()) }
        #[cfg(not(debug_assertions))]
        unsafe { self.replace(value.into()).map(|c| Box::from_raw(Box::into_raw(c) as *mut T)) }
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

    pub logger_config: EditorLoggerConfig,

    // Scale for font/line-height
    pub scale: f32,
    pub win_w: f32,
    pub win_h: f32,
    pub is_our_window_focused: bool,

    // Cursor blink
    pub blink_epoch:         Instant,
    pub last_cursor_visible: bool,

    pub last_messager_count: u32,

    // Mouse
    pub mouse_pos:          (f32, f32),
    pub mouse_left_pressed: bool,
    pub is_cursor_visible:  bool,

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

    pub hooks: Hooks,
    pub custom_data: EditorCustomData,

    pub director:        Director,
    pub messager:        Messager,
    pub audioer:         Audioer,
}

impl Deref for Editor {
    type Target = EditorCustomData;
    fn deref(&self) -> &Self::Target { &self.custom_data }
}

impl DerefMut for Editor {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.custom_data }
}

impl Editor {
    pub fn new(audioer: Audioer, logger_config: EditorLoggerConfig) -> Self {
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

        let canonicalized_current_working_directory: SmallString<[_; _]> = std::env::args().nth(1)
            .and_then(|p| Path::new(&p).parent().map(|p| p.to_path_buf()))
            .and_then(|p| std::fs::canonicalize(p).ok())
            .unwrap_or_else(|| std::fs::canonicalize(".").unwrap_or_default())
            .into_os_string()
            .into_string()
            .unwrap()
            .into();

        let canonicalized_path_to_buffer_id = FastHashMap::with_capacity_and_hasher(128, Default::default());

        let mut editor = Self {
            buffers,
            views,
            panels,
            canonicalized_path_to_buffer_id,
            logger_config,
            hooks: Default::default(),
            last_input_time: Instant::now(),
            win_h: 0.0,
            win_w: 0.0,
            last_cursor_visible: false,
            is_cursor_visible: true,
            buffer_cycle_index: None,
            custom_data: EditorCustomData(None),
            last_messager_count: u32::MAX,
            scratch_paren: Vec::with_capacity(256),
            active_panel: root_panel,
            root_panel,
            scale:        1.0,
            blink_epoch:  Instant::now(),
            last_frame_time:  Instant::now(),
            mouse_pos:    (0.0, 0.0),
            is_our_window_focused: false,
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
    pub fn custom_data<T: 'static>(&self) -> &T {
        self.custom_data.get()
    }

    #[inline]
    pub fn custom_data_mut<T: 'static>(&mut self) -> &mut T {
        self.custom_data.get_mut()
    }

    #[inline]
    pub fn set_custom_data<T: 'static>(&mut self, value: impl Into<Box<T>>) -> Option<Box<T>> {
        self.custom_data.set(value)
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
            PanelKind::Split(_) => VIEW_MAIN,

            PanelKind::Custom(c) => {
                if let Some(active_view_id_hook) = self.hooks.active_view_id {
                    active_view_id_hook(self, c)
                } else {
                    unreachable!()
                }
            }
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
        if let Some(layout_panels_hook) = self.hooks.layout_panels {
            layout_panels_hook(self, win_rect);
        }
    }

    pub fn layout_panel(&mut self, id: PanelId, rect: Rect) {
        self.panels[id].rect = rect;

        let split = match self.panels[id].kind {
            PanelKind::Split(s) => s,
            PanelKind::Leaf { .. } => return,

            _other => {
                if let Some(layout_panel_hook) = self.hooks.layout_panel {
                    layout_panel_hook(self, id, rect);
                }

                return;
            }
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
        let should_short_circuit = self.hooks.register_new_buffer_in_most_recently_used_list.map_or(
            false,
            |f| f(self, buffer_id)
        );

        if should_short_circuit { return }

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
        let should_short_circuit = self.hooks.set_active_panel.map_or(
            false,
            |f| f(self, panel_id)
        );

        if should_short_circuit {
            return;
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

pub const MIN_SCALE:  f32 = 0.75;
pub const MAX_SCALE:  f32 = 5.00;
pub const SCALE_STEP: f32 = 0.25;

// :FeelImprovement
//
// For whatever reason, when I user scrolls with move_down/move_up,
// meaning that the cursor goes a tiny bit of screen, it starts to jiggle
// or flicker a bit, this isn't good.

// nocheckin @Incomplete: Make these frame rate dependent
pub const SCROLL_ANIM_RATE: f32 = 23.67;
pub const CURSOR_ANIM_RATE: f32 = 99.420;

pub const BLINK_ON_MS:  u128 = 530;
pub const BLINK_OFF_MS: u128 = 370;

pub const BLINK_START_DELAY_MS: u128 = 500;  // Start blinking after 500ms idle
pub const BLINK_STOP_IDLE_MS:   u128 = 5000; // Stop  blinking after 5s    idle

pub const         DELETE_ANIM_DURATION: f32 = 0.115; // nocheckin @Tune

pub const ANIMATED_RANGE_ANIM_DURATION: f32 = 1.98;  // nocheckin @Tune

pub const PASTE_ANIMATION_BITS:     usize = 4;
pub const PASTE_ANIMATION_PER_WORD: usize = 64  / PASTE_ANIMATION_BITS;        // 16
pub const PASTE_ANIMATION_MASK:     u64   = (1 << PASTE_ANIMATION_BITS) - 1;   // 0b1111

pub const PASTE_ANIMATION_MAX_ID: usize = 7;  // pastes: 1..=7
pub const COPY_ANIMATION_MAX_ID:  usize = 15; // copies: 8..=15

pub fn layout_update_currently_animated_regions(
    layout: &mut TextLayout,

    pastes: &[AnimatedRegion],
    copies: &[AnimatedRegion],
) {
    layout.glyph_insertion_ids.clear();
    if pastes.is_empty() && copies.is_empty() { return; }

    let n     = layout.glyphs.len();
    let words = (n + PASTE_ANIMATION_PER_WORD - 1) / PASTE_ANIMATION_PER_WORD;

    layout.glyph_insertion_ids.resize(words, 0u64);

    for (i, g) in layout.glyphs.iter().enumerate() {
        let byte = g.byte_offset as usize;
        let anims = copies.iter().chain(pastes.iter());  // Copies take priority!
        for a in anims {
            if byte >= a.byte_start as usize && byte < a.byte_start as usize + a.byte_len as usize {
                let word =  i / PASTE_ANIMATION_PER_WORD;
                let bit  = (i % PASTE_ANIMATION_PER_WORD) * PASTE_ANIMATION_BITS;
                layout.glyph_insertion_ids[word] |= (a.id as u64) << bit;
                break;
            }
        }
    }
}

pub fn cursor_visible(epoch: &Instant, last_input: &Instant) -> bool {
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

pub fn animate(editor: &mut Editor, dt: f32) -> bool {
    let _tracy = tracy::span!("animate");

    let mut still_animating = false;

    let epsilon = 0.5f32;       // Stop animating when close enough
    let line_h  = editor.line_h();

    for view in editor.views.values_mut() {
        //
        // Scroll
        //
        {
            let dist = (view.scroll - view.scroll_anim).abs();

            let stiffness = 300.0 + (dist * 8.0).min(2200.0);
            let damping = 2.0 * stiffness.sqrt();

            let target = view.scroll;
            let x      = view.scroll_anim;
            let v      = view.scroll_vel;

            let delta = target - x;

            // Spring force
            let accel = stiffness * delta - damping * v;

            view.scroll_vel  += accel * dt;
            view.scroll_anim += view.scroll_vel * dt;

            view.scroll_vel *= 0.98;  // Soft damping

            if delta.abs() > 0.01 || view.scroll_vel.abs() > 0.01 {
                still_animating = true;
            } else {
                view.scroll_anim = target;
                view.scroll_vel  = 0.0;
            }

            view.scroll_anim = view.scroll_anim.round();
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

            } else if dy.abs() < line_h * 1.5 {
                // Single line down, just snap don't animate
                view.cursor_anim_y = target_y;

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
    // Advance animations regions in every buffer
    //
    for buffer in editor.buffers.values_mut() {
        let before = buffer.currently_animated_pastes.len() + buffer.currently_animated_copies.len();
        for vec in [&mut buffer.currently_animated_pastes, &mut buffer.currently_animated_copies] {
            vec.retain_mut(|a| {
                a.t = (a.t + dt / ANIMATED_RANGE_ANIM_DURATION).min(1.0);
                a.t < 1.0
            });
        }

        let after = buffer.currently_animated_pastes.len() + buffer.currently_animated_copies.len();
        if after < before {
            buffer.is_dirty = true;  // @Hack nocheckin @DocumentThis
        }

        if !buffer.currently_animated_pastes.is_empty()
        || !buffer.currently_animated_copies.is_empty()
        {
            still_animating = true;
        }
    }

    //
    // Advance deletion animations per buffer
    //
    for buffer in editor.buffers.values_mut() {
        let before = buffer.currently_animated_deletions.len();
        buffer.currently_animated_deletions.retain_mut(|a| {
            a.t = (a.t + dt / DELETE_ANIM_DURATION).min(1.0);
            a.t < 1.0
        });
        if buffer.currently_animated_deletions.len() < before {
            buffer.is_dirty = true; // @Hack nocheckin @DocumentThis
        }
        if !buffer.currently_animated_deletions.is_empty() {
            still_animating = true;
        }
    }

    if let Some(animate_hook) = editor.hooks.animate {
        animate_hook(editor, dt, &mut still_animating);
    }

    still_animating
}

pub fn find_matching_paren(
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

    let mut stack = SmallVec::<[PanelId; 48]>::with_capacity((panels.len() as f32 * 1.5) as usize);

    if let Some(collect_leaf_panels_init_stack) = editor.hooks.collect_leaf_panels_init_stack {
        collect_leaf_panels_init_stack(editor, id, &mut stack);
    }

    stack.push(id);

    while let Some(id) = stack.pop() {
        match panels[id].kind {
            PanelKind::Leaf { view_id } => out.push((id, view_id, panels[id].rect)),

            PanelKind::Split(split) => {
                stack.push(split.right_id);
                stack.push(split.left_id);
            }

            PanelKind::Custom(c) => {
                if let Some(collect_leaf_panels_hook) = editor.hooks.collect_leaf_panels {
                    let leaves = collect_leaf_panels_hook(editor, id, c, &mut stack);
                    out.extend(leaves);
                }
            }
        }
    }
}

pub fn apply_scale(editor: &mut Editor, new_scale: f32, anchor_my: Option<f32>) {
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

pub fn rescale(editor: &mut Editor, gpu: &mut Gpu, new_scale: f32) {
    let new = new_scale.clamp(MIN_SCALE, MAX_SCALE);
    if new != editor.scale {
        reset_atlas(gpu);
        apply_scale(editor, new, None);
        force_layouts_from_all_views_to_rebuild(editor);
    }
}

pub fn force_layouts_from_all_views_to_rebuild(editor: &mut Editor) {
    for view in editor.views.values_mut() {
        view.layout = None;
    }
}

pub fn scroll_page(editor: &mut Editor, _gpu: &Gpu, direction: i32) {
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

pub fn editor_handle_left_mouse_click(cx: &mut CommandContext) -> bool {
    let mut should_request_redraw = false;

    //
    // Custom layer takes priority
    //
    let (
        custom_window_redraw_requested,
        should_short_circuit
    ) = cx.editor.hooks.left_mouse_clicked.map_or(
        (false, false),
        |f| f(cx)
    );

    should_request_redraw |= custom_window_redraw_requested;

    if should_short_circuit {
        return should_request_redraw;
    }

    let (mx, my) = cx.editor.mouse_pos;
    let pid = cx.editor.panel_at(mx, my).unwrap_or(cx.editor.active_panel);

    let PanelKind::Leaf { view_id } = cx.editor.panels[pid].kind else {
        return should_request_redraw;
    };

    should_request_redraw |= true;

    let rect        = cx.editor.panels[pid].rect;
    let buf_id      = cx.editor.views[view_id].buffer_id;
    let scroll_anim = cx.editor.views[view_id].scroll_anim;

    let (line, col) = if let Some(layout) = &cx.editor.views[view_id].layout {
        layout.hit_test(mx, my, scroll_anim)
    } else {  // @Robustness
        let line_h = cx.editor.line_h();
        let line = ((my - rect.y + cx.editor.views[view_id].scroll) / line_h) as usize;
        let line = line.min(cx.editor.buffers[buf_id].text.len_lines().saturating_sub(1));
        (line as u32, 0)  // col 0 - no glyph metrics without layout
    };

    let view = &mut cx.editor.views[view_id];
    cx.editor.buffers[buf_id].set_cursor_line_col(line, col, &mut view.cursor);
    view.cursor_target_line = line;
    view.cursor_target_col  = col;

    if cx.editor.mouse_left_pressed {
        if !view.cursor.is_anchor_set() {
            view.cursor.set_anchor();
        }
    } else {
        view.cursor.unset_anchor();
    }

    cx.editor.set_active_panel(pid);
    cx.editor.reset_blink();

    should_request_redraw
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
            let target_y = layout.rect.y + line as f32 * line_h;
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

pub fn does_view_need_layout_rebuild(
    editor: &Editor,

    view: ViewId, buffer: BufferId,

    rect: Rect,
) -> bool {
    let font_size    = editor.font_size();
    let line_h       = editor.line_h();

    let screen_lines = (rect.h / line_h) as u32;
    let anim_first = (editor.views[view].scroll_anim / line_h) as u32;

    let is_dirty = editor.buffers[buffer].is_dirty || editor.views[view].layout.as_ref().map(|l| {
        anim_first < l.first_buffer_line
            || (
                anim_first + screen_lines > l.first_buffer_line + l.lines.len() as u32
                    && screen_lines < l.lines.len() as u32
            )
            || (l.rect.w - rect.w).abs() > 0.5
            || (l.rect.h - rect.h).abs() > 0.5
            || (l.font_size - font_size).abs() > 0.01
    }).unwrap_or(true);

    let is_custom_dirty = editor.hooks.does_view_need_layout_rebuild.map_or(
        false,
        |f| f(editor, view, buffer, rect)
    );

    is_dirty || is_custom_dirty
}

pub fn open_initial_buffer(editor: &mut Editor) {
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
