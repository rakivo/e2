#![allow(unused, dead_code)]

use std::ops::{Deref, Index};

#[inline(always)]
pub fn lerp_color(a: GpuColor, b: GpuColor, t: f32) -> GpuColor {
    GpuColor([
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ])
}

#[derive(Default, Debug, Copy, Clone)]
pub struct Color {
    pub r: u8, pub g: u8, pub b: u8, pub a: u8
}

impl From<GpuColor> for Color {
    fn from(gpu: GpuColor) -> Self {
        gpu.into_color()
    }
}

impl Into<[f32; 4]> for Color {
    fn into(self) -> [f32; 4] {
        self.into_gpu().0
    }
}

impl From<[f32; 4]> for Color {
    fn from([r, g, b, a]: [f32; 4]) -> Self {
        Self::rgba(
            (r * 255.0) as u8,
            (g * 255.0) as u8,
            (b * 255.0) as u8,
            (a * 255.0) as u8,
        )
    }
}

impl Into<wgpu::Color> for Color {
    fn into(self) -> wgpu::Color {
        let gpu: GpuColor = self.into();
        gpu.into()
    }
}

impl Color {
    #[inline(always)]
    pub const fn into_gpu(self) -> GpuColor {
        GpuColor::rgba(self.r, self.g, self.b, self.a)
    }

    #[inline(always)]
    pub fn hsv(h: f32, s: f32, v: f32) -> Self {
        GpuColor::hsv(h, s, v).into()
    }

    #[inline(always)]
    pub const fn hex(rgb: u32) -> Self {
        let r = ((rgb >> 16) & 0xff) as u8;
        let g = ((rgb >>  8) & 0xff) as u8;
        let b = ((rgb      ) & 0xff) as u8;
        Self::rgba(r, g, b, 255)
    }

    #[inline(always)]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color { r, g, b, a }
    }

    #[inline]
    pub fn over(self, bg: Color) -> Color {
        let a = self.a as f32 / 255.0;

        // Convert bg to linear
        let bg_r = (bg.r as f32 / 255.0).powf(2.2);
        let bg_g = (bg.g as f32 / 255.0).powf(2.2);
        let bg_b = (bg.b as f32 / 255.0).powf(2.2);

        // Composite in linear space
        let r = (self.r as f32 / 255.0) * a + bg_r * (1.0 - a);
        let g = (self.g as f32 / 255.0) * a + bg_g * (1.0 - a);
        let b = (self.b as f32 / 255.0) * a + bg_b * (1.0 - a);

        // Convert back to sRGB (0..255)
        Color {
            r: (r.powf(1.0 / 2.2) * 255.0) as u8,
            g: (g.powf(1.0 / 2.2) * 255.0) as u8,
            b: (b.powf(1.0 / 2.2) * 255.0) as u8,
            a: 255,
        }
    }

    /// 0..1 amount
    #[inline]
    pub fn darken(self, amount: f32) -> Color {
        let factor = (1.0 - amount).clamp(0.0, 1.0);
        Color {
            r: (self.r as f32 * factor) as u8,
            g: (self.g as f32 * factor) as u8,
            b: (self.b as f32 * factor) as u8,
            a: self.a,
        }
    }
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuColor(pub [f32; 4]);

impl Index<usize> for GpuColor {
    type Output = f32;

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl From<[f32; 4]> for GpuColor {
    fn from(value: [f32; 4]) -> Self {
        Self(value)
    }
}

impl From<Color> for GpuColor {
    fn from(Color { r, g, b, a}: Color) -> Self {
        let a = a as f32 / 255.0;
        [
            r as f32 / 255.0 * a,
            g as f32 / 255.0 * a,
            b as f32 / 255.0 * a,
            a,
        ].into()
    }
}

impl Into<wgpu::Color> for GpuColor {
    fn into(self) -> wgpu::Color {
        wgpu::Color {
            r: self[0] as f64,
            g: self[1] as f64,
            b: self[2] as f64,
            a: self[3] as f64,
        }
    }
}

impl Deref for GpuColor {
    type Target = [f32; 4];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Default for GpuColor {
    fn default() -> Self {
        Self::rgba(238, 130, 238, 255)
    }
}

impl GpuColor {
    #[inline(always)]
    pub const fn into_color(self) -> Color {
        Color {
            r: (self.0[0] * 255.0) as _,
            g: (self.0[1] * 255.0) as _,
            b: (self.0[2] * 255.0) as _,
            a: (self.0[3] * 255.0) as _,
        }
    }

    #[inline(always)]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a as f32 / 255.0])
    }

    #[inline(always)]
    pub const fn hsv(h: f32, s: f32, v: f32) -> Self {
        let i = (h * 6.0) as u32;
        let f = h * 6.0 - i as f32;
        let (p, q, t) = (v*(1.0-s), v*(1.0-s*f), v*(1.0-s*(1.0-f)));
        let (r, g, b) = match i % 6 {
            0 => (v, t, p),
            1 => (q, v, p),
            2 => (p, v, t),
            3 => (p, q, v),
            4 => (t, p, v),
            _ => (v, p, q),
        };
        Self([r, g, b, 1.0])
    }
}
