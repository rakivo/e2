#![allow(unsafe_op_in_unsafe_fn)]

use crate::messager::MESSAGER_FONT_SIZE;
use crate::util::format_bytes;
use crate::{Editor, Glyph, PASTE_ANIMATION_BITS, PASTE_ANIMATION_MASK, PASTE_ANIMATION_MAX_ID, PASTE_ANIMATION_PER_WORD, SCALE_STEP, palette, scale_base_font_size};
use crate::color::{Color, GpuColor, lerp_color};

use std::ffi::CStr;
use std::ops::Range;

use ash::vk;
use smallvec::SmallVec;
use gpu_allocator::MemoryLocation;
use raw_window_handle::{DisplayHandle, WindowHandle};
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme, Allocator, AllocatorCreateDesc};

pub const ATLAS_SIZE: u32          = 4096;
pub const ATLAS_RESET_RATIO: f32   = 0.8;
pub const INITIAL_VERTEX_BUFFER_CAPACITY: u64 = 8 * 1024 * 1024;
pub const INITIAL_BLUR_VERTEX_BUFFER_CAPACITY: u64 = 4 * 1024 * 1024;
pub const FRAMES_IN_FLIGHT: usize      = 2;

#[derive(Default, Debug, Clone, Copy)]
pub struct GpuStar {
    pub atlas_x: u16,
    pub atlas_y: u16,
    pub size:    u16,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct GpuGlyph {
    pub uv_x: u16, pub uv_y: u16,
    pub uv_w: u16, pub uv_h: u16,
    pub w: u16,    pub h: u16,
    pub bearing_x: i16, pub bearing_y: i16,
    pub advance: f32,
}

#[repr(C)]
#[derive(Copy, Clone, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vert {
    pub pos:   [f32; 2],
    pub uv:    [f32; 2],
    pub uv2:   [f32; 3],
    pub color: GpuColor,
}

impl Vert {
    #[inline]
    pub const fn new(pos: [f32; 2], uv: [f32; 2], color: GpuColor) -> Self {
        Self { color, pos, uv, uv2: [0.0; _] }
    }
}

pub struct Batch {
    pub verts: Vec<Vert>,
    pub clip:  [f32; 4],
}

impl Batch {
    #[inline]
    pub const fn new(clip: [f32; 4]) -> Self {
        Self { verts: Vec::new(), clip }
    }

    #[inline]
    pub const fn full_window(w: f32, h: f32) -> Self {
        Self::new([0.0, 0.0, w, h])
    }
}

struct BlurFrameResources {
    captured_img:  AllocImage,
    captured_view: vk::ImageView,
    blur_a_img:    AllocImage,
    blur_a_view:   vk::ImageView,
    blur_b_img:    AllocImage,
    blur_b_view:   vk::ImageView,
    desc_set_h:    vk::DescriptorSet,
    desc_set_v:    vk::DescriptorSet,
}

struct Frame {
    cmd_pool:        vk::CommandPool,
    cmd_buf:         vk::CommandBuffer,
    image_available: vk::Semaphore,
    render_done:     vk::Semaphore,
    fence:           vk::Fence,

    // Buffers retired this frame, destroyed when fence signals next time
    retired_buffers: Vec<(vk::Buffer, Allocation)>,
}

struct AllocBuffer {
    buffer:     vk::Buffer,
    allocation: Allocation,
}

struct AllocImage {
    img:        vk::Image,
    allocation: Allocation,
}

struct DrawCmd {
    range: Range<u32>,
    clip:  [f32; 4],
}

pub struct PendingAtlasUpload {
    x: u32, y: u32, w: u32, h: u32,
    data: *const u8,  // Points into glyph_pixels value, valid until glyph_pixels is mutated (which it never is)
    data_len: usize,
}

const VERT_SPV: &[u8] = include_bytes!("../assets/vert.spv");
const FRAG_SPV: &[u8] = include_bytes!("../assets/frag.spv");

const BLUR_H_SPV: &[u8] = include_bytes!("../assets/blur_h.spv");
const BLUR_V_SPV: &[u8] = include_bytes!("../assets/blur_v.spv");

pub struct Gpu {
    pub overlay_mode: bool,

    //
    // Swash font state
    //
    pub font_data:     &'static [u8],
    pub font_offset:   u32,
    pub font_key:      swash::CacheKey,
    pub scale_context: swash::scale::ScaleContext,

    //
    // Atlas CPU state
    //
    pub atlas_cur_x: u16,
    pub atlas_cur_y: u16,
    pub atlas_row_h: u16,
    pub glyphs: rustc_hash::FxHashMap<(char, u32), GpuGlyph>,

    pub glyph_pixels: rustc_hash::FxHashMap<(char, u32), Box<[u8]>>,
    pub pending_atlas_uploads: Vec<PendingAtlasUpload>,

    //
    // Batch vertex state
    //
    pub batch_pool:  Vec<Batch>,
    pub batch_count: usize,
    pub clip_stack:  Vec<[f32; 4]>,

    pub glyph_scratch: Vec<(GpuGlyph, f32)>,
    pub regions_scratch: Vec<vk::BufferImageCopy>,

    //
    // Window size
    //
    pub win_w: f32,
    pub win_h: f32,

    //
    // Vulkan core. @Important: Order matters for Drop
    //
    allocator: Option<Allocator>,

    vertex_buffer: Option<AllocBuffer>,
    current_vertex_buffer_capacity: u64,

    staging_buffer: Option<AllocBuffer>,   // Reused for atlas uploads
    staging_capacity: u64,

    atlas_image:  Option<AllocImage>,
    atlas_view: vk::ImageView,

    pdev:            vk::PhysicalDevice,

    sampler:         vk::Sampler,
    descriptor_pool: vk::DescriptorPool,
    desc_set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline:        vk::Pipeline,
    render_pass:     vk::RenderPass,
    pub one_shot_cmd_pool:  vk::CommandPool,
    pub one_shot_cmd_buf:   vk::CommandBuffer,
    pub upload_fence:       vk::Fence,
    pub upload_in_flight:   bool,

    pub blur_batch_pool:  Vec<Batch>,
    pub blur_batch_count: usize,
    blur_vertex_buffer:               Option<AllocBuffer>,
    current_blur_vertex_buffer_capacity: u64,
    blur_draw_scratch: Vec<DrawCmd>,

    blur_sampler:    vk::Sampler,

    composite_render_pass:   vk::RenderPass,
    composite_framebuffers:  Vec<vk::Framebuffer>,
    composite_pipeline:      vk::Pipeline,

    _blur_desc_set_layout: vk::DescriptorSetLayout,
    blur_pipeline_layout: vk::PipelineLayout,
    blur_h_pipeline:      vk::Pipeline,
    blur_v_pipeline:      vk::Pipeline,
    _blur_descriptor_pool: vk::DescriptorPool,

    swapchain_images: Vec<vk::Image>,
    queue_family:     u32,

    framebuffers:    Vec<vk::Framebuffer>,
    swapchain_views: Vec<vk::ImageView>,
    swapchain:       vk::SwapchainKHR,
    surface_format:  vk::SurfaceFormatKHR,
    swapchain_ext:   ash::khr::swapchain::Device,

    frames:       Vec<Frame>,
    frame_index:  usize,
    draw_scratch: Vec<DrawCmd>,

    queue:          vk::Queue,
    device:         ash::Device,
    surface:        vk::SurfaceKHR,
    surface_ext:    ash::khr::surface::Instance,
    _instance:      ash::Instance,
    _entry:         ash::Entry,

    blur_desc_sets_h_stored: [vk::DescriptorSet; FRAMES_IN_FLIGHT],
    blur_desc_sets_v_stored: [vk::DescriptorSet; FRAMES_IN_FLIGHT],

    blur_resources:  [Option<BlurFrameResources>; FRAMES_IN_FLIGHT],
    descriptor_sets: [vk::DescriptorSet; FRAMES_IN_FLIGHT],

    pub star: GpuStar,

    _star_pixels: Vec<Box<[u8]>>
}

impl Gpu {
    #[inline]
    pub fn verts_mut(&mut self) -> &mut Vec<Vert> {
        if self.overlay_mode {
            &mut self.blur_batch_pool[self.blur_batch_count - 1].verts
        } else {
            &mut self.batch_pool[self.batch_count - 1].verts
        }
    }

    #[inline]
    pub fn current_clip(&self) -> [f32; 4] {
        if self.overlay_mode {
            self.blur_batch_pool[self.blur_batch_count - 1].clip
        } else {
            self.batch_pool[self.batch_count - 1].clip
        }
    }

    #[inline]
    pub fn blur_verts_mut(&mut self) -> &mut Vec<Vert> {
        &mut self.blur_batch_pool[self.blur_batch_count - 1].verts
    }

    #[inline]
    pub fn blur_current_clip(&self) -> [f32; 4] {
        self.blur_batch_pool[self.blur_batch_count - 1].clip
    }

    #[inline]
    pub fn measure_message(&mut self, s: &str) -> f32 {
        measure_text(self, s, MESSAGER_FONT_SIZE)
    }

    #[inline]
    pub fn resize(&mut self, w: u32, h: u32) {
        self.win_w = w as f32;
        self.win_h = h as f32;
        self.batch_pool[0].clip = [0.0, 0.0, self.win_w, self.win_h];
        self.blur_batch_pool[0].clip = [0.0, 0.0, self.win_w, self.win_h];
        unsafe { self.recreate_swapchain() };
    }

    #[inline]
    pub fn submit_frame(&mut self) -> Result<(), vk::Result> {
        let _tracy = tracy::span!("submit_frame");

        unsafe { self.submit_frame_impl() }
    }
}

pub fn prewarm_glyphs(gpu: &mut Gpu, font_size: f32) {
    // ASCII printable
    for c in ' '..='~' {
        get_glyph_no_upload(gpu, c, font_size);
    }

    // Box drawing
    for c in '\u{2500}'..='\u{257F}' {
        get_glyph_no_upload(gpu, c, font_size);
    }

    flush_atlas_uploads(gpu);
}

pub fn prewarm_glyphs_and_print_preallocation_memory_usage(editor: &Editor, gpu: &mut Gpu) {
    let mut builtin_prewarmed_font_sizes: SmallVec<[f32; 16]> = [
        editor.scale,
        editor.scale - SCALE_STEP,
        editor.scale + SCALE_STEP,
        editor.scale - 2.0 * SCALE_STEP,
        editor.scale + 2.0 * SCALE_STEP,
        editor.scale + 3.0 * SCALE_STEP,
    ].into_iter().map(scale_base_font_size).collect();

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
    for font_size in builtin_prewarmed_font_sizes {
        prewarm_glyphs(gpu, font_size);
    }

    prewarm_glyphs(gpu, MESSAGER_FONT_SIZE);

    let vertex_batch_pool_allocation = gpu.batch_pool.iter()
        .map(|b| b.verts.capacity())
        .sum::<usize>();

    eprintln!("[Vertex batch pool preallocation]: {}", format_bytes(vertex_batch_pool_allocation));
    eprintln!("[Vertex buffer size]:              {}", format_bytes(gpu.current_vertex_buffer_capacity as _));
    eprintln!("[Glyph memory usage]:              {}", format_bytes(gpu.glyphs.capacity() * std::mem::size_of::<((char, u32), GpuGlyph)>())); // @Cleanup

    print_atlas_usage(gpu);
}

pub fn print_atlas_usage(gpu: &Gpu) {
    let bytes_per_pixel = 1; // R8Unorm
    let total_bytes = ATLAS_SIZE * ATLAS_SIZE * bytes_per_pixel;

    // This is a rough estimate - current row is partially used
    let used_bytes = gpu.atlas_cur_y as u32 * ATLAS_SIZE * bytes_per_pixel;
    eprintln!(
        "[Atlas] used ~= {} / {} bytes ({:.2}%)",
        format_bytes(used_bytes as _),
        format_bytes(total_bytes as _),
        (used_bytes as f32 / total_bytes as f32) * 100.0
    );
}

pub fn flush_atlas_uploads(gpu: &mut Gpu) {
    unsafe { flush_atlas_uploads_impl(gpu) }
}

