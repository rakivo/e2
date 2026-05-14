#![feature(likely_unlikely)]
#![allow(non_camel_case_types)]

// TODO: Mouse double left click should select the word

// TODO: Multi-cursors

// TODO: Undo+redo
// TODO: mark-sexp
// TODO: move-line
// TODO: backward-list/forward-list
// TODO: backward-list/forward-list
// TODO: beginning-of-defun/end-of-defun
// TODO: align-rexegp

// TODO: [messages] buffer

// TODO: Lexer support for HERE strings

// TODO: Lexer is STILL buggy with escapes in chars or strings

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
pub mod ts;
pub mod atum;

use audioer::Audioer;
use lexer::token_color;
use messager::{MAX_MESSAGE_COUNT, MESSAGE_DURATION_IN_MILLISECONDS, MESSAGER_FONT_SIZE, Messager};
use session::CustomChunkId;
use buffer::{AnimatedRegion, Buffer, Cursor};
use color::{Color, GpuColor};
use command::{CommandContext, CommandAtom};
use director::Director;
use ts::TreeSitter;

use std::any::Any;
use std::num::NonZero;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use std::fmt::Write as _;
use std::collections::VecDeque;
use std::ops::{BitOr, BitAnd, BitOrAssign, BitAndAssign};

use cranelift_entity::packed_option::ReservedValue;
use cranelift_entity::{EntityRef, PrimaryMap, SecondaryMap};
use smallstr::SmallString;
use smallvec::SmallVec;
use wgpu::naga::{FastHashMap, FastHashSet};
use winit::window::Window;
use gpu::{Gpu, GpuGlyph, draw_text_for_editor};

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

        pub animate:        fn (&mut Editor, dt: f32, still_animating: &mut ShouldRequestFrameRedraw),

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

        pub inside_about_to_wait_should_request_redraw: fn (&mut Editor) -> ShouldRequestFrameRedraw,
        pub        at_the_end_of_redraw_should_request_redraw: fn (&mut Editor) -> ShouldRequestFrameRedraw,

        /// You usually wanna do your ticks here.
        pub about_to_redraw_a_frame:        fn (&mut CommandContext, dt: f32) -> ShouldRequestFrameRedraw,

        pub about_to_rebuild_dirty_layouts: fn (&mut CommandContext)          -> ShouldRequestFrameRedraw,

        /// Rebuilt all dirty layouts, about to animate()!
        pub rebuilt_all_dirty_layouts:      fn (&mut CommandContext)          -> ShouldRequestFrameRedraw,

        /// Animated all animations,   about to draw!
        pub animated_all_animations:        fn (&mut CommandContext)          -> ShouldRequestFrameRedraw,

        /// Returns `true` if should skip rendering this specific panel.
        pub about_to_draw_this_panel:       fn (&mut CommandContext, PanelId, ViewId, Rect) -> bool,

        pub about_to_draw_selection:                             fn (&mut Editor, &mut Gpu, ViewId, &LayoutRenderingContext),
        pub drew_selection_about_to_draw_current_line_highlight: fn (&mut Editor, &mut Gpu, ViewId, &LayoutRenderingContext),
        pub drew_current_line_highlight_about_to_draw_cursor:    fn (&mut Editor, &mut Gpu, ViewId, &LayoutRenderingContext),
        pub drew_cursor_about_to_draw_text:                      fn (&mut Editor, &mut Gpu, ViewId, &LayoutRenderingContext),
        pub drew_text_about_to_return:                           fn (&mut Editor, &mut Gpu, ViewId, &LayoutRenderingContext),

        pub should_view_have_panel_bar:     fn (&Editor, ViewId) -> bool,
        /// NOTE: Custom layer should do `write!(&mut editor.scratch_panel_bar)`!
        pub format_panel_bar:               fn (&mut Editor, ViewId),
        /// First return is the color of the panel bar itself,
        /// the second (optional) return is the color of the border separating the bar and the editor's background.
        pub panel_bar_color:                fn (&mut Editor, ViewId) -> (Color, Option<Color>),

        pub render_panel_bar:               fn (&mut Editor, &mut Gpu, ViewId),

        pub drew_all_leaf_panels:           fn (&mut CommandContext)          -> ShouldRequestFrameRedraw,

        pub opened_file:                    fn (&mut Editor, inside: BufferId),
        /// NOTE: Use [`Buffer::last_insert`] and [`Buffer::last_delete`] if you want to get the latest modification info.
        pub modified_file:                  fn (&mut Editor, inside: BufferId),

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

        pub dont_serialize_these_buffers_while_saving_session: FastHashSet<BufferId>,

        pub session_save_chunks: Vec<
        fn(
            &Editor,
            &FastHashMap<ViewId, u32>,   // View serial index map
            &FastHashMap<BufferId, u32>, // Buffer serial index map
        ) -> Option<(CustomChunkId, Vec<u8>)>>,

        pub session_restore_chunks: Vec<
        fn(
            &mut Editor,
            CustomChunkId, // chunk ID
            &[u8],         // chunk data
            &[ViewId],     // serial index -> real ViewId
            &[BufferId]    // serial index -> real BufferId
        )>,

        pub exiting:                                fn (&mut Editor),

        pub collect_leaf_panels_init_stack:         fn (&Editor, root: PanelId,              stack: &mut SmallVec<[PanelId; 12]>),
        pub collect_leaf_panels:                    fn (&Editor, root: PanelId, CustomPanel, stack: &mut SmallVec<[PanelId; 12]>) -> SmallVec<[(PanelId, ViewId, Rect, Rect); 2]>, // @Memory
        pub collect_leaf_panels_for_session_saving: fn (&Editor, root: PanelId, CustomPanel, stack: &mut SmallVec<[PanelId; 12]>) -> SmallVec<[(PanelId, ViewId); 2]>,

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
        // cursor:           Color::hex(0xc3a983),
        cursor:           Color::hex(0xff0014),
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
        2.0 // nocheckin
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

pub const ADDITIONAL_BOTTOM_SCROLL_SPACE: f32 = 50.0;

