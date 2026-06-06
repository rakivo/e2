use crate::color::{Color, lerp_color};
use crate::gpu::{self, Gpu};

#[cfg(debug_assertions)]
use std::cell::RefCell;

use std::boxed::Box as Boxed;
use std::collections::HashMap;

use smallvec::SmallVec;
use cranelift_entity::PrimaryMap;

const DEFAULT_PADDING:   f32 = 4.0;
const DEFAULT_FONT_SIZE: f32 = 14.0;

#[derive(Eq, PartialEq, PartialOrd, Copy, Clone, Debug, Hash)]
pub struct LabelHash(pub u64);

#[derive(Eq, PartialEq, PartialOrd, Copy, Clone, Debug, Hash)]
pub struct BoxRef(pub u32);

cranelift_entity::entity_impl!(BoxRef);

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Default)]
    pub struct BoxFlags: u32 {
        const DRAW_BG         = 0x0;
        const DRAW_BORDER     = 0x1;
        const DRAW_TEXT       = 0x2;
        const CLICKABLE       = 0x4;
        const HOVERABLE       = 0x8;
        const CLIP_CHILDREN   = 0x10;
        const SCROLL_CHILDREN = 0x20;
    }
}

pub struct Box {
    pub key: LabelHash,   // Links to BoxPersist

    pub children: SmallVec<[BoxRef; 4]>,
    pub parent:   Option<BoxRef>,

    // Appearance
    pub flags:        BoxFlags,
    pub bg_color:     Color,
    pub hover_color:  Color,
    pub border_color: Color,
    pub text_color:   Color,
    pub text:         Boxed<str>,  // @Memory

    //
    // NOTE: This expects an already scaled font size,
    // meaning that this is the FINAL pixel size, and
    // there will not be any mutation of it downstream.
    //
    pub font_size:    f32,

    // Layout INPUT
    pub pref_size:     [Size; 2],
    pub floating_pos:  Option<[f32; 2]>,
    pub child_axis:    Axis,
    pub padding:       f32,  // @Cleanup
    pub padding_left:  f32,
    pub padding_right: f32,

    // Layout OUTPUT
    pub rect:          [f32; 4],         // x0, y0, x1, y1
    pub computed_size: [f32; 2],         // w, h
    pub clip_rect:     Option<[f32; 4]>, // x0, y0, y1, y1
}

impl Default for Box {
    fn default() -> Self {
        Self {
            key:           LabelHash(0),
            floating_pos:  Default::default(),
            parent:        None,
            children:      SmallVec::default(),
            pref_size:     [Size::fill(), Size::fill()],
            child_axis:    Axis::X,
            padding:       DEFAULT_PADDING,
            padding_left:  DEFAULT_PADDING,
            padding_right: DEFAULT_PADDING,
            flags:         BoxFlags::empty(),
            bg_color:      Color::default(),
            hover_color:   Color::default(),
            border_color:  Color::default(),
            text:          Boxed::default(),
            text_color:    Color::rgba(255, 255, 255, 255),
            computed_size: [0.0; 2],
            rect:          [0.0; 4],
            clip_rect:     None,
            font_size:     DEFAULT_FONT_SIZE,
        }
    }
}

#[derive(Default)]
pub struct BoxPersist {
    pub hot_t:              f32,   // hover  [0..1]
    pub active_t:           f32,   // click  [0..1]

    pub last_frame_touched: u64,

    pub cursor_visual_x:    f32,

    pub scroll_offset:      f32,
    pub scroll_target:      f32,
    pub scroll_overscroll:  f32,
    pub scroll_visual:      f32,
    pub scroll_velocity:    f32,
}

#[derive(Clone, Copy)]
pub enum SizeKind {
    Pixels(f32),
    TextContent,
    ParentPct(f32),
    ChildrenSum,
}

#[derive(Clone, Copy)]
pub struct Size {
    pub kind: SizeKind,
    pub strictness: f32, // 0.0 = flexible, 1.0 = rigid
}

impl Size {
    pub fn px(v: f32)  -> Self { Self { kind: SizeKind::Pixels(v),       strictness: 1.0 } }
    pub fn fill()      -> Self { Self { kind: SizeKind::ParentPct(1.0),  strictness: 0.0 } }
    pub fn text()      -> Self { Self { kind: SizeKind::TextContent,     strictness: 1.0 } }
    pub fn pct(v: f32) -> Self { Self { kind: SizeKind::ParentPct(v),    strictness: 0.0 } }
    pub fn children()  -> Self { Self { kind: SizeKind::ChildrenSum,     strictness: 1.0 } }
}