pub fn wait_for_atlas_upload(gpu: &mut Gpu) {
    unsafe {
        if gpu.upload_in_flight {
            gpu.device.wait_for_fences(&[gpu.upload_fence], true, u64::MAX).unwrap();
            gpu.device.reset_fences(&[gpu.upload_fence]).unwrap();
            gpu.upload_in_flight = false;
        }
    }
}

pub unsafe fn flush_atlas_uploads_impl(gpu: &mut Gpu) {
    if gpu.pending_atlas_uploads.is_empty() { return; }

    if gpu.upload_in_flight {
        gpu.device.wait_for_fences(&[gpu.upload_fence], true, u64::MAX).unwrap();
        gpu.device.reset_fences(&[gpu.upload_fence]).unwrap();
        gpu.upload_in_flight = false;
    }

    //
    // Size staging buf to fit all pending data at once
    //
    let total: u64 = gpu.pending_atlas_uploads.iter()
        .map(|u| u.data_len as u64)
        .sum();

    if total > gpu.staging_capacity {
        let new_cap = total * 2;
        let alloc   = gpu.allocator.as_mut().unwrap();
        let old     = gpu.staging_buffer.take().unwrap();
        alloc.free(old.allocation).unwrap();
        gpu.device.destroy_buffer(old.buffer, None);
        let (buf, allocation) = create_buffer(
            &gpu.device, alloc, new_cap,
            vk::BufferUsageFlags::TRANSFER_SRC, MemoryLocation::CpuToGpu,
        );
        gpu.staging_buffer   = Some(AllocBuffer { buffer: buf, allocation });
        gpu.staging_capacity = new_cap;
    }

    //
    // Pack all pixel data into staging
    //
    let staging_ptr = gpu.staging_buffer.as_mut().unwrap()
        .allocation.mapped_ptr().unwrap().as_ptr() as *mut u8;

    let mut buffer_offset = 0u64;

    gpu.regions_scratch.clear();
    gpu.regions_scratch.extend(gpu.pending_atlas_uploads.iter()
        .map(|u| {
            std::ptr::copy_nonoverlapping(u.data, staging_ptr.add(buffer_offset as usize), u.data_len);

            let region = vk::BufferImageCopy::default()
                .buffer_offset(buffer_offset)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0, base_array_layer: 0, layer_count: 1,
                })
                .image_offset(vk::Offset3D { x: u.x as i32, y: u.y as i32, z: 0 })
                .image_extent(vk::Extent3D { width: u.w, height: u.h, depth: 1 });
            buffer_offset += u.data_len as u64;
            region
        }));

    _ = gpu.device.reset_command_pool(gpu.one_shot_cmd_pool, vk::CommandPoolResetFlags::empty());
    begin_one_shot(&gpu.device, gpu.one_shot_cmd_buf);

    transition_image_layout(
        &gpu.device, gpu.one_shot_cmd_buf,
        gpu.atlas_image.as_ref().unwrap().img,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
    );

    gpu.device.cmd_copy_buffer_to_image(
        gpu.one_shot_cmd_buf,
        gpu.staging_buffer.as_ref().unwrap().buffer,
        gpu.atlas_image.as_ref().unwrap().img,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        &gpu.regions_scratch,
    );

    transition_image_layout(
        &gpu.device, gpu.one_shot_cmd_buf,
        gpu.atlas_image.as_ref().unwrap().img,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
    );

    gpu.device.end_command_buffer(gpu.one_shot_cmd_buf).unwrap();

    //
    // Submit with fence
    //

    let submit_info = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&gpu.one_shot_cmd_buf));

    gpu.device.queue_submit(gpu.queue, &[submit_info], gpu.upload_fence).unwrap();
    gpu.upload_in_flight = true;

    gpu.pending_atlas_uploads.clear();
}

pub fn get_glyph(gpu: &mut Gpu, c: char, size: f32) -> Option<GpuGlyph> {
    let g = get_glyph_no_upload(gpu, c, size);
    flush_atlas_uploads(gpu);
    wait_for_atlas_upload(gpu);
    g
}

pub fn get_glyph_no_upload(gpu: &mut Gpu, c: char, size: f32) -> Option<GpuGlyph> {
    use swash::FontRef;
    use swash::zeno::Format;
    use swash::scale::{Render, Source};

    let size = (size * 2.0).round() / 2.0;
    let key = (c, (size * 2.0) as u32);
    if let Some(g) = gpu.glyphs.get(&key) {
        return Some(*g);
    }

    let font_ref = FontRef { data: &gpu.font_data, offset: gpu.font_offset, key: gpu.font_key };

    let glyph_id = font_ref.charmap().map(c);
    let advance = font_ref.glyph_metrics(&[]).scale(size).advance_width(glyph_id);

    if glyph_id == 0 {
        let g = GpuGlyph { advance, ..Default::default() };
        gpu.glyphs.insert(key, g);
        return Some(g);
    }

    let mut scaler = gpu.scale_context.builder(font_ref)
        .size(size)
        .build();

    let image = Render::new(&[Source::Outline])
        .format(Format::Alpha)
        .render(&mut scaler, glyph_id);

    let Some(image) = image else {
        let g = GpuGlyph { advance, ..Default::default() };
        gpu.glyphs.insert(key, g);
        return Some(g);
    };

    let w = image.placement.width as u16;
    let h = image.placement.height as u16;

    if w == 0 || h == 0 || !image.data.iter().any(|&b| b > 0) {
        let g = GpuGlyph { advance, ..Default::default() };
        gpu.glyphs.insert(key, g);
        return Some(g);
    }

    let atlas_used = gpu.atlas_cur_y as u32 * ATLAS_SIZE + gpu.atlas_cur_x as u32;
    let atlas_total = ATLAS_SIZE * ATLAS_SIZE;
    if (atlas_used as f32 / atlas_total as f32) > ATLAS_RESET_RATIO {
        reset_atlas(gpu);
    }

    if gpu.atlas_cur_x + w + 1 > ATLAS_SIZE as u16 {
        gpu.atlas_cur_y += gpu.atlas_row_h + 1;
        gpu.atlas_cur_x = 1;
        gpu.atlas_row_h = 0;
    }

    if gpu.atlas_cur_y + h + 1 > ATLAS_SIZE as u16 {
        eprintln!("ATLAS FULL");
        return None;
    }

    let atlas_x = gpu.atlas_cur_x;
    let atlas_y = gpu.atlas_cur_y;

    let image_data: Box<[u8]> = image.data.into();
    let (data, data_len) = (image_data.as_ptr(), image_data.len());
    gpu.glyph_pixels.insert(key, image_data);
    gpu.pending_atlas_uploads.push(PendingAtlasUpload {
        x: atlas_x as u32, y: atlas_y as u32,
        w: w as u32,       h: h as u32,
        data, data_len
    });

    let bearing_x = image.placement.left as i16;
    let bearing_y = image.placement.top as i16;

    let g = GpuGlyph {
        uv_x: gpu.atlas_cur_x,
        uv_y: gpu.atlas_cur_y,
        uv_w: w,
        uv_h: h,
        w, h,
        bearing_x,
        bearing_y,
        advance,
    };

    gpu.atlas_cur_x += w + 1;
    if h > gpu.atlas_row_h { gpu.atlas_row_h = h; }

    debug_assert!(g.uv_w > 0 && g.uv_h > 0);
    gpu.glyphs.insert(key, g);
    Some(g)
}

#[inline]
pub fn reset_atlas(gpu: &mut Gpu) {
    eprintln!("[Resetting Atlas!]");

    gpu.glyphs.clear();
    gpu.atlas_cur_x = 1;
    gpu.atlas_cur_y = 1;
    gpu.atlas_row_h = 0;
}

#[inline]
pub fn push_overlay_mode(gpu: &mut Gpu) {
    gpu.overlay_mode = true;
}

#[inline]
pub fn pop_overlay_mode(gpu: &mut Gpu) {
    gpu.overlay_mode = false;
}

#[inline]
pub fn push_clip(gpu: &mut Gpu, x: f32, y: f32, w: f32, h: f32) {
    let current = gpu.current_clip();
    gpu.clip_stack.push(current);

    let (pool, count) = if gpu.overlay_mode {
        (&mut gpu.blur_batch_pool, &mut gpu.blur_batch_count)
    } else {
        (&mut gpu.batch_pool, &mut gpu.batch_count)
    };

    let i = *count;
    if i >= pool.len() {
        pool.push(Batch::new([x, y, w, h]));
    } else {
        pool[i].clip = [x, y, w, h];
        pool[i].verts.clear();
    }

    *count += 1;
}

#[inline]
pub fn pop_clip(gpu: &mut Gpu) {
    let parent_clip = gpu.clip_stack.pop().expect("unbalanced pop_clip");

    let (pool, count) = if gpu.overlay_mode {
        (&mut gpu.blur_batch_pool, &mut gpu.blur_batch_count)
    } else {
        (&mut gpu.batch_pool, &mut gpu.batch_count)
    };

    let i = *count;
    if i >= pool.len() {
        pool.push(Batch::new(parent_clip));
    } else {
        pool[i].clip = parent_clip;
        pool[i].verts.clear();
    }

    *count += 1;
}

#[inline(always)]
pub fn draw_rect_rounded(gpu: &mut Gpu, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Color) {
    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;
    let hw = w * 0.5;
    let hh = h * 0.5;
    let x0 =  x       * inv_sw * 2.0 - 1.0;
    let x1 = (x + w)  * inv_sw * 2.0 - 1.0;
    let y0 =  1.0 - y       * inv_sh * 2.0;
    let y1 =  1.0 - (y + h) * inv_sh * 2.0;
    let c: GpuColor = color.into();
    let r_norm = (radius / hh.min(hw)).clamp(0.0, 1.0);
    let verts = gpu.verts_mut();
    verts.reserve(6);
    macro_rules! v {
        ($px:expr, $py:expr) => {
            Vert { pos: [$px, $py], uv: [29000.0 + x, y], uv2: [x + w, y + h, r_norm], color: c }
        };
    }
    verts.push(v!(x0, y0));
    verts.push(v!(x1, y0));
    verts.push(v!(x0, y1));
    verts.push(v!(x1, y0));
    verts.push(v!(x1, y1));
    verts.push(v!(x0, y1));
}

#[inline(always)]
pub fn draw_rect_rounded_outline(gpu: &mut Gpu, x: f32, y: f32, w: f32, h: f32, radius: f32, thickness: f32, color: Color) {
    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;
    let hw = w * 0.5;
    let hh = h * 0.5;
    let x0 =  x       * inv_sw * 2.0 - 1.0;
    let x1 = (x + w)  * inv_sw * 2.0 - 1.0;
    let y0 =  1.0 - y       * inv_sh * 2.0;
    let y1 =  1.0 - (y + h) * inv_sh * 2.0;
    let c: GpuColor = color.into();
    let r_norm = (radius    / hh.min(hw)).clamp(0.0, 1.0);
    let t_norm = (thickness / hh.min(hw)).clamp(0.0, 1.0);
    let c = [c[0], c[1], c[2], r_norm];
    let verts = gpu.verts_mut();
    verts.reserve(6);
    macro_rules! v {
        ($px:expr, $py:expr) => {
            Vert { pos: [$px, $py], uv: [39000.0 + x, y], uv2: [x + w, y + h, t_norm], color: GpuColor(c) }
        };
    }
    verts.push(v!(x0, y0));
    verts.push(v!(x1, y0));
    verts.push(v!(x0, y1));
    verts.push(v!(x1, y0));
    verts.push(v!(x1, y1));
    verts.push(v!(x0, y1));
}

#[inline(always)]
pub fn draw_blur_rect(gpu: &mut Gpu, x: f32, y: f32, w: f32, h: f32, tint: Color) {
    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;
    let x0 =  x       * inv_sw * 2.0 - 1.0;
    let x1 = (x + w)  * inv_sw * 2.0 - 1.0;
    let y0 =  1.0 - y       * inv_sh * 2.0;
    let y1 =  1.0 - (y + h) * inv_sh * 2.0;

    let verts = gpu.blur_verts_mut();
    let c  = tint.into_gpu();
    let uv = [-1.0f32, -1.0f32];
    verts.push(Vert::new([x0, y0], uv, c));
    verts.push(Vert::new([x1, y0], uv, c));
    verts.push(Vert::new([x0, y1], uv, c));
    verts.push(Vert::new([x1, y0], uv, c));
    verts.push(Vert::new([x1, y1], uv, c));
    verts.push(Vert::new([x0, y1], uv, c));
}