define_base_and_scale! {
    const BASE_LINE_HEIGHT:   f32 = 16.35;
    const BASE_FONT_SIZE:     f32 = 15.0;
    const BASE_CURSOR_HEIGHT: f32 = 2.0;
    const BASE_CURSOR_WIDTH:  f32 = 2.0;
    const BASE_CURSOR_OUTLINE_THICKNESS: f32 = 1.2;
    const BASE_PANEL_BAR_BORDER_THICKNESS: f32 = 1.0;
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

    rect: Rect
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
        rect
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

    rect: Rect
) -> TextLayout {
    let _tracy = tracy::span!("build_text_layout");

    let old_layout = editor.views[view_id].layout.take();

    let font_size = editor.font_size();
    let line_h = editor.line_h();

    let view      = &editor.views[view_id];
    let buffer_id = view.buffer_id;

    //
    // Calculate the base visible range based on the animation (what we see now)
    //
    let mut first_line = (view.scroll_anim.round() / line_h).floor() as u32;
    let mut last_line  = ((view.scroll_anim.round() + rect.h) / line_h).ceil() as u32;

    //
    // Add a tiny bit of padding so lines don't pop at the very edges
    //
    first_line = first_line.saturating_sub(20);
    last_line  = last_line.saturating_add(20);

    //
    // If the animation is moving DOWN, pad the BOTTOM more.
    // If the animation is moving UP,   pad the TOP    more.
    //
    let delta = view.scroll - view.scroll_anim;
    if delta > 0.0 {
        // We are scrolling DOWN (target is below current anim)
        // Add prelex by remaining distance
        let prelex_line_count = (delta / line_h).clamp(100.0, 400.0) as u32;
        last_line = last_line.saturating_add(prelex_line_count);
    } else if delta < 0.0 {
        // We are scrolling UP (target is above current anim)
        // Add prelex by remaining distance
        let prelex_line_count = ((-delta) / line_h).clamp(100.0, 400.0) as u32;
        first_line = first_line.saturating_sub(prelex_line_count);
    }

    let total_lines = editor.buffers[buffer_id].text.len_lines() as u32;
    first_line = first_line.min(total_lines);
    last_line  = last_line.min(total_lines);

    let line_count = last_line.max(first_line) - first_line;

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

/// NOTE: Most fields here are just copies either from the layout or the view OR the Editor,
/// and this is done intentionally, in the purposes of convenience.
pub struct LayoutRenderingContext {
    pub view_id: ViewId,
    pub view_scroll_anim: f32,
    pub view_cursor_anim_x: f32,
    pub view_cursor_anim_y: f32,

    pub layout_cursor_style: CursorStyle,

    pub show_cursor: bool,

    pub cursor_col:  u32,
    pub cursor_line: u32,

    pub rect: Rect,

    // @Redundant
    pub line_h: f32,
    pub font_size: f32,
    pub cursor_h: f32,
    pub cursor_w: f32,
    pub min_cursor_w: f32,
    pub origin_x: f32,

    pub first_visible_line: u32,
    pub last_visible_line:  u32,
}

impl LayoutRenderingContext {
    pub fn line_y(&self, buffer_line: u32) -> f32 {
        self.rect.y + buffer_line as f32 * self.line_h - self.view_scroll_anim.round()
    }

    pub fn cursor_rect(&self, cursor_glyph_w: f32) -> Rect {
        let cursor_width = if self.layout_cursor_style == CursorStyle::Stick {
            self.cursor_w
        } else {
            cursor_glyph_w
        };

        Rect {
            x: self.view_cursor_anim_x,
            y: self.view_cursor_anim_y + self.cursor_h,
            w: cursor_width,
            h: self.line_h + self.cursor_h,
        }
    }
}

pub fn render_text_layout(
    editor:      &mut Editor,
    gpu:         &mut Gpu,
    view_id:     ViewId,
    show_cursor: bool,
) {
    let _tracy = tracy::span!("render_text_layout");

    let view = &editor.views[view_id];
    let Some(layout) = &view.layout else { return };

    let buffer_id = view.buffer_id;
    let buffer = &editor.buffers[buffer_id];

    let scale = editor.scale;

    let active_view_id = editor.active_view_id();
    let is_our_window_focused = editor.is_our_window_focused;
    let is_this_view_focused = is_our_window_focused && active_view_id == view.id;

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

    let context = LayoutRenderingContext {
        cursor_col,
        cursor_line,
        origin_x,
        min_cursor_w,
        first_visible_line: vis_start,
        last_visible_line: vis_end,
        font_size,
        line_h,
        show_cursor,
        rect,
        view_id,
        view_scroll_anim: view.scroll_anim,
        cursor_h,
        cursor_w: scale_base_cursor_width(scale),
        layout_cursor_style: layout.cursor_style,
        view_cursor_anim_x: view.cursor_anim_x,
        view_cursor_anim_y: view.cursor_anim_y,
    };

    //
    //
    // Selection
    //
    //

    if let Some(hook) = editor.hooks.about_to_draw_selection {
        hook(editor, gpu, view_id, &context);
    }

    // Reload borrows...
    let view = &editor.views[view_id];
    let Some(layout) = &view.layout else { return };

    let buffer_id = view.buffer_id;
    let buffer = &editor.buffers[buffer_id];

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
                let y = context.line_y(line_index) + cursor_h*2.0;

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

    if let Some(hook) = editor.hooks.drew_selection_about_to_draw_current_line_highlight {
        hook(editor, gpu, view_id, &context);
    }

    // Reload borrows again...
    let view = &editor.views[view_id];
    let Some(layout) = &view.layout else { return };
    let buffer_id = view.buffer_id;
    let buffer = &editor.buffers[buffer_id];

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

    if let Some(hook) = editor.hooks.drew_current_line_highlight_about_to_draw_cursor {
        hook(editor, gpu, view_id, &context);
    }

    // Reload borrows again...
    let view = &editor.views[view_id];
    let Some(layout) = &view.layout else { return };

    //
    //
    // Cursor (on the focused view (filled in rectangle))
    //
    //

    if show_cursor && is_this_view_focused
        && let Some(ll) = layout.line_for_buffer_line(cursor_line)
    {
        let cursor_glyph_w = layout.glyph_width_at_col(cursor_col, min_cursor_w, ll).max(min_cursor_w);
        let crect = context.cursor_rect(cursor_glyph_w);

        const TRAIL_STEPS: usize = 40;

        let dist = ((crect.x - view.cursor_ghost_x).powi(2) + (crect.y - view.cursor_ghost_y).powi(2)).sqrt();

        if dist > 2.0 {
            for i in 0..TRAIL_STEPS {
                let t = i as f32 / TRAIL_STEPS as f32; // 0 = ghost, 1 = cursor
                let x = view.cursor_ghost_x + (crect.x - view.cursor_ghost_x) * t;
                let y = view.cursor_ghost_y + (crect.y - view.cursor_ghost_y) * t;
                let alpha = t * 0.4; // linear, fades to nothing at ghost end
                let w = cursor_glyph_w * 0.6; // @Tune, same size throughout
                let h = crect.h * 0.7;        // @Tune
                let y_centered = y + (crect.h - h) * 0.5;
                let x_centered = x + (crect.w - w) * 0.5;
                gpu::draw_rect(gpu, x_centered, y_centered, w, h, palette().cursor.with_alpha(alpha));
            }
        }

        gpu::draw_rect(gpu, crect.x, crect.y, crect.w, crect.h, palette().cursor.with_alpha(view.cursor_opacity));
    }

    if let Some(hook) = editor.hooks.drew_cursor_about_to_draw_text {
        hook(editor, gpu, view_id, &context);
    }

    // Reload borrows again...
    let view = &editor.views[view_id];
    let Some(layout) = &view.layout else { return };
    let buffer_id = view.buffer_id;
    let buffer = &editor.buffers[buffer_id];

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

            let y = context.line_y(ll.buffer_line) + line_h;

            // :Configuration
            // let cursor_col_glyph_index = None;
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

    // :TreeSitter :Configuration
    //
    // Function call overlay
    //
    if is_this_view_focused &&
        let Some(overlay) = &view.overlay.current &&
        let Some(ll) = layout.line_for_buffer_line(cursor_line)
    {
        let cursor_glyph_w =
            layout.glyph_width_at_col(cursor_col, min_cursor_w, ll)
            .max(min_cursor_w);

        let crect = context.cursor_rect(cursor_glyph_w);

        let Some(info) = crate::ts::lookup_overlay(
            overlay,
            &editor.tree_sitter.func_table,
            &editor.tree_sitter.atom_table
        ) else {
            return;
        };

        let pad_x     = 6.0;
        let pad_y_top = 4.0;
        let pad_y_bot = 6.0;
        let font_size = 14.0;

        let mut scratch: SmallVec<[u8; 256]> = SmallVec::new();

        //
        // Pre-measure total scratch size and record (start, end, is_active) per param
        //
        struct PieceRange { start: u32, end: u32, active: bool }
        let mut param_ranges: SmallVec<[PieceRange; 8]> = Default::default();

        for (i, param) in info.params.iter().enumerate() {
            let name     = editor.tree_sitter.atom_table.lookup_ref(param.name);
            let type_str = editor.tree_sitter.atom_table.lookup_ref(param.type_str);
            let start    = scratch.len() as _;
            scratch.extend_from_slice(name.as_bytes());
            scratch.extend_from_slice(b": ");
            scratch.extend_from_slice(type_str.as_bytes());
            let end      = scratch.len() as _;
            param_ranges.push(PieceRange { start, end, active: i == overlay.arg_index as usize });
        }

        //
        // Now scratch is done growing, safe to slice
        //
        let func_name = editor.tree_sitter.atom_table.lookup_ref(overlay.call_kind.function_name());
        let mut pieces: SmallVec<[(&str, bool); 16]> = SmallVec::new();
        pieces.push((func_name.as_str(), false));
        pieces.push(("(", false));
        for (i, pr) in param_ranges.iter().enumerate() {
            if i > 0 { pieces.push((", ", false)); }
            let s = unsafe {
                std::str::from_utf8_unchecked(&scratch[pr.start as usize..pr.end as usize])
            };
            pieces.push((s, pr.active));
        }
        pieces.push((")", false));

        //
        // Measure
        //
        let mut text_w: f32 = 0.0;
        for (text, _) in &pieces {
            text_w += gpu::measure_str(gpu, text, font_size);
        }
        let overlay_w = text_w + pad_x * 2.0;
        let overlay_h = font_size + pad_y_top + pad_y_bot;

        //
        // Position
        //
        let margin = 8.0;
        let mut ox = crect.x + 14.0;
        let mut oy = crect.y - overlay_h - 6.0;
        if ox + overlay_w > rect.x + rect.w - margin { ox = rect.x + rect.w - overlay_w - margin; }
        if oy < rect.y + margin { oy = crect.y + crect.h + 6.0; }

        //
        // Draw
        //
        gpu::draw_rect(gpu, ox, oy, overlay_w, overlay_h, Color::hex(0x1E1E1E));
        gpu::draw_rect_outline(gpu, ox, oy, overlay_w, overlay_h, 1.0, Color::hex(0x3A3A3A));

        //
        // Text
        //
        let mut tx = ox + pad_x;
        let ty = oy + pad_y_top + font_size;
        for (text, active) in &pieces {
            let color = if *active { Color::hex(0xD7BA7D) } else { Color::hex(0x9CDCFE) };
            gpu::draw_text(gpu, text, tx, ty, font_size, color);
            if *active {
                let w = gpu::measure_str(gpu, text, font_size);
                gpu::draw_rect(gpu, tx, ty + 3.0, w, 1.0, Color::hex(0xD7BA7D));
            }
            tx += gpu::measure_str(gpu, text, font_size);
        }
    }

    if let Some(hook) = editor.hooks.drew_text_about_to_return {
        hook(editor, gpu, view_id, &context);
    }

    // Reload borrows again...
    let view = &editor.views[view_id];
    let Some(layout) = &view.layout else { return };

    //
    //
    // Cursor (on the UNfocused view (outlined rectangle))
    //
    //
    if !is_this_view_focused && layout.cursor_style == CursorStyle::Block
        && let Some(ll) = layout.line_for_buffer_line(cursor_line)
    {
        let cursor_glyph_w = layout.glyph_width_at_col(cursor_col, min_cursor_w, ll).max(min_cursor_w);
        let rect = context.cursor_rect(cursor_glyph_w);
        gpu::draw_rect_outline(gpu, rect.x, rect.y, rect.w, rect.h, cursor_outline_thickness, palette().cursor);
    }
}

pub fn find_panel_split_context(editor: &Editor, target: PanelId) -> Option<(bool /*vertical*/, bool /*is_left_or_top*/)> {
    let mut stack = SmallVec::<[PanelId; 12]>::new();

    stack.push(editor.root_panel);
    while let Some(id) = stack.pop() {
        if let PanelKind::Split(s) = editor.panels[id].kind {
            if s.left_id == target  { return Some((s.vertical, true));  }
            if s.right_id == target { return Some((s.vertical, false)); }
            stack.push(s.left_id);
            stack.push(s.right_id);
        }
    }

    None
}

pub struct BorderedEdges {
    pub top:    bool,
    pub bottom: bool,
    pub left:   bool,
    pub right:  bool,
}

pub fn find_bordered_edges(editor: &Editor, target: PanelId) -> BorderedEdges {
    let mut edges = BorderedEdges { top: false, bottom: false, left: false, right: false };
    accumulate_edges(editor, editor.root_panel, target, &mut edges);
    edges
}

pub fn accumulate_edges(editor: &Editor, current: PanelId, target: PanelId, edges: &mut BorderedEdges) -> bool {
    match editor.panels[current].kind {
        PanelKind::Split(s) => {
            if accumulate_edges(editor, s.left_id, target, edges) {
                // Target is somewhere in the left/top subtree
                if s.vertical {
                    edges.right  = true;  // Left panel gets right border
                } else {
                    edges.bottom = true;  // Top panel gets bottom border
                }

                return true;
            }

            if accumulate_edges(editor, s.right_id, target, edges) {
                // Target is somewhere in the right/bottom subtree
                if s.vertical {
                    edges.left   = true;  // Right panel gets left border
                } else {
                    edges.top    = true;  // Bottom panel gets top border
                }

                return true;
            }

            false
        }

        PanelKind::Leaf { .. } => current == target,

        // :Configuration ?
        PanelKind::Custom(_)   => current == target,
    }
}

pub fn render_panel_bar(gpu: &mut Gpu, editor: &mut Editor, view_id: ViewId) {
    if let Some(hook) = editor.hooks.render_panel_bar {
        //
        // Custom layer's hooks take priority!
        //
        hook(editor, gpu, view_id);
        return;
    }

    let view = &editor.views[view_id];
    let Some(layout) = &view.layout else { return };

    let rect = layout.rect;
    let bar_h = editor.panel_bar_h();
    let bar_y = rect.y - bar_h;

    let (panel_bar_color, border_color) = if let Some(hook) = editor.hooks.panel_bar_color {
        hook(editor, view_id)
    } else {
        (Color::hex(0x3d2a0f), None)  // @PaletteRefactor
    };

    gpu::draw_rect(gpu, rect.x, bar_y, rect.w, bar_h, panel_bar_color);

    if let Some(border_color) = border_color {
        let border_thickness = editor.panel_bar_border_thickness();
        let panel_id = editor.views[view_id].panel_id().unwrap();
        let edges    = find_bordered_edges(editor, panel_id);

        gpu::draw_rect(gpu, rect.x, bar_y + bar_h - border_thickness, rect.w, border_thickness, border_color);

        //
        // Right edge - full panel height including bar
        //
        if edges.right {
            gpu::draw_rect(gpu, rect.x + rect.w - border_thickness, bar_y, border_thickness, bar_h + rect.h, border_color);
        }

        //
        // Left edge - full panel height including bar
        //
        if edges.left {
            gpu::draw_rect(gpu, rect.x, bar_y, border_thickness, bar_h + rect.h, border_color);
        }

        //
        // Bottom of bar
        //
        if edges.bottom {
            let panel_rect = editor.views[view_id].panel_id().map_or(
                rect,
                |panel_id| editor.panels[panel_id].rect_including_panel_bar
            );
            gpu::draw_rect(
                gpu,
                panel_rect.x, panel_rect.y + panel_rect.h - border_thickness*2.0,
                panel_rect.w, border_thickness,
                border_color
            );
        }

        //
        // Top of bar
        //
        if edges.top {
            gpu::draw_rect(
                gpu,
                rect.x, bar_y,
                rect.w, border_thickness,
                border_color
            );
        }
    }

    editor.scratch_panel_bar.clear();
    if let Some(format_panel_bar) = editor.hooks.format_panel_bar {
        format_panel_bar(editor, view_id);
    }

    let pad = (bar_h-editor.font_size())/2.0;

    let center_y = bar_y + bar_h * 0.5;
    let y = center_y + editor.font_size() * 0.34; // nocheckin

    gpu::draw_text(
        gpu,
        &editor.scratch_panel_bar,
        rect.x+pad, y,
        editor.font_size(),
        Color::rgba(174, 131, 60, 255)
    );
}

pub fn render_split_seams(gpu: &mut Gpu, editor: &Editor, panel_id: PanelId, color: Color) {
    match editor.panels[panel_id].kind {
        PanelKind::Split(s) => {
            let left = editor.panels[s.left_id].rect_including_panel_bar;
            let border_thickness = editor.panel_bar_border_thickness(); // :Configuration ?

            if s.vertical {
                // Draw at the seam between left and right
                let x = left.x + left.w;
                gpu::draw_rect(gpu, x, left.y, border_thickness, left.h, color);
            } else {
                // Draw at the seam between top and bottom
                let y = left.y + left.h;
                gpu::draw_rect(gpu, left.x, y, left.w, border_thickness, color);
            }

            render_split_seams(gpu, editor, s.left_id,  color);
            render_split_seams(gpu, editor, s.right_id, color);
        }

        PanelKind::Leaf { .. } => {
            // Bar/buffer separator
            let bar_h = editor.panel_bar_h();
            let panel = editor.panels[panel_id].rect_including_panel_bar;
            let border_thickness = editor.panel_bar_border_thickness(); // :Configuration ?
            gpu::draw_rect(
                gpu,
                panel.x, panel.y + bar_h,
                panel.w, border_thickness,
                color
            );
        }

        // :Configuration ?
        PanelKind::Custom(_) => {}
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
    let margin_top   = editor.panel_bar_h()*1.88;
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

impl Eq for PanelSplit {}

impl PartialEq for PanelSplit {
    fn eq(&self, other: &Self) -> bool {
        self.vertical == other.vertical &&
        self.left_id == other.left_id &&
        self.right_id == other.right_id
    }
}

#[derive(Eq, PartialEq, PartialOrd, Ord, Debug, Copy, Clone, Default)]
pub struct CustomPanel {
    pub extra0: u32, pub extra1: u32, pub extra2: u32,
}

impl CustomPanel {
    pub const UNIT: Self = unsafe { core::mem::zeroed() };
}

#[derive(Eq, PartialEq, Copy, Clone, Debug)]
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
    pub rect_including_panel_bar: Rect, // @Memory: This is very naive
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

    pub cursor_opacity: f32, // 0..1

    pub cursor_ghost_x: f32,
    pub cursor_ghost_y: f32,

    pub cursor_target_line: u32,
    pub cursor_target_col:  u32,

    pub cursor: Cursor,
    pub layout: Option<TextLayout>,

    pub overlay: crate::ts::OverlayState,
    pub persistent_state_per_buffer: FastHashMap<BufferId, ViewState>,
}

impl View {
    pub fn new_with_scroll(id: ViewId, buffer_id: BufferId, scroll: f32) -> Self {
        Self {
            id, buffer_id, scroll, cursor: Cursor::new(), layout: None,
            cursor_ghost_y: 0.0,
            cursor_ghost_x: 0.0,
            cursor_anim_x: f32::NAN,
            cursor_anim_y: f32::NAN,
            scroll_vel: 0.0,
            cursor_target_line: 0, cursor_target_col: 0,
            scroll_anim: scroll,
            cursor_opacity: 1.0,
            persistent_state_per_buffer: Default::default(),
            overlay: Default::default(),
            panel_id: PanelId::reserved_value()  // Set on first layout
        }
    }

    pub fn new(id: ViewId, buffer_id: BufferId) -> Self {
        Self::new_with_scroll(id, buffer_id, 0.0)
    }

    #[inline]
    pub fn is_cursor_visible(&self) -> bool {
        self.cursor_opacity > 0.3
    }

    #[inline]
    pub fn reset_cursor(&mut self) {
        self.scroll = 0.0;
        self.cursor_target_line = 0;
        self.cursor_target_col = 0;
        self.cursor = Cursor::new();
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
        if old == new {
            return;
        }

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
        let cursor_bottom = cursor_top + line_h + (ADDITIONAL_BOTTOM_SCROLL_SPACE/2.0);

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
    pub fn scroll_to_cursor_centered(&mut self, line: u32, line_h: f32, rect: Rect) {
        let cursor_top = line as f32 * line_h;
        self.scroll = (cursor_top - rect.h / 2.0 + line_h / 2.0).max(0.0);
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

pub struct FlyingCursor {
    pub x:        f32,
    pub y:        f32,
    pub target_x: f32,
    pub target_y: f32,
    pub alpha:    f32,  // Fades out as it arrives
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

#[derive(Default)]
pub struct EditorCustomData {  // @DocumentThis
    pub transient:  Option<Box<dyn Any>>,
    pub persistent: Option<Box<dyn Any>>,
}

impl Deref for EditorCustomData {
    type Target = Option<Box<dyn Any>>;
    #[inline] fn deref(&self) -> &Self::Target { &self.persistent }
}

impl DerefMut for EditorCustomData {
    #[inline] fn deref_mut(&mut self) -> &mut Self::Target { &mut self.persistent }
}

impl EditorCustomData {
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn get<T: 'static>(&self) -> &T {
        unsafe {
            &*(self.persistent.as_ref()
               .unwrap_or_else(|| panic!(
                   "EditorCustomData::get::<{}>() called but persistent data was never initialized.",
                   std::any::type_name::<T>()
               ))
               .as_ref() as *const dyn Any as *const T)
        }
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn get_mut<T: 'static>(&mut self) -> &mut T {
        unsafe {
            &mut *(self.persistent.as_mut()
                   .unwrap_or_else(|| panic!(
                       "EditorCustomData::get_mut::<{}>() called but persistent data was never initialized.",
                       std::any::type_name::<T>()
                   ))
                   .as_mut() as *mut dyn Any as *mut T)
        }
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn set<T: 'static>(&mut self, value: impl Into<Box<T>>) {
        self.persistent.replace(value.into());
    }
}

impl EditorCustomData {
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn get_transient<T: 'static>(&self) -> &T {
        unsafe {
            &*(self.transient.as_ref()
               .unwrap_or_else(|| panic!(
                   "EditorCustomData::get_transient::<{}>() called but transient data was never initialized.",
                   std::any::type_name::<T>()
               ))
               .as_ref() as *const dyn Any as *const T)
        }
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn get_transient_mut<T: 'static>(&mut self) -> &mut T {
        unsafe {
            &mut *(self.transient.as_mut()
                   .unwrap_or_else(|| panic!(
                       "EditorCustomData::get_transient_mut::<{}>() called but transient data was never initialized.",
                       std::any::type_name::<T>()
                   ))
                   .as_mut() as *mut dyn Any as *mut T)
        }
    }

    #[inline]
    pub fn set_transient<T: 'static>(&mut self, value: impl Into<Box<T>>) {
        self.transient.replace(value.into());
    }
}

pub struct Editor {
    /// NOTE: If you want to push a buffer here, you probably wanna use [`Editor::push_buffer`] instead.
    pub buffers: PrimaryMap<BufferId, Buffer>,
    pub views:   PrimaryMap<ViewId,   View>,
    pub panels:  PrimaryMap<PanelId,  Panel>,

    pub canonicalized_path_to_buffer_id: FastHashMap<Arc<Path>, BufferId>,

    // Which panel is active (receives keyboard input)
    pub active_panel:  PanelId,

    // Root panel id - its rect always equals the window
    pub root_panel:    PanelId,
    pub root_buffer:   BufferId,

    pub flying_cursor: Option<FlyingCursor>,

    pub scratch_paren:     Vec<char>,
    pub scratch_panel_bar: String,

    pub most_recently_used_buffers:  VecDeque<BufferId>,
    pub buffer_cycle_index:          Option<usize>,

    pub logger_config: EditorLoggerConfig,

    pub tree_sitter: TreeSitter,

    // Scale for font/line-height
    pub scale: f32,
    pub win_w: f32,
    pub win_h: f32,
    pub is_our_window_focused: bool,

    pub did_we_apply_any_sessions: bool,

    // Cursor blink
    pub blink_epoch:         Instant,

    pub last_messager_count: u32,

    // Mouse
    pub mouse_pos:          (f32, f32),
    pub mouse_left_pressed: bool,
    pub is_cursor_visible:  bool,

    pub clipboard:       Option<arboard::Clipboard>,

    pub last_cursor_position: SecondaryMap<ViewId, u32>,

    pub frame_count:     u32,
    pub fps:             f32,

    pub refresh_rate_millihertz: u32,

    pub last_fps_time:   Instant,
    pub last_frame_time: Instant,
    pub last_input_time: Instant,

    pub relex_us_acc:    f32,
    pub build_us_acc:    f32,
    pub render_us_acc:   f32,

    pub relex_us:        f32,
    pub build_us:        f32,
    pub render_us:       f32,

    pub canonicalized_current_working_directory: SmallString<[u8; 256]>,
    pub canonicalized_last_scanned_directory:    SmallString<[u8; 256]>,

    pub hooks: Hooks,
    pub custom_data: EditorCustomData,

    pub director:        Director,
    pub messager:        Messager,
    pub audioer:         Audioer,

    pub redraw_reasons:  Vec<ReasonEntry>,
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
            rect_including_panel_bar: Rect::default(),
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

        Self {
            buffers,
            views,
            panels,
            canonicalized_path_to_buffer_id,
            scratch_panel_bar: String::with_capacity(256),
            logger_config,
            hooks: Default::default(),
            last_input_time: Instant::now(),
            refresh_rate_millihertz: u32::MAX,
            win_h: 0.0,
            win_w: 0.0,
            root_buffer,
            is_cursor_visible: true,
            buffer_cycle_index: None,
            custom_data: EditorCustomData::default(),
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
            flying_cursor: None,
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
            last_cursor_position: Default::default(),
            audioer,
            director: Director::new(),
            messager: Messager::new(),
            did_we_apply_any_sessions: false,
            redraw_reasons: Vec::with_capacity(64),
            tree_sitter: crate::ts::spawn(),
        }
    }

    #[inline]
    pub fn was_custom_data_ever_initialized(&self) -> bool {
        self.custom_data.is_some()
    }

    #[inline]
    pub fn recompute_buffer_display_names(&mut self) {
        recompute_pretty_paths(&mut self.buffers);
    }

    #[inline]
    pub fn push_buffer(&mut self, mut buffer: Buffer) -> BufferId {
        if let Some(path) = &buffer.path &&
           let Some(filestem) = path.file_stem() &&
           let Some(filestem) = filestem.to_str()
        {
            buffer.filestem_atom = self.tree_sitter.atom_table.intern(filestem);
        }

        let buffer_id = self.buffers.push(buffer);
        self.mru_register_new_buffer(buffer_id);

        if let Some(hook) = self.hooks.opened_file {  // @Design: Move this into the if-block below?
            hook(self, buffer_id);
        }

        if let Some(canon) = self.buffers[buffer_id].path.as_ref().and_then(|p| p.canonicalize().ok()) {
            self.canonicalized_path_to_buffer_id.insert(canon.into(), buffer_id);  // @Clone @Refactor
        }

        self.recompute_buffer_display_names();
        self.tree_sitter.send_init(buffer_id, self.buffers[buffer_id].text.clone()); // nocheckin

        buffer_id
    }

    #[inline]
    pub fn max_scroll_of_impl(&self, line_count: u32, rect: Rect) -> f32 {
        ((line_count as f32 * self.line_h()) - rect.h).max(0.0) + ADDITIONAL_BOTTOM_SCROLL_SPACE
    }

    #[inline]
    pub fn max_scroll_of(&self, view_id: ViewId) -> f32 {
        let (view, buf) = self.view_and_buffer(view_id);
        let panel_id = view.panel_id().unwrap();

        let line_count = buf.text.len_lines();
        let rect = self.panel(panel_id).rect;

        self.max_scroll_of_impl(line_count as _, rect)
    }

    #[inline]
    pub fn command_finish(&mut self, dont_reset_blink: bool) {
        adjust_cursors_after_buffer_mutation(self);
        scroll_to_cursor(self);

        if !dont_reset_blink {
            self.reset_blink();
        }
    }

    /// Returns true if should request redraw
    #[inline]
    pub fn window_event_finish(&mut self) -> bool {
        let mut redraw = false;

        // @Hack
        {
            let current_buffer_id = self.active_view().buffer_id;
            let current_buffer = &self.buffers[current_buffer_id];
            let modified = !current_buffer.ts_edits_in_this_frame.is_empty();

            if modified {
                if let Some(hook) = self.hooks.modified_file {
                    hook(self, current_buffer_id);
                }
            }
        }

        // @Hack :TreeSitter
        //
        // @Important :FeelImprovement: Gate overlay appearance behind some user input,
        // so that if the user jumps around the source code, overlays don't flicker around.
        //
        {
            let (view, buf) = self.active_view_and_buffer_mut();
            let view_id     = view.id;
            let buffer_id   = view.buffer_id;
            let char_index  = view.cursor.char_index;
            let cursor_byte = buf.text.char_to_byte(char_index);
            let rope        = buf.text.clone();
            let last_pos    = self.last_cursor_position[view_id];

            //
            // Immediately shift the main thread tree coords
            //
            //
            // @Note @Robustness: We should do tree versioning to avoid background thread
            // mutating tree with an older version after this write from the main thread?
            //
            {
                let buf = self.active_buffer();
                if !buf.ts_edits_in_this_frame.is_empty() {
                    if let Some(mut tree_mut) = self.tree_sitter.trees.get_mut(&buffer_id) {
                        for &edit in &buf.ts_edits_in_this_frame {
                            tree_mut.edit(&edit.into()); // @Speed
                        }
                    }
                }
            }

            //
            // Send the edits to the background thread
            //
            {
                let buf = self.active_buffer();
                for &edit in &buf.ts_edits_in_this_frame {
                    self.tree_sitter.send_edit(buffer_id, edit, rope.clone());
                }
            }

            let (view, buf) = self.active_view_and_buffer_mut();

            let needs_cursor_query = !buf.ts_edits_in_this_frame.is_empty()
                || char_index as u32 != last_pos;

            for edit in &buf.ts_edits_in_this_frame {
                view.overlay.on_edit(&edit);
            }

            if view.overlay.needs_reset_cursor_byte(cursor_byte as _) {
                view.overlay.current = None;
            }

            //
            // Query only after local invalidation
            //
            if needs_cursor_query {
                let overlay = self.tree_sitter.query_cursor_overlay(buffer_id, cursor_byte, &rope);

                let (view, _buf) = self.active_view_and_buffer_mut();
                view.overlay.current = overlay;

                if view.overlay.current.is_none() {
                    redraw |= true;
                } else {
                    redraw |= true;
                }
            }
        }

        //
        // Reset the edits!
        //
        for buffer in self.buffers.values_mut() {
            // Reset inside adjust_cursors_after_buffer_mutation
            // buffer.last_insert = None;
            // buffer.last_delete = None;
            buffer.ts_edits_in_this_frame.clear();
        }

        redraw
    }

    pub fn always_on_update(&mut self) -> ShouldRequestFrameRedraw {
        let mut redraw = ShouldRequestFrameRedraw::No;

        // @Cleanup :TreeSitter
        {
            while let Ok(result) = self.tree_sitter.result.try_recv() {
                let buffer_filename = self.buffers[result.buffer_id].filestem_atom;
                for info in result.functions {
                    self.tree_sitter.func_table.insert(info, buffer_filename);
                }

                // Update overlay result for the view that made the cursor query
                if let Some(overlay_result) = result.overlay {
                    for view in self.views.values_mut() {
                        if view.buffer_id == result.buffer_id {
                            redraw = redraw.or_msg("Overlay update", &mut self.redraw_reasons);

                            view.overlay.current = Some(overlay_result);
                            break;  // Cursor query came from one specific view
                        }
                    }
                } else {
                    // Bg thread found no call expression at cursor, clear overlay
                    for view in self.views.values_mut() {
                        if view.buffer_id == result.buffer_id {
                            redraw = redraw.or_msg("Overlay update", &mut self.redraw_reasons);

                            view.overlay.current = None;
                            break;
                        }
                    }
                }
            }
        }

        redraw
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
    pub fn set_custom_data<T: 'static>(&mut self, value: impl Into<Box<T>>) {
        self.custom_data.set(value)
    }

    #[inline]
    pub fn custom_transient_data<T: 'static>(&self) -> &T {
        self.custom_data.get_transient()
    }

    #[inline]
    pub fn custom_transient_data_mut<T: 'static>(&mut self) -> &mut T {
        self.custom_data.get_transient_mut()
    }

    #[inline]
    pub fn set_custom_transient_data<T: 'static>(&mut self, value: impl Into<Box<T>>) {
        self.custom_data.set_transient(value)
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

    pub fn view_and_buffer(&self, view_id: ViewId) -> (&View, &Buffer) {
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

    pub fn active_view_and_buffer(&self) -> (&View, &Buffer) {
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
    pub fn layout_panels(&mut self) {
        let win_rect = Rect::full(self.win_w, self.win_h);
        self.layout_panel(self.root_panel, win_rect);
        if let Some(layout_panels_hook) = self.hooks.layout_panels {
            layout_panels_hook(self, win_rect);
        }
    }

    pub fn layout_panel(&mut self, id: PanelId, rect: Rect) {
        self.panels[id].rect = rect;
        self.panels[id].rect_including_panel_bar = rect;

        let split = match self.panels[id].kind {
            PanelKind::Split(s) => s,

            PanelKind::Leaf { view_id } => {
                let should_have_bar = self.hooks.should_view_have_panel_bar.map_or(
                    true,
                    |f| f(self, view_id)
                );
                if should_have_bar {
                    let bar_h = self.panel_bar_h();
                    self.panels[id].rect.y += bar_h;
                    self.panels[id].rect.h -= bar_h;
                }

                return;
            }

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
    pub fn split_active(&mut self, vertical: bool, ratio: f32) {
        self.split_active_no_layout(vertical, ratio);
        self.layout_panels();
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

        self.panels.push(Panel { id: left_id,  kind: PanelKind::Leaf { view_id: old_view_id }, rect: Rect::default(), rect_including_panel_bar: Default::default() });
        self.panels.push(Panel { id: right_id, kind: PanelKind::Leaf { view_id: new_view_id }, rect: Rect::default(), rect_including_panel_bar: Default::default() });

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

        self.layout_panels();
    }

    pub fn toggle_active_panel(&mut self) {
        let mut leaves = Default::default();
        collect_leaves(self, self.root_panel, &mut leaves);

        if leaves.len() <= 1 {
            return;
        }

        let current_pos = leaves.iter().position(|(id, ..)| *id == self.active_panel).unwrap_or(0);
        let next = (current_pos + 1) % leaves.len();
        let (to_switch_to, ..) = leaves[next];

        let (from_x, from_y) = {
            let view = self.active_view();
            (view.cursor_anim_x, view.cursor_anim_y)
        };

        self.set_active_panel(to_switch_to);

        let (to_x, to_y) = {
            let active_view = self.active_view();
            (active_view.cursor_anim_x, active_view.cursor_anim_y)
        };
        self.flying_cursor = Some(FlyingCursor {
            x: from_x,
            y: from_y,
            target_x: to_x,
            target_y: to_y,
            alpha: 1.0,
        });

        // Also snap the new view's ghost to from position so trail flows naturally
        let active_view = self.active_view_mut();
        active_view.cursor_ghost_x = from_x;
        active_view.cursor_ghost_y = from_y;
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

    #[inline] pub fn line_h(&self)    -> f32 { scale_base_line_height(self.scale) }
    #[inline] pub fn font_size(&self) -> f32 { scale_base_font_size(self.scale) }
    #[inline] pub fn panel_bar_border_thickness(&self) -> f32 { scale_base_panel_bar_border_thickness(self.scale) }
    #[inline] pub fn panel_bar_h(&self) -> f32 { self.line_h() + self.cursor_h() + self.scale * 3.5 }
    #[inline] pub fn cursor_w(&self)  -> f32 { scale_base_cursor_width(self.scale) }
    #[inline] pub fn cursor_h(&self)  -> f32 { scale_base_cursor_height(self.scale) }

    #[inline]
    pub fn reset_blink(&mut self) {
        self.blink_epoch     = Instant::now();
        self.last_input_time = Instant::now();
    }

    #[inline]
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

pub const BLINK_START_DELAY_MS: u128 = 500;  // Start blinking after 500ms idle
pub const BLINK_STOP_IDLE_MS:   u128 = 5700;

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

const NO_PARENT: u16 = u16::MAX;

#[derive(Clone, Copy)]
pub struct ReasonEntry {
    msg:    &'static str,
    parent: u16,
}

#[derive(Default, Debug, Clone, Copy)]
pub enum ShouldRequestFrameRedraw {
    Yes(u16),
    #[default]
    No,
}

// a | b  ≡  a.or(b)
impl BitOr for ShouldRequestFrameRedraw {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { self.or(rhs) }
}

// a & b  ≡  a.and(b)
impl BitAnd for ShouldRequestFrameRedraw {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self { self.and(rhs) }
}

// a |= b
impl BitOrAssign for ShouldRequestFrameRedraw {
    fn bitor_assign(&mut self, rhs: Self) { *self = *self | rhs; }
}

// a &= b
impl BitAndAssign for ShouldRequestFrameRedraw {
    fn bitand_assign(&mut self, rhs: Self) { *self = *self & rhs; }
}

impl PartialEq for ShouldRequestFrameRedraw {
    fn eq(&self, other: &Self) -> bool {
        matches!((self, other),
            (Self::No, Self::No) | (Self::Yes(_), Self::Yes(_)))
    }
}

impl From<ShouldRequestFrameRedraw> for bool {
    fn from(s: ShouldRequestFrameRedraw) -> bool { s.is_yes() }
}

pub struct ShouldRequestFrameRedrawDisplay<'a> {
    s: ShouldRequestFrameRedraw,
    reasons: &'a [ReasonEntry]
}

impl std::fmt::Display for ShouldRequestFrameRedrawDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.s {
            ShouldRequestFrameRedraw::No => write!(f, "No"),
            ShouldRequestFrameRedraw::Yes(_) => {
                let chain = self.s.reason_chain(self.reasons);
                write!(f, "Yes({})", chain.join(" <- "))
            }
        }
    }
}

impl ShouldRequestFrameRedraw {
    #[inline]
    pub fn display<'a>(self, reasons: &'a [ReasonEntry]) -> ShouldRequestFrameRedrawDisplay<'a> {
        ShouldRequestFrameRedrawDisplay {
            s: self,
            reasons
        }
    }

    /// Call once at the start of each frame to reset the reason store.
    #[inline]
    pub fn begin_frame(reasons: &mut Vec<ReasonEntry>) {
        #[cfg(debug_assertions)]
        reasons.clear();
    }

    #[inline]
    #[must_use]
    pub fn yes(msg: &'static str, reasons: &mut Vec<ReasonEntry>) -> Self {
        #[cfg(debug_assertions)]
        return Self::push(msg, NO_PARENT, reasons);
        #[cfg(not(debug_assertions))]
        { let _ = (msg, reasons); Self::Yes(0) }
    }

    #[inline]
    fn push(msg: &'static str, parent: u16, reasons: &mut Vec<ReasonEntry>) -> Self {
        #[cfg(not(debug_assertions))]
        { let _ = (msg, parent, reasons); return Self::Yes(0); }
        #[cfg(debug_assertions)]
        {
            let idx = reasons.len() as u16;
            reasons.push(ReasonEntry { msg, parent });
            Self::Yes(idx)
        }
    }

    /// Attach an additional reason on top of an existing one, forming a chain.
    /// If self is No, stays No.
    #[inline]
    #[must_use]
    pub fn because(self, msg: &'static str, reasons: &mut Vec<ReasonEntry>) -> Self {
        #[cfg(not(debug_assertions))]
        { let _ = (msg, reasons); return self; }
        #[cfg(debug_assertions)]
        match self {
            Self::Yes(parent) => Self::push(msg, parent, reasons),
            Self::No          => Self::No,
        }
    }

    /// Redraw if either does.  Keeps self's reason if both are Yes.
    #[inline]
    #[must_use]
    pub fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Yes(_), _) | (_, Self::No) => self,
            _                                 => other,
        }
    }

    /// Redraw only if both do.
    #[inline]
    #[must_use]
    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Yes(_), Self::Yes(_)) => self,
            _                            => Self::No,
        }
    }

    #[inline]
    #[must_use]
    pub fn if_msg(condition: bool, msg: &'static str, reasons: &mut Vec<ReasonEntry>) -> Self {
        if condition { Self::yes(msg, reasons) } else { Self::default() }
    }

    /// Convenience: set self to Yes if the condition is true.
    #[inline]
    #[must_use]
    pub fn or_if(self, condition: bool, msg: &'static str, reasons: &mut Vec<ReasonEntry>) -> Self {
        if condition { self.or(Self::yes(msg, reasons)) } else { self }
    }

    /// Convenience: set self to Yes if the condition is true.
    #[inline]
    #[must_use]
    pub fn or_msg(self, msg: &'static str, reasons: &mut Vec<ReasonEntry>) -> Self {
        self.or_if(true, msg, reasons)
    }

    #[inline]
    pub fn is_yes(self) -> bool { matches!(self, Self::Yes(_)) }

    #[inline]
    pub fn is_no (self) -> bool { matches!(self, Self::No)     }

    /// Walk the reason chain from most-recent to root.
    #[inline]
    pub fn reason_chain(self, reasons: &[ReasonEntry]) -> Vec<&'static str> {
        #[cfg(not(debug_assertions))]
        { let _ = reasons; return Vec::new(); }
        #[cfg(debug_assertions)]
        {
            let mut out = Vec::new();
            let Self::Yes(mut id) = self else { return out };
            loop {
                let e = reasons[id as usize];
                out.push(e.msg);
                if e.parent == NO_PARENT { break; }
                id = e.parent;
            }
            out
        }
    }

    #[inline]
    pub fn reason(self, reasons: &[ReasonEntry]) -> Option<&'static str> {
        #[cfg(not(debug_assertions))]
        { let _ = reasons; return None; }
        #[cfg(debug_assertions)]
        {
            let Self::Yes(id) = self else { return None };
            Some(reasons[id as usize].msg)
        }
    }
}

