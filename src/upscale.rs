//! Local AI upscaling with Real-ESRGAN (ONNX, built from the official weights).
//!
//! The image is processed in fixed-size overlapping tiles: memory use stays flat for any
//! photo size, the GPU sees the same tensor shape every time (fast), and the overlap
//! hides tile seams.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use image::RgbImage;
use ndarray::Array4;
use ort::session::Session;
use ort::value::Tensor;

use crate::depth::{Device, open_session};
use crate::models::UpscalerInfo;
use crate::settings::{UpscaleMode, UpscaleSettings};

/// Input pixels per tile (before overlap padding).
const TILE: u32 = 256;
/// Context pixels added on every side of a tile and thrown away afterwards.
const PAD: u32 = 16;

pub struct Upscaler {
    session: Session,
    pub info: UpscalerInfo,
    pub device: Device,
    pub gpu_note: Option<String>,
    input_name: String,
}

impl Upscaler {
    pub fn load(info: &UpscalerInfo, path: &Path, use_gpu: bool) -> Result<Self> {
        let (session, device, gpu_note) = open_session(path, use_gpu)?;
        let input_name = session
            .inputs()
            .first()
            .context("upscaler model has no inputs")?
            .name()
            .to_string();
        log::info!("loaded {} on {device}", info.name);
        Ok(Self {
            session,
            info: info.clone(),
            device,
            gpu_note,
            input_name,
        })
    }

    /// Upscale by the network's factor. `progress(done, total)` is called after each tile;
    /// returning `false` cancels.
    pub fn upscale(&mut self, img: &RgbImage, mut progress: impl FnMut(u32, u32) -> bool) -> Result<RgbImage> {
        let (w, h) = img.dimensions();
        let s = self.info.scale;
        let m = self.info.multiple.max(1);
        let span = (TILE + 2 * PAD).div_ceil(m) * m;
        let os = (span * s) as usize;
        let plane = (span * span) as usize;
        let mut out = RgbImage::new(w * s, h * s);
        let (nx, ny) = (w.div_ceil(TILE), h.div_ceil(TILE));
        let total = nx * ny;
        let input_name = self.input_name.clone();

        for ty in 0..ny {
            for tx in 0..nx {
                let (x0, y0) = (tx * TILE, ty * TILE);
                let (tw, th) = (TILE.min(w - x0), TILE.min(h - y0));

                // Patch with PAD context on each side; outside the photo, repeat the edge pixels.
                let mut data = vec![0f32; 3 * plane];
                for yy in 0..span {
                    let sy = (y0 as i64 - PAD as i64 + yy as i64).clamp(0, h as i64 - 1) as u32;
                    for xx in 0..span {
                        let sx = (x0 as i64 - PAD as i64 + xx as i64).clamp(0, w as i64 - 1) as u32;
                        let p = img.get_pixel(sx, sy).0;
                        let i = (yy * span + xx) as usize;
                        data[i] = p[0] as f32 / 255.0;
                        data[plane + i] = p[1] as f32 / 255.0;
                        data[2 * plane + i] = p[2] as f32 / 255.0;
                    }
                }
                let t = Tensor::from_array(Array4::from_shape_vec(
                    (1, 3, span as usize, span as usize),
                    data,
                )?)
                .map_err(|e| anyhow!("{e}"))?;
                let outputs = self
                    .session
                    .run(ort::inputs![input_name.clone() => t])
                    .map_err(|e| anyhow!("upscaling failed: {e}"))?;
                let (shape, y) = outputs[0]
                    .try_extract_tensor::<f32>()
                    .map_err(|e| anyhow!("{e}"))?;
                anyhow::ensure!(
                    shape.len() == 4 && shape[2] as usize == os && shape[3] as usize == os,
                    "unexpected upscaler output shape {shape:?}"
                );

                // Keep only the centre (drop the scaled-up padding).
                let oplane = os * os;
                let off = (PAD * s) as usize;
                for yy in 0..(th * s) as usize {
                    for xx in 0..(tw * s) as usize {
                        let i = (yy + off) * os + (xx + off);
                        let px = [
                            to_u8(y[i]),
                            to_u8(y[oplane + i]),
                            to_u8(y[2 * oplane + i]),
                        ];
                        out.put_pixel(x0 * s + xx as u32, y0 * s + yy as u32, image::Rgb(px));
                    }
                }
                anyhow::ensure!(progress(ty * nx + tx + 1, total), "cancelled");
            }
        }
        Ok(out)
    }
}

#[inline]
fn to_u8(v: f32) -> u8 {
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Does the user's policy call for upscaling a photo `width` pixels wide?
pub fn wanted(width: u32, s: &UpscaleSettings) -> bool {
    match s.mode {
        UpscaleMode::Off => false,
        UpscaleMode::Always => true,
        UpscaleMode::Auto => width < auto_threshold(s),
    }
}

/// In Auto mode, photos narrower than this are upscaled (two thirds of the target width).
pub fn auto_threshold(s: &UpscaleSettings) -> u32 {
    s.target_width * 2 / 3
}

/// Upscale, then bring the result down to the target width if the network overshot it.
pub fn apply(
    up: &mut Upscaler,
    img: &RgbImage,
    s: &UpscaleSettings,
    progress: impl FnMut(u32, u32) -> bool,
) -> Result<RgbImage> {
    let big = up.upscale(img, progress)?;
    let (bw, bh) = big.dimensions();
    if s.target_width > 0 && bw > s.target_width {
        let nh = ((bh as f64 * s.target_width as f64 / bw as f64).round() as u32).max(1);
        return Ok(image::imageops::resize(
            &big,
            s.target_width,
            nh,
            image::imageops::FilterType::Lanczos3,
        ));
    }
    Ok(big)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_only_upscales_small_photos() {
        let s = UpscaleSettings {
            mode: UpscaleMode::Auto,
            target_width: 3840,
            ..Default::default()
        };
        assert!(wanted(1200, &s));
        assert!(!wanted(3000, &s));
        let off = UpscaleSettings {
            mode: UpscaleMode::Off,
            ..s.clone()
        };
        assert!(!wanted(100, &off));
        let always = UpscaleSettings {
            mode: UpscaleMode::Always,
            ..s
        };
        assert!(wanted(9000, &always));
    }
}