#[derive(Clone, Copy, PartialEq, Default)]
pub enum Axis { #[default] X, Y }

// These let callers push/pop style without passing params everywhere
#[derive(Default)]
struct Stacks {
    size_x:        Vec<Size>,
    size_y:        Vec<Size>,
    bg_color:      Vec<Color>,
    text_color:    Vec<Color>,
    child_axis:    Vec<Axis>,
    padding:       Vec<f32>,
    padding_left:  Vec<f32>,
    padding_right: Vec<f32>,
    font_size:     Vec<f32>,
}

impl Stacks {
    fn top_size_x(&self)        -> Size  { self.size_x.last().copied().unwrap_or(Size::fill()) }
    fn top_size_y(&self)        -> Size  { self.size_y.last().copied().unwrap_or(Size::fill()) }
    fn top_bg(&self)            -> Color { self.bg_color.last().copied().unwrap_or(Color::default()) }
    fn top_text_color(&self)    -> Color { self.text_color.last().copied().unwrap_or(Color::rgba(255,255,255,255)) }
    fn top_font_size(&self)     -> f32   { self.font_size.last().copied().unwrap_or(DEFAULT_FONT_SIZE) }
    fn top_axis(&self)          -> Axis  { self.child_axis.last().copied().unwrap_or(Axis::X) }
    fn top_padding(&self)       -> f32   { self.padding.last().copied().unwrap_or(DEFAULT_PADDING) }
    fn top_padding_left(&self)  -> f32   { self.padding_left.last().copied().unwrap_or(self.top_padding()) }
    fn top_padding_right(&self) -> f32   { self.padding_right.last().copied().unwrap_or(self.top_padding()) }
}

pub struct UiState {
    // box storage - rebuilt every frame
    pub boxes:      PrimaryMap<BoxRef, Box>,
    pub root:       Option<BoxRef>,

    // parent stack for building the tree
    parent_stack:   Vec<BoxRef>,

    // context stacks
    stacks:         Stacks,

    // persistent state - survives across frames
    pub persist:    HashMap<LabelHash, BoxPersist>,

    // interaction
    pub hot_key:    LabelHash,  // Currently hovered box, 0 => no box
    pub active_key: LabelHash,  // Currently pressed box, 0 => no box

    // Used to reap dead persist entries
    pub frame_counter: u64,

    // Needed for root box and ParentPct
    pub win_w:      f32,
    pub win_h:      f32,

    layout_scratch: Vec<BoxRef>,

    #[cfg(debug_assertions)]
    __debug_label_strings: RefCell<HashMap<LabelHash, Boxed<str>>>
}

#[inline]
fn collect_preorder(scratch: &mut Vec<BoxRef>, boxes: &PrimaryMap<BoxRef, Box>, root: BoxRef) {
    scratch.clear();
    scratch.push(root);

    let mut i = 0;
    while i < scratch.len() {
        let id = scratch[i];
        for &c in &boxes[id].children {
            scratch.push(c);
        }

        i += 1;
    }
}

impl UiState {
    #[inline]
    pub fn new(win_w: f32, win_h: f32) -> Self {
        Self {
            win_w,
            win_h,

            boxes:         PrimaryMap::default(),
            root:          None,
            layout_scratch: Vec::with_capacity(128),
            parent_stack:  Vec::new(),
            stacks:        Stacks::default(),
            persist:       HashMap::new(),
            hot_key:       LabelHash(0),
            active_key:    LabelHash(0),
            frame_counter: 0,

            #[cfg(debug_assertions)]
            __debug_label_strings: Default::default()
        }
    }

    #[inline]
    pub fn hash_str(&self, s: impl AsRef<str>) -> LabelHash {
        #[inline]
        fn hash_str_impl(s: &str) -> LabelHash {
            let mut h: u64 = 5381;
            for b in s.bytes() {
                h = h.wrapping_mul(33).wrapping_add(b as u64);
            }

            LabelHash(h)
        }

        let s = s.as_ref();
        let hash = hash_str_impl(s);

        #[cfg(debug_assertions)] {
            self.__debug_label_strings.borrow_mut().insert(hash, s.into());
        }

        hash
    }

