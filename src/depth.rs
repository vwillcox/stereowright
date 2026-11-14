//! Local AI depth estimation with ONNX Runtime (the GPU when available: CUDA on Linux,
//! DirectML on Windows, Core ML on macOS; otherwise the CPU).
//! Everything happens in this process; the model is a file on disk.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use half::f16;
use image::RgbImage;
use ndarray::Array4;
use ort::environment::Environment;
use ort::session::builder::{GraphOptimizationLevel, SessionBuilder};
use ort::session::{RunOptions, Session};
use ort::value::{Tensor, TensorElementType};
use rayon::prelude::*;

use crate::models::{ModelInfo, OutputKind};

/// Raw model output at model resolution, converted so that larger = nearer.
/// Not normalised: see [`crate::postprocess`].
#[derive(Clone, Debug)]
pub struct RawDepth {
    pub width: u32,
    pub height: u32,
    pub data: Vec<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    #[cfg_attr(not(feature = "cuda"), allow(dead_code))]
    Cuda,
    #[cfg_attr(not(feature = "directml"), allow(dead_code))]
    DirectMl,
    #[cfg_attr(not(feature = "coreml"), allow(dead_code))]
    CoreMl,
    Cpu,
}

impl std::fmt::Display for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Device::Cuda => "NVIDIA GPU (CUDA)",
            Device::DirectMl => "GPU (DirectML)",
            Device::CoreMl => "Apple GPU / Neural Engine (Core ML)",
            Device::Cpu => "CPU",
        })
    }
}

/// The GPU option's label in the app, for the backend this build has.
pub const GPU_LABEL: &str = if cfg!(feature = "cuda") {
    "Use NVIDIA GPU (CUDA)"
} else if cfg!(feature = "directml") {
    "Use the GPU (DirectML)"
} else if cfg!(feature = "coreml") {
    "Use the GPU (Core ML)"
} else {
    "Use the GPU (not in this build)"
};

/// Most image patches (tokens) the depth AI may look at in one go. Attention memory grows
/// with the *square* of this: 10 000 tokens is ~3.2 GB per attention buffer for DA-V2 Large
/// (fp16), which fits a 16 GB GPU. "Ultra" detail on a 16:9 photo would be ~14 500 tokens
/// (6.7 GB per buffer), more than the card has.
pub const MAX_TOKENS: u32 = 10_000;
/// Never step below this when the GPU runs out of memory (about "Fast" detail).
const MIN_TOKENS: u32 = 1_200;

pub struct DepthEstimator {
    session: Session,
    pub model: ModelInfo,
    pub device: Device,
    /// Why the GPU is not being used, if it was requested but unavailable.
    pub gpu_note: Option<String>,
    /// Set when GPU memory forced a lower AI resolution than asked for.
    pub memory_note: Option<String>,
    input_name: String,
    input_f16: bool,
    run_options: RunOptions,
    /// Plain run options (keep GPU memory between runs) for back-to-back work like video.
    steady_options: RunOptions,
    /// Give GPU memory back after each run (right for photos now and then; for video
    /// frames re-allocating every time is wasted work).
    pub release_memory: bool,
    /// Current token budget; lowered (and kept) if the GPU runs out of memory.
    token_budget: u32,
}

/// Run options that hand unused arena memory back after every run. Without this, ONNX
/// Runtime keeps its peak allocation for the life of the app (seen holding 12.5 GB).
pub(crate) fn shrinking_run_options(device: Device) -> Result<RunOptions> {
    let mut ro = RunOptions::new().map_err(|e| anyhow!("{e}"))?;
    // Only CUDA has a GPU memory arena to shrink; DirectML and Core ML manage their own.
    let arenas = match device {
        Device::Cuda => "gpu:0",
        Device::DirectMl | Device::CoreMl | Device::Cpu => "cpu:0",
    };
    ro.set("memory.enable_memory_arena_shrinkage", arenas)
        .map_err(|e| anyhow!("{e}"))?;
    Ok(ro)
}

/// Did this inference fail because GPU (or CPU) memory ran out?
fn is_out_of_memory(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}");
    ["Failed to allocate memory", "out of memory", "ALLOC_FAILED", "cudaErrorMemoryAllocation"]
        .iter()
        .any(|k| m.contains(k))
}

fn environment() -> Result<&'static Environment> {
    static ENV: OnceLock<Environment> = OnceLock::new();
    if let Some(e) = ENV.get() {
        return Ok(e);
    }
    let env = ort::init()
        .with_name("stereowright")
        // ONNX Runtime only has telemetry on Windows; switch it off explicitly anyway.
        .with_telemetry(false)
        .build()
        .map_err(|e| anyhow!("could not start ONNX Runtime: {e}"))?;
    Ok(ENV.get_or_init(|| env))
}

