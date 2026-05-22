#![allow(unsafe_op_in_unsafe_fn)]

use crate::messager::MESSAGER_FONT_SIZE;
use crate::util::format_bytes;
use crate::{Editor, Glyph, PASTE_ANIMATION_BITS, PASTE_ANIMATION_MASK, PASTE_ANIMATION_MAX_ID, PASTE_ANIMATION_PER_WORD, SCALE_STEP, palette, scale_base_font_size, tracy};
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
pub const FRAMES_IN_FLIGHT: usize      = 2;

#[derive(Default, Debug, Clone, Copy)]
pub struct GpuGlyph {
    pub uv_x: u16, pub uv_y: u16,
    pub uv_w: u16, pub uv_h: u16,
    pub w: u16,    pub h: u16,
    pub bearing_x: i16, pub bearing_y: i16,
    pub advance: f32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vert {
    pub pos:   [f32; 2],
    pub uv:    [f32; 2],
    pub color: GpuColor,
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

struct Frame {
    cmd_pool:        vk::CommandPool,
    cmd_buf:         vk::CommandBuffer,
    image_available: vk::Semaphore,
    render_done:     vk::Semaphore,
    fence:           vk::Fence,
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

pub struct Gpu {
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
    pub clip_depth:  i32,

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
    descriptor_set:  vk::DescriptorSet,
    desc_set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline:        vk::Pipeline,
    render_pass:     vk::RenderPass,
    pub one_shot_cmd_pool:  vk::CommandPool,
    pub one_shot_cmd_buf:   vk::CommandBuffer,
    pub upload_fence:       vk::Fence,
    pub upload_in_flight:   bool,

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
}

impl Gpu {
    #[inline]
    pub fn verts_mut(&mut self) -> &mut Vec<Vert> {
        &mut self.batch_pool[self.batch_count - 1].verts
    }

    #[inline]
    pub fn current_clip(&self) -> [f32; 4] {
        self.batch_pool[self.batch_count - 1].clip
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
        w: w as u32,        h: h as u32,
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
pub fn push_clip(gpu: &mut Gpu, x: f32, y: f32, w: f32, h: f32) {
    gpu.clip_depth += 1;

    let i = gpu.batch_count;
    if i >= gpu.batch_pool.len() {
        gpu.batch_pool.push(Batch::new([x, y, w, h]));
    } else {
        gpu.batch_pool[i].clip = [x, y, w, h];
        gpu.batch_pool[i].verts.clear();
    }

    gpu.batch_count += 1;
}

#[inline]
pub fn pop_clip(gpu: &mut Gpu) {
    gpu.clip_depth -= 1;
    debug_assert!(gpu.clip_depth >= 0, "unbalanced pop_clip");

    let clip = if gpu.batch_count >= 2 {
        gpu.batch_pool[gpu.batch_count - 2].clip
    } else {
        [0.0, 0.0, gpu.win_w, gpu.win_h]
    };

    let i = gpu.batch_count;
    if i >= gpu.batch_pool.len() {
        gpu.batch_pool.push(Batch::new(clip));
    } else {
        gpu.batch_pool[i].clip = clip;
        gpu.batch_pool[i].verts.clear();
    }

    gpu.batch_count += 1;
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
        p.add(0).write(Vert { pos: [x0, y0], uv: [0.0, 0.0], color });
        p.add(1).write(Vert { pos: [x1, y0], uv: [0.0, 0.0], color });
        p.add(2).write(Vert { pos: [x0, y1], uv: [0.0, 0.0], color });
        p.add(3).write(Vert { pos: [x1, y0], uv: [0.0, 0.0], color });
        p.add(4).write(Vert { pos: [x1, y1], uv: [0.0, 0.0], color });
        p.add(5).write(Vert { pos: [x0, y1], uv: [0.0, 0.0], color });
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
        p.add(0).write(Vert { pos: [x0, y0], uv: [0.0, 0.0], color: cl }); // top-left
        p.add(1).write(Vert { pos: [x1, y0], uv: [0.0, 0.0], color: cr }); // top-right
        p.add(2).write(Vert { pos: [x0, y1], uv: [0.0, 0.0], color: cl }); // bottom-left
        p.add(3).write(Vert { pos: [x1, y0], uv: [0.0, 0.0], color: cr }); // top-right
        p.add(4).write(Vert { pos: [x1, y1], uv: [0.0, 0.0], color: cr }); // bottom-right
        p.add(5).write(Vert { pos: [x0, y1], uv: [0.0, 0.0], color: cl }); // bottom-left
        verts.set_len(base + 6);
    }
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
            v.add(0).write(Vert { pos: [x0, y0], uv: [u0, v0], color });
            v.add(1).write(Vert { pos: [x1, y0], uv: [u1, v0], color });
            v.add(2).write(Vert { pos: [x0, y1], uv: [u0, v1], color });
            v.add(3).write(Vert { pos: [x1, y0], uv: [u1, v0], color });
            v.add(4).write(Vert { pos: [x1, y1], uv: [u1, v1], color });
            v.add(5).write(Vert { pos: [x0, y1], uv: [u0, v1], color });
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
            v.add(0).write(Vert { pos: [x0, y0], uv: [u0, v0], color });
            v.add(1).write(Vert { pos: [x1, y0], uv: [u1, v0], color });
            v.add(2).write(Vert { pos: [x0, y1], uv: [u0, v1], color });
            v.add(3).write(Vert { pos: [x1, y0], uv: [u1, v0], color });
            v.add(4).write(Vert { pos: [x1, y1], uv: [u1, v1], color });
            v.add(5).write(Vert { pos: [x0, y1], uv: [u0, v1], color });
        }
        count += 6;
    }

    unsafe { verts.set_len(base + count); }
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
        let pipeline_layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(std::slice::from_ref(&desc_set_layout)),
            None,
        ).unwrap();
        let pipeline = create_pipeline(&device, pipeline_layout, render_pass);

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
            vk::DescriptorPoolSize { ty: vk::DescriptorType::SAMPLED_IMAGE,   descriptor_count: 1 },
            vk::DescriptorPoolSize { ty: vk::DescriptorType::SAMPLER,         descriptor_count: 1 },
        ];
        let descriptor_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(1)
                .pool_sizes(&pool_sizes),
            None,
        ).unwrap();
        let descriptor_set = device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(descriptor_pool)
                .set_layouts(std::slice::from_ref(&desc_set_layout)),
        ).unwrap()[0];

        //
        // Write descriptor
        //
        let img_info = [vk::DescriptorImageInfo::default()
                        .image_view(atlas_view)
                        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let smp_info = [vk::DescriptorImageInfo::default()
                        .sampler(sampler)];
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
        // Vertex buffer
        //
        let (vbuf, valloc) = create_buffer(
            &device, &mut allocator,
            INITIAL_VERTEX_BUFFER_CAPACITY,
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
            Frame { cmd_pool: pool, cmd_buf: cmd, image_available, render_done, fence }
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

        Gpu {
            one_shot_cmd_pool,
            one_shot_cmd_buf,
            upload_fence:       device.create_fence(
                &vk::FenceCreateInfo::default(),
                None,
            ).unwrap(),
            upload_in_flight:   false,

            font_key, font_offset, font_data,
            scale_context: swash::scale::ScaleContext::new(),

            atlas_cur_x: 1, atlas_cur_y: 1, atlas_row_h: 0,

            glyphs: rustc_hash::FxHashMap::with_capacity_and_hasher(1024, Default::default()),
            regions_scratch: Vec::with_capacity(1024),
            pending_atlas_uploads: Vec::with_capacity(1024),
            glyph_pixels: rustc_hash::FxHashMap::with_capacity_and_hasher(1024, Default::default()),

            batch_pool:  vec![Batch::full_window(win_w as _, win_h as _)],
            batch_count: 1,
            clip_depth:  0,
            glyph_scratch: Vec::with_capacity(256),

            win_w: win_w as f32,
            win_h: win_h as f32,

            allocator: Some(allocator),
            vertex_buffer: Some(AllocBuffer { buffer: vbuf, allocation: valloc }),
            current_vertex_buffer_capacity: INITIAL_VERTEX_BUFFER_CAPACITY,
            staging_buffer: Some(AllocBuffer { buffer: sbuf, allocation: salloc }),
            staging_capacity,
            atlas_image:  Some(AllocImage { img: atlas_image_raw, allocation: atlas_alloc }),
            atlas_view,
            pdev,
            sampler,
            descriptor_pool,
            descriptor_set,
            desc_set_layout,
            pipeline_layout,
            pipeline,
            render_pass,
            framebuffers,
            swapchain_views,
            swapchain,
            surface_format,
            swapchain_ext,
            frames,
            frame_index: 0,
            draw_scratch: Vec::new(),
            queue,
            device,
            surface,
            surface_ext,
            _instance: instance,
            _entry: entry,
        }
    }

    unsafe fn submit_frame_impl(&mut self) -> Result<(), vk::Result> {
        let fi = self.frame_index % FRAMES_IN_FLIGHT;
        let frame = &self.frames[fi];

        //
        // Wait for this frame slot to be free
        //
        self.device.wait_for_fences(&[frame.fence], true, u64::MAX).unwrap();

        //
        // Acquire swapchain image
        //
        let (img_idx, suboptimal) = match self.swapchain_ext.acquire_next_image(
            self.swapchain, u64::MAX, frame.image_available, vk::Fence::null(),
        ) {
            Ok(r) => r,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.recreate_swapchain();
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        self.device.reset_fences(&[frame.fence]).unwrap();
        self.device.reset_command_pool(frame.cmd_pool, vk::CommandPoolResetFlags::empty()).unwrap();

        //
        // Build vertex data
        //
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
        let cmd = frame.cmd_buf;
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
            &[self.descriptor_set], &[],
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
        self.device.end_command_buffer(cmd).unwrap();

        //
        // Submit
        //
        let wait_sems   = [frame.image_available];
        let signal_sems = [frame.render_done];
        let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let cmds        = [cmd];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait_sems)
            .wait_dst_stage_mask(&wait_stages)
            .command_buffers(&cmds)
            .signal_semaphores(&signal_sems);
        self.device.queue_submit(self.queue, &[submit], frame.fence).unwrap();

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

    let image_count = (caps.min_image_count + 1).min(
        if caps.max_image_count == 0 { u32::MAX } else { caps.max_image_count }
    );

    let extent = vk::Extent2D {
        width:  w.clamp(caps.min_image_extent.width,  caps.max_image_extent.width),
        height: h.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
    };

    let available_present_modes = surface_ext
        .get_physical_device_surface_present_modes(pdev, surface)
        .unwrap();

    let present_mode = available_present_modes
        .iter()
        .copied()
        .find(|&m| m == vk::PresentModeKHR::MAILBOX)
        .unwrap_or(vk::PresentModeKHR::FIFO);

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
        vk::VertexInputAttributeDescription { location: 0, binding: 0, format: vk::Format::R32G32_SFLOAT,          offset: 0  },
        vk::VertexInputAttributeDescription { location: 1, binding: 0, format: vk::Format::R32G32_SFLOAT,          offset: 8  },
        vk::VertexInputAttributeDescription { location: 2, binding: 0, format: vk::Format::R32G32B32A32_SFLOAT,    offset: 16 },
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
        (vk::ImageLayout::UNDEFINED, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL) => (
            vk::AccessFlags::empty(),
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::AccessFlags::SHADER_READ,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
        ),
        (vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, vk::ImageLayout::TRANSFER_DST_OPTIMAL) => (
            vk::AccessFlags::SHADER_READ,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::PipelineStageFlags::TRANSFER,
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