pub fn recompute_pretty_paths(buffers: &mut PrimaryMap<BufferId, Buffer>) {  // @Memory @Speed
    let mut by_name: FastHashMap<String, Vec<BufferId>> = Default::default();
    for (id, buf) in buffers.iter() {
        let Some(path) = &buf.path else { continue };
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        by_name.entry(name.to_string()).or_default().push(id);
    }

    for buf in buffers.values_mut() {
        if buf.path.is_none() {
            buf.pretty_path = "*scratch*".into();
        }
    }

    let mut suffixes: Vec<Box<str>> = Vec::new();

    for (name, ids) in &by_name {
        if ids.len() == 1 {
            buffers[ids[0]].pretty_path = name.as_str().into();
            continue;
        }

        'depth: for depth in 1..=32usize {
            suffixes.clear();

            let path_strs = ids.iter()
                .map(|id| buffers[*id].path.as_deref().unwrap().to_str().unwrap_or("?"));
            for s in path_strs {
                suffixes.push(parent_suffix(s, name, depth - 1));
            }

            let unique = (0..suffixes.len())
                .all(|i| (i+1..suffixes.len())
                    .all(|j| suffixes[i] != suffixes[j]));

            if unique || depth == 32 {
                for (id, suffix) in ids.iter().zip(&suffixes) {
                    buffers[*id].pretty_path = if suffix.is_empty() {
                        name.as_str().into()
                    } else {
                        format!("{}<{}>", name, suffix).into()
                    };
                }

                break 'depth;
            }
        }
    }
}

