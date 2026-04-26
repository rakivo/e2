use crate::color::{Color, GpuColor, lerp_color};
use crate::{Glyph, PASTE_ANIMATION_BITS, PASTE_ANIMATION_MASK, PASTE_ANIMATION_MAX_ID, PASTE_ANIMATION_PER_WORD, palette, tracy};

use std::ops::Range;
use std::sync::Arc;

use wgpu::naga::FastHashMap;
use winit::window::Window;

 // @Note: Must match ATLAS_SIZE in the shader
pub const ATLAS_SIZE: u32 = 1024; // 1MB
pub const ATLAS_RESET_RATIO: f32 = 0.8; // 80%

pub const INITIAL_VERTEX_BUFFER_CAPACITY: u64 = 8 * 1024 * 1024;

#[derive(Default, Debug, Clone, Copy)]
pub struct GpuGlyph {
    pub uv_x: u16,      pub uv_y: u16,  // Divided by atlas size in shader
    pub uv_w: u16,      pub uv_h: u16,
    pub w: u16,         pub h: u16,
    pub bearing_x: i16, pub bearing_y: i16,
    pub advance: f32,
}

#[repr(C)]
#[derive(Default, Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
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

pub struct Gpu {
    pub glyph_scratch: Vec<(GpuGlyph, f32)>,

    pub current_vertex_buffer_capacity: u64,
    pub batch_pool:     Vec<Batch>,
    pub batch_count:    usize,

    pub atlas_tex:      wgpu::Texture,
    pub atlas_cur_x:    u16,
    pub atlas_cur_y:    u16,
    pub atlas_row_h:    u16,
    pub glyphs:         FastHashMap<(char, u32), GpuGlyph>, // (char, (font size * 10.0) as u32) -> Glyph
    pub font:           fontdue::Font,

    pub surface:        wgpu::Surface<'static>,
    pub surface_config: wgpu::SurfaceConfiguration,
    pub device:         wgpu::Device,
    pub queue:          wgpu::Queue,
    pub win_w:          f32,
    pub win_h:          f32,

    pub pipeline:       wgpu::RenderPipeline,
    pub bind_group:     wgpu::BindGroup,
    pub vertex_buffer:  wgpu::Buffer,
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
}

#[inline]
pub fn init(window: Arc<Window>) -> Gpu {
    pollster::block_on(init_async(window))
}

async fn init_async(window: Arc<Window>) -> Gpu {
    let size = window.inner_size();
    let (w, h) = (size.width.max(1), size.height.max(1));

    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    });

    let surface = instance.create_surface(window).unwrap();

    let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions {
        compatible_surface: Some(&surface),
        ..Default::default()
    }).await.unwrap();

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default())
        .await
        .unwrap();

    let caps = surface.get_capabilities(&adapter);
    let format = caps.formats.iter()
        .find(|f| **f == wgpu::TextureFormat::Bgra8Unorm)
        .copied()
        .unwrap_or(caps.formats[0]);

    let surface_config = wgpu::SurfaceConfiguration {
        usage:                         wgpu::TextureUsages::RENDER_ATTACHMENT,
        format, width: w, height: h,
        present_mode:                  wgpu::PresentMode::Fifo,
        // present_mode:                  wgpu::PresentMode::Mailbox,
        alpha_mode:                    caps.alpha_modes[0],
        view_formats:                  Vec::new(),
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &surface_config);

    let atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("atlas"),
        size:  wgpu::Extent3d { width: ATLAS_SIZE, height: ATLAS_SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1, sample_count: 1,
        dimension:    wgpu::TextureDimension::D2,
        format:       wgpu::TextureFormat::R8Unorm,
        usage:        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let atlas_view    = atlas_tex.create_view(&Default::default());
    let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0, visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                }, count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1, visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None, layout: &bgl,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&atlas_view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&atlas_sampler) },
        ],
    });

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None, source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None, bind_group_layouts: &[&bgl], immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None, layout: Some(&pipeline_layout),

        vertex: wgpu::VertexState {
            module: &shader, entry_point: Some("vs_main"),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Vert>() as u64,
                step_mode:    wgpu::VertexStepMode::Vertex,
                attributes:   &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4],
            }],
            compilation_options: Default::default(),
        },

        fragment: Some(wgpu::FragmentState {
            module: &shader, entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),

        primitive:      wgpu::PrimitiveState::default(),
        depth_stencil:  None,
        multisample:    wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache:          None,
    });

    let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None, size: INITIAL_VERTEX_BUFFER_CAPACITY,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let font_bytes = include_bytes!("../assets/font.ttf");
    let font = fontdue::Font::from_bytes(font_bytes.as_ref(), fontdue::FontSettings::default()).unwrap();

    Gpu {
        font,

        surface, surface_config, device, queue,

        win_w: w as f32, win_h: h as f32,

        pipeline, bind_group, vertex_buffer,

        atlas_tex, atlas_cur_x: 1, atlas_cur_y: 1, atlas_row_h: 0,

        glyphs: FastHashMap::with_capacity_and_hasher(2048, Default::default()),
        current_vertex_buffer_capacity: INITIAL_VERTEX_BUFFER_CAPACITY,

        glyph_scratch: Vec::with_capacity(256),

        batch_pool:  vec![Batch::full_window(w as _, h as _)],
        batch_count: 1
    }
}