/// 4 rects, 24 verts
#[inline(always)]
pub fn draw_rect_outline(gpu: &mut Gpu, x: f32, y: f32, w: f32, h: f32, thickness: f32, color: Color) {
    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;
    let color: GpuColor = color.into();
    let verts  = gpu.verts_mut();

    verts.reserve(24);
    draw_rect_impl(verts, inv_sw, inv_sh, x,                 y,         w,         thickness, color); // Top
    draw_rect_impl(verts, inv_sw, inv_sh, x,                 y + h,     w,         thickness, color); // Bottom
    draw_rect_impl(verts, inv_sw, inv_sh, x,                 y,         thickness, h,         color); // Left
    draw_rect_impl(verts, inv_sw, inv_sh, x + w - thickness, y,         thickness, h,         color); // Right
}

/// Primitive rect - caller provides pre-baked reciprocals and verts ref.
#[inline(always)]
pub fn draw_rect_impl(
    verts: &mut Vec<Vert>,

    inv_sw: f32, inv_sh: f32,
    x: f32, y: f32, w: f32, h: f32,

    color: GpuColor,
) {
    let x0 =  x             * inv_sw * 2.0 - 1.0;
    let x1 = (x + w)        * inv_sw * 2.0 - 1.0;
    let y0 =  1.0 - y       * inv_sh * 2.0;
    let y1 =  1.0 - (y + h) * inv_sh * 2.0;

    verts.reserve(6);
    let base = verts.len();
    unsafe {
        let p = verts.as_mut_ptr().add(base);
        p.add(0).write(Vert::new([x0, y0], [0.0, 0.0], color));
        p.add(1).write(Vert::new([x1, y0], [0.0, 0.0], color));
        p.add(2).write(Vert::new([x0, y1], [0.0, 0.0], color));
        p.add(3).write(Vert::new([x1, y0], [0.0, 0.0], color));
        p.add(4).write(Vert::new([x1, y1], [0.0, 0.0], color));
        p.add(5).write(Vert::new([x0, y1], [0.0, 0.0], color));
        verts.set_len(base + 6);
    }
}

/// Convenience wrapper for call sites that still have a &mut Gpu handy.
#[inline(always)]
pub fn draw_rect(gpu: &mut Gpu, x: f32, y: f32, w: f32, h: f32, color: Color) {
    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;
    let color  = color.into();
    draw_rect_impl(gpu.verts_mut(), inv_sw, inv_sh, x, y, w, h, color);
}

#[inline]
pub fn draw_rect_gradient_h(gpu: &mut Gpu, x: f32, y: f32, w: f32, h: f32, color_left: Color, color_right: Color) {
    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;
    let cl: GpuColor = color_left.into();
    let cr: GpuColor = color_right.into();

    let x0 = x * inv_sw * 2.0 - 1.0;
    let y0 = 1.0 - y * inv_sh * 2.0;
    let x1 = (x + w) * inv_sw * 2.0 - 1.0;
    let y1 = 1.0 - (y + h) * inv_sh * 2.0;

    let verts = gpu.verts_mut();
    verts.reserve(6);
    let base = verts.len();
    unsafe {
        let p = verts.as_mut_ptr().add(base);
        p.add(0).write(Vert::new([x0, y0], [0.0, 0.0], cl)); // top-left
        p.add(1).write(Vert::new([x1, y0], [0.0, 0.0], cr)); // top-right
        p.add(2).write(Vert::new([x0, y1], [0.0, 0.0], cl)); // bottom-left
        p.add(3).write(Vert::new([x1, y0], [0.0, 0.0], cr)); // top-right
        p.add(4).write(Vert::new([x1, y1], [0.0, 0.0], cr)); // bottom-right
        p.add(5).write(Vert::new([x0, y1], [0.0, 0.0], cl)); // bottom-left
        verts.set_len(base + 6);
    }
}

#[inline]
pub fn draw_star(gpu: &mut Gpu, cx: f32, cy: f32, radius: f32, color: Color, star: GpuStar) {
    let x = cx - radius;
    let y = cy - radius;
    let w = radius * 2.0;
    let h = radius * 2.0;
    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;
    let color: GpuColor = color.into();

    let u0 = star.atlas_x as f32;
    let v0 = star.atlas_y as f32;
    let u1 = star.atlas_x as f32 + star.size as f32;
    let v1 = star.atlas_y as f32 + star.size as f32;

    let x0 =  x      * inv_sw * 2.0 - 1.0;
    let x1 = (x + w) * inv_sw * 2.0 - 1.0;
    let y0 = 1.0 -  y      * inv_sh * 2.0;
    let y1 = 1.0 - (y + h) * inv_sh * 2.0;

    let verts = gpu.verts_mut();
    verts.reserve(6);
    let base = verts.len();
    unsafe {
        let p = verts.as_mut_ptr().add(base);
        p.add(0).write(Vert::new([x0, y0], [u0, v0], color));
        p.add(1).write(Vert::new([x1, y0], [u1, v0], color));
        p.add(2).write(Vert::new([x0, y1], [u0, v1], color));
        p.add(3).write(Vert::new([x1, y0], [u1, v0], color));
        p.add(4).write(Vert::new([x1, y1], [u1, v1], color));
        p.add(5).write(Vert::new([x0, y1], [u0, v1], color));
        verts.set_len(base + 6);
    }
}

pub fn generate_star8_texture(size: u32) -> Vec<u8> {
    let mut pixels = vec![0u8; (size * size) as usize];
    let half      = size as f32 * 0.5;
    let outer_r   = half * 0.82;
    let inner_r   = outer_r * 0.72;
    let glow_r    = half * 0.98;

    for py in 0..size {
        for px in 0..size {
            let x = px as f32 - half + 0.5;
            let y = py as f32 - half + 0.5;

            let d = sdf_star8(x, y, outer_r, inner_r);

            let aa    = 1.2_f32;
            let core  = smoothstep(aa, -aa, d);
            let glow_t = (d / (glow_r - outer_r)).max(0.0);
            let glow  = (-glow_t * glow_t * 2.0).exp() * 0.5 * (1.0 - core);
            let alpha = (core + glow).min(1.0);

            pixels[(py * size + px) as usize] = (alpha * 255.0 + 0.5) as u8;
        }
    }

    pixels
}

fn sdf_star8(x: f32, y: f32, outer_r: f32, inner_r: f32) -> f32 {
    use std::f32::consts::PI;
    let an = PI / 8.0;
    let en = PI / 4.0;

    let angle  = y.atan2(x);
    let sector = (angle / en).round() * en;
    let (s, c) = sector.sin_cos();
    let px =  c * x + s * y;
    let py = -s * x + c * y;

    // Reflect into upper half
    let py = py.abs();

    let tip   = (outer_r, 0.0f32);
    let inner = (an.cos() * inner_r, an.sin() * inner_r);

    let edge  = (inner.0 - tip.0, inner.1 - tip.1);
    let to_p  = (px - tip.0,      py - tip.1);
    let len_sq = edge.0*edge.0 + edge.1*edge.1;
    let t      = ((to_p.0*edge.0 + to_p.1*edge.1) / len_sq).clamp(0.0, 1.0);
    let closest = (tip.0 + t*edge.0, tip.1 + t*edge.1);

    let dist  = ((px-closest.0).powi(2) + (py-closest.1).powi(2)).sqrt();
    let cross = edge.0*to_p.1 - edge.1*to_p.0;

    // Positive cross = left of edge = inside star
    if cross > 0.0 { -dist } else { dist }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline(always)]
pub fn measure_text(gpu: &mut Gpu, s: &str, font_size: f32) -> f32 {
    s.chars().map(|c| {
        get_glyph(gpu, c, font_size).map(|g| g.advance).unwrap_or(8.0)
    }).sum()
}

// Draw text with a per-character color - pass a closure that maps char index -> color
pub fn draw_text_colored(
    gpu:            &mut Gpu,
    text:           &str,
    mut x:          f32,
    y:              f32,
    font_size:      f32,
    color_callback: impl Fn(usize) -> Color,
) {
    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;

    gpu.glyph_scratch.clear();
    for c in text.chars() {
        let advance = match get_glyph(gpu, c, font_size) {
            Some(g) => { gpu.glyph_scratch.push((g, x)); g.advance }
            None    => 8.0,
        };
        x += advance;
    }

    let glyph_scratch: &Vec<(GpuGlyph, f32)> = unsafe { &*(&gpu.glyph_scratch as *const _) }; // @Hack

    let verts = gpu.verts_mut();
    verts.reserve(glyph_scratch.len() * 6);

    let base = verts.len();
    let ptr  = unsafe { verts.as_mut_ptr().add(base) };
    let mut count = 0usize;

    for (i, (g, gx_origin)) in glyph_scratch.iter().enumerate() {
        if g.w == 0 || g.h == 0 { continue; }

        let gx = (gx_origin + g.bearing_x as f32).round();
        let gy = (y         - g.bearing_y as f32).round();

        let x0 =  gx                      * inv_sw * 2.0 - 1.0;
        let x1 = (gx + g.w as f32)        * inv_sw * 2.0 - 1.0;
        let y0 =  1.0 - gy                * inv_sh * 2.0;
        let y1 =  1.0 - (gy + g.h as f32) * inv_sh * 2.0;

        let u0 =  g.uv_x            as f32;
        let v0 =  g.uv_y            as f32;
        let u1 = (g.uv_x + g.uv_w)  as f32;
        let v1 = (g.uv_y + g.uv_h)  as f32;

        let color: GpuColor = color_callback(i).into();

        unsafe {
            let v = ptr.add(count);
            v.add(0).write(Vert::new([x0, y0], [u0, v0], color));
            v.add(1).write(Vert::new([x1, y0], [u1, v0], color));
            v.add(2).write(Vert::new([x0, y1], [u0, v1], color));
            v.add(3).write(Vert::new([x1, y0], [u1, v0], color));
            v.add(4).write(Vert::new([x1, y1], [u1, v1], color));
            v.add(5).write(Vert::new([x0, y1], [u0, v1], color));
        }
        count += 6;
    }

    unsafe { verts.set_len(base + count); }
}

// Flat color convenience wrapper
#[inline]
pub fn draw_text(
    gpu: &mut Gpu,
    text: &str,
    x: f32, y: f32,
    font_size: f32,
    color: Color,
) {
    draw_text_colored(gpu, text, x, y, font_size, |_| color);
}

