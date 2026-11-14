//! Depth-image-based rendering: synthesise left/right eye views from one photo and its
//! depth map.
//!
//! Each row is treated as a 1-D triangle strip: every source pixel is moved horizontally
//! by its parallax, the spans between neighbours are rasterised with linear interpolation,
//! and a depth test makes nearer surfaces win. In `Fill` mode the strip is torn wherever
//! it would stretch a lot (a depth edge), and the revealed gap is filled by mirroring the
//! adjacent *background* texture, which keeps foreground outlines crisp.

use image::RgbImage;
use rayon::prelude::*;

use crate::postprocess::DepthMap;
use crate::settings::{EdgeMode, StereoSettings};

pub struct StereoPair {
    pub left: RgbImage,
    pub right: RgbImage,
}

/// Max horizontal shift for one eye, in pixels, for an image `width` wide.
pub fn eye_shift_px(s: &StereoSettings, width: u32) -> f32 {
    s.strength / 100.0 * width as f32 / 2.0
}

pub fn synthesize(img: &RgbImage, depth: &DepthMap, s: &StereoSettings) -> StereoPair {
    assert_eq!(img.dimensions(), (depth.width, depth.height), "depth/image size mismatch");
    let k = eye_shift_px(s, img.width());
    // Nearer than the convergence plane => crossed parallax (in front of the screen):
    // the left eye sees it shifted right, the right eye shifted left.
    let (left, right) = rayon::join(
        || render_eye(img, depth, k, s),
        || render_eye(img, depth, -k, s),
    );
    if s.swap_eyes {
        StereoPair {
            left: right,
            right: left,
        }
    } else {
        StereoPair { left, right }
    }
}

fn render_eye(img: &RgbImage, depth: &DepthMap, shift: f32, s: &StereoSettings) -> RgbImage {
    let (w, h) = img.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    let src = img.as_raw();
    let mut out = vec![0u8; wu * hu * 3];
    let mut z = vec![0f32; wu * hu];
    let mut filled = vec![false; wu * hu];
    out.par_chunks_mut(wu * 3)
        .zip(z.par_chunks_mut(wu))
        .zip(filled.par_chunks_mut(wu))
        .enumerate()
        .for_each(|(y, ((orow, zrow), frow))| {
            let srow = &src[y * wu * 3..(y + 1) * wu * 3];
            let drow = &depth.data[y * wu..(y + 1) * wu];
            warp_row(srow, drow, shift, s, orow, zrow, frow);
        });
    if s.edge_mode == EdgeMode::Fill {
        // ~1.5 px per 1000 px of width, so previews and full-size exports look alike.
        let r = ((w as f32 * 1.5 / 1000.0).round() as usize).max(1);
        smooth_fill(&mut out, &z, &filled, wu, hu, r);
    }
    RgbImage::from_raw(w, h, out).expect("size matches")
}

