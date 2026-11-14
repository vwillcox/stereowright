//! Turns raw model output into a clean, normalised depth map at the target resolution,
//! applying the user's depth tuning (clip, curve, smoothing, edge dilation).

use rayon::prelude::*;

use crate::depth::RawDepth;
use crate::settings::DepthSettings;

/// Normalised depth, same size as the image it belongs to. 0 = farthest, 1 = nearest.
#[derive(Clone, Debug)]
pub struct DepthMap {
    pub width: u32,
    pub height: u32,
    pub data: Vec<f32>,
}

impl DepthMap {
    /// Bilinear sample at pixel-space coordinates (clamped).
    pub fn sample(&self, x: f32, y: f32) -> f32 {
        let (w, h) = (self.width as usize, self.height as usize);
        let x = x.clamp(0.0, (w - 1) as f32);
        let y = y.clamp(0.0, (h - 1) as f32);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let d = &self.data;
        let top = d[y0 * w + x0] * (1.0 - fx) + d[y0 * w + x1] * fx;
        let bot = d[y1 * w + x0] * (1.0 - fx) + d[y1 * w + x1] * fx;
        top * (1.0 - fy) + bot * fy
    }

    pub fn to_gray8(&self) -> image::GrayImage {
        image::GrayImage::from_raw(
            self.width,
            self.height,
            self.data.par_iter().map(|v| (v * 255.0).round() as u8).collect(),
        )
        .expect("size matches")
    }

    pub fn to_gray16(&self) -> image::ImageBuffer<image::Luma<u16>, Vec<u16>> {
        image::ImageBuffer::from_raw(
            self.width,
            self.height,
            self.data.par_iter().map(|v| (v * 65535.0).round() as u16).collect(),
        )
        .expect("size matches")
    }
}

/// Build the final depth map for an image of size `w`×`h`.
pub fn prepare(raw: &RawDepth, w: u32, h: u32, s: &DepthSettings) -> DepthMap {
    prepare_in_range(raw, robust_range(&raw.data), w, h, s)
}

/// Like [`prepare`], but `raw` values `lo`..`hi` map to far..near (video uses a range that
/// is steadied over time instead of each frame's own).
pub fn prepare_in_range(raw: &RawDepth, (lo, hi): (f32, f32), w: u32, h: u32, s: &DepthSettings) -> DepthMap {
    let span = (hi - lo).max(1e-6);

    let far = s.far_clip.clamp(0.0, 0.99);
    let near = s.near_clip.clamp(far + 0.01, 1.0);
    let curve = s.curve.clamp(0.1, 10.0);
    let invert = s.invert;

    let (sw, sh) = (raw.width as usize, raw.height as usize);
    let (wu, hu) = (w as usize, h as usize);
    let sx = sw as f32 / wu as f32;
    let sy = sh as f32 / hu as f32;
    let src = &raw.data;

    let mut data = vec![0f32; wu * hu];
    data.par_chunks_mut(wu).enumerate().for_each(|(y, row)| {
        let fy = ((y as f32 + 0.5) * sy - 0.5).clamp(0.0, (sh - 1) as f32);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let ty = fy - y0 as f32;
        for (x, out) in row.iter_mut().enumerate() {
            let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let tx = fx - x0 as f32;
            let top = src[y0 * sw + x0] * (1.0 - tx) + src[y0 * sw + x1] * tx;
            let bot = src[y1 * sw + x0] * (1.0 - tx) + src[y1 * sw + x1] * tx;
            let v = top * (1.0 - ty) + bot * ty;

            let mut v = ((v - lo) / span).clamp(0.0, 1.0);
            if invert {
                v = 1.0 - v;
            }
            v = ((v - far) / (near - far)).clamp(0.0, 1.0);
            if curve != 1.0 {
                v = v.powf(curve);
            }
            *out = v;
        }
    });

    let radius = (s.edge_dilation * w as f32 / 1000.0).round() as usize;
    if radius > 0 {
        max_filter(&mut data, wu, hu, radius);
    }
    let sigma = s.smoothing / 100.0 * w as f32;
    if sigma >= 0.5 {
        gaussian_blur(&mut data, wu, hu, sigma);
    }

    DepthMap {
        width: w,
        height: h,
        data,
    }
}