#[inline(always)]
pub fn draw_text_for_editor(
    verts:    &mut Vec<Vert>,

    inv_sw:   f32,                          // 1.0 / win_w - baked outside the line loop
    inv_sh:   f32,                          // 1.0 / win_h

    glyphs:   &[Glyph],

    origin_x: f32,
    y:        f32,

    cursor_col_glyph_index: Option<usize>,  // None = not cursor line

    cursor_color:    GpuColor,
    paste_highlight: GpuColor,
    copy_highlight:  GpuColor,

    insertion_ids:     &[u64],
    global_glyph_start: usize,   // ll.glyph_start
    insertion_ts:       [f32; PASTE_ANIMATION_MAX_ID * 2 + 2],
) {
    let animated = !insertion_ids.is_empty();
    let cursor_ci = cursor_col_glyph_index.unwrap_or(usize::MAX);

    // Reserve once for all glyphs on this line
    // 6 verts per glyph (two tris).
    let needed = glyphs.len() * 6;
    verts.reserve(needed);

    // SAFETY: we just reserved exactly `needed` elements above.
    // We write exactly 6 Verts per non-zero-size glyph, all fields initialized.
    // We update len once at the end.
    let base = verts.len();
    let ptr = unsafe { verts.as_mut_ptr().add(base) };
    let mut count = 0usize;

    for (i, g) in glyphs.iter().enumerate() {
        let gg = g.gpu_glyph;
        if std::hint::unlikely(gg.w == 0 || gg.h == 0) { continue; }

        // x is already the accumulated advance from layout - use g.x directly.
        let gx = (origin_x + g.x + gg.bearing_x as f32).round();
        let gy = (y              - gg.bearing_y as f32).round();
        let gw = gg.w as f32;
        let gh = gg.h as f32;

        let x0 =  gx              * inv_sw * 2.0 - 1.0;
        let x1 = (gx + gw)        * inv_sw * 2.0 - 1.0;
        let y0 =  1.0 - gy        * inv_sh * 2.0;
        let y1 =  1.0 - (gy + gh) * inv_sh * 2.0;

        let u0 =  gg.uv_x            as f32;
        let v0 =  gg.uv_y            as f32;
        let u1 = (gg.uv_x + gg.uv_w) as f32;
        let v1 = (gg.uv_y + gg.uv_h) as f32;

        let t = (i == cursor_ci) as u32 as f32;  // 0.0 or 1.0
        let base_color = GpuColor([
            g.color[0] + t * (cursor_color[0] - g.color[0]),
            g.color[1] + t * (cursor_color[1] - g.color[1]),
            g.color[2] + t * (cursor_color[2] - g.color[2]),
            g.color[3] + t * (cursor_color[3] - g.color[3]),
        ]);

        let color = if std::hint::unlikely(animated) {
            let gi   = global_glyph_start + i;
            let bit  = (gi % PASTE_ANIMATION_PER_WORD) * PASTE_ANIMATION_BITS;
            let word = gi / PASTE_ANIMATION_PER_WORD;
            let id   = ((insertion_ids[word] >> bit) & PASTE_ANIMATION_MASK) as usize;

            let t_raw = insertion_ts[id]; // id=0 -> 1.0 sentinel, id=1..=N -> actual t
            let ease  = 1.0 - (1.0 - t_raw).powi(3);

            let highlight_color = if id <= PASTE_ANIMATION_MAX_ID {
                paste_highlight
            } else {
                copy_highlight
            };

            lerp_color(highlight_color, base_color, ease)
        } else {
            base_color
        };

        unsafe {
            let v = ptr.add(count);
            v.add(0).write(Vert::new([x0, y0], [u0, v0], color));
            v.add(1).write(Vert::new([x1, y0], [u1, v0], color));
            v.add(2).write(Vert::new([x0, y1], [u0, v1], color));
            v.add(3).write(Vert::new([x1, y0], [u1, v0], color));
            v.add(4).write(Vert::new([x1, y1], [u1, v1], color));
            v.add(5).write(Vert::new([x0, y1], [u0, v1], color));
        }
        count += 6;
    }

    unsafe { verts.set_len(base + count); }
}

pub fn draw_line(
    gpu: &mut Gpu,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    thickness: f32,
    color: Color,
) {
    let dx = x1 - x0;
    let dy = y1 - y0;

    let len = (dx * dx + dy * dy).sqrt();
    if len <= 0.0001 {
        return;
    }

    let dir_x = dx / len;
    let dir_y = dy / len;

    //
    // Perpendicular vector
    //
    let nx = -dir_y;
    let ny =  dir_x;

    //
    // SDF Math setup
    // Prevent division by zero if thickness is exactly 0
    //
    let r_visual = (thickness * 0.5).max(0.1);
    let aa_margin = 1.5;  // 1.5 pixels of padding for soft anti-aliasing
    let r_quad = r_visual + aa_margin;

    //
    // Normalize the UV coordinates so that `1.0` is exactly the visual edge
    //
    let v_edge = r_quad / r_visual;

    let ox = nx * r_quad;
    let oy = ny * r_quad;

    let ax = x0 - ox;
    let ay = y0 - oy;

    let bx = x0 + ox;
    let by = y0 + oy;

    let cx = x1 + ox;
    let cy = y1 + oy;

    //
    // Renamed to avoid shadowing the dx/dy calculation above
    //
    let dx_vert = x1 - ox;
    let dy_vert = y1 - oy;

    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;

    let verts = gpu.verts_mut();
    let color: GpuColor = color.into();

    verts.reserve(6);

    let line_sentinel = 20000.0;

    let mut push = |x: f32, y: f32, v: f32| {
        let px = x * inv_sw * 2.0 - 1.0;
        let py = 1.0 - y * inv_sh * 2.0;

        verts.push(Vert::new(
            [px, py],
            [line_sentinel, v],
            color,
        ));
    };

    // Tri 1
    push(ax, ay, -v_edge);
    push(bx, by, v_edge);
    push(cx, cy, v_edge);

    // Tri 2
    push(ax, ay, -v_edge);
    push(cx, cy, v_edge);
    push(dx_vert, dy_vert, -v_edge);
}

pub fn draw_flashlight(gpu: &mut Gpu, center_x: f32, center_y: f32, radius: f32, color: Color) {
    let x0 = center_x - radius;
    let y0 = center_y - radius;
    let x1 = center_x + radius;
    let y1 = center_y + radius;

    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;
    let c: GpuColor = color.into();

    let sentinel = 49000.0;

    let uv_x  = sentinel + center_x;
    let uv_y  = center_y;
    let uv2_x = radius;
    let uv2_y = 0.0;
    let uv2_z = 0.0;

    gpu.verts_mut().reserve(6);

    let mut push = |x: f32, y: f32| {
        let px = x * inv_sw * 2.0 - 1.0;
        let py = 1.0 - y * inv_sh * 2.0;
        gpu.verts_mut().push(Vert {
            pos:   [px, py],
            uv:    [uv_x, uv_y],
            uv2:   [uv2_x, uv2_y, uv2_z],
            color: c,
        });
    };

    push(x0, y0);
    push(x1, y0);
    push(x1, y1);
    push(x0, y0);
    push(x1, y1);
    push(x0, y1);
}

pub fn draw_circle(
    gpu: &mut Gpu,
    cx: f32,
    cy: f32,
    radius: f32,
    color: Color,
) {
    let cx = cx.round();
    let cy = cy.round();

    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;
    let verts = gpu.verts_mut();
    let color: GpuColor = color.into();

    verts.reserve(6);

    let mut push = |x: f32, y: f32, uv_x: f32, uv_y: f32| {
        let px = x * inv_sw * 2.0 - 1.0;
        let py = 1.0 - y * inv_sh * 2.0;

        verts.push(Vert::new(
            [px, py],
            [uv_x, uv_y],
            color,
        ));
    };

    let min_x = cx - radius;
    let max_x = cx + radius;
    let min_y = cy - radius;
    let max_y = cy + radius;

    let circle_sentinel = 10000.0;

    // Tri 1
    push(min_x, min_y, -1.0 + circle_sentinel, -1.0 + circle_sentinel);
    push(max_x, min_y,  1.0 + circle_sentinel, -1.0 + circle_sentinel);
    push(max_x, max_y,  1.0 + circle_sentinel,  1.0 + circle_sentinel);

    // Tri 2
    push(min_x, min_y, -1.0 + circle_sentinel, -1.0 + circle_sentinel);
    push(max_x, max_y,  1.0 + circle_sentinel,  1.0 + circle_sentinel);
    push(min_x, max_y, -1.0 + circle_sentinel,  1.0 + circle_sentinel);
}

pub fn draw_circle_with_border(
    gpu: &mut Gpu,
    cx: f32, cy: f32, radius: f32,
    thickness: f32,
    fill_color: Color,
    border_color: Color,
) {
    // Draw the border circle (slightly larger)
    draw_circle(gpu, cx, cy, radius + thickness, border_color);
    // Draw the inner circle (the core)
    draw_circle(gpu, cx, cy, radius, fill_color);
}

pub fn upload_star_texture(gpu: &mut Gpu, size: u32) -> GpuStar {
    let pixels = generate_star8_texture(size);
    let s = size as u16;

    if gpu.atlas_cur_x + s > ATLAS_SIZE as u16 {
        gpu.atlas_cur_x  = 1;
        gpu.atlas_cur_y += gpu.atlas_row_h + 1;
        gpu.atlas_row_h  = 0;
    }
    let ax = gpu.atlas_cur_x;
    let ay = gpu.atlas_cur_y;
    gpu.atlas_cur_x += s + 1;
    gpu.atlas_row_h  = gpu.atlas_row_h.max(s);

    gpu._star_pixels.push(pixels.into_boxed_slice());  // @KindaHack

    let pixels = gpu._star_pixels.last().unwrap();
    gpu.pending_atlas_uploads.push(PendingAtlasUpload {
        x: ax as u32,
        y: ay as u32,
        w: size,
        h: size,
        data_len: pixels.len(),
        data: pixels.as_ptr(),
    });

    GpuStar { atlas_x: ax, atlas_y: ay, size: s }
}

impl Gpu {
    pub fn new(
        window_w: u32, window_h: u32,
        display_handle: DisplayHandle, window_handle: WindowHandle
    ) -> Gpu {
        unsafe { Self::new_impl(window_w, window_h, display_handle, window_handle) }
    }

    unsafe fn new_impl(
        win_w: u32, win_h: u32,
        display_handle: DisplayHandle, window_handle: WindowHandle
    ) -> Gpu {
        //
        // Entry and instance
        //
        let entry = ash::Entry::load().expect("failed to load Vulkan");

        let app_info = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2);

        //
        // Surface extensions required by winit
        //
        let display_handle = display_handle.as_raw();
        let surface_extensions = ash_window::enumerate_required_extensions(display_handle)
            .unwrap();

        //
        // No validation layers
        //
        let instance = entry.create_instance(
            &vk::InstanceCreateInfo::default()
                .application_info(&app_info)
                .enabled_extension_names(&surface_extensions),
            None,
        ).unwrap();

        //
        // Surface
        //
        let surface_ext = ash::khr::surface::Instance::new(&entry, &instance);
        let surface = ash_window::create_surface(
            &entry, &instance,
            display_handle,
            window_handle.as_raw(),
            None,
        ).unwrap();

        //
        // Physical device
        //
        let (pdev, queue_family) = pick_physical_device(&instance, &surface_ext, surface);