    #[inline]
    #[cfg(debug_assertions)]
    pub fn __debug_hash_to_str(&self, hash: LabelHash) -> Option<Boxed<str>> {
        self.__debug_label_strings.borrow().get(&hash).cloned()
    }

    #[inline]
    pub fn begin_frame(&mut self, win_w: f32, win_h: f32) {
        self.win_w = win_w;
        self.win_h = win_h;
        self.frame_counter += 1;
        self.boxes.clear();
        self.parent_stack.clear();

        // Make root box
        let root = self.boxes.push(Box {
            pref_size:  [Size::px(win_w), Size::px(win_h)],
            child_axis: Axis::Y,
            computed_size: [win_w, win_h],
            ..Default::default()
        });
        self.root = Some(root);
        self.parent_stack.push(root);
    }

    #[inline]
    pub fn end_frame(&mut self) {
        // Remove persist entries not touched this frame
        self.persist.retain(|_, p| p.last_frame_touched == self.frame_counter);
    }

    #[inline]
    pub fn is_active(&self, id: BoxRef) -> bool {
        self.boxes[id].key == self.active_key
    }

    #[inline]
    pub fn is_hot(&self, id: BoxRef) -> bool {
        self.boxes[id].key == self.hot_key
    }

    /// Push a new box as child of current parent, return its key
    #[inline]
    pub fn push_box(&mut self, label_hash: LabelHash, flags: BoxFlags) -> BoxRef {
        let persist = self.persist.entry(label_hash).or_default();
        persist.last_frame_touched = self.frame_counter;

        let parent = self.parent_stack.last().copied();

        let id = self.boxes.push(Box {
            key:        label_hash,
            parent,
            font_size:  self.stacks.top_font_size(),
            pref_size:  [self.stacks.top_size_x(), self.stacks.top_size_y()],
            child_axis: self.stacks.top_axis(),
            padding:    self.stacks.top_padding(),
            padding_left:  self.stacks.top_padding_left(),
            padding_right: self.stacks.top_padding_right(),
            flags,
            bg_color:   self.stacks.top_bg(),
            text_color: self.stacks.top_text_color(),
            ..Default::default()
        });

        // Link into parent
        if let Some(pk) = parent {
            self.boxes[pk].children.push(id);
        }

        id
    }

    #[inline]
    pub fn push_parent(&mut self, id: BoxRef) {
        self.parent_stack.push(id);
    }

    #[inline]
    pub fn pop_parent(&mut self) {
        self.parent_stack.pop();
    }

    #[inline]
    pub fn get_scroll(&self, key: &str) -> f32 {
        let k = self.hash_str(key);
        self.persist.get(&k).map(|p| p.scroll_offset).unwrap_or(0.0)
    }

    #[inline]
    pub fn clamp_scroll(&mut self, key: &str, content_h: f32, viewport_h: f32) {
        let k = self.hash_str(key);
        if let Some(p) = self.persist.get_mut(&k) {
            let max_scroll = (content_h - viewport_h).max(0.0);
            p.scroll_target = p.scroll_target.clamp(0.0, max_scroll);
            p.scroll_offset  = p.scroll_offset.clamp(0.0, max_scroll);
        }
    }

    #[inline]
    pub fn scroll_by(&mut self, key: &str, delta: f32, content_h: f32, viewport_h: f32) {
        let k = self.hash_str(key);
        if let Some(p) = self.persist.get_mut(&k) {
            let max_scroll = (content_h - viewport_h).max(0.0);
            let new_target = p.scroll_target + delta;

            p.scroll_target = new_target.clamp(0.0, max_scroll);

            // Overscroll only when hitting a hard boundary
            if new_target < 0.0 {
                p.scroll_overscroll = new_target.max(-120.0);
            } else if new_target > max_scroll {
                p.scroll_overscroll = (new_target - max_scroll).min(120.0);
            } else {
                p.scroll_overscroll = 0.0;  // clear it when scrolling normally
            }
        }
    }

