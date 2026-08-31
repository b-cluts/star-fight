//! Procedural space backdrop: layered starfield plus value-noise nebula,
//! generated once at startup into a texture. Purely cosmetic.

use bevy::prelude::*;
use bevy::render::render_asset::RenderAssetUsages;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Deterministic lattice value in [0, 1).
fn lattice(ix: i64, iy: i64, seed: u64) -> f32 {
    let mut h = (ix as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (iy as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ seed;
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    (h & 0xFFFFFF) as f32 / 16_777_216.0
}

fn smooth(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

fn value_noise(x: f32, y: f32, seed: u64) -> f32 {
    let (ix, iy) = (x.floor() as i64, y.floor() as i64);
    let (fx, fy) = (x - x.floor(), y - y.floor());
    let (sx, sy) = (smooth(fx), smooth(fy));
    let v00 = lattice(ix, iy, seed);
    let v10 = lattice(ix + 1, iy, seed);
    let v01 = lattice(ix, iy + 1, seed);
    let v11 = lattice(ix + 1, iy + 1, seed);
    let a = v00 + (v10 - v00) * sx;
    let b = v01 + (v11 - v01) * sx;
    a + (b - a) * sy
}

fn fbm(x: f32, y: f32, seed: u64) -> f32 {
    let mut sum = 0.0;
    let mut amp = 0.55;
    let mut freq = 1.0;
    for octave in 0..4u64 {
        sum += amp * value_noise(x * freq, y * freq, seed.wrapping_add(octave * 101));
        amp *= 0.5;
        freq *= 2.1;
    }
    sum
}

pub fn starfield_image(w: u32, h: u32, seed: u64) -> Image {
    let mut data = vec![0u8; (w * h * 4) as usize];

    // Nebula: two independent noise fields tinted purple and teal, only
    // their upper reaches visible so the clouds stay wispy.
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let wisp = |n: f32| ((n - 0.52).max(0.0) * 2.4).min(1.0);
            let p = wisp(fbm(fx * 3.0, fy * 3.0, seed));
            let t = wisp(fbm(fx * 3.0 + 11.3, fy * 3.0 + 5.7, seed ^ 0x9E3779B9));
            let r = 0.015 + 0.17 * p + 0.02 * t;
            let g = 0.015 + 0.03 * p + 0.11 * t;
            let b = 0.04 + 0.24 * p + 0.15 * t;
            let i = ((y * w + x) * 4) as usize;
            data[i] = (r.min(1.0) * 255.0) as u8;
            data[i + 1] = (g.min(1.0) * 255.0) as u8;
            data[i + 2] = (b.min(1.0) * 255.0) as u8;
            data[i + 3] = 255;
        }
    }

    // Stars: mostly faint singles, a few bright ones with a tiny cross.
    let mut rng = seed | 1;
    let put = |data: &mut Vec<u8>, x: i64, y: i64, v: u8, bluish: bool| {
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
            return;
        }
        let i = ((y as u32 * w + x as u32) * 4) as usize;
        data[i] = data[i].max(v.saturating_sub(if bluish { 25 } else { 0 }));
        data[i + 1] = data[i + 1].max(v.saturating_sub(if bluish { 15 } else { 0 }));
        data[i + 2] = data[i + 2].max(v);
    };
    for _ in 0..1100 {
        let x = (xorshift(&mut rng) % w as u64) as i64;
        let y = (xorshift(&mut rng) % h as u64) as i64;
        let v = 70 + (xorshift(&mut rng) % 186) as u8;
        let bluish = xorshift(&mut rng) % 3 == 0;
        put(&mut data, x, y, v, bluish);
        if v > 225 {
            let d = (v / 3).max(60);
            for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                put(&mut data, x + dx, y + dy, d, bluish);
            }
        }
    }

    Image::new(
        Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}