fn base_builder() -> Result<SessionBuilder> {
    let b = Session::builder(environment()?).map_err(|e| anyhow!("{e}"))?;
    let b = b
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .unwrap_or_else(|e| e.recover());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    Ok(b.with_intra_threads(threads).unwrap_or_else(|e| e.recover()))
}

/// Open an ONNX model on the GPU when asked and available, otherwise the CPU.
/// Returns the session, where it runs, and why the GPU isn't used (if it was requested).
pub(crate) fn open_session(path: &Path, use_gpu: bool) -> Result<(Session, Device, Option<String>)> {
    if use_gpu {
        match load_gpu(path) {
            Ok((s, device)) => return Ok((s, device, None)),
            Err(e) => {
                log::warn!("GPU unavailable, using CPU: {e:#}");
                return Ok((load_cpu(path)?, Device::Cpu, Some(format!("{e:#}"))));
            }
        }
    }
    Ok((load_cpu(path)?, Device::Cpu, None))
}

/// Try each GPU backend this build has, in order.
#[allow(unused_variables, unused_mut)]
fn load_gpu(path: &Path) -> Result<(Session, Device)> {
    let mut errors: Vec<String> = Vec::new();
    #[cfg(feature = "cuda")]
    match load_cuda(path) {
        Ok(s) => return Ok((s, Device::Cuda)),
        Err(e) => errors.push(format!("{e:#}")),
    }
    #[cfg(feature = "directml")]
    match load_directml(path) {
        Ok(s) => return Ok((s, Device::DirectMl)),
        Err(e) => errors.push(format!("{e:#}")),
    }
    #[cfg(feature = "coreml")]
    match load_coreml(path) {
        Ok(s) => return Ok((s, Device::CoreMl)),
        Err(e) => errors.push(format!("{e:#}")),
    }
    if errors.is_empty() {
        anyhow::bail!("built without GPU support");
    }
    Err(anyhow!(errors.join("; ")))
}

#[cfg(feature = "directml")]
fn load_directml(path: &Path) -> Result<Session> {
    use ort::ep;
    let dml = ep::DirectML::default().build().error_on_failure();
    // DirectML needs memory patterns off and sequential execution.
    let mut b = base_builder()?
        .with_memory_pattern(false)
        .unwrap_or_else(|e| e.recover())
        .with_parallel_execution(false)
        .unwrap_or_else(|e| e.recover())
        .with_execution_providers([dml])
        .map_err(|e| anyhow!("DirectML not available ({e}). Is the graphics driver up to date?"))?;
    b.commit_from_file(path)
        .map_err(|e| anyhow!("loading model on GPU failed: {e}"))
}

#[cfg(feature = "coreml")]
fn load_coreml(path: &Path) -> Result<Session> {
    use ort::ep::{
        self,
        coreml::{ComputeUnits, ModelFormat},
    };
    // Compiled Core ML models are cached so the next start is quick.
    let cache = crate::models::data_dir().join("coreml-cache");
    let _ = std::fs::create_dir_all(&cache);
    let coreml = ep::CoreML::default()
        .with_model_format(ModelFormat::MLProgram)
        .with_compute_units(ComputeUnits::All)
        .with_model_cache_dir(cache.display())
        .build()
        .error_on_failure();
    let mut b = base_builder()?
        .with_execution_providers([coreml])
        .map_err(|e| anyhow!("Core ML not available ({e})"))?;
    b.commit_from_file(path)
        .map_err(|e| anyhow!("loading model with Core ML failed: {e}"))
}

#[cfg(feature = "cuda")]
fn load_cuda(path: &Path) -> Result<Session> {
    use ort::ep::{self, ArenaExtendStrategy, cuda::ConvAlgorithmSearch};
    let cuda = ep::CUDA::default()
        // Exhaustive search re-benchmarks for every new image size; heuristic is near-identical and instant.
        .with_conv_algorithm_search(ConvAlgorithmSearch::Heuristic)
        // Grow GPU memory only by what is needed (the default doubles each time and never
        // gives memory back, which can hog most of a 16 GB card).
        .with_arena_extend_strategy(ArenaExtendStrategy::SameAsRequested)
        .build()
        .error_on_failure();
    let mut b = base_builder()?
        .with_execution_providers([cuda])
        .map_err(|e| anyhow!("CUDA not available ({e}). Is cuDNN 9 installed? (sudo pacman -S cudnn)"))?;
    b.commit_from_file(path)
        .map_err(|e| anyhow!("loading model on GPU failed: {e}"))
}

fn load_cpu(path: &Path) -> Result<Session> {
    base_builder()?
        .commit_from_file(path)
        .map_err(|e| anyhow!("loading model {} failed: {e}", path.display()))
}

