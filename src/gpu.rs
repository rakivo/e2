use crate::color::{GpuColor, Color};
use crate::palette;
use crate::util::px;

use std::ops::Range;
use std::sync::Arc;
use std::collections::HashMap;

use winit::window::Window;

pub const FONT_SIZE: f32 = 14.0;

const ATLAS_SIZE: u32 = 1024; // 1MB
const ATLAS_RESET_RATIO: f32 = 0.8; // 80%

const VTX_BUF_CAP: u64 = 64 * 1024;

#[derive(Default, Clone, Copy)]
pub struct Glyph {
    pub uv_x: f32,      pub uv_y: f32,
    pub uv_w: f32,      pub uv_h: f32,
    pub w: u32,         pub h: u32,
    pub bearing_x: i32, pub bearing_y: i32,
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
    verts: Vec<Vert>,
    clip:  [f32; 4],
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
    pub surface:        wgpu::Surface<'static>,
    pub surface_config: wgpu::SurfaceConfiguration,
    pub device:         wgpu::Device,
    pub queue:          wgpu::Queue,
    pub win_w:          f32,
    pub win_h:          f32,

    pub pipeline:       wgpu::RenderPipeline,
    pub bind_group:     wgpu::BindGroup,
    pub vtx_buf:        wgpu::Buffer,

    pub atlas_tex:      wgpu::Texture,
    pub atlas_cur_x:    u32,
    pub atlas_cur_y:    u32,
    pub atlas_row_h:    u32,
    pub glyphs:         HashMap<(char, u32), Glyph>, // (char, (font size * 10.0) as u32) -> Glyph
    pub font:           fontdue::Font,

    pub vtx_buf_cap:    u64,
    pub batch_pool:     Vec<Batch>,
    pub batch_count:    usize,
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

    let vtx_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None, size: VTX_BUF_CAP,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let font_bytes = include_bytes!("../assets/font.ttf");
    let font = fontdue::Font::from_bytes(font_bytes.as_ref(), fontdue::FontSettings::default()).unwrap();

    Gpu {
        font,

        surface, surface_config, device, queue,

        win_w: w as f32, win_h: h as f32,

        pipeline, bind_group, vtx_buf,

        atlas_tex, atlas_cur_x: 1, atlas_cur_y: 1, atlas_row_h: 0,

        glyphs: Default::default(),
        vtx_buf_cap: VTX_BUF_CAP,

        batch_pool:  vec![Batch::full_window(w as _, h as _)],
        batch_count: 1
    }
}

//
// Glyph rasterization
//

