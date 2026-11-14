//! Steady depth for video.
//!
//! The depth AI looks at each frame on its own, and its answer is only "relative": every
//! frame gets its own arbitrary scale and offset. Normalising each frame by its own
//! nearest/farthest points (as photos do) makes the 3D "breathe" whenever those extremes
//! wobble, and small patches flicker. Two fixes, both scaled by one "steady" strength:
//!
//! 1. Each frame is lined up with the previous result by a least-squares scale + offset
//!    fit over the parts of the picture that didn't move, so the whole scene anchors the
//!    depth scale rather than a few extreme pixels. The alignment slowly lets go, so after
//!    the scene changes the 3D range re-fills within about a second.
//! 2. Each pixel's depth is blended with the previous frame's, but only where the picture
//!    itself is still. Where something moves the new depth is used straight away, so moving
//!    people don't drag a ghost of their old depth behind them.
//!
//! A scene cut (the picture changes almost everywhere) starts afresh.

use rayon::prelude::*;

use crate::depth::RawDepth;
use crate::postprocess::{gaussian_blur, robust_range};

/// Average picture change (0..1 luma) above which a frame counts as a new scene.
const SCENE_CUT: f32 = 0.14;
/// Per-pixel change below this is treated as noise (still), above `MOVING` as motion.
const STILL: f32 = 0.025;
const MOVING: f32 = 0.10;

pub struct DepthStabilizer {
    /// Frames of memory for the depth scale and for still pixels (1 = none).
    range_tau: f32,
    pixel_tau: f32,
    prev: Option<Vec<f32>>,
    prev_luma: Option<Vec<f32>>,
    dims: (u32, u32),
    pub scene_cuts: u32,
}

impl DepthStabilizer {
    /// `steady` 0 (off) … 1 (strongest); `fps` so the smoothing lasts the same *time*
    /// whatever the frame rate.
    pub fn new(steady: f32, fps: f64) -> Self {
        let s = steady.clamp(0.0, 1.0);
        let per_30 = (fps as f32 / 30.0).clamp(0.25, 8.0);
        Self {
            // ~1/3 s of memory for the scale at the default 0.5, ~1 s at full strength.
            range_tau: 1.0 + 30.0 * s.powf(1.6) * per_30,
            // Lighter for pixels: more would smear depth on slowly moving things.
            pixel_tau: 1.0 + 4.0 * s.powf(1.6) * per_30,
            prev: None,
            prev_luma: None,
            dims: (0, 0),
            scene_cuts: 0,
        }
    }

    /// Take this frame's raw AI depth and its picture; returns steadied depth, same size as
    /// `raw`, normalised so 0..1 is far..near (values may stray slightly outside).
    pub fn push(&mut self, frame: &image::RgbImage, raw: &RawDepth) -> RawDepth {
        let (w, h) = (raw.width, raw.height);
        let luma = small_luma(frame, w, h);
        let (lo, hi) = robust_range(&raw.data);
        let span = (hi - lo).max(1e-6);
        let mut cur: Vec<f32> = raw.data.par_iter().map(|v| (v - lo) / span).collect();

        // Nothing to carry over after a change of AI size (e.g. reduced after running short
        // of GPU memory), or when steadying is off.
        let history = match (&self.prev, &self.prev_luma) {
            (Some(p), Some(l)) if self.dims == (w, h) && self.range_tau > 1.0 => Some((p, l)),
            _ => None,
        };
        if let Some((prev, prev_luma)) = history {
            let change: Vec<f32> = luma.par_iter().zip(prev_luma).map(|(a, b)| (a - b).abs()).collect();
            let mean = change.par_iter().sum::<f32>() / change.len().max(1) as f32;
            if mean > SCENE_CUT {
                self.scene_cuts += 1;
            } else {
                let moving: Vec<f32> = change.par_iter().map(|&c| smoothstep(STILL, MOVING, c)).collect();
                // 1. Line up with the previous result, letting go a little each frame.
                let (a, b) = fit(&cur, prev, &moving);
                let leak = 1.0 / self.range_tau;
                let (a, b) = (a + (1.0 - a) * leak, b * (1.0 - leak));
                // 2. Blend still pixels over time; moving ones take the new depth.
                let still_k = 1.0 / self.pixel_tau;
                cur.par_iter_mut()
                    .zip(prev.par_iter())
                    .zip(moving.par_iter())
                    .for_each(|((c, &p), &m)| {
                        let aligned = a * *c + b;
                        let k = still_k + (1.0 - still_k) * m;
                        *c = p + k * (aligned - p);
                    });
            }
        }
        self.dims = (w, h);
        self.prev = Some(cur.clone());
        self.prev_luma = Some(luma);
        RawDepth {
            width: w,
            height: h,
            data: cur,
        }
    }
}

