//! High-level "photo in, VR files out" pipeline shared by the CLI and the GUI.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage};

use crate::depth::{DepthEstimator, RawDepth};
use crate::export;
use crate::postprocess;
use crate::settings::{OutputSettings, Settings, UpscaleSettings};
use crate::stereo;
use crate::temporal::DepthStabilizer;
use crate::upscale::{self, Upscaler};
use crate::video::{self, Encoder, EncoderOptions, PixelFormat};

pub const IMAGE_EXTENSIONS: [&str; 7] = ["jpg", "jpeg", "png", "webp", "tif", "tiff", "bmp"];

/// Suffixes Stereowright writes; inputs with these are skipped so re-running on a folder
/// doesn't convert its own outputs.
const OUTPUT_SUFFIXES: [&str; 6] = ["_LR", "_3dh", "_TB", "_anaglyph", "_depth", "_mesh"];

/// Photo or video Stereowright can convert?
pub fn is_input(path: &Path) -> bool {
    is_image(path) || video::is_video(path)
}

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

fn is_own_output(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| OUTPUT_SUFFIXES.iter().any(|suf| s.ends_with(suf)))
}

/// Expand files and folders (non-recursive) into a sorted list of photos and videos.
pub fn collect_inputs(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut v: Vec<PathBuf> = std::fs::read_dir(p)
                .map(|rd| rd.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            v.retain(|f| f.is_file() && is_input(f) && !is_own_output(f));
            v.sort();
            out.extend(v);
        } else if p.is_file() && is_input(p) {
            out.push(p.clone());
        }
    }
    out.dedup();
    out
}

/// Load an image, honouring the EXIF orientation that phone cameras rely on.
pub fn load_image(path: &Path) -> Result<RgbImage> {
    let mut decoder = ImageReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()?
        .into_decoder()
        .with_context(|| format!("unsupported image {}", path.display()))?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder)
        .with_context(|| format!("decoding {}", path.display()))?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok(img.to_rgb8())
}

/// Downscale so the width is at most `max_w` (0 = unchanged).
pub fn fit_width(img: RgbImage, max_w: u32) -> RgbImage {
    let (w, h) = img.dimensions();
    if max_w == 0 || w <= max_w {
        return img;
    }
    let nh = ((h as f64 * max_w as f64 / w as f64).round() as u32).max(1);
    image::imageops::resize(&img, max_w, nh, image::imageops::FilterType::Lanczos3)
}

/// Downscale so neither side exceeds `max_side`.
pub fn fit_box(img: &RgbImage, max_side: u32) -> RgbImage {
    let (w, h) = img.dimensions();
    if w.max(h) <= max_side {
        return img.clone();
    }
    let s = max_side as f64 / w.max(h) as f64;
    let (nw, nh) = (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1));
    image::imageops::resize(img, nw, nh, image::imageops::FilterType::Triangle)
}

pub fn output_dir(input: &Path, o: &OutputSettings) -> PathBuf {
    match &o.out_dir {
        Some(d) => d.clone(),
        None => input
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("stereowright"),
    }
}

/// The upscale target actually in force: the user's target, capped by "max width".
pub fn effective_upscale(s: &Settings) -> UpscaleSettings {
    let mut u = s.upscale.clone();
    if s.output.max_width > 0 && (u.target_width == 0 || s.output.max_width < u.target_width) {
        u.target_width = s.output.max_width;
    }
    u
}

/// Does this photo (of the given width) get AI-upscaled with these settings?
pub fn wants_upscale(width: u32, s: &Settings) -> bool {
    let u = effective_upscale(s);
    upscale::wanted(width, &u) && (u.target_width == 0 || width < u.target_width)
}

/// Load an image and make it the size the 3D conversion works at: AI-upscaled when the
/// settings call for it and an upscaler is given, then capped at "max width".
pub fn prepare_image(
    input: &Path,
    up: Option<&mut Upscaler>,
    s: &Settings,
    progress: impl FnMut(u32, u32) -> bool,
) -> Result<RgbImage> {
    let img = load_image(input)?;
    let img = match up {
        Some(up) if wants_upscale(img.width(), s) => {
            upscale::apply(up, &img, &effective_upscale(s), progress)?
        }
        _ => img,
    };
    Ok(fit_width(img, s.output.max_width))
}

/// Convert one image file with the given settings. Returns the files written.
pub fn convert_file(
    est: &mut DepthEstimator,
    up: Option<&mut Upscaler>,
    input: &Path,
    s: &Settings,
) -> Result<Vec<PathBuf>> {
    let img = prepare_image(input, up, s, |_, _| true)?;
    let raw = est.estimate(&img, s.depth.detail)?;
    render_outputs(&img, &raw, s, &output_dir(input, &s.output), &output_stem(input))
}

/// Base name for an input's output files.
pub fn output_stem(input: &Path) -> String {
    input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image")
        .to_string()
}