    #[inline]
    pub fn update_interaction_local(
        &mut self,
        refs: impl Iterator<Item = BoxRef> + core::iter::DoubleEndedIterator,
        mouse: [f32; 2], clicked: bool
    ) {
        self.hot_key = LabelHash(0);

        // Iterate in reverse so last-rendered (topmost) boxes win,
        // and break on first hit.
        for id in refs.rev() {
            let b = &self.boxes[id];

            if !b.flags.contains(BoxFlags::CLICKABLE) { continue }

            let [x0, y0, x1, y1] = b.rect;

            // Clip to scroll parent's visible rect if one exists
            let [cx0, cy0, cx1, cy1] = b.clip_rect.unwrap_or([x0, y0, x1, y1]);

            let mx = mouse[0];
            let my = mouse[1];

            if mx >= x0  && mx <= x1
            && my >= y0  && my <= y1
            && mx >= cx0 && mx <= cx1
            && my >= cy0 && my <= cy1 {
                self.hot_key = b.key;
                break;
            }
        }

        if clicked {
            self.active_key = self.hot_key;
        } else if self.active_key != LabelHash(0) {
            self.active_key = LabelHash(0);
        }
    }

    #[inline]
    pub fn update_interaction_global(&mut self, mouse: [f32; 2], clicked: bool) {
        self.update_interaction_local(self.boxes.keys(), mouse, clicked);
    }

    #[inline]
    pub fn solve_layout(
        &mut self,
        root: BoxRef,
        origin: [f32; 2], parent_size: [f32; 2],
        measure: &mut impl FnMut(&str, f32) -> [f32; 2]
    ) {
        self.pass1_standalone(root, measure);
        self.pass2a_parent_pct(root, parent_size);
        self.pass2b_children_sum(root);
        self.resolve_overflow(root);
        self.pass3_place(root, origin, origin, None);
    }

    #[inline]
    pub fn solve_and_interact(
        &mut self,
        root: BoxRef,
        origin: [f32; 2], parent_size: [f32; 2],
        measure: &mut impl FnMut(&str, f32) -> [f32; 2],
        refs: impl Iterator<Item = BoxRef> + core::iter::DoubleEndedIterator,
        mouse: [f32; 2], clicked: bool,
    ) {
        self.solve_layout(root, origin, parent_size, measure);
        self.update_interaction_local(refs, mouse, clicked);
    }

    #[inline]
    pub fn clear_active(&mut self) {
        self.active_key = LabelHash(0);
    }

    #[inline]
    pub fn tick_animations(&mut self) {
        for (key, p) in &mut self.persist {
            //
            // Hover or active flashes
            //

            let hot_target    = if *key == self.hot_key    { 1.0 } else { 0.0 };
            let active_target = if *key == self.active_key { 1.0 } else { 0.0 };

            p.hot_t    += (hot_target - p.hot_t) * 0.15;
            p.active_t *= 0.75;
            if active_target == 1.0 { p.active_t = 1.0; }

            //
            // Scroll: spring toward target, with overscroll bounce
            //

            let spring = (p.scroll_target - p.scroll_offset) * 0.12;

            p.scroll_velocity   += spring;
            p.scroll_velocity   *= 0.72;  // damping
            p.scroll_offset += (p.scroll_target - p.scroll_offset) * 0.25;

            // overscroll bounces back
            p.scroll_overscroll *= 0.82;
            if p.scroll_overscroll.abs() < 0.1 { p.scroll_overscroll = 0.0; }
            p.scroll_visual = p.scroll_offset + p.scroll_overscroll;
        }
    }

    #[inline]
    pub fn pass1_standalone(
        &mut self,
        root: BoxRef,
        measure_callback: &mut impl FnMut(&str, f32) -> [f32; 2]
    ) {
        collect_preorder(&mut self.layout_scratch, &self.boxes, root);

        for &id in &self.layout_scratch {
            for axis in [Axis::X, Axis::Y] {
                let axis = axis as usize;
                match self.boxes[id].pref_size[axis].kind {
                    SizeKind::Pixels(v) => {
                        self.boxes[id].computed_size[axis] = v;
                    }

                    SizeKind::TextContent => {
                        let font_size = self.boxes[id].font_size;
                        let pl = self.boxes[id].padding_left;
                        let pr = self.boxes[id].padding_right;
                        let text = &self.boxes[id].text;
                        let measured = measure_callback(&text, font_size);
                        self.boxes[id].computed_size[axis] = measured[axis] + pl + pr;
                    }

                    _ => {}
                }
            }
        }
    }