impl DepthEstimator {
    pub fn load(model: &ModelInfo, path: &Path, use_gpu: bool) -> Result<Self> {
        let (session, device, gpu_note) = open_session(path, use_gpu)?;

        let input = session
            .inputs()
            .first()
            .context("model has no inputs")?;
        let input_name = input.name().to_string();
        let input_f16 = input.dtype().tensor_type() == Some(TensorElementType::Float16);
        log::info!(
            "loaded {} on {device} (input '{input_name}', {})",
            model.name,
            if input_f16 { "fp16" } else { "fp32" }
        );
        Ok(Self {
            session,
            model: model.clone(),
            device,
            gpu_note,
            memory_note: None,
            input_name,
            input_f16,
            run_options: shrinking_run_options(device)?,
            steady_options: RunOptions::new().map_err(|e| anyhow!("{e}"))?,
            release_memory: true,
            token_budget: MAX_TOKENS,
        })
    }

    /// Run the model. `detail` is the target short-side resolution fed to the network.
    /// If memory runs out, retries at a lower resolution (and remembers that budget).
    pub fn estimate(&mut self, img: &RgbImage, detail: u32) -> Result<RawDepth> {
        self.estimate_with(img, None, detail)
    }

    /// The network input size for a `w`×`h` picture (to resize ahead of time, see
    /// [`Self::estimate_with`]).
    pub fn input_size(&self, w: u32, h: u32, detail: u32) -> (u32, u32) {
        network_size(w, h, detail, self.model.patch.max(1), self.token_budget)
    }

    /// Resize a picture to the network input size (the same way `estimate` does).
    pub fn resize_for_network(img: &RgbImage, (tw, th): (u32, u32)) -> RgbImage {
        image::imageops::resize(img, tw, th, image::imageops::FilterType::CatmullRom)
    }