        //
        // Logical device
        //
        let queue_priorities = [1.0f32];
        let queue_create = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&queue_priorities);

        let dev_extensions = [ash::khr::swapchain::NAME.as_ptr()];
        let device = instance.create_device(
            pdev,
            &vk::DeviceCreateInfo::default()
                .queue_create_infos(std::slice::from_ref(&queue_create))
                .enabled_extension_names(&dev_extensions),
            None,
        ).unwrap();

        let queue = device.get_device_queue(queue_family, 0);
        let swapchain_ext = ash::khr::swapchain::Device::new(&instance, &device);

        //
        // Allocator
        //
        let allocator = Allocator::new(&AllocatorCreateDesc {
            instance: instance.clone(),
            device:   device.clone(),
            physical_device: pdev,
            debug_settings: Default::default(),
            buffer_device_address: false,
            allocation_sizes: Default::default()
        }).unwrap();

        //
        // Swapchain + Render pass + Pipeline
        //
        let surface_format = choose_surface_format(&surface_ext, pdev, surface);
        let (swapchain, swapchain_images) = create_swapchain(
            &swapchain_ext, &surface_ext, pdev, surface, surface_format, win_w, win_h, vk::SwapchainKHR::null(),
        );
        let swapchain_views = create_image_views(&device, &swapchain_images, surface_format.format);
        let render_pass     = create_render_pass(&device, surface_format.format);
        let framebuffers    = create_framebuffers(&device, &swapchain_views, render_pass, win_w, win_h);

        let desc_set_layout = create_desc_set_layout(&device);

        //
        // Atlas texture
        //
        let mut allocator = allocator;
        let (atlas_image_raw, atlas_alloc) = create_image(
            &device, &mut allocator,
            ATLAS_SIZE, ATLAS_SIZE, vk::Format::R8_UNORM,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
        );
        let atlas_view = create_image_view(&device, atlas_image_raw, vk::Format::R8_UNORM);

        //
        // Transition atlas to SHADER_READ_ONLY_OPTIMAL
        //
        {
            let pool = create_cmd_pool(&device, queue_family);
            let cmd  = alloc_cmd_buf(&device, pool);
            begin_one_shot(&device, cmd);
            transition_image_layout(
                &device, cmd, atlas_image_raw,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            );
            submit_one_shot(&device, cmd, queue);
            device.destroy_command_pool(pool, None);
        }

        //
        // Sampler
        //
        let sampler = device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        ).unwrap();

        //
        // Descriptor pool
        //
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLED_IMAGE,
                descriptor_count: (FRAMES_IN_FLIGHT * 2) as u32,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLER,
                descriptor_count: (FRAMES_IN_FLIGHT * 2) as u32,
            },
        ];
        let descriptor_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(FRAMES_IN_FLIGHT as u32)
                .pool_sizes(&pool_sizes),
            None,
        ).unwrap();

        let desc_layouts = vec![desc_set_layout; FRAMES_IN_FLIGHT];
        let desc_sets_vec = device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(descriptor_pool)
                .set_layouts(&desc_layouts),
        ).unwrap();
        let mut descriptor_sets = [vk::DescriptorSet::null(); FRAMES_IN_FLIGHT];
        for i in 0..FRAMES_IN_FLIGHT {
            descriptor_sets[i] = desc_sets_vec[i];
            // Write atlas + sampler into each set
            let img_info = [vk::DescriptorImageInfo::default()
                            .image_view(atlas_view)
                            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let smp_info = [vk::DescriptorImageInfo::default()
                            .sampler(sampler)];
            device.update_descriptor_sets(&[
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_sets[i])
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&img_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_sets[i])
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&smp_info),
            ], &[]);
        }

        //
        // Write descriptor
        //
        let img_info = [vk::DescriptorImageInfo::default()
                        .image_view(atlas_view)
                        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let smp_info = [vk::DescriptorImageInfo::default()
                        .sampler(sampler)];

        let descriptor_set = descriptor_sets[0];

        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(&img_info),
            vk::WriteDescriptorSet::default()
                .dst_set(descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(&smp_info),
        ];
        device.update_descriptor_sets(&writes, &[]);

        //
        // Composite render pass + pipeline
        //
        let composite_render_pass = create_composite_render_pass(&device, surface_format.format);
        let composite_framebuffers = create_framebuffers(
            &device, &swapchain_views, composite_render_pass, win_w, win_h
        );

        //
        // Add push constants to pipeline layout for screen_size
        //
        let push_range = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(8);
        let pipeline_layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(std::slice::from_ref(&desc_set_layout))
                .push_constant_ranges(std::slice::from_ref(&push_range)),
            None,
        ).unwrap();
        let pipeline = create_pipeline(&device, pipeline_layout, render_pass);
        let composite_pipeline = create_pipeline(&device, pipeline_layout, composite_render_pass);

        //
        // Blur compute setup
        //
        let blur_sampler = device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        ).unwrap();

        let blur_pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: (FRAMES_IN_FLIGHT * 2) as u32,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: (FRAMES_IN_FLIGHT * 2) as u32,
            },
        ];
        let blur_descriptor_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets((FRAMES_IN_FLIGHT * 2) as u32)
                .pool_sizes(&blur_pool_sizes),
            None,
        ).unwrap();

        let blur_desc_set_layout  = create_blur_desc_set_layout(&device);
        let blur_desc_set_layouts = vec![blur_desc_set_layout; FRAMES_IN_FLIGHT * 2];
        let all_blur_sets = device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(blur_descriptor_pool)
                .set_layouts(&blur_desc_set_layouts),
        ).unwrap();

        let blur_pipeline_layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(std::slice::from_ref(&blur_desc_set_layout)),
            None,
        ).unwrap();
        let blur_h_pipeline = create_blur_pipeline(&device, blur_pipeline_layout, BLUR_H_SPV);
        let blur_v_pipeline = create_blur_pipeline(&device, blur_pipeline_layout, BLUR_V_SPV);

        //
        // Per-frame blur resources
        //
        let blur_fmt = vk::Format::R8G8B8A8_UNORM;
        let mut blur_resources: [Option<BlurFrameResources>; FRAMES_IN_FLIGHT] =
            std::array::from_fn(|_| None);

        let mut blur_desc_sets_h_stored = [vk::DescriptorSet::null(); FRAMES_IN_FLIGHT];
        let mut blur_desc_sets_v_stored = [vk::DescriptorSet::null(); FRAMES_IN_FLIGHT];

        {
            let pool = create_cmd_pool(&device, queue_family);
            let cmd  = alloc_cmd_buf(&device, pool);
            begin_one_shot(&device, cmd);

            for i in 0..FRAMES_IN_FLIGHT {
                let desc_set_h = all_blur_sets[i * 2];
                let desc_set_v = all_blur_sets[i * 2 + 1];

                blur_desc_sets_h_stored[i] = desc_set_h;
                blur_desc_sets_v_stored[i] = desc_set_v;

                let (cap_raw, cap_alloc) = create_captured_image(
                    &device, &mut allocator, win_w, win_h, surface_format.format,
                );
                let cap_view = create_image_view(&device, cap_raw, surface_format.format);
                transition_image_layout(&device, cmd, cap_raw,
                                        vk::ImageLayout::UNDEFINED, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);

                let (a_raw, a_alloc) = create_blur_image(
                    &device, &mut allocator, win_w, win_h, blur_fmt,
                );
                let a_view = create_image_view(&device, a_raw, blur_fmt);
                transition_image_layout(&device, cmd, a_raw,
                                        vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL);

                let (b_raw, b_alloc) = create_blur_image(
                    &device, &mut allocator, win_w, win_h, blur_fmt,
                );
                let b_view = create_image_view(&device, b_raw, blur_fmt);
                transition_image_layout(&device, cmd, b_raw,
                                        vk::ImageLayout::UNDEFINED, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);

                write_blur_desc_set(&device, desc_set_h, cap_view, blur_sampler, a_view);
                write_blur_desc_set(&device, desc_set_v, a_view,   blur_sampler, b_view);

                // Write blur_b into this frame's main descriptor set
                let blur_img_info = [vk::DescriptorImageInfo::default()
                                     .image_view(b_view)
                                     .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                let blur_smp_info = [vk::DescriptorImageInfo::default()
                                     .sampler(blur_sampler)];
                device.update_descriptor_sets(&[
                    vk::WriteDescriptorSet::default()
                        .dst_set(descriptor_sets[i])
                        .dst_binding(2)
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .image_info(&blur_img_info),
                    vk::WriteDescriptorSet::default()
                        .dst_set(descriptor_sets[i])
                        .dst_binding(3)
                        .descriptor_type(vk::DescriptorType::SAMPLER)
                        .image_info(&blur_smp_info),
                ], &[]);

                blur_resources[i] = Some(BlurFrameResources {
                    captured_img:  AllocImage { img: cap_raw, allocation: cap_alloc },
                    captured_view: cap_view,
                    blur_a_img:    AllocImage { img: a_raw,   allocation: a_alloc },
                    blur_a_view:   a_view,
                    blur_b_img:    AllocImage { img: b_raw,   allocation: b_alloc },
                    blur_b_view:   b_view,
                    desc_set_h,
                    desc_set_v,
                });
            }

            submit_one_shot(&device, cmd, queue);
            device.destroy_command_pool(pool, None);
        }

        //
        // Vertex buffer
        //
        let (vbuf, valloc) = create_buffer(
            &device, &mut allocator,
            INITIAL_VERTEX_BUFFER_CAPACITY,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            MemoryLocation::CpuToGpu,
        );
        let (blur_vbuf, blur_valloc) = create_buffer(
            &device, &mut allocator,
            INITIAL_BLUR_VERTEX_BUFFER_CAPACITY,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            MemoryLocation::CpuToGpu,
        );

        //
        // Staging buffer for atlas uploads
        //
        let staging_capacity = 512 * 1024;  // nocheckin @Tune
        let (sbuf, salloc) = create_buffer(
            &device, &mut allocator,
            staging_capacity,
            vk::BufferUsageFlags::TRANSFER_SRC,
            MemoryLocation::CpuToGpu,
        );

        //
        // Per-frame sync
        //
        let frames = (0..FRAMES_IN_FLIGHT).map(|_| {
            let pool = create_cmd_pool(&device, queue_family);
            let cmd  = alloc_cmd_buf(&device, pool);
            let image_available = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).unwrap();
            let render_done     = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).unwrap();
            let fence = device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None,
            ).unwrap();
            Frame {
                cmd_pool: pool, cmd_buf: cmd, image_available, render_done, fence,
                retired_buffers: Default::default()
            }
        }).collect();

        // :Configuration
        //
        // Font
        //
        let font_data   = include_bytes!("../assets/font.ttf");
        let font_ref    = swash::FontRef::from_index(&font_data[..], 0).unwrap();
        let font_offset = font_ref.offset;
        let font_key    = font_ref.key;

        let one_shot_cmd_pool = create_cmd_pool(&device, queue_family);
        let one_shot_cmd_buf  = alloc_cmd_buf(&device, one_shot_cmd_pool);

        let mut gpu = Gpu {
            overlay_mode: false,

            blur_desc_sets_h_stored,
            blur_desc_sets_v_stored,

            one_shot_cmd_pool,
            one_shot_cmd_buf,
            upload_fence: device.create_fence(&vk::FenceCreateInfo::default(), None).unwrap(),
            upload_in_flight: false,

            font_key, font_offset, font_data,
            scale_context: swash::scale::ScaleContext::new(),

            atlas_cur_x: 1, atlas_cur_y: 1, atlas_row_h: 0,
            glyphs: rustc_hash::FxHashMap::with_capacity_and_hasher(1024, Default::default()),
            regions_scratch: Vec::with_capacity(1024),
            pending_atlas_uploads: Vec::with_capacity(1024),
            glyph_pixels: rustc_hash::FxHashMap::with_capacity_and_hasher(1024, Default::default()),

            batch_pool:  vec![Batch::full_window(win_w as _, win_h as _)],
            batch_count: 1,
            blur_batch_pool:  vec![Batch::full_window(win_w as _, win_h as _)],
            blur_batch_count: 1,
            clip_stack:    Vec::with_capacity(32),
            glyph_scratch: Vec::with_capacity(256),

            win_w: win_w as f32,
            win_h: win_h as f32,

            allocator: Some(allocator),
            vertex_buffer: Some(AllocBuffer { buffer: vbuf, allocation: valloc }),
            current_vertex_buffer_capacity: INITIAL_VERTEX_BUFFER_CAPACITY,
            blur_vertex_buffer: Some(AllocBuffer { buffer: blur_vbuf, allocation: blur_valloc }),
            current_blur_vertex_buffer_capacity: INITIAL_BLUR_VERTEX_BUFFER_CAPACITY,
            staging_buffer: Some(AllocBuffer { buffer: sbuf, allocation: salloc }),
            staging_capacity,

            atlas_image: Some(AllocImage { img: atlas_image_raw, allocation: atlas_alloc }),
            atlas_view,
            pdev,
            sampler,
            blur_sampler,

            descriptor_pool,
            descriptor_sets,
            desc_set_layout,
            pipeline_layout,
            pipeline,
            composite_pipeline,
            render_pass,
            composite_render_pass,
            framebuffers,
            composite_framebuffers,

            swapchain_views,
            swapchain_images,
            swapchain,
            surface_format,
            swapchain_ext,

            blur_resources,
            _blur_descriptor_pool: blur_descriptor_pool,
            _blur_desc_set_layout: blur_desc_set_layout,
            blur_pipeline_layout,
            blur_h_pipeline,
            blur_v_pipeline,

            frames,
            frame_index: 0,
            draw_scratch:      Vec::new(),
            blur_draw_scratch: Vec::with_capacity(16),

            queue,
            queue_family,
            device,
            surface,
            surface_ext,
            _instance: instance,
            _entry: entry,

            star: Default::default(),
            _star_pixels: Default::default()
        };

        gpu.star = upload_star_texture(&mut gpu, 100);

        gpu
    }

    unsafe fn submit_frame_impl(&mut self) -> Result<(), vk::Result> {
        let fi = self.frame_index % FRAMES_IN_FLIGHT;
        let frame = &self.frames[fi];

        //
        // Wait for this frame slot to be free
        //
        self.device.wait_for_fences(&[frame.fence], true, u64::MAX).unwrap();

        // Flush retired buffers from this frame slot (fence just signaled)
        let frame = &mut self.frames[fi];
        for (buf, alloc) in frame.retired_buffers.drain(..) {
            self.allocator.as_mut().unwrap().free(alloc).unwrap();
            self.device.destroy_buffer(buf, None);
        }

        let blur_total: usize = self.blur_batch_pool[..self.blur_batch_count]
            .iter().map(|b| b.verts.len()).sum();
        let blur_bytes = (blur_total * std::mem::size_of::<Vert>()) as u64;

        //
        // Blur vertex buffer resize
        //
        if blur_bytes > self.current_blur_vertex_buffer_capacity {
            let new_cap = (blur_bytes * 2).max(INITIAL_BLUR_VERTEX_BUFFER_CAPACITY);
            let alloc   = self.allocator.as_mut().unwrap();
            let old     = self.blur_vertex_buffer.take().unwrap();

            self.frames[fi].retired_buffers.push((old.buffer, old.allocation));

            let (buf, allocation) = create_buffer(
                &self.device, alloc, new_cap,
                vk::BufferUsageFlags::VERTEX_BUFFER, MemoryLocation::CpuToGpu,
            );
            self.blur_vertex_buffer = Some(AllocBuffer { buffer: buf, allocation });
            self.current_blur_vertex_buffer_capacity = new_cap;
        }

        let image_available = self.frames[fi].image_available;
        let render_done     = self.frames[fi].render_done;
        let fence           = self.frames[fi].fence;
        let cmd             = self.frames[fi].cmd_buf;
        let cmd_pool        = self.frames[fi].cmd_pool;

        //
        // Acquire swapchain image
        //
        let (img_idx, suboptimal) = match self.swapchain_ext.acquire_next_image(
            self.swapchain, u64::MAX, image_available, vk::Fence::null(),
        ) {
            Ok(r) => r,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.recreate_swapchain();
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        self.device.reset_fences(&[fence]).unwrap();
        self.device.reset_command_pool(cmd_pool, vk::CommandPoolResetFlags::empty()).unwrap();

        //
        // Build vertex data
        //

        self.blur_draw_scratch.clear();

        if blur_bytes > 0 {
            let vb  = self.blur_vertex_buffer.as_mut().unwrap();
            let ptr = vb.allocation.mapped_ptr().unwrap().as_ptr() as *mut u8;
            let mut offset = 0usize;
            let mut vert_offset = 0u32;
            for batch in &self.blur_batch_pool[..self.blur_batch_count] {
                if batch.verts.is_empty() { continue; }
                let bytes = bytemuck::cast_slice::<Vert, u8>(&batch.verts);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(offset), bytes.len());
                let end = vert_offset + batch.verts.len() as u32;
                self.blur_draw_scratch.push(DrawCmd { range: vert_offset..end, clip: batch.clip });
                offset      += bytes.len();
                vert_offset  = end;
            }
        }

        // Reset blur batches
        for batch in &mut self.blur_batch_pool[..self.blur_batch_count] { batch.verts.clear(); }
        self.blur_batch_count = 1;
        self.blur_batch_pool[0].clip = [0.0, 0.0, self.win_w, self.win_h];

        let total_verts: usize = self.batch_pool[..self.batch_count].iter().map(|b| b.verts.len()).sum();
        let byte_size = (total_verts * std::mem::size_of::<Vert>()) as u64;

        self.draw_scratch.clear();

        if byte_size > 0 {
            //
            // Grow vertex buffer if needed
            //
            if byte_size > self.current_vertex_buffer_capacity {
                let new_cap = (byte_size * 2).max(INITIAL_VERTEX_BUFFER_CAPACITY);
                let alloc   = self.allocator.as_mut().unwrap();
                let old     = self.vertex_buffer.take().unwrap();
                alloc.free(old.allocation).unwrap();
                self.device.destroy_buffer(old.buffer, None);
                let (buf, allocation) = create_buffer(
                    &self.device, alloc, new_cap,
                    vk::BufferUsageFlags::VERTEX_BUFFER, MemoryLocation::CpuToGpu,
                );
                self.vertex_buffer = Some(AllocBuffer { buffer: buf, allocation });
                self.current_vertex_buffer_capacity = new_cap;
            }

            //
            // Write verts into mapped buffer
            //
            let vb = self.vertex_buffer.as_mut().unwrap();
            let ptr = vb.allocation.mapped_ptr().unwrap().as_ptr() as *mut u8;
            let mut offset = 0usize;
            let mut vert_offset = 0u32;

            for batch in &self.batch_pool[..self.batch_count] {
                if batch.verts.is_empty() { continue; }

                let bytes = bytemuck::cast_slice::<Vert, u8>(&batch.verts);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(offset), bytes.len());
                let end = vert_offset + batch.verts.len() as u32;
                self.draw_scratch.push(DrawCmd { range: vert_offset..end, clip: batch.clip });

                offset      += bytes.len();
                vert_offset  = end;
            }
        }

        //
        // Reset batches
        //
        for batch in &mut self.batch_pool[..self.batch_count] {
            batch.verts.clear();
        }
        self.batch_count   = 1;
        self.batch_pool[0].clip = [0.0, 0.0, self.win_w, self.win_h];

        //
        // Record command buffer
        //
        self.device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default()).unwrap();

        let clear = vk::ClearValue {
            color: vk::ClearColorValue { float32: palette().background.into_gpu().0 },
        };
        let rp_begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(self.framebuffers[img_idx as usize])
            .render_area(vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D { width: self.win_w as u32, height: self.win_h as u32 },
            })
            .clear_values(std::slice::from_ref(&clear));

        self.device.cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE);
        self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
        self.device.cmd_bind_descriptor_sets(
            cmd, vk::PipelineBindPoint::GRAPHICS,
            self.pipeline_layout, 0,
            &[self.descriptor_sets[fi]], &[],
        );
        self.device.cmd_set_viewport(cmd, 0, &[vk::Viewport {
            x: 0.0, y: 0.0,
            width:  self.win_w,
            height: self.win_h,
            min_depth: 0.0,
            max_depth: 1.0,
        }]);

        if byte_size > 0 {
            self.device.cmd_bind_vertex_buffers(cmd, 0, &[self.vertex_buffer.as_ref().unwrap().buffer], &[0]);

            for DrawCmd { range, clip } in &self.draw_scratch {
                let cx = clip[0].max(0.0) as u32;
                let cy = clip[1].max(0.0) as u32;
                let right  = (clip[0] + clip[2]).min(self.win_w) as u32;
                let bottom = (clip[1] + clip[3]).min(self.win_h) as u32;
                let cw = right.saturating_sub(cx);
                let ch = bottom.saturating_sub(cy);
                if cw == 0 || ch == 0 { continue; }

                self.device.cmd_set_scissor(cmd, 0, &[vk::Rect2D {
                    offset: vk::Offset2D { x: cx as i32, y: cy as i32 },
                    extent: vk::Extent2D { width: cw, height: ch },
                }]);
                self.device.cmd_draw(cmd, range.end - range.start, 1, range.start, 0);
            }
        }

        self.device.cmd_end_render_pass(cmd);

        if blur_bytes > 0 {
            let res = self.blur_resources[fi].as_ref().unwrap();
            let cap_img = res.captured_img.img;
            let blur_a  = res.blur_a_img.img;
            let blur_b  = res.blur_b_img.img;
            let dsh     = res.desc_set_h;
            let dsv     = res.desc_set_v;

            // swapchain -> TRANSFER_SRC
            image_barrier(&self.device, cmd, self.swapchain_images[img_idx as usize],
                          vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                          vk::PipelineStageFlags::TRANSFER,
                          vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                          vk::AccessFlags::TRANSFER_READ,
                          vk::ImageLayout::PRESENT_SRC_KHR,
                          vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            );
            // captured -> TRANSFER_DST
            image_barrier(&self.device, cmd, cap_img,
                          vk::PipelineStageFlags::COMPUTE_SHADER,
                          vk::PipelineStageFlags::TRANSFER,
                          vk::AccessFlags::SHADER_READ,
                          vk::AccessFlags::TRANSFER_WRITE,
                          vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                          vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );

            let subresource = vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0, base_array_layer: 0, layer_count: 1,
            };
            self.device.cmd_copy_image(
                cmd,
                self.swapchain_images[img_idx as usize],
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                cap_img,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::ImageCopy::default()
                  .src_subresource(subresource)
                  .dst_subresource(subresource)
                  .extent(vk::Extent3D {
                      width: self.win_w as u32, height: self.win_h as u32, depth: 1,
                  })],
            );

            // swapchain -> PRESENT_SRC (restore)
            image_barrier(&self.device, cmd, self.swapchain_images[img_idx as usize],
                          vk::PipelineStageFlags::TRANSFER,
                          vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                          vk::AccessFlags::TRANSFER_READ,
                          vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                          vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                          vk::ImageLayout::PRESENT_SRC_KHR,
            );
            // captured -> SHADER_READ
            image_barrier(&self.device, cmd, cap_img,
                          vk::PipelineStageFlags::TRANSFER,
                          vk::PipelineStageFlags::COMPUTE_SHADER,
                          vk::AccessFlags::TRANSFER_WRITE,
                          vk::AccessFlags::SHADER_READ,
                          vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                          vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            );
            // blur_a -> GENERAL (ready for write)
            image_barrier(&self.device, cmd, blur_a,
                          vk::PipelineStageFlags::COMPUTE_SHADER,
                          vk::PipelineStageFlags::COMPUTE_SHADER,
                          vk::AccessFlags::SHADER_READ,
                          vk::AccessFlags::SHADER_WRITE,
                          vk::ImageLayout::GENERAL,
                          vk::ImageLayout::GENERAL,
            );

            // Blur H
            self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.blur_h_pipeline);
            self.device.cmd_bind_descriptor_sets(
                cmd, vk::PipelineBindPoint::COMPUTE,
                self.blur_pipeline_layout, 0, &[dsh], &[],
            );
            let gx = (self.win_w as u32 + 15) / 16;
            let gy = (self.win_h as u32 + 15) / 16;
            self.device.cmd_dispatch(cmd, gx, gy, 1);

            // blur_a -> SHADER_READ
            image_barrier(&self.device, cmd, blur_a,
                          vk::PipelineStageFlags::COMPUTE_SHADER,
                          vk::PipelineStageFlags::COMPUTE_SHADER,
                          vk::AccessFlags::SHADER_WRITE,
                          vk::AccessFlags::SHADER_READ,
                          vk::ImageLayout::GENERAL,
                          vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            );
            // blur_b -> GENERAL for write
            image_barrier(&self.device, cmd, blur_b,
                          vk::PipelineStageFlags::FRAGMENT_SHADER,
                          vk::PipelineStageFlags::COMPUTE_SHADER,
                          vk::AccessFlags::SHADER_READ,
                          vk::AccessFlags::SHADER_WRITE,
                          vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                          vk::ImageLayout::GENERAL,
            );

            // Blur V
            self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.blur_v_pipeline);
            self.device.cmd_bind_descriptor_sets(
                cmd, vk::PipelineBindPoint::COMPUTE,
                self.blur_pipeline_layout, 0, &[dsv], &[],
            );
            self.device.cmd_dispatch(cmd, gx, gy, 1);

            // blur_b -> SHADER_READ for frag
            image_barrier(&self.device, cmd, blur_b,
                          vk::PipelineStageFlags::COMPUTE_SHADER,
                          vk::PipelineStageFlags::FRAGMENT_SHADER,
                          vk::AccessFlags::SHADER_WRITE,
                          vk::AccessFlags::SHADER_READ,
                          vk::ImageLayout::GENERAL,
                          vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            );
            // restore blur_a -> GENERAL for next frame
            image_barrier(&self.device, cmd, blur_a,
                          vk::PipelineStageFlags::COMPUTE_SHADER,
                          vk::PipelineStageFlags::COMPUTE_SHADER,
                          vk::AccessFlags::SHADER_READ,
                          vk::AccessFlags::SHADER_WRITE,
                          vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                          vk::ImageLayout::GENERAL,
            );

            // Composite pass
            let rp_begin = vk::RenderPassBeginInfo::default()
                .render_pass(self.composite_render_pass)
                .framebuffer(self.composite_framebuffers[img_idx as usize])
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D { width: self.win_w as u32, height: self.win_h as u32 },
                })
                .clear_values(&[]);

            self.device.cmd_begin_render_pass(cmd, &rp_begin, vk::SubpassContents::INLINE);
            self.device.cmd_bind_pipeline(
                cmd, vk::PipelineBindPoint::GRAPHICS, self.composite_pipeline);
            self.device.cmd_bind_descriptor_sets(
                cmd, vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_layout, 0, &[self.descriptor_sets[fi]], &[],
            );
            self.device.cmd_set_viewport(cmd, 0, &[vk::Viewport {
                x: 0.0, y: 0.0, width: self.win_w, height: self.win_h,
                min_depth: 0.0, max_depth: 1.0,
            }]);
            self.device.cmd_push_constants(
                cmd, self.pipeline_layout, vk::ShaderStageFlags::FRAGMENT,
                0, bytemuck::cast_slice(&[self.win_w, self.win_h]),
            );

            self.device.cmd_bind_vertex_buffers(
                cmd, 0, &[self.blur_vertex_buffer.as_ref().unwrap().buffer], &[0]);
            for DrawCmd { range, clip } in &self.blur_draw_scratch {
                let cx     = clip[0].max(0.0) as u32;
                let cy     = clip[1].max(0.0) as u32;
                let right  = (clip[0] + clip[2]).min(self.win_w) as u32;
                let bottom = (clip[1] + clip[3]).min(self.win_h) as u32;
                let cw = right.saturating_sub(cx);
                let ch = bottom.saturating_sub(cy);
                if cw == 0 || ch == 0 { continue; }
                self.device.cmd_set_scissor(cmd, 0, &[vk::Rect2D {
                    offset: vk::Offset2D { x: cx as i32, y: cy as i32 },
                    extent: vk::Extent2D { width: cw, height: ch },
                }]);
                self.device.cmd_draw(cmd, range.end - range.start, 1, range.start, 0);
            }
            self.device.cmd_end_render_pass(cmd);
        }

        self.device.end_command_buffer(cmd).unwrap();

        //
        // Submit
        //
        let wait_sems   = [image_available];
        let signal_sems = [render_done];
        let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let cmds        = [cmd];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait_sems)
            .wait_dst_stage_mask(&wait_stages)
            .command_buffers(&cmds)
            .signal_semaphores(&signal_sems);
        self.device.queue_submit(self.queue, &[submit], fence).unwrap();

        //
        // Present
        //
        let swapchains  = [self.swapchain];
        let img_indices = [img_idx];
        let present = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal_sems)
            .swapchains(&swapchains)
            .image_indices(&img_indices);

        match self.swapchain_ext.queue_present(self.queue, &present) {
            Ok(_) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {}
            Err(e) => return Err(e),
        }
        if suboptimal { self.recreate_swapchain(); }

        self.frame_index += 1;
        Ok(())
    }

    unsafe fn recreate_swapchain(&mut self) {
        self.device.device_wait_idle().unwrap();

        for fb in self.framebuffers.drain(..) { self.device.destroy_framebuffer(fb, None); }
        for iv in self.swapchain_views.drain(..) { self.device.destroy_image_view(iv, None); }
        let old = self.swapchain;

        let (sc, images) = create_swapchain(
            &self.swapchain_ext, &self.surface_ext,
            self.pdev,
            self.surface, self.surface_format,
            self.win_w as u32, self.win_h as u32, old,
        );
        self.swapchain_ext.destroy_swapchain(old, None);
        self.swapchain       = sc;
        self.swapchain_views = create_image_views(&self.device, &images, self.surface_format.format);
        self.framebuffers    = create_framebuffers(
            &self.device, &self.swapchain_views, self.render_pass,
            self.win_w as u32, self.win_h as u32,
        );

        self.swapchain_images = images;
        self.composite_framebuffers = create_framebuffers(
            &self.device, &self.swapchain_views, self.composite_render_pass,
            self.win_w as u32, self.win_h as u32,
        );

        //
        //
        // Destroy per-frame blur resources
        //
        //

        for i in 0..FRAMES_IN_FLIGHT {
            if let Some(res) = self.blur_resources[i].take() {
                self.device.destroy_image_view(res.captured_view, None);
                self.device.destroy_image_view(res.blur_a_view, None);
                self.device.destroy_image_view(res.blur_b_view, None);
                let alloc = self.allocator.as_mut().unwrap();
                alloc.free(res.captured_img.allocation).unwrap();
                self.device.destroy_image(res.captured_img.img, None);
                alloc.free(res.blur_a_img.allocation).unwrap();
                self.device.destroy_image(res.blur_a_img.img, None);
                alloc.free(res.blur_b_img.allocation).unwrap();
                self.device.destroy_image(res.blur_b_img.img, None);
            }
        }

        //
        //
        // Recreate per-frame blur resources
        //
        //

        let blur_fmt = vk::Format::R8G8B8A8_UNORM;
        {
            let pool = create_cmd_pool(&self.device, self.queue_family);
            let cmd  = alloc_cmd_buf(&self.device, pool);
            begin_one_shot(&self.device, cmd);

            for i in 0..FRAMES_IN_FLIGHT {
                let (cap_raw, cap_alloc) = create_captured_image(
                    &self.device, self.allocator.as_mut().unwrap(),
                    self.win_w as u32, self.win_h as u32, self.surface_format.format,
                );
                let cap_view = create_image_view(&self.device, cap_raw, self.surface_format.format);
                transition_image_layout(&self.device, cmd, cap_raw,
                                        vk::ImageLayout::UNDEFINED, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);

                let (a_raw, a_alloc) = create_blur_image(
                    &self.device, self.allocator.as_mut().unwrap(),
                    self.win_w as u32, self.win_h as u32, blur_fmt,
                );
                let a_view = create_image_view(&self.device, a_raw, blur_fmt);
                transition_image_layout(&self.device, cmd, a_raw,
                                        vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL);

                let (b_raw, b_alloc) = create_blur_image(
                    &self.device, self.allocator.as_mut().unwrap(),
                    self.win_w as u32, self.win_h as u32, blur_fmt,
                );
                let b_view = create_image_view(&self.device, b_raw, blur_fmt);
                transition_image_layout(&self.device, cmd, b_raw,
                                        vk::ImageLayout::UNDEFINED, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);

                write_blur_desc_set(&self.device,
                                    self.blur_desc_sets_h_stored[i], cap_view, self.blur_sampler, a_view);
                write_blur_desc_set(&self.device,
                                    self.blur_desc_sets_v_stored[i], a_view,   self.blur_sampler, b_view);

                // Update main descriptor set
                let blur_img_info = [vk::DescriptorImageInfo::default()
                                     .image_view(b_view)
                                     .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                let blur_smp_info = [vk::DescriptorImageInfo::default()
                                     .sampler(self.blur_sampler)];
                self.device.update_descriptor_sets(&[
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.descriptor_sets[i])
                        .dst_binding(2)
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .image_info(&blur_img_info),
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.descriptor_sets[i])
                        .dst_binding(3)
                        .descriptor_type(vk::DescriptorType::SAMPLER)
                        .image_info(&blur_smp_info),
                ], &[]);

                self.blur_resources[i] = Some(BlurFrameResources {
                    captured_img:  AllocImage { img: cap_raw, allocation: cap_alloc },
                    captured_view: cap_view,
                    blur_a_img:    AllocImage { img: a_raw,   allocation: a_alloc },
                    blur_a_view:   a_view,
                    blur_b_img:    AllocImage { img: b_raw,   allocation: b_alloc },
                    blur_b_view:   b_view,
                    desc_set_h:    self.blur_desc_sets_h_stored[i],
                    desc_set_v:    self.blur_desc_sets_v_stored[i],
                });
            }

            submit_one_shot(&self.device, cmd, self.queue);
            self.device.destroy_command_pool(pool, None);
        }

        self.blur_batch_pool[0].clip = [0.0, 0.0, self.win_w, self.win_h];
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            self.device.device_wait_idle().unwrap();

            for frame in &self.frames {
                self.device.destroy_command_pool(frame.cmd_pool, None);
                self.device.destroy_semaphore(frame.image_available, None);
                self.device.destroy_semaphore(frame.render_done, None);
                self.device.destroy_fence(frame.fence, None);
            }

            for fb in &self.framebuffers { self.device.destroy_framebuffer(*fb, None); }
            for iv in &self.swapchain_views { self.device.destroy_image_view(*iv, None); }
            self.swapchain_ext.destroy_swapchain(self.swapchain, None);

            self.device.destroy_image_view(self.atlas_view, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_descriptor_pool(self.descriptor_pool, None);
            self.device.destroy_descriptor_set_layout(self.desc_set_layout, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.pipeline_layout, None);
            self.device.destroy_render_pass(self.render_pass, None);

            let alloc = self.allocator.as_mut().unwrap();
            if let Some(b) = self.vertex_buffer.take()  { alloc.free(b.allocation).unwrap(); self.device.destroy_buffer(b.buffer, None); }
            if let Some(b) = self.staging_buffer.take() { alloc.free(b.allocation).unwrap(); self.device.destroy_buffer(b.buffer, None); }
            if let Some(i) = self.atlas_image.take()   { alloc.free(i.allocation).unwrap(); self.device.destroy_image(i.img, None); }
            drop(self.allocator.take());

            self.surface_ext.destroy_surface(self.surface, None);
        }
    }
}