/// Produce every enabled output for an image whose depth has already been estimated.
pub fn render_outputs(
    img: &RgbImage,
    raw: &RawDepth,
    s: &Settings,
    dir: &Path,
    stem: &str,
) -> Result<Vec<PathBuf>> {
    let o = &s.output;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let (w, h) = img.dimensions();
    let depth = postprocess::prepare(raw, w, h, &s.depth);
    let ext = o.format.ext();
    let mut written = Vec::new();

    let needs_pair = o.sbs_full || o.sbs_half || o.top_bottom || o.anaglyph;
    if needs_pair {
        let pair = stereo::synthesize(img, &depth, &s.stereo);
        let mut save = |name: String, im: RgbImage| -> Result<()> {
            let p = dir.join(name);
            export::save_rgb(&im, &p, o)?;
            written.push(p);
            Ok(())
        };
        if o.sbs_full {
            save(format!("{stem}_LR.{ext}"), export::sbs_full(&pair))?;
        }
        if o.sbs_half {
            save(format!("{stem}_3dh.{ext}"), export::sbs_half(&pair))?;
        }
        if o.top_bottom {
            save(format!("{stem}_TB.{ext}"), export::top_bottom(&pair))?;
        }
        if o.anaglyph {
            save(format!("{stem}_anaglyph.{ext}"), export::anaglyph(&pair))?;
        }
    }
    if o.depth_png {
        let p = dir.join(format!("{stem}_depth.png"));
        export::save_depth_png(&depth, &p)?;
        written.push(p);
    }
    if o.glb_mesh {
        let p = dir.join(format!("{stem}_mesh.glb"));
        export::write_glb(img, &depth, o, &p)?;
        written.push(p);
    }
    Ok(written)
}

/// Returned (as the error) when a conversion was stopped on request.
#[derive(Debug)]
pub struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stopped")
    }
}

impl std::error::Error for Stopped {}

/// Does any of the chosen outputs work for video? (The .glb mesh is photo-only.)
pub fn has_video_output(o: &OutputSettings) -> bool {
    o.sbs_full || o.sbs_half || o.top_bottom || o.anaglyph || o.depth_png
}

/// Size a video is converted at (each eye).
pub fn video_size(info: &video::VideoInfo, s: &Settings) -> (u32, u32) {
    video::work_size(info, s.video.max_width)
}

#[derive(Clone, Copy)]
enum Layout {
    Sbs,
    HalfSbs,
    TopBottom,
    Anaglyph,
    Depth,
}