fn warp_row(
    src: &[u8],
    d: &[f32],
    shift: f32,
    s: &StereoSettings,
    out: &mut [u8],
    z: &mut [f32],
    filled: &mut [bool],
) {
    let w = d.len();
    if w < 2 || shift == 0.0 {
        out.copy_from_slice(src);
        z.copy_from_slice(d);
        return;
    }
    let c = s.convergence;
    let tear = match s.edge_mode {
        EdgeMode::Fill => 1.0 + s.tear_threshold.max(0.0),
        EdgeMode::Stretch => f32::INFINITY,
    };
    z.fill(f32::NEG_INFINITY);
    let target = |x: usize| x as f32 + shift * (d[x] - c);
    let last = (w - 1) as f32;

    for x in 0..w - 1 {
        let (t0, t1) = (target(x), target(x + 1));
        let span = t1 - t0;
        if span > tear {
            continue; // depth edge: leave a gap, filled from the background below
        }
        let (a, b) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
        if b < 0.0 || a > last {
            continue;
        }
        let start = a.ceil().max(0.0) as usize;
        let end = b.floor().min(last) as usize;
        if start > end {
            continue;
        }
        let (z0, z1) = (d[x], d[x + 1]);
        let (p0, p1) = (&src[x * 3..x * 3 + 3], &src[x * 3 + 3..x * 3 + 6]);
        for p in start..=end {
            let t = if span.abs() > 1e-6 {
                ((p as f32 - t0) / span).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let zz = z0 + (z1 - z0) * t;
            if zz > z[p] {
                z[p] = zz;
                for ch in 0..3 {
                    let v = p0[ch] as f32 + (p1[ch] as f32 - p0[ch] as f32) * t;
                    out[p * 3 + ch] = (v + 0.5) as u8;
                }
            }
        }
    }
    fill_holes(out, z, filled);
}

/// Depth difference within which two pixels count as "the same surface".
const SAME_SURFACE: f32 = 0.08;
/// Pixels right next to a depth edge are often tinted by the foreground; skip this many.
const EDGE_MARGIN: usize = 2;

/// Fill uncovered pixels by mirroring the texture on the farther (background) side of
/// each gap, stopping before it would copy something at a different depth.
fn fill_holes(out: &mut [u8], z: &mut [f32], filled: &mut [bool]) {
    let w = z.len();
    let hole = f32::NEG_INFINITY;
    let mut x = 0;
    while x < w {
        if z[x] != hole {
            x += 1;
            continue;
        }
        let a = x;
        while x < w && z[x] == hole {
            x += 1;
        }
        let b = x - 1;
        let len = b - a + 1;
        let left = a.checked_sub(1);
        let right = (b + 1 < w).then_some(b + 1);
        let from_right = match (left, right) {
            (Some(l), Some(r)) => z[r] < z[l],
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => {
                out.fill(0);
                return;
            }
        };
        // Is pixel `s` on the same (background) surface as depth `zb`?
        fn same(z: &[f32], s: usize, zb: f32) -> bool {
            s < z.len() && z[s] != f32::NEG_INFINITY && (z[s] - zb).abs() < SAME_SURFACE
        }
        if from_right {
            let zr = z[b + 1];
            let mut base = b + 1;
            while base - (b + 1) < EDGE_MARGIN && same(z, base + 1, zr) {
                base += 1;
            }
            let mut last_ok = base;
            for i in 0..len {
                if same(z, base + i, zr) {
                    last_ok = base + i;
                }
                copy_px(out, last_ok, b - i);
                z[b - i] = zr;
                filled[b - i] = true;
            }
        } else {
            let zl = z[a - 1];
            let mut base = a - 1;
            while (a - 1) - base < EDGE_MARGIN && base > 0 && same(z, base - 1, zl) {
                base -= 1;
            }
            let mut last_ok = base;
            for i in 0..len {
                if let Some(s) = base.checked_sub(i)
                    && same(z, s, zl) {
                        last_ok = s;
                    }
                copy_px(out, last_ok, a + i);
                z[a + i] = zl;
                filled[a + i] = true;
            }
        }
    }
}

/// Soften filled gaps with a box blur that only looks at background pixels (never the
/// foreground object that caused the gap). Turns row-by-row streaks into a calm fill.
fn smooth_fill(out: &mut [u8], z: &[f32], filled: &[bool], w: usize, h: usize, r: usize) {
    if !filled.iter().any(|&f| f) {
        return;
    }
    let src = out.to_vec();
    out.par_chunks_mut(w * 3).enumerate().for_each(|(y, orow)| {
        let fill_row = &filled[y * w..(y + 1) * w];
        if !fill_row.iter().any(|&f| f) {
            return;
        }
        let (y0, y1) = (y.saturating_sub(r), (y + r).min(h - 1));
        for x in 0..w {
            if !fill_row[x] {
                continue;
            }
            let zb = z[y * w + x] + 0.02;
            let (x0, x1) = (x.saturating_sub(r), (x + r).min(w - 1));
            let mut acc = [0u32; 3];
            let mut n = 0u32;
            for yy in y0..=y1 {
                for xx in x0..=x1 {
                    let j = yy * w + xx;
                    if z[j] <= zb {
                        acc[0] += src[j * 3] as u32;
                        acc[1] += src[j * 3 + 1] as u32;
                        acc[2] += src[j * 3 + 2] as u32;
                        n += 1;
                    }
                }
            }
            for ch in 0..3 {
                // Rounded average; no same-surface neighbours (n = 0) leaves the pixel as is.
                if let Some(v) = (acc[ch] + n / 2).checked_div(n) {
                    orow[x * 3 + ch] = v as u8;
                }
            }
        }
    });
}

#[inline]
fn copy_px(buf: &mut [u8], from: usize, to: usize) {
    buf.copy_within(from * 3..from * 3 + 3, to * 3);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(w: u32, h: u32, v: f32) -> DepthMap {
        DepthMap {
            width: w,
            height: h,
            data: vec![v; (w * h) as usize],
        }
    }

    fn gradient_img(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, _| image::Rgb([(x * 255 / (w - 1)) as u8, 100, 50]))
    }

    #[test]
    fn depth_at_convergence_is_unchanged() {
        let img = gradient_img(64, 4);
        let s = StereoSettings {
            convergence: 0.5,
            ..Default::default()
        };
        let pair = synthesize(&img, &flat(64, 4, 0.5), &s);
        assert_eq!(pair.left, img);
        assert_eq!(pair.right, img);
    }

    #[test]
    fn near_object_shifts_opposite_ways() {
        // A bright near square on a dark far background.
        let (w, h) = (200u32, 3u32);
        let img = RgbImage::from_fn(w, h, |x, _| {
            if (90..110).contains(&x) {
                image::Rgb([255, 255, 255])
            } else {
                image::Rgb([0, 0, 0])
            }
        });
        let mut d = flat(w, h, 0.0);
        for y in 0..h as usize {
            for x in 90..110 {
                d.data[y * w as usize + x] = 1.0;
            }
        }
        let s = StereoSettings {
            strength: 10.0, // 20 px total, 10 px per eye
            convergence: 0.0,
            ..Default::default()
        };
        let pair = synthesize(&img, &d, &s);
        let first_white = |im: &RgbImage| (0..w).find(|&x| im.get_pixel(x, 1)[0] > 128).unwrap();
        let l = first_white(&pair.left);
        let r = first_white(&pair.right);
        assert!(l > 90 && r < 90, "left {l} right {r}");
        assert_eq!(l - r, 20);
    }

    #[test]
    fn holes_are_filled_from_background() {
        let (w, h) = (100u32, 1u32);
        let img = RgbImage::from_fn(w, h, |x, _| {
            if x >= 50 {
                image::Rgb([255, 0, 0]) // near: red
            } else {
                image::Rgb([0, 0, 255]) // far: blue
            }
        });
        let mut d = flat(w, h, 0.0);
        for x in 50..100 {
            d.data[x] = 1.0;
        }
        let s = StereoSettings {
            strength: 20.0,
            convergence: 0.0,
            edge_mode: EdgeMode::Fill,
            ..Default::default()
        };
        let pair = synthesize(&img, &d, &s);
        // Left eye: near red region moves right by 10px, revealing a gap at 50..60
        // which must be filled with blue background, not red.
        for x in 50..60 {
            assert_eq!(pair.left.get_pixel(x, 0)[2], 255, "x={x}");
        }
        // No black (unfilled) pixels anywhere.
        assert!(pair.left.pixels().all(|p| p.0 != [0, 0, 0]));
        assert!(pair.right.pixels().all(|p| p.0 != [0, 0, 0]));
    }
}