    /// [`Self::estimate`], with the picture optionally already resized to
    /// [`Self::input_size`] (video does that on another thread). A wrong size is ignored.
    pub fn estimate_with(&mut self, img: &RgbImage, resized: Option<&RgbImage>, detail: u32) -> Result<RawDepth> {
        let (w, h) = img.dimensions();
        let patch = self.model.patch.max(1);
        loop {
            let (tw, th) = network_size(w, h, detail, patch, self.token_budget);
            let own;
            let input = match resized {
                Some(r) if r.dimensions() == (tw, th) => r,
                _ => {
                    own = Self::resize_for_network(img, (tw, th));
                    &own
                }
            };
            match self.infer(input) {
                Ok(d) => return Ok(d),
                Err(e) if is_out_of_memory(&e) => {
                    let tokens = (tw / patch) * (th / patch);
                    if tokens <= MIN_TOKENS {
                        return Err(e.context("not enough GPU memory even at the lowest detail; close other GPU apps or untick the GPU option"));
                    }
                    self.token_budget = (tokens * 3 / 5).max(MIN_TOKENS);
                    let (nw, nh) = network_size(w, h, detail, patch, self.token_budget);
                    let note = format!(
                        "GPU memory is short: AI detail reduced to {} px (asked for {detail})",
                        nw.min(nh)
                    );
                    log::warn!("{note}");
                    self.memory_note = Some(note);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn infer(&mut self, resized: &RgbImage) -> Result<RawDepth> {
        let t0 = Instant::now();
        let (tw, th) = resized.dimensions();

        // HWC u8 -> NCHW normalised f32.
        let (mean, std) = (self.model.mean, self.model.std);
        let plane = (tw * th) as usize;
        let mut chw = vec![0f32; 3 * plane];
        let (r, rest) = chw.split_at_mut(plane);
        let (g, b) = rest.split_at_mut(plane);
        let px = resized.as_raw();
        r.par_iter_mut()
            .zip(g.par_iter_mut())
            .zip(b.par_iter_mut())
            .enumerate()
            .for_each(|(i, ((r, g), b))| {
                *r = (px[i * 3] as f32 / 255.0 - mean[0]) / std[0];
                *g = (px[i * 3 + 1] as f32 / 255.0 - mean[1]) / std[1];
                *b = (px[i * 3 + 2] as f32 / 255.0 - mean[2]) / std[2];
            });
        let shape = (1, 3, th as usize, tw as usize);

        let input_name = self.input_name.clone();
        let opts = if self.release_memory {
            &self.run_options
        } else {
            &self.steady_options
        };
        let outputs = if self.input_f16 {
            let data: Vec<f16> = chw.par_iter().map(|&v| f16::from_f32(v)).collect();
            let t = Tensor::from_array(Array4::from_shape_vec(shape, data)?)
                .map_err(|e| anyhow!("{e}"))?;
            self.session
                .run_with_options(ort::inputs![input_name => t], opts)
                .map_err(|e| anyhow!("inference failed: {e}"))?
        } else {
            let t = Tensor::from_array(Array4::from_shape_vec(shape, chw)?)
                .map_err(|e| anyhow!("{e}"))?;
            self.session
                .run_with_options(ort::inputs![input_name => t], opts)
                .map_err(|e| anyhow!("inference failed: {e}"))?
        };

        let out = &outputs[0];
        let (oh, ow, mut data) = match out.dtype().tensor_type() {
            Some(TensorElementType::Float16) => {
                let a = out.try_extract_array::<f16>().map_err(|e| anyhow!("{e}"))?;
                let (oh, ow) = last2(a.shape())?;
                (oh, ow, a.iter().map(|v| v.to_f32()).collect::<Vec<_>>())
            }
            _ => {
                let a = out.try_extract_array::<f32>().map_err(|e| anyhow!("{e}"))?;
                let (oh, ow) = last2(a.shape())?;
                (oh, ow, a.iter().copied().collect::<Vec<_>>())
            }
        };
        anyhow::ensure!(data.len() == oh * ow, "unexpected model output shape");

        if self.model.output == OutputKind::Depth {
            // Convert depth to disparity so "bigger = nearer" everywhere downstream.
            data.par_iter_mut().for_each(|v| *v = 1.0 / v.max(1e-3));
        }
        // Guard against NaN/inf from fp16 overflow.
        data.par_iter_mut().for_each(|v| {
            if !v.is_finite() {
                *v = 0.0;
            }
        });
        log::debug!(
            "depth {}x{} -> {ow}x{oh} in {:?} on {}",
            tw,
            th,
            t0.elapsed(),
            self.device
        );
        Ok(RawDepth {
            width: ow as u32,
            height: oh as u32,
            data,
        })
    }
}

fn last2(shape: &[usize]) -> Result<(usize, usize)> {
    anyhow::ensure!(shape.len() >= 2, "model output has rank {}", shape.len());
    Ok((shape[shape.len() - 2], shape[shape.len() - 1]))
}

/// Size the network input: short side ≈ `detail`, both sides multiples of `patch`,
/// aspect ratio preserved, long side capped so huge panoramas don't exhaust memory, and at
/// most `max_tokens` patches in total (attention memory grows with its square).
pub fn network_size(w: u32, h: u32, detail: u32, patch: u32, max_tokens: u32) -> (u32, u32) {
    let patch = patch.max(1) as f32;
    let short = w.min(h).max(1) as f32;
    let mut scale = detail as f32 / short;
    let long = w.max(h) as f32 * scale;
    let cap = detail as f32 * 3.0;
    if long > cap {
        scale *= cap / long;
    }
    let tokens = (w as f32 * scale / patch) * (h as f32 * scale / patch);
    if tokens > max_tokens as f32 {
        scale *= (max_tokens as f32 / tokens).sqrt();
        // Round down so the budget really holds.
        let floor = |v: f32| (((v * scale) / patch).floor().max(1.0) * patch) as u32;
        return (floor(w as f32), floor(h as f32));
    }
    let round = |v: f32| (((v * scale) / patch).round().max(1.0) * patch) as u32;
    (round(w as f32), round(h as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_size_is_patch_aligned() {
        let (w, h) = network_size(4000, 3000, 518, 14, MAX_TOKENS);
        assert_eq!(w % 14, 0);
        assert_eq!(h % 14, 0);
        assert_eq!(h, 518);
        assert!((w as f32 / h as f32 - 4.0 / 3.0).abs() < 0.05);
    }

    #[test]
    fn network_size_caps_panoramas() {
        let (w, h) = network_size(20000, 1000, 518, 14, MAX_TOKENS);
        assert!(w <= 518 * 3 + 14);
        assert!(h >= 14);
    }

    #[test]
    fn token_budget_caps_ultra_detail() {
        // Ultra on a 16:9 photo would be 91 x 162 = 14 742 patches (the 6.7 GB failure).
        let (w, h) = network_size(3840, 2160, 1274, 14, MAX_TOKENS);
        assert!((w / 14) * (h / 14) <= MAX_TOKENS);
        assert!((w as f32 / h as f32 - 16.0 / 9.0).abs() < 0.05);
        // Normal detail is untouched by the budget.
        assert_eq!(network_size(3000, 2000, 770, 14, MAX_TOKENS).1, 770);
    }

    #[test]
    fn recognises_out_of_memory() {
        let e = anyhow!("inference failed: ... BFCArena::AllocateRawInternal Failed to allocate memory for requested buffer of size 6700188928");
        assert!(is_out_of_memory(&e));
        assert!(!is_out_of_memory(&anyhow!("inference failed: shape mismatch")));
    }
}