/// Convert a video: depth per frame (steadied over time), a 3D view per frame, and one
/// MP4 per chosen output, with the original sound. `progress(frames_done, frames_total)`
/// is called before each frame; returning false stops (the error is then [`Stopped`]).
pub fn convert_video(
    est: &mut DepthEstimator,
    input: &Path,
    s: &Settings,
    mut progress: impl FnMut(u64, u64) -> bool,
) -> Result<Vec<PathBuf>> {
    let o = &s.output;
    anyhow::ensure!(
        has_video_output(o),
        "pick a format that works for video (side-by-side, top/bottom, anaglyph or depth)"
    );
    let info = video::probe(input)?;
    let (w, h) = video_size(&info, s);
    let dir = output_dir(input, o);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let stem = output_stem(input);

    let mut outs: Vec<(Layout, Encoder)> = Vec::new();
    let mut add = |on: bool, layout: Layout, suffix: &str, size: (u32, u32), fmt: PixelFormat| -> Result<()> {
        if on {
            let opts = EncoderOptions {
                width: size.0,
                height: size.1,
                format: fmt,
                info: &info,
                audio_from: matches!(fmt, PixelFormat::Rgb).then_some(input),
                quality: s.video.quality,
            };
            outs.push((layout, Encoder::create(&dir.join(format!("{stem}{suffix}.mp4")), &opts)?));
        }
        Ok(())
    };
    add(o.sbs_full, Layout::Sbs, "_LR", (w * 2, h), PixelFormat::Rgb)?;
    add(o.sbs_half, Layout::HalfSbs, "_3dh", (w, h), PixelFormat::Rgb)?;
    add(o.top_bottom, Layout::TopBottom, "_TB", (w, h * 2), PixelFormat::Rgb)?;
    add(o.anaglyph, Layout::Anaglyph, "_anaglyph", (w, h), PixelFormat::Rgb)?;
    add(o.depth_png, Layout::Depth, "_depth", (w, h), PixelFormat::Gray)?;

    let mut decoder = video::Decoder::open(input, &info, w, h)?;
    let done = AtomicU64::new(0);
    let abort = AtomicBool::new(false);
    let total = info.frames;

    // Keep the AI's GPU memory between frames (given back once the video is done).
    est.release_memory = false;
    let result = std::thread::scope(|scope| {
        // Depth AI on this thread (GPU); 3D rendering and encoding on another (CPU), so
        // the two overlap.
        let (tx, rx) = mpsc::sync_channel::<(RgbImage, RawDepth)>(2);
        let (done, abort) = (&done, &abort);
        let writer = scope.spawn(move || -> Result<Vec<PathBuf>> {
            let mut steady = DepthStabilizer::new(s.video.steady, info.fps());
            let needs_pair = outs.iter().any(|(l, _)| !matches!(l, Layout::Depth));
            let mut times = [0f64; 4];
            let mut lap = |i: usize, t: &mut Instant| {
                times[i] += t.elapsed().as_secs_f64();
                *t = Instant::now();
            };
            for (frame, raw) in rx {
                if abort.load(Ordering::Relaxed) {
                    return Err(Stopped.into()); // dropping the encoders deletes the parts
                }
                let mut t = Instant::now();
                let d = steady.push(&frame, &raw);
                lap(0, &mut t);
                let depth = postprocess::prepare_in_range(&d, (0.0, 1.0), w, h, &s.depth);
                lap(1, &mut t);
                let pair = needs_pair.then(|| stereo::synthesize(&frame, &depth, &s.stereo));
                lap(2, &mut t);
                for (layout, enc) in outs.iter_mut() {
                    let pair = pair.as_ref();
                    match layout {
                        Layout::Sbs => enc.write(export::sbs_full(pair.expect("pair")).as_raw())?,
                        Layout::HalfSbs => enc.write(export::sbs_half(pair.expect("pair")).as_raw())?,
                        Layout::TopBottom => enc.write(export::top_bottom(pair.expect("pair")).as_raw())?,
                        Layout::Anaglyph => enc.write(export::anaglyph(pair.expect("pair")).as_raw())?,
                        Layout::Depth => enc.write(depth.to_gray8().as_raw())?,
                    }
                }
                lap(3, &mut t);
                done.fetch_add(1, Ordering::Relaxed);
            }
            let n = done.load(Ordering::Relaxed).max(1) as f64 / 1000.0;
            log::debug!(
                "video 3D per frame: steady {:.0} ms, depth map {:.0} ms, stereo {:.0} ms, encode {:.0} ms",
                times[0] / n, times[1] / n, times[2] / n, times[3] / n
            );
            if abort.load(Ordering::Relaxed) {
                return Err(Stopped.into());
            }
            anyhow::ensure!(done.load(Ordering::Relaxed) > 0, "the video has no frames");
            if steady.scene_cuts > 0 {
                log::debug!("{}: {} scene cut(s)", stem, steady.scene_cuts);
            }
            outs.into_iter().map(|(_, e)| e.finish()).collect()
        });

        // Decoding and resizing for the AI on a third thread, overlapping the AI too.
        let (ftx, frx) = mpsc::sync_channel::<(RgbImage, RgbImage)>(2);
        let net_size = est.input_size(w, h, s.depth.detail);
        let reader = scope.spawn(move || -> Result<()> {
            while let Some(frame) = decoder.next_frame()? {
                let small = DepthEstimator::resize_for_network(&frame, net_size);
                if ftx.send((frame, small)).is_err() || abort.load(Ordering::Relaxed) {
                    break;
                }
            }
            Ok(())
        });

        let (mut t_ai, mut t_wait) = (0f64, 0f64);
        let read = (|| -> Result<bool> {
            for (frame, small) in frx.iter() {
                if !progress(done.load(Ordering::Relaxed), total) {
                    return Ok(false);
                }
                let t = Instant::now();
                let raw = est.estimate_with(&frame, Some(&small), s.depth.detail)?;
                t_ai += t.elapsed().as_secs_f64();
                let t = Instant::now();
                let sent = tx.send((frame, raw));
                t_wait += t.elapsed().as_secs_f64();
                if sent.is_err() {
                    return Ok(true); // the writer failed; its error says why
                }
            }
            Ok(true)
        })();
        drop(frx);
        let read = match (read, reader.join().map_err(|_| anyhow!("the video reader crashed"))?) {
            (Ok(true), Err(e)) => Err(e), // decoding failed
            (r, _) => r,
        };
        drop(tx);
        let n = done.load(Ordering::Relaxed).max(1) as f64 / 1000.0;
        log::debug!(
            "video AI per frame: depth AI {:.0} ms, waiting for 3D {:.0} ms",
            t_ai / n, t_wait / n
        );
        if !matches!(read, Ok(true)) {
            abort.store(true, Ordering::Relaxed);
        }
        let written = writer.join().map_err(|_| anyhow!("the video writer crashed"))?;
        match read {
            Err(e) => Err(e),
            Ok(false) => Err(Stopped.into()),
            Ok(true) => {
                progress(total, total);
                written
            }
        }
    });
    est.release_memory = true;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_own_outputs() {
        assert!(is_own_output(Path::new("a/photo_LR.jpg")));
        assert!(is_own_output(Path::new("a/photo_depth.png")));
        assert!(!is_own_output(Path::new("a/photo.jpg")));
        assert!(is_image(Path::new("x.JPG")));
        assert!(!is_image(Path::new("x.txt")));
    }

    #[test]
    fn max_width_caps_upscaling() {
        let mut s = Settings::default(); // Auto, target 3840
        assert!(wants_upscale(1200, &s));
        assert!(!wants_upscale(3000, &s));
        s.output.max_width = 1000;
        assert!(!wants_upscale(1200, &s)); // already wider than the cap
        assert_eq!(effective_upscale(&s).target_width, 1000);
        s.upscale.mode = crate::settings::UpscaleMode::Always;
        assert!(wants_upscale(600, &s));
        assert!(!wants_upscale(1000, &s));
    }
}