//
// Glyph rasterization
//

pub fn prewarm_glyphs(gpu: &mut Gpu, font_size: f32) {
    // ASCII printable
    for c in ' '..='~' {
        get_glyph(gpu, c, font_size);
    }
    // Box drawing
    for c in '\u{2500}'..='\u{257F}' {
        get_glyph(gpu, c, font_size);
    }
}

pub fn get_glyph(gpu: &mut Gpu, c: char, size: f32) -> Option<GpuGlyph> {
    let size = (size * 2.0).round() / 2.0; // snap to 0.5px increments
    let key = (c, (size * 2.0) as u32);
    if let Some(g) = gpu.glyphs.get(&key) {
        return Some(*g);
    }

    let (metrics, bitmap) = gpu.font.rasterize(c, size);
    if metrics.width == 0 || metrics.height == 0 {
        // Cache the miss so we don't re-rasterize
        let g = GpuGlyph { advance: metrics.advance_width, ..Default::default() };
        gpu.glyphs.insert(key, g);
        return Some(g);
    }

    let (w, h) = (metrics.width as u16, metrics.height as u16);

    let atlas_used = gpu.atlas_cur_y as u32 * ATLAS_SIZE + gpu.atlas_cur_x as u32;
    let atlas_total = ATLAS_SIZE * ATLAS_SIZE;
    if (atlas_used as f32 / atlas_total as f32) > ATLAS_RESET_RATIO {
        reset_atlas(gpu);
    }

    if gpu.atlas_cur_x + w + 1 > ATLAS_SIZE as u16 {
        // Row wrap
        gpu.atlas_cur_y += gpu.atlas_row_h + 1;
        gpu.atlas_cur_x = 1;
        gpu.atlas_row_h = 0;
    }
    if gpu.atlas_cur_y + h + 1 > ATLAS_SIZE as u16 {
        eprintln!("atlas full");
        return None; // Shouldn't really happen though!
    }

    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &gpu.atlas_tex, mip_level: 0,
            origin: wgpu::Origin3d { x: gpu.atlas_cur_x as u32, y: gpu.atlas_cur_y as u32, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        &bitmap,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w as u32), rows_per_image: Some(h as u32) },
        wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
    );

    let g = GpuGlyph {
        uv_x: gpu.atlas_cur_x as u16,
        uv_y: gpu.atlas_cur_y as u16,
        uv_w: w as u16,
        uv_h: h as u16,
        w, h,
        bearing_x: metrics.xmin as i16,
        bearing_y: metrics.ymin as i16,
        advance: metrics.advance_width,
    };

    gpu.atlas_cur_x += w + 1;
    if h > gpu.atlas_row_h { gpu.atlas_row_h = h; }

    gpu.glyphs.insert(key, g);

    Some(g)
}

#[inline]
pub fn reset_atlas(gpu: &mut Gpu) {
    gpu.glyphs.clear();
    gpu.atlas_cur_x = 1;
    gpu.atlas_cur_y = 1;
    gpu.atlas_row_h = 0;
}