/// Weighted least squares `a·x + b ≈ y`, weighting still pixels (1 - moving).
fn fit(x: &[f32], y: &[f32], moving: &[f32]) -> (f32, f32) {
    let zero = || (0f64, 0f64, 0f64, 0f64, 0f64);
    let (sw, sx, sy, sxx, sxy) = x
        .par_iter()
        .zip(y.par_iter())
        .zip(moving.par_iter())
        .map(|((&x, &y), &m)| {
            let (w, x, y) = (1.0 - m as f64, x as f64, y as f64);
            (w, w * x, w * y, w * x * x, w * x * y)
        })
        .reduce(zero, |p, q| (p.0 + q.0, p.1 + q.1, p.2 + q.2, p.3 + q.3, p.4 + q.4));
    let den = sw * sxx - sx * sx;
    // Almost everything moving, or a flat depth map: nothing reliable to align to.
    if sw < 0.05 * x.len() as f64 || den.abs() < 1e-9 * sw * sw {
        return (1.0, 0.0);
    }
    let a = ((sw * sxy - sx * sy) / den).clamp(0.5, 2.0);
    let b = (sy - a * sx) / sw;
    (a as f32, b as f32)
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The frame's brightness at the depth map's size, lightly blurred so sensor noise and
/// compression shimmer don't count as motion.
fn small_luma(frame: &image::RgbImage, w: u32, h: u32) -> Vec<f32> {
    let (fw, fh) = frame.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    let px = frame.as_raw();
    let mut out = vec![0f32; wu * hu];
    out.par_chunks_mut(wu).enumerate().for_each(|(y, row)| {
        // Average a small block of source pixels per output pixel.
        let y0 = (y as u64 * fh as u64 / h as u64) as u32;
        let y1 = (((y + 1) as u64 * fh as u64 / h as u64) as u32).clamp(y0 + 1, fh);
        for (x, o) in row.iter_mut().enumerate() {
            let x0 = (x as u64 * fw as u64 / w as u64) as u32;
            let x1 = (((x + 1) as u64 * fw as u64 / w as u64) as u32).clamp(x0 + 1, fw);
            let (mut sum, mut n) = (0u32, 0u32);
            let step = ((y1 - y0).max(x1 - x0) / 4).max(1) as usize;
            for yy in (y0..y1).step_by(step) {
                for xx in (x0..x1).step_by(step) {
                    let i = (yy as usize * fw as usize + xx as usize) * 3;
                    sum += 54 * px[i] as u32 + 183 * px[i + 1] as u32 + 19 * px[i + 2] as u32;
                    n += 1;
                }
            }
            *o = sum as f32 / (n.max(1) as f32 * 256.0 * 255.0);
        }
    });
    gaussian_blur(&mut out, wu, hu, 1.0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(w: u32, h: u32, f: impl Fn(u32, u32) -> f32) -> RawDepth {
        RawDepth {
            width: w,
            height: h,
            data: (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| f(x, y)).collect(),
        }
    }

    #[test]
    fn calms_breathing_but_follows_motion() {
        let (w, h) = (64u32, 48u32);
        let still = image::RgbImage::from_fn(w, h, |x, _| image::Rgb([(x * 4) as u8; 3]));
        // A ramp, plus a small near object (2 % of pixels) whose AI depth jumps around:
        // per-frame normalisation would rescale the whole scene by up to 30 % each frame.
        let scene = |near: f32| raw(w, h, move |x, y| if x < 4 && y < 16 { near } else { x as f32 });
        let probe = (h / 2 * w + 40) as usize;
        let per_frame = |near: f32| 40.0 / near;
        assert!((per_frame(70.0) - per_frame(100.0)).abs() > 0.15);

        let mut st = DepthStabilizer::new(0.5, 30.0);
        let mut outs = Vec::new();
        for i in 0..30 {
            let d = st.push(&still, &scene(if i % 2 == 0 { 70.0 } else { 100.0 }));
            outs.push(d.data[probe]);
        }
        let swing = (outs[28] - outs[29]).abs();
        assert!(swing < 0.05, "scene still breathes: {swing}");

        // Something moves into the top-right corner (1/16 of the picture, so not a scene
        // cut): its depth must switch over at once, not fade in.
        let inside = move |x: u32, y: u32| x >= 48 && y < 12;
        let moved = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([if inside(x, y) { 0 } else { (x * 4) as u8 }; 3])
        });
        let before = st.push(&still, &raw(w, h, |x, _| x as f32));
        let after = st.push(&moved, &raw(w, h, |x, y| if inside(x, y) { 0.0 } else { x as f32 }));
        let i = (6 * w + 60) as usize;
        assert!(before.data[i] > 0.5, "{}", before.data[i]);
        // Its new AI depth (0) is the far end of the scene, so it must land where a still
        // pixel with that depth sits (the steadied scale need not start exactly at 0).
        let far = after.data[(h / 2 * w) as usize];
        assert!(
            (after.data[i] - far).abs() < 0.05,
            "moving area should take the new depth: {} vs far {far}",
            after.data[i]
        );
        // ...while the still rest keeps its depth.
        let j = (h / 2 * w + 20) as usize;
        assert!((after.data[j] - before.data[j]).abs() < 0.05);
        assert_eq!(st.scene_cuts, 0);
    }

    #[test]
    fn off_means_per_frame_and_cuts_reset() {
        let (w, h) = (16u32, 8u32);
        let img = image::RgbImage::new(w, h);
        let mut st = DepthStabilizer::new(0.0, 30.0);
        st.push(&img, &raw(w, h, |x, _| x as f32));
        let d = st.push(&img, &raw(w, h, |x, _| 3.0 * x as f32 + 5.0));
        let (lo, hi) = robust_range(&d.data);
        assert!(lo.abs() < 0.1 && (hi - 1.0).abs() < 0.1, "{lo} {hi}");

        let mut st = DepthStabilizer::new(1.0, 30.0);
        st.push(&img, &raw(w, h, |x, _| x as f32));
        let white = image::RgbImage::from_pixel(w, h, image::Rgb([255; 3]));
        let d = st.push(&white, &raw(w, h, |x, _| -(x as f32)));
        assert_eq!(st.scene_cuts, 1);
        assert!(d.data[0] > 0.9, "a new scene starts from its own depth");
    }
}