unsafe fn pick_physical_device(
    instance: &ash::Instance,
    surface_ext: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> (vk::PhysicalDevice, u32) {
    let pdevs = instance.enumerate_physical_devices().unwrap();
    for pdev in pdevs {
        let _props = instance.get_physical_device_properties(pdev);
        let queue_families = instance.get_physical_device_queue_family_properties(pdev);
        for (i, qf) in queue_families.iter().enumerate() {
            let graphics = qf.queue_flags.contains(vk::QueueFlags::GRAPHICS);
            let present  = surface_ext
                .get_physical_device_surface_support(pdev, i as u32, surface)
                .unwrap_or(false);

            if graphics && present {
                return (pdev, i as _);
            }
        }
    }

    panic!("no suitable Vulkan device found");
}

unsafe fn choose_surface_format(
    surface_ext: &ash::khr::surface::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
) -> vk::SurfaceFormatKHR {
    let formats = surface_ext.get_physical_device_surface_formats(pdev, surface).unwrap();
    formats.iter()
        .find(|f| f.format == vk::Format::B8G8R8A8_UNORM && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
        .copied()
        .unwrap_or(formats[0])
}

unsafe fn create_swapchain(
    sc_ext: &ash::khr::swapchain::Device,
    surface_ext: &ash::khr::surface::Instance,
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    format: vk::SurfaceFormatKHR,
    w: u32, h: u32,
    old: vk::SwapchainKHR,
) -> (vk::SwapchainKHR, Vec<vk::Image>) {
    let caps = surface_ext.get_physical_device_surface_capabilities(pdev, surface).unwrap();

    let available_present_modes = surface_ext
        .get_physical_device_surface_present_modes(pdev, surface)
        .unwrap();

    let present_mode = if available_present_modes.contains(&vk::PresentModeKHR::MAILBOX) {
        vk::PresentModeKHR::MAILBOX
    } else if available_present_modes.contains(&vk::PresentModeKHR::IMMEDIATE) {
        vk::PresentModeKHR::IMMEDIATE
    } else {
        vk::PresentModeKHR::FIFO
    };

    let target_image_count = if present_mode == vk::PresentModeKHR::MAILBOX {
        caps.min_image_count.max(3)
    } else {
        caps.min_image_count.max(2)
    };

    let image_count = if caps.max_image_count > 0 {
        target_image_count.min(caps.max_image_count)
    } else {
        target_image_count
    };

    let extent = vk::Extent2D {
        width:  w.clamp(caps.min_image_extent.width,  caps.max_image_extent.width),
        height: h.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
    };

    let sc = sc_ext.create_swapchain(
        &vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(image_count)
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(present_mode)
            .clipped(true)
            .old_swapchain(old),

        None,
    ).unwrap();

    let images = sc_ext.get_swapchain_images(sc).unwrap();
    (sc, images)
}

unsafe fn create_image_views(
    device: &ash::Device,
    images: &[vk::Image],
    format: vk::Format,
) -> Vec<vk::ImageView> {
    images.iter().map(|&img| create_image_view(device, img, format)).collect()
}

unsafe fn create_image_view(device: &ash::Device, img: vk::Image, format: vk::Format) -> vk::ImageView {
    device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(img)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0, level_count: 1,
                base_array_layer: 0, layer_count: 1,
            }),

        None,
    ).unwrap()
}