    #[inline]
    pub fn pass2a_parent_pct(&mut self, root: BoxRef, parent_size: [f32; 2]) {
        collect_preorder(&mut self.layout_scratch, &self.boxes, root);

        for i in 0..self.layout_scratch.len() {
            let id = self.layout_scratch[i];
            for axis in [Axis::X, Axis::Y] {
                let axis = axis as usize;
                if let SizeKind::ParentPct(pct) = self.boxes[id].pref_size[axis].kind {
                    let parent_s = if i == 0 { parent_size[axis] } else {
                        let parent_id = self.boxes[id].parent.unwrap();
                        self.boxes[parent_id].computed_size[axis]
                    };

                    self.boxes[id].computed_size[axis] = parent_s * pct;
                }
            }
        }
    }

    #[inline]
    pub fn pass2b_children_sum(&mut self, root: BoxRef) {
        collect_preorder(&mut self.layout_scratch, &self.boxes, root);

        for i in (0..self.layout_scratch.len()).rev() {
            let id = self.layout_scratch[i];
            let child_axis = self.boxes[id].child_axis as usize;

            for axis in [Axis::X, Axis::Y] {
                let axis = axis as usize;

                if !matches!(self.boxes[id].pref_size[axis].kind, SizeKind::ChildrenSum) {
                    continue;
                }

                let v = if axis == child_axis {
                    self.boxes[id].children.iter().map(|&c| self.boxes[c].computed_size[axis]).sum()
                } else {
                    self.boxes[id].children.iter().map(|&c| self.boxes[c].computed_size[axis]).fold(0.0_f32, f32::max)
                };

                self.boxes[id].computed_size[axis] = v;
            }
        }
    }

    pub fn resolve_overflow(&mut self, root: BoxRef) {
        collect_preorder(&mut self.layout_scratch, &self.boxes, root);

        //
        // Go post order: resolve children before parents so inner overflow is settled first
        //

        for i in (0..self.layout_scratch.len()).rev() {
            let id = self.layout_scratch[i];

            let axis = self.boxes[id].child_axis as usize;
            let parent_size = self.boxes[id].computed_size[axis];

            let children = self.boxes[id].children.clone();
            let total: f32 = children.iter().map(|&c| self.boxes[c].computed_size[axis]).sum();

            let overflow = (total - parent_size).max(0.0);
            if overflow > 0.0 {
                let total_give: f32 = children.iter().map(|&c| {
                    let b = &self.boxes[c];
                    b.computed_size[axis] * (1.0 - b.pref_size[axis].strictness)
                }).sum();

                if total_give > 0.0 {
                    for &c in &children {
                        let b = &self.boxes[c];
                        let size = b.computed_size[axis];
                        let give = size * (1.0 - b.pref_size[axis].strictness);
                        let shrink = overflow * (give / total_give);
                        self.boxes[c].computed_size[axis] = (size - shrink).max(0.0);
                    }
                }
            }
        }
    }

    pub fn pass3_place(&mut self, root: BoxRef, origin: [f32; 2], parent_origin: [f32; 2], clip: Option<[f32; 4]>) {
        collect_preorder(&mut self.layout_scratch, &self.boxes, root);

        //
        // We need parent's rect to place each child,
        // store per-node (origin, parent_origin, clip) as we go.
        //
        // Since scratch is pre-order, parent is always at a lower index than its children.
        //

        // Seed the root
        let root_origin = if let Some(fp) = self.boxes[root].floating_pos {
            [parent_origin[0] + fp[0], parent_origin[1] + fp[1]]  // Relative to parent
        } else {
            origin
        };

        let root_size = self.boxes[root].computed_size;
        self.boxes[root].rect = [
            root_origin[0].round(), root_origin[1].round(),
            (root_origin[0] + root_size[0]).round(), (root_origin[1] + root_size[1]).round(),
        ];
        self.boxes[root].clip_rect = clip;

        for i in 1..self.layout_scratch.len() {
            let id       = self.layout_scratch[i];
            let parent   = self.boxes[id].parent.unwrap();
            let p_rect   = self.boxes[parent].rect;
            let p_clip   = self.boxes[parent].clip_rect;
            let p_flags  = self.boxes[parent].flags;
            let p_axis   = self.boxes[parent].child_axis as usize;
            let p_cross  = 1 - p_axis;

            // If this box clips its children, it becomes the new clip rect
            let child_clip = if p_flags.contains(BoxFlags::CLIP_CHILDREN) {
                Some(p_rect)
            } else {
                p_clip
            };

            // Apply scroll offset to children origin
            let scroll_off = if p_flags.contains(BoxFlags::SCROLL_CHILDREN) {
                let k = self.boxes[parent].key;
                self.persist.get(&k).map(|p| p.scroll_visual).unwrap_or(0.0)
            } else {
                0.0
            };

            let parent_origin = [p_rect[0], p_rect[1]];

            let node_origin = if let Some(fp) = self.boxes[id].floating_pos {
                [parent_origin[0] + fp[0], parent_origin[1] + fp[1]]

            } else {
                // @Speed
                //
                // Find cursor: sum of preceding siblings along parent axis
                //

                let siblings = &self.boxes[parent].children;
                let pos = siblings.iter().position(|&s| s == id).unwrap();
                let cursor: f32 = siblings[..pos].iter()
                    .map(|&s| self.boxes[s].computed_size[p_axis])
                    .sum();

                let mut o = parent_origin;
                o[p_axis]  = (parent_origin[p_axis] + cursor - scroll_off).round();
                o[p_cross] = parent_origin[p_cross].round();
                o
            };

            let size = self.boxes[id].computed_size;
            self.boxes[id].rect = [
                node_origin[0].round(), node_origin[1].round(),
                (node_origin[0] + size[0]).round(), (node_origin[1] + size[1]).round(),
            ];
            self.boxes[id].clip_rect = child_clip;
        }
    }