fn parent_suffix(s: &str, name: &str, depth: usize) -> Box<str> {
    let end = s.len().saturating_sub(name.len() + 1);
    let bytes = s.as_bytes();
    let mut remaining = depth;
    let mut i = end;
    while i > 0 {
        i -= 1;
        if bytes[i] == b'/' || bytes[i] == b'\\' {
            if remaining == 0 {
                return s[i+1..end].into();
            }
            remaining -= 1;
        }
    }
    s[..end].into()
}

pub fn animate(editor: &mut Editor, dt: f32) -> ShouldRequestFrameRedraw {
    let _tracy = tracy::span!("animate");

    let mut should_redraw = ShouldRequestFrameRedraw::No;

    let epsilon = 0.5f32;       // Stop animating when close enough
    let line_h  = editor.line_h();
    let active_view_id = editor.active_view_id();

    for view in editor.views.values_mut() {
        //
        //
        // Scroll
        //
        //

        {
            let target = view.scroll;
            let delta  = target - view.scroll_anim;

            if delta.abs() > epsilon {
                let speed = 20.0 + (delta.abs() / line_h).sqrt() * 3.0; // @Tune 3.0
                // let speed = (20.0 + delta.abs() / line_h * 0.5).min(35.0); // @Tune
                view.scroll_anim += delta * (1.0 - (-speed * dt).exp());
                should_redraw = should_redraw.or(ShouldRequestFrameRedraw::yes(
                    "View scroll animation", &mut editor.redraw_reasons
                ));
            } else {
                view.scroll_anim = target;
                view.scroll_vel  = 0.0;
            }
        }

        //
        //
        // Cursor position
        //
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
                should_redraw = should_redraw.or(ShouldRequestFrameRedraw::yes(
                    "Cursor anim", &mut editor.redraw_reasons
                ));

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

        //
        // Cursor ghost trail
        //
        {
            let gx = view.cursor_ghost_x;
            let gy = view.cursor_ghost_y;
            let tx = view.cursor_anim_x;
            let ty = view.cursor_anim_y;

            let dx = tx - gx;
            let dy = ty - gy;

            if dx.abs() > epsilon || dy.abs() > epsilon {
                const GHOST_RATE: f32 = 25.0; // @Tune - lower = longer/slower trail

                view.cursor_ghost_x += dx * (1.0 - (-GHOST_RATE * dt).exp());
                view.cursor_ghost_y += dy * (1.0 - (-GHOST_RATE * dt).exp());
                should_redraw = should_redraw.or(ShouldRequestFrameRedraw::yes(
                    "Cursor ghost trail", &mut editor.redraw_reasons
                ));
            } else {
                view.cursor_ghost_x = tx;
                view.cursor_ghost_y = ty;
            }
        }

        // :Configuration
        //
        // Cursor opacity
        //
        {
            let target_opacity = if active_view_id != view.id {
                0.25
            } else {
                let since_input = editor.last_input_time.elapsed().as_millis();

                if since_input < BLINK_START_DELAY_MS || since_input > BLINK_STOP_IDLE_MS {
                    // Solid - just typed, or been idle too long
                    1.0
                } else {
                    let t = editor.blink_epoch.elapsed().as_secs_f32();
                    let period = 2.0 * std::f32::consts::PI / 6.0;
                    let phase = (t % period) / period;
                    let k = 0.15;
                    let wave = if phase < k {
                        phase / k
                    } else if phase < 0.5 {
                        1.0
                    } else if phase < 0.5 + k {
                        1.0 - (phase - 0.5) / k
                    } else {
                        0.0
                    };
                    wave * 0.8
                }
            };

            let delta = target_opacity - view.cursor_opacity;

            if delta.abs() > 0.01 {
                let rate = 18.0;

                view.cursor_opacity += delta * (1.0 - (-rate * dt).exp());

                should_redraw = should_redraw.or(
                    ShouldRequestFrameRedraw::yes(
                        "Cursor opacity",
                        &mut editor.redraw_reasons,
                    )
                );
            } else {
                view.cursor_opacity = target_opacity;
            }
        }
    }

    if let Some(fc) = &mut editor.flying_cursor {
        const FLYING_RATE: f32 = 40.0; // @Tune
        const FLYING_FADE: f32 = 3.0;  // @Tune, How fast it fades on arrival

        let dx = fc.target_x - fc.x;
        let dy = fc.target_y - fc.y;
        fc.x += dx * (1.0 - (-FLYING_RATE * dt).exp());
        fc.y += dy * (1.0 - (-FLYING_RATE * dt).exp());

        let dist = (dx * dx + dy * dy).sqrt();
        // Start fading once close
        if dist < line_h * 2.0 {
            fc.alpha -= FLYING_FADE * dt;
        }

        if fc.alpha <= 0.0 {
            editor.flying_cursor = None;
        } else {
            should_redraw = should_redraw.or(ShouldRequestFrameRedraw::yes(
                "Flying cursor", &mut editor.redraw_reasons
            ));
        }
    }

    //
    // Advance animated regions in every buffer
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

        should_redraw = should_redraw.or_if(
            !buffer.currently_animated_pastes.is_empty()
                || !buffer.currently_animated_copies.is_empty(),

            "paste/copy animation",

            &mut editor.redraw_reasons
        );
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

        should_redraw = should_redraw.or_if(
            !buffer.currently_animated_deletions.is_empty(),
            "deletion animation",
            &mut editor.redraw_reasons
        );
    }

    if let Some(animate_hook) = editor.hooks.animate {
        animate_hook(editor, dt, &mut should_redraw);
    }

    should_redraw
}