/// 0.5th and 99.5th percentile: ignores a few extreme pixels so they don't squash the range.
pub fn robust_range(v: &[f32]) -> (f32, f32) {
    let mut s: Vec<f32> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return (0.0, 1.0);
    }
    let n = s.len();
    let lo_i = (n as f32 * 0.005) as usize;
    let hi_i = ((n as f32 * 0.995) as usize).min(n - 1);
    let lo = *s.select_nth_unstable_by(lo_i, f32::total_cmp).1;
    let hi = *s.select_nth_unstable_by(hi_i, f32::total_cmp).1;
    (lo, hi)
}

fn transpose(src: &[f32], w: usize, h: usize) -> Vec<f32> {
    let mut dst = vec![0f32; w * h];
    dst.par_chunks_mut(h).enumerate().for_each(|(x, col)| {
        for (y, v) in col.iter_mut().enumerate() {
            *v = src[y * w + x];
        }
    });
    dst
}

/// Apply a per-row operation horizontally then vertically (via transpose).
fn separable(data: &mut Vec<f32>, w: usize, h: usize, op: impl Fn(&mut [f32]) + Sync) {
    data.par_chunks_mut(w).for_each(&op);
    let mut t = transpose(data, w, h);
    t.par_chunks_mut(h).for_each(&op);
    *data = transpose(&t, h, w);
}

/// Grey-scale dilation (grows near regions) with a square window of the given radius.
fn max_filter(data: &mut Vec<f32>, w: usize, h: usize, r: usize) {
    separable(data, w, h, |row| {
        let src = row.to_vec();
        let n = src.len();
        for (i, out) in row.iter_mut().enumerate() {
            let a = i.saturating_sub(r);
            let b = (i + r).min(n - 1);
            *out = src[a..=b].iter().copied().fold(f32::MIN, f32::max);
        }
    });
}

/// Gaussian approximated by three box blurs (clamped edges).
pub fn gaussian_blur(data: &mut Vec<f32>, w: usize, h: usize, sigma: f32) {
    // Three boxes of radius r have variance 3·r(r+1)/3 = r(r+1) ≈ σ².
    let r = (((1.0 + 4.0 * sigma * sigma).sqrt() - 1.0) / 2.0).round().max(1.0) as usize;
    separable(data, w, h, |row| {
        let mut tmp = row.to_vec();
        for _ in 0..3 {
            box_blur_row(&tmp, row, r);
            tmp.copy_from_slice(row);
        }
    });
}

fn box_blur_row(src: &[f32], dst: &mut [f32], r: usize) {
    let n = src.len() as isize;
    let r = r as isize;
    let at = |i: isize| src[i.clamp(0, n - 1) as usize];
    let norm = 1.0 / (2 * r + 1) as f32;
    let mut sum: f32 = (-r..=r).map(at).sum();
    for i in 0..n {
        dst[i as usize] = sum * norm;
        sum += at(i + r + 1) - at(i - r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(w: u32, h: u32, f: impl Fn(u32, u32) -> f32) -> RawDepth {
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                data.push(f(x, y));
            }
        }
        RawDepth {
            width: w,
            height: h,
            data,
        }
    }

    #[test]
    fn normalises_to_unit_range() {
        let r = raw(32, 32, |x, _| x as f32 * 10.0 + 5.0);
        let s = DepthSettings {
            smoothing: 0.0,
            edge_dilation: 0.0,
            ..Default::default()
        };
        let d = prepare(&r, 64, 64, &s);
        let (mn, mx) = d
            .data
            .iter()
            .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
        assert!(mn <= 0.01 && mx >= 0.99, "{mn} {mx}");
        // Left = far (small raw value), right = near.
        assert!(d.data[0] < d.data[63]);
    }

    #[test]
    fn invert_flips() {
        let r = raw(16, 16, |x, _| x as f32);
        let s = DepthSettings {
            invert: true,
            smoothing: 0.0,
            edge_dilation: 0.0,
            ..Default::default()
        };
        let d = prepare(&r, 16, 16, &s);
        assert!(d.data[0] > d.data[15]);
    }

    #[test]
    fn dilation_grows_near_region() {
        let mut data = vec![0.0; 20];
        data[10] = 1.0;
        max_filter(&mut data, 20, 1, 2);
        assert_eq!(&data[8..13], &[1.0; 5]);
        assert_eq!(data[7], 0.0);
    }

    #[test]
    fn blur_preserves_constant() {
        let mut data = vec![0.7; 100];
        gaussian_blur(&mut data, 10, 10, 2.0);
        assert!(data.iter().all(|v| (v - 0.7).abs() < 1e-5));
    }
}