#[inline]
pub fn push_clip(gpu: &mut Gpu, x: f32, y: f32, w: f32, h: f32) {
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

//
//
// Draw primitives
//
//

/// 4 rects, 24 verts - reserve once for all of them.
#[inline(always)]
pub fn draw_rect_outline(gpu: &mut Gpu, x: f32, y: f32, w: f32, h: f32, thickness: f32, color: Color) {
    let inv_sw = 1.0 / gpu.win_w;
    let inv_sh = 1.0 / gpu.win_h;
    let color: GpuColor = color.into();
    let verts  = gpu.verts_mut();

    verts.reserve(24);
    draw_rect_impl(verts, inv_sw, inv_sh, x,       y,       w,         thickness, color); // Top
    draw_rect_impl(verts, inv_sw, inv_sh, x,       y + h,   w,         thickness, color); // Bottom
    draw_rect_impl(verts, inv_sw, inv_sh, x,       y,       thickness, h,         color); // Left
    draw_rect_impl(verts, inv_sw, inv_sh, x + w,   y,       thickness, h,         color); // Right
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

    // uv defaults to [0,0] - zero-uv means solid color.
    // Reserve once, write raw.
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

#[inline(always)]
pub fn measure_str(gpu: &mut Gpu, s: &str, font_size: f32) -> f32 {
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

    // Collect glyphs first so we can then hold &mut verts without aliasing.
    // Stack-allocate for short strings; fall back to a bump on the caller's
    // stack via a small fixed array. Typical UI strings are <128 chars.
    // If you have a scratch Vec available on the caller, pass it in instead.
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
        let gy = (y - g.bearing_y as f32 - g.h as f32).round();

        let x0 =  gx           * inv_sw * 2.0 - 1.0;
        let x1 = (gx + g.w as f32) * inv_sw * 2.0 - 1.0;
        let y0 =  1.0 - gy              * inv_sh * 2.0;
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

    insertion_ids:     &[u64],
    global_glyph_start: usize,   // ll.glyph_start
    insertion_ts:       [f32; PASTE_ANIMATION_MAX_ID + 1],
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
        let gy = (y              - gg.bearing_y as f32 - gg.h as f32).round();
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
            let ease  = 1.0 - (1.0 - t_raw).powi(4);
            lerp_color(paste_highlight, base_color, ease)
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

pub fn submit_frame(gpu: &mut Gpu) -> Result<(), wgpu::SurfaceError> {
    struct Draw {
        range: Range<u32>,
        clip: [f32; 4],
    }

    let _tracy = tracy::span!("submit_frame");

    //
    // Build draw list and upload verts directly from each batch to the GPU buffer
    //

    let total_verts = gpu.batch_pool.iter().map(|b| b.verts.len()).sum::<usize>();
    let byte_size = (total_verts * size_of::<Vert>()) as u64;

    let mut draws = Vec::new();

    if byte_size > 0 {
        if byte_size > gpu.current_vertex_buffer_capacity {
            // Grow if needed
            let new_cap = (byte_size * 2).max(INITIAL_VERTEX_BUFFER_CAPACITY);
            gpu.vertex_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: None, size: new_cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            gpu.current_vertex_buffer_capacity = new_cap;
        }

        let mut vert_offset = 0u32;
        for batch in &gpu.batch_pool[..gpu.batch_count] {
            if batch.verts.is_empty() { continue }

            let byte_offset = (vert_offset as usize * size_of::<Vert>()) as u64;
            gpu.queue.write_buffer(&gpu.vertex_buffer, byte_offset, bytemuck::cast_slice(&batch.verts));

            let end = vert_offset + batch.verts.len() as u32;
            draws.push(Draw { range: vert_offset..end, clip: batch.clip });

            vert_offset = end;
        }
    }

    //
    // Reset batch state for next frame
    //
    for batch in &mut gpu.batch_pool[..gpu.batch_count] {
        batch.verts.clear();
    }
    gpu.batch_count = 1;
    gpu.batch_pool[0].clip = [0.0, 0.0, gpu.win_w, gpu.win_h];

    //
    // Begin the frame
    //
    let output = gpu.surface.get_current_texture()?;
    let view   = output.texture.create_view(&Default::default());
    let mut enc = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load:  wgpu::LoadOp::Clear(palette().bg.into()),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });

        if !draws.is_empty() {
            pass.set_pipeline(&gpu.pipeline);
            pass.set_bind_group(0, &gpu.bind_group, &[]);
            pass.set_vertex_buffer(0, gpu.vertex_buffer.slice(..));

            for Draw { range, clip } in &draws {
                let cx = clip[0].max(0.0) as u32;
                let cy = clip[1].max(0.0) as u32;
                let cw = (clip[2] as u32).min(gpu.win_w as u32 - cx);
                let ch = (clip[3] as u32).min(gpu.win_h as u32 - cy);

                if cw == 0 || ch == 0 { continue }

                pass.set_scissor_rect(cx, cy, cw, ch);
                pass.draw(range.clone(), 0..1);
            }
        }
    }

    gpu.queue.submit([enc.finish()]);
    output.present();

    Ok(())
}

//
// Shader
//

const SHADER: &str = r#"
struct V {
    @location(0) pos:   vec2<f32>,
    @location(1) uv:    vec2<f32>,
    @location(2) color: vec4<f32>
}

struct F {
    @builtin(position) pos:   vec4<f32>,
    @location(0)       uv:    vec2<f32>,
    @location(1)       color: vec4<f32>
}

@vertex
fn vs_main(v: V) -> F {
    return F(
        vec4<f32>(v.pos, 0.0, 1.0),
        v.uv,
        v.color
    );
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;

const ATLAS_SIZE: f32 = 1024.0; // @Note: Must match ATLAS_SIZE in gpu

@fragment
fn fs_main(f: F) -> @location(0) vec4<f32> {
    if f.uv.x == 0.0 && f.uv.y == 0.0 { return f.color; } // @Hack

    let uv = f.uv / ATLAS_SIZE;
    let a = textureSample(tex, smp, uv).r;

    return f.color * a;
}
"#;