pub fn char_at_line_col(buffer: &Buffer, line: u32, col: u32) -> Option<char> {
    let line_text = buffer.text.line(line as usize);
    line_text.chars().nth(col as usize)
}

pub fn collect_leaves(editor: &Editor, id: PanelId, out: &mut SmallVec<[(PanelId, ViewId, Rect, Rect); 5]>) { // @Memory
    let panels = &editor.panels;

    let mut stack = SmallVec::<[PanelId; 12]>::with_capacity((panels.len() as f32 * 1.5) as usize);

    if let Some(collect_leaf_panels_init_stack) = editor.hooks.collect_leaf_panels_init_stack {
        collect_leaf_panels_init_stack(editor, id, &mut stack);
    }

    stack.push(id);

    while let Some(id) = stack.pop() {
        match panels[id].kind {
            PanelKind::Leaf { view_id } => out.push((id, view_id, panels[id].rect, panels[id].rect_including_panel_bar)),

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

pub fn rescale(editor: &mut Editor, new_scale: f32) {
    let new = new_scale.clamp(MIN_SCALE, MAX_SCALE);
    if new != editor.scale {
        apply_scale(editor, new, None);
        editor.layout_panels();
        force_layouts_from_all_views_to_rebuild(editor);
    }
}

pub fn force_layouts_from_all_views_to_rebuild(editor: &mut Editor) {
    for view in editor.views.values_mut() {
        view.layout = None;
    }
}

pub fn scroll_page(editor: &mut Editor, direction: i32) {
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

    // Move cursor by a full page
    let new_line = ((cur_line as isize + delta).max(0) as usize)
        .min(total.saturating_sub(1)) as u32;

    editor.buffers[buf_id].set_cursor_line_col(
        new_line, cur_col, &mut editor.views[view_id].cursor
    );
    editor.views[view_id].cursor_target_line = new_line;
    editor.views[view_id].cursor_target_col  = cur_col;

    // Only scroll if cursor is now outside the visible region
    let scroll     = editor.views[view_id].scroll;
    let cursor_y   = new_line as f32 * line_h;
    let max_scroll = editor.max_scroll_of(view_id);

    let new_scroll = if cursor_y < scroll {
        // Cursor above viewport, scroll up to show it
        cursor_y
    } else if cursor_y + line_h > scroll + rect.h {
        // Cursor below viewport, scroll down to show it
        cursor_y + line_h - rect.h
    } else {
        scroll
    };

    editor.views[view_id].scroll = new_scroll.clamp(0.0, max_scroll);
}

pub fn editor_write_buffer_onto_disk(editor: &mut Editor, buffer_id: BufferId) -> std::io::Result<()> {
    editor.buffers[buffer_id].write_onto_disk()
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

    for view in editor.views.values_mut().filter(|view| view.buffer_id == buffer) { // @Redundant?
        view.reset_cursor();
    }
}

pub fn open_buffer_from_path_in(editor: &mut Editor, view: ViewId, path: impl Into<Box<Path>>) {
    let path = path.into();

    let Ok(canon) = std::fs::canonicalize(&path) else { return };

    let canon: &Path = canon.as_ref();
    let buffer_id = if let Some(&existing_buffer_id) = editor.canonicalized_path_to_buffer_id.get(canon) {
        existing_buffer_id
    } else {
        let Ok(buffer) = Buffer::from_file(path) else { return };
        editor.push_buffer(buffer)
    };

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
        let layout_rect = l.rect;

        anim_first < l.first_buffer_line
            || (
                anim_first + screen_lines > l.first_buffer_line + l.lines.len() as u32
                    && screen_lines < l.lines.len() as u32
            )
            || (layout_rect.w - rect.w).abs() > 0.5
            || (layout_rect.h - rect.h).abs() > 0.5
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

    let buffer_id = if editor.did_we_apply_any_sessions {
        editor.buffers.push(buffer)
    } else {
        editor.buffers[editor.root_buffer] = buffer;
        editor.root_buffer
    };

    editor.mru_register_new_buffer(buffer_id); // @Refactor

    editor.views[VIEW_MAIN].switch_buffer(buffer_id);

    if let Some(p) = canon {
        editor.canonicalized_path_to_buffer_id.insert(p.into(), buffer_id);
    }
}