    #[inline] pub fn push_size(&mut self, x: Size, y: Size) { self.stacks.size_x.push(x); self.stacks.size_y.push(y); }
    #[inline] pub fn pop_size(&mut self)                    { self.stacks.size_x.pop(); self.stacks.size_y.pop(); }
    #[inline] pub fn push_bg(&mut self, c: Color)           { self.stacks.bg_color.push(c); }
    #[inline] pub fn pop_bg(&mut self)                      { self.stacks.bg_color.pop(); }
    #[inline] pub fn push_axis(&mut self, a: Axis)          { self.stacks.child_axis.push(a); }
    #[inline] pub fn pop_axis(&mut self)                    { self.stacks.child_axis.pop(); }
}

//
// Render pass
//
// Walks the finished laid-out box tree and emits gpu draw calls.
//

#[inline]
pub fn render(ui: &UiState, gpu: &mut Gpu) {
    if let Some(root) = ui.root {
        render_box(root, ui, gpu);
    }
}

pub fn render_box(id: BoxRef, ui: &UiState, gpu: &mut Gpu) {
    let b = &ui.boxes[id];
    let [x0, y0, x1, y1] = b.rect;
    let (x, y, w, h) = (x0, y0, x1 - x0, y1 - y0);

    let persist  = ui.persist.get(&b.key);
    let hot_t    = persist.map(|p| p.hot_t).unwrap_or(0.0);
    let active_t = persist.map(|p| p.active_t).unwrap_or(0.0);

    if b.flags.contains(BoxFlags::DRAW_BG) {
        let color = if b.hover_color.a > 0 {
            lerp_color(b.bg_color.into(), b.hover_color.into(), hot_t)
        } else {
            b.bg_color.into()
        };

        let flash = active_t * 0.15;
        let flashed_color = [
            (color[0] + flash).min(1.0),
            (color[1] + flash).min(1.0),
            (color[2] + flash).min(1.0),
            (color[3])
        ].into();

        gpu::draw_rect(gpu, x, y, w, h, flashed_color);
    }

    //
    // 1px border on bottom edge only
    //
    if b.flags.contains(BoxFlags::DRAW_BORDER) {
        // Bottom border
        gpu::draw_rect(gpu, x, y + h - 1.0, w, 1.0, b.border_color);
        // Left border
        gpu::draw_rect(gpu, x, y, 1.0, h, b.border_color);
    }

    if b.flags.contains(BoxFlags::DRAW_TEXT) {
        let text_x = x + b.padding_left;
        let text_y = y + h * 0.5 + b.font_size * 0.35;

        let display_text = b.text.find("##") // Strip ##
            .map(|i| &b.text[..i])
            .unwrap_or(&b.text);

        if !display_text.is_empty() {
            gpu::draw_text(gpu, display_text, text_x, text_y, b.font_size, b.text_color);
        }
    }

    if b.flags.contains(BoxFlags::CLIP_CHILDREN) {
        gpu::push_clip(gpu, x, y, w, h);
    }

    for &child in &b.children {
        render_box(child, ui, gpu);
    }

    if b.flags.contains(BoxFlags::CLIP_CHILDREN) {
        gpu::pop_clip(gpu);
    }
}