pub fn get_glyph(gpu: &mut Gpu, c: char, size: f32) -> Option<Glyph> {
    let size = (size * 2.0).round() / 2.0; // snap to 0.5px increments
    let key = (c, (size * 2.0) as u32);
    if let Some(g) = gpu.glyphs.get(&key) {
        return Some(*g);
    }

    let (metrics, bitmap) = gpu.font.rasterize(c, size);
    if metrics.width == 0 || metrics.height == 0 {
        // Cache the miss so we don't re-rasterize
        let g = Glyph { advance: metrics.advance_width, ..Default::default() };
        gpu.glyphs.insert(key, g);
        return Some(g);
    }

    let (w, h) = (metrics.width as u32, metrics.height as u32);

    let atlas_used = gpu.atlas_cur_y * ATLAS_SIZE + gpu.atlas_cur_x;
    let atlas_total = ATLAS_SIZE * ATLAS_SIZE;
    if (atlas_used as f32 / atlas_total as f32) > ATLAS_RESET_RATIO {
        reset_atlas(gpu);
    }

    if gpu.atlas_cur_x + w + 1 > ATLAS_SIZE {
        // Row wrap
        gpu.atlas_cur_y += gpu.atlas_row_h + 1;
        gpu.atlas_cur_x = 1;
        gpu.atlas_row_h = 0;
    }
    if gpu.atlas_cur_y + h + 1 > ATLAS_SIZE {
        eprintln!("atlas full");
        return None; // Shouldn't really happen though!
    }

    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &gpu.atlas_tex, mip_level: 0,
            origin: wgpu::Origin3d { x: gpu.atlas_cur_x, y: gpu.atlas_cur_y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        &bitmap,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w), rows_per_image: Some(h) },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );

    let g = Glyph {
        uv_x: gpu.atlas_cur_x as f32 / ATLAS_SIZE as f32,
        uv_y: gpu.atlas_cur_y as f32 / ATLAS_SIZE as f32,
        uv_w: w as f32 / ATLAS_SIZE as f32,
        uv_h: h as f32 / ATLAS_SIZE as f32,
        w, h,
        bearing_x: metrics.xmin,
        bearing_y: metrics.ymin,
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
// Draw primitives
//

pub fn draw_rect(gpu: &mut Gpu, x: f32, y: f32, w: f32, h: f32, color: Color) {
    let (sw, sh) = (gpu.win_w, gpu.win_h);

    let [x0, y0] = px(x,   y,   sw, sh);
    let [x1, y1] = px(x+w, y+h, sw, sh);

    let color = color.into();
    gpu.verts_mut().extend_from_slice(&[
        Vert { pos:[x0,y0], color, ..Default::default() },
        Vert { pos:[x1,y0], color, ..Default::default() },
        Vert { pos:[x0,y1], color, ..Default::default() },
        Vert { pos:[x1,y0], color, ..Default::default() },
        Vert { pos:[x1,y1], color, ..Default::default() },
        Vert { pos:[x0,y1], color, ..Default::default() },
    ]);
}

// Draw text with a per-character color - pass a closure that maps char index -> color
pub fn draw_text_colored(
    gpu: &mut Gpu,
    text: &str,
    mut x: f32,
    y: f32,
    font_size: f32,
    color_callback: impl Fn(usize) -> Color // Glyph index -> Color
) {
    let (sw, sh) = (gpu.win_w, gpu.win_h);

    for (i, c) in text.chars().enumerate() {
        let Some(g) = get_glyph(gpu, c, font_size) else {
            x += 8.0;
            continue;
        };

        if g.w > 0 && g.h > 0 {
            let gx = (x + g.bearing_x as f32).round();
            let gy = (y - g.bearing_y as f32 - g.h as f32).round();

            let [x0, y0] = px(gx,              gy,              sw, sh);
            let [x1, y1] = px(gx + g.w as f32, gy + g.h as f32, sw, sh);

            let (u0, v0) = (g.uv_x,            g.uv_y);
            let (u1, v1) = (g.uv_x + g.uv_w,   g.uv_y + g.uv_h);

            let color: GpuColor = color_callback(i).into();
            gpu.verts_mut().extend_from_slice(&[
                Vert { pos:[x0, y0], uv:[u0, v0], color },
                Vert { pos:[x1, y0], uv:[u1, v0], color },
                Vert { pos:[x0, y1], uv:[u0, v1], color },
                Vert { pos:[x1, y0], uv:[u1, v0], color },
                Vert { pos:[x1, y1], uv:[u1, v1], color },
                Vert { pos:[x0, y1], uv:[u0, v1], color },
            ]);
        }

        x += g.advance;
    }
}

// Flat color convenience wrapper
#[inline]
pub fn draw_text(gpu: &mut Gpu, text: &str, x: f32, y: f32, font_size: f32, color: Color) {
    draw_text_colored(gpu, text, x, y, font_size, |_| color);
}

pub fn submit_frame(gpu: &mut Gpu) -> Result<(), wgpu::SurfaceError> {
    struct Draw {
        range: Range<u32>,
        clip: [f32; 4],
    }

    //
    // Build draw list and upload verts directly from each batch to the GPU buffer
    //

    let total_verts = gpu.batch_pool.iter().map(|b| b.verts.len()).sum::<usize>();
    let byte_size = (total_verts * size_of::<Vert>()) as u64;

    let mut draws = Vec::new();

    if byte_size > 0 {
        if byte_size > gpu.vtx_buf_cap {
            // Grow if needed
            let new_cap = (byte_size * 2).max(VTX_BUF_CAP);
            gpu.vtx_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: None, size: new_cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            gpu.vtx_buf_cap = new_cap;
        }

        let mut vert_offset = 0u32;
        for batch in &gpu.batch_pool[..gpu.batch_count] {
            if batch.verts.is_empty() { continue }

            let byte_offset = (vert_offset as usize * size_of::<Vert>()) as u64;
            gpu.queue.write_buffer(&gpu.vtx_buf, byte_offset, bytemuck::cast_slice(&batch.verts));

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
            pass.set_vertex_buffer(0, gpu.vtx_buf.slice(..));

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
struct V { @location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) color: vec4<f32> }
struct F { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, @location(1) color: vec4<f32> }

@vertex fn vs_main(v: V) -> F {
    return F(vec4<f32>(v.pos, 0.0, 1.0), v.uv, v.color);
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;

@fragment fn fs_main(f: F) -> @location(0) vec4<f32> {
    if f.uv.x == 0.0 && f.uv.y == 0.0 { return f.color; }
    // verts already premultiplied, just scale by glyph alpha
    let a = textureSample(tex, smp, f.uv).r;
    return f.color * a;
}
"#;