unsafe fn create_render_pass(device: &ash::Device, format: vk::Format) -> vk::RenderPass {
    let attachments = [vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];

    let color_ref = [vk::AttachmentReference {
        attachment: 0,
        layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
    }];

    let subpass = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_ref)];

    let dependency = [vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags::empty())
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];

    device.create_render_pass(
        &vk::RenderPassCreateInfo::default()
            .attachments(&attachments)
            .subpasses(&subpass)
            .dependencies(&dependency),

        None,
    ).unwrap()
}

unsafe fn create_framebuffers(
    device: &ash::Device,
    views: &[vk::ImageView],
    render_pass: vk::RenderPass,
    w: u32, h: u32,
) -> Vec<vk::Framebuffer> {
    views.iter().map(|view| {
        let attachments = [*view];
        device.create_framebuffer(
            &vk::FramebufferCreateInfo::default()
                .render_pass(render_pass)
                .attachments(&attachments)
                .width(w).height(h).layers(1),

            None,
        ).unwrap()
    }).collect()
}

unsafe fn create_desc_set_layout(device: &ash::Device) -> vk::DescriptorSetLayout {
    let bindings = [
        vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        vk::DescriptorSetLayoutBinding::default()
            .binding(1)
            .descriptor_type(vk::DescriptorType::SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        vk::DescriptorSetLayoutBinding::default()
            .binding(2)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        vk::DescriptorSetLayoutBinding::default()
            .binding(3)
            .descriptor_type(vk::DescriptorType::SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT),
    ];
    device.create_descriptor_set_layout(
        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
        None,
    ).unwrap()
}

unsafe fn create_pipeline(
    device: &ash::Device,
    layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
) -> vk::Pipeline {
    let vert_code = ash::util::read_spv(&mut std::io::Cursor::new(VERT_SPV)).unwrap();
    let frag_code = ash::util::read_spv(&mut std::io::Cursor::new(FRAG_SPV)).unwrap();

    let vert_module = device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&vert_code), None,
    ).unwrap();

    let frag_module = device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&frag_code), None,
    ).unwrap();

    let entry = CStr::from_bytes_with_nul(b"main\0").unwrap();
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_module)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_module)
            .name(entry),
    ];

    let vert_stride = std::mem::size_of::<Vert>() as u32;
    let bindings = [vk::VertexInputBindingDescription {
        binding: 0, stride: vert_stride,
        input_rate: vk::VertexInputRate::VERTEX,
    }];
    let attributes = [
        vk::VertexInputAttributeDescription { location: 0, binding: 0, format: vk::Format::R32G32_SFLOAT,       offset: 0  }, // pos
        vk::VertexInputAttributeDescription { location: 1, binding: 0, format: vk::Format::R32G32_SFLOAT,       offset: 8  }, // uv
        vk::VertexInputAttributeDescription { location: 2, binding: 0, format: vk::Format::R32G32B32_SFLOAT,    offset: 16 }, // uv2
        vk::VertexInputAttributeDescription { location: 3, binding: 0, format: vk::Format::R32G32B32A32_SFLOAT, offset: 28 }, // color
    ];

    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attributes);

    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);

    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default()
        .dynamic_states(&dynamic_states);

    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);

    let rasterizer = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::CLOCKWISE)
        .line_width(1.0);

    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);

    // Premultiplied alpha blend
    let blend_attachment = [vk::PipelineColorBlendAttachmentState {
        blend_enable: vk::TRUE,
        src_color_blend_factor: vk::BlendFactor::ONE,
        // src_color_blend_factor: vk::BlendFactor::SRC_ALPHA,
        dst_color_blend_factor: vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
        color_blend_op:         vk::BlendOp::ADD,
        src_alpha_blend_factor: vk::BlendFactor::ONE,
        dst_alpha_blend_factor: vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
        alpha_blend_op:         vk::BlendOp::ADD,
        color_write_mask:       vk::ColorComponentFlags::RGBA,
    }];
    let blend = vk::PipelineColorBlendStateCreateInfo::default()
        .attachments(&blend_attachment);

    let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterizer)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic)
        .layout(layout)
        .render_pass(render_pass);

    let pipeline = device.create_graphics_pipelines(
        vk::PipelineCache::null(), &[pipeline_info], None,
    ).unwrap()[0];

    device.destroy_shader_module(vert_module, None);
    device.destroy_shader_module(frag_module, None);

    pipeline
}