pub struct BoxBuilder<'a> {
    ui:           &'a mut UiState,

    force_clickable:     bool,

    label_hash:    LabelHash,

    flags:         BoxFlags,
    floating_pos:  Option<[f32; 2]>,
    size_x:        Option<Size>,
    size_y:        Option<Size>,
    bg:            Option<Color>,
    color:         Option<Color>,
    font_size:     Option<f32>,
    hover_color:   Option<Color>,
    border_color:  Option<Color>,
    axis:          Option<Axis>,
    text:          Option<Boxed<str>>,
    padding:       Option<f32>,  // @Cleanup
    padding_left:  Option<f32>,
    padding_right: Option<f32>,
}

impl<'a> BoxBuilder<'a> {
    #[inline]
    fn new(ui: &'a mut UiState, key: &str, flags: BoxFlags) -> Self {
        Self {
            label_hash: ui.hash_str(key),
            padding: None,
            size_x: None, size_y: None,
            font_size: None,
            force_clickable: false,
            bg: None, color: None, hover_color: None, border_color: None, axis: None,
            text: None,
            floating_pos: None,
            padding_right: None, padding_left: None,

            ui,
            flags,
        }
    }

    #[inline]
    pub fn size(mut self, x: Size, y: Size) -> Self {
        self.size_x = Some(x);
        self.size_y = Some(y);
        self
    }

    #[inline]
    pub fn floating(mut self, x: f32, y: f32) -> Self {
        self.floating_pos = Some([x, y]);
        self
    }

    #[inline]
    pub fn bg(mut self, c: Color) -> Self {
        self.bg = Some(c);
        self
    }

    #[inline]
    pub fn clip(mut self) -> Self {
        self.flags |= BoxFlags::CLIP_CHILDREN;
        self
    }

    #[inline]
    pub fn color(mut self, c: Color) -> Self {
        self.color = Some(c);
        self
    }

    //
    // NOTE: This expects an already scaled font size,
    // meaning that this is the FINAL pixel size, and
    // there will not be any mutation of it downstream.
    //
    #[inline]
    pub fn font_size(mut self, s: f32) -> Self {
        self.font_size = Some(s);
        self
    }

    #[inline]
    pub fn border(mut self, c: Color) -> Self {
        self.border_color = Some(c);
        self
    }

    #[inline]
    pub fn hover_color(mut self, c: Color) -> Self {
        self.hover_color = Some(c);
        self
    }

    #[inline]
    pub fn clickable(mut self) -> Self {
        self.force_clickable = true;
        self
    }

    #[inline]
    pub fn padding(mut self, p: f32) -> Self {
        self.padding = Some(p); // @Cleanup
        self
    }

    #[inline]
    pub fn padding_left(mut self, left: f32) -> Self {
        self.padding_left  = Some(left);
        self
    }

    #[inline]
    pub fn padding_right(mut self, right: f32) -> Self {
        self.padding_right  = Some(right);
        self
    }

    #[inline]
    pub fn padding_x(mut self, left: f32, right: f32) -> Self {
        self.padding_left  = Some(left);
        self.padding_right = Some(right);
        self
    }

    #[inline]
    pub fn text(mut self, t: impl Into<Boxed<str>>) -> Self {
        self.text = Some(t.into());
        self
    }

    // terminal, no children
    #[inline]
    pub fn build(self) -> BoxRef {
        self.build_impl(None::<fn(&mut UiState)>)
    }

    // container, has children via closure
    #[inline]
    pub fn build_children(self, f: impl FnOnce(&mut UiState)) -> BoxRef {
        self.build_impl(Some(f))
    }