unsafe fn create_buffer(
    device: &ash::Device,
    allocator: &mut Allocator,
    size: u64,
    usage: vk::BufferUsageFlags,
    location: MemoryLocation,
) -> (vk::Buffer, Allocation) {
    let buf = device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE),

        None,
    ).unwrap();

    let reqs = device.get_buffer_memory_requirements(buf);
    let allocation = allocator.allocate(&AllocationCreateDesc {
        name: "buffer",
        requirements: reqs,
        location,
        linear: true,
        allocation_scheme: AllocationScheme::GpuAllocatorManaged,
    }).unwrap();

    device.bind_buffer_memory(buf, allocation.memory(), allocation.offset()).unwrap();

    (buf, allocation)
}

unsafe fn create_image(
    device: &ash::Device,
    allocator: &mut Allocator,
    w: u32, h: u32,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
) -> (vk::Image, Allocation) {
    let img = device.create_image(
        &vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width: w, height: h, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED),

        None,
    ).unwrap();

    let reqs = device.get_image_memory_requirements(img);
    let allocation = allocator.allocate(&AllocationCreateDesc {
        name: "image",
        requirements: reqs,
        location: MemoryLocation::GpuOnly,
        linear: false,
        allocation_scheme: AllocationScheme::GpuAllocatorManaged,
    }).unwrap();

    device.bind_image_memory(img, allocation.memory(), allocation.offset()).unwrap();

    (img, allocation)
}

unsafe fn transition_image_layout(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    img: vk::Image,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
) {
    let (src_access, src_stage, dst_access, dst_stage) = match (from, to) {
        (vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL) => (
            vk::AccessFlags::empty(),
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::PipelineStageFlags::TRANSFER,
        ),
        (vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL) => (
            vk::AccessFlags::TRANSFER_WRITE,
            vk::PipelineStageFlags::TRANSFER,
            vk::AccessFlags::SHADER_READ,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
        ),
        (vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, vk::ImageLayout::TRANSFER_DST_OPTIMAL) => (
            vk::AccessFlags::SHADER_READ,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::PipelineStageFlags::TRANSFER,
        ),
        (vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL) => (
            vk::AccessFlags::empty(),
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::AccessFlags::SHADER_WRITE,
            vk::PipelineStageFlags::COMPUTE_SHADER,
        ),
        (vk::ImageLayout::UNDEFINED, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL) => (
            vk::AccessFlags::empty(),
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::AccessFlags::SHADER_READ,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
        ),

        _ => panic!("unsupported layout transition {from:?} -> {to:?}"),
    };

    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(from)
        .new_layout(to)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(img)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0, level_count: 1,
            base_array_layer: 0, layer_count: 1,
        })
        .src_access_mask(src_access)
        .dst_access_mask(dst_access);

    device.cmd_pipeline_barrier(
        cmd, src_stage, dst_stage,
        vk::DependencyFlags::empty(),
        &[], &[], &[barrier],
    );
}

unsafe fn create_cmd_pool(device: &ash::Device, queue_family: u32) -> vk::CommandPool {
    device.create_command_pool(
        &vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family)
            .flags(vk::CommandPoolCreateFlags::TRANSIENT),
        None,
    ).unwrap()
}

unsafe fn alloc_cmd_buf(device: &ash::Device, pool: vk::CommandPool) -> vk::CommandBuffer {
    device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1),
    ).unwrap()[0]
}

unsafe fn begin_one_shot(device: &ash::Device, cmd: vk::CommandBuffer) {
    device.begin_command_buffer(
        cmd,
        &vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
    ).unwrap();
}

unsafe fn submit_one_shot(device: &ash::Device, cmd: vk::CommandBuffer, queue: vk::Queue) {
    device.end_command_buffer(cmd).unwrap();
    device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&[cmd])], vk::Fence::null()).unwrap();
    device.queue_wait_idle(queue).unwrap();
}

unsafe fn create_captured_image(
    device: &ash::Device,
    allocator: &mut Allocator,
    w: u32, h: u32,
    format: vk::Format,
) -> (vk::Image, Allocation) {
    create_image(
        device, allocator, w, h, format,
        vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
    )
}

unsafe fn create_blur_image(
    device: &ash::Device,
    allocator: &mut Allocator,
    w: u32, h: u32,
    format: vk::Format,
) -> (vk::Image, Allocation) {
    create_image(
        device, allocator, w, h, format,
        vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
    )
}

unsafe fn create_blur_desc_set_layout(device: &ash::Device) -> vk::DescriptorSetLayout {
    let bindings = [
        vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::COMPUTE),
        vk::DescriptorSetLayoutBinding::default()
            .binding(1)
            .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::COMPUTE),
    ];
    device.create_descriptor_set_layout(
        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
        None,
    ).unwrap()
}

unsafe fn create_blur_pipeline(
    device: &ash::Device,
    layout: vk::PipelineLayout,
    spv: &[u8],
) -> vk::Pipeline {
    // SPIR-V words must be 4-byte aligned
    assert!(spv.len() % 4 == 0, "SPIR-V size not multiple of 4");
    let spv_words: Vec<u32> = spv.chunks(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();

    let module = device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default()
            .code(&spv_words),
        None,
    ).unwrap();
    let pipeline = device.create_compute_pipelines(
        vk::PipelineCache::null(),
        &[vk::ComputePipelineCreateInfo::default()
            .stage(vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main"))
            .layout(layout)],
        None,
    ).unwrap()[0];
    device.destroy_shader_module(module, None);
    pipeline
}

unsafe fn write_blur_desc_set(
    device: &ash::Device,
    set: vk::DescriptorSet,
    src_view: vk::ImageView,
    src_sampler: vk::Sampler,
    dst_view: vk::ImageView,
) {
    let src_info = [vk::DescriptorImageInfo::default()
        .image_view(src_view)
        .sampler(src_sampler)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let dst_info = [vk::DescriptorImageInfo::default()
        .image_view(dst_view)
        .image_layout(vk::ImageLayout::GENERAL)];
    device.update_descriptor_sets(&[
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&src_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(1)
            .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
            .image_info(&dst_info),
    ], &[]);
}

unsafe fn image_barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    src_stage: vk::PipelineStageFlags,
    dst_stage: vk::PipelineStageFlags,
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
) {
    let barrier = vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask:      vk::ImageAspectFlags::COLOR,
            base_mip_level:   0,
            level_count:      1,
            base_array_layer: 0,
            layer_count:      1,
        });
    device.cmd_pipeline_barrier(
        cmd, src_stage, dst_stage,
        vk::DependencyFlags::empty(),
        &[], &[], &[barrier],
    );
}

unsafe fn create_composite_render_pass(
    device: &ash::Device,
    format: vk::Format,
) -> vk::RenderPass {
    let attachment = vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)
        .initial_layout(vk::ImageLayout::PRESENT_SRC_KHR)
        .final_layout(vk::ImageLayout::PRESENT_SRC_KHR);

    let color_ref = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(std::slice::from_ref(&color_ref));

    let dependency = vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);

    device.create_render_pass(
        &vk::RenderPassCreateInfo::default()
            .attachments(std::slice::from_ref(&attachment))
            .subpasses(std::slice::from_ref(&subpass))
            .dependencies(std::slice::from_ref(&dependency)),
        None,
    ).unwrap()
}