    fn build_impl(self, f: Option<impl FnOnce(&mut UiState)>) -> BoxRef {
        let ui = self.ui;

        // Push stacks
        if let Some(x) = self.size_x { ui.stacks.size_x.push(x) }
        if let Some(y) = self.size_y { ui.stacks.size_y.push(y) }
        if let Some(c) = self.bg     { ui.stacks.bg_color.push(c) }
        if let Some(c) = self.color  { ui.stacks.text_color.push(c) }
        if let Some(a) = self.axis   { ui.stacks.child_axis.push(a) }
        if let Some(p) = self.padding { ui.stacks.padding.push(p); }
        if let Some(p) = self.padding_left { ui.stacks.padding_left.push(p); }
        if let Some(p) = self.padding_right { ui.stacks.padding_right.push(p); }

        let id = ui.push_box(self.label_hash, self.flags);

        // Set these directly since they're not a stack
        if let Some(c) = self.hover_color {
            ui.boxes[id].hover_color = c;
            ui.boxes[id].flags |= BoxFlags::HOVERABLE;
        }
        if self.force_clickable {
            ui.boxes[id].flags |= BoxFlags::CLICKABLE;
        }
        if let Some(s) = self.font_size {
            ui.boxes[id].font_size = s;
        }
        if let Some(c) = self.border_color {
            ui.boxes[id].border_color = c;
            ui.boxes[id].flags |= BoxFlags::DRAW_BORDER;
        }
        if let Some(f) = self.floating_pos {
            ui.boxes[id].floating_pos = Some(f);
        }

        // Set text directly on the box after creation
        if let Some(t) = self.text {
            ui.boxes[id].text = t;
            ui.boxes[id].flags |= BoxFlags::DRAW_TEXT;
        }

        // Pop bg BEFORE building children so they don't inherit it
        if self.bg.is_some() { ui.stacks.bg_color.pop(); }

        if let Some(f) = f {
            ui.push_parent(id);
            f(ui);
            ui.pop_parent();
        }

        // Pop remaining stacks after children
        _ = ui.stacks.size_x.pop();
        _ = ui.stacks.size_y.pop();
        _ = ui.stacks.text_color.pop();
        _ = ui.stacks.child_axis.pop();
        _ = ui.stacks.padding.pop();
        _ = ui.stacks.padding_left.pop();
        _ = ui.stacks.padding_right.pop();

        id
    }
}

//
// UiState builder methods
//

impl UiState {
    /// Horizontal container
    #[inline]
    pub fn row<'a>(&'a mut self, key: &str) -> BoxBuilder<'a> {
        let mut b = BoxBuilder::new(self, key, BoxFlags::DRAW_BG);
        b.axis = Some(Axis::X);
        b
    }

    /// Vertical container
    #[inline]
    pub fn col<'a>(&'a mut self, key: &str) -> BoxBuilder<'a> {
        let mut b = BoxBuilder::new(self, key, BoxFlags::DRAW_BG);
        b.axis = Some(Axis::Y);
        b
    }

    /// Text label, no background
    #[inline]
    pub fn label<'a>(&'a mut self, key: &str) -> BoxBuilder<'a> {
        BoxBuilder::new(self, key, BoxFlags::DRAW_TEXT)
    }

    /// Scrollable vertical container
    #[inline]
    pub fn scroll<'a>(&'a mut self, key: &str) -> BoxBuilder<'a> {
        let mut b = BoxBuilder::new(
            self, key,
            BoxFlags::DRAW_BG | BoxFlags::SCROLL_CHILDREN | BoxFlags::CLIP_CHILDREN
        );
        b.axis = Some(Axis::Y);
        b
    }

    /// Clickable row, hoverable + clickable flags
    #[inline]
    pub fn button<'a>(&'a mut self, key: &str) -> BoxBuilder<'a> {
        BoxBuilder::new(
            self, key,
            BoxFlags::DRAW_BG | BoxFlags::HOVERABLE | BoxFlags::CLICKABLE
        )
    }

    /// Fixed-size empty spacer, for virtual list padding
    #[inline]
    pub fn spacer(&mut self, key: &str, axis: Axis, size: f32) {
        let sz = Size::px(size);
        let (sx, sy) = match axis {
            Axis::X => (sz, Size::px(0.0)),   // sy is 0, not fill
            Axis::Y => (Size::fill(), sz),
        };
        self.push_size(sx, sy);
        self.push_box(self.hash_str(key), BoxFlags::empty());
        self.pop_size();
    }

    #[inline]
    pub fn spacer_fill(&mut self, key: &str, axis: Axis) {
        let (sx, sy) = match axis {
            Axis::X => (Size::fill(), Size::px(0.0)),
            Axis::Y => (Size::px(0.0), Size::fill()),
        };
        self.push_size(sx, sy);
        self.push_box(self.hash_str(key), BoxFlags::empty());
        self.pop_size();
    }

    #[inline]
    pub fn tick(&mut self) {
        self.tick_animations();
    }
}
