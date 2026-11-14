//! Background thread that owns the AI model, so the window never freezes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Instant, SystemTime};

use anyhow::Result;
use image::RgbImage;

use crate::depth::{DepthEstimator, Device, RawDepth};
use crate::models::{Registry, UpscalerInfo};
use crate::pipeline;
use crate::settings::{Settings, UpscaleMode, UpscaleSettings};
use crate::upscale::{self, Upscaler};

/// Longest side of the in-app preview image.
pub const PREVIEW_MAX: u32 = 1600;

pub enum Request {
    Preview {
        generation: u64,
        path: PathBuf,
        settings: Box<Settings>,
    },
    Export {
        jobs: Vec<(usize, PathBuf)>,
        settings: Box<Settings>,
    },
    /// Free the AI models and stop (sent when the window closes).
    Shutdown,
}

/// Lets the GUI stop the worker cleanly. The AI sessions hold CUDA/cuDNN state inside
/// ONNX Runtime; they must be freed *before* the process exits, or their teardown can race
/// with C++ global destructors running in `exit()` (seen as a SIGSEGV in `cudnnDestroy`).
pub struct Handle {
    cancel: Arc<AtomicBool>,
    done: Receiver<()>,
}

impl Handle {
    /// Cancel running work, ask the worker to free its models, and wait (bounded).
    pub fn shutdown(&self, tx: &Sender<Request>) {
        self.cancel.store(true, Ordering::Relaxed);
        if tx.send(Request::Shutdown).is_ok()
            && self.done.recv_timeout(std::time::Duration::from_secs(20)).is_err()
        {
            log::warn!("AI worker did not stop in time");
        }
    }
}

pub enum Response {
    Status(String),
    ModelReady {
        name: String,
        device: Device,
        gpu_note: Option<String>,
    },
    Preview {
        generation: u64,
        image: Arc<RgbImage>,
        raw: Arc<RawDepth>,
        full_size: (u32, u32),
        /// Original size, when the photo was AI-upscaled.
        upscaled_from: Option<(u32, u32)>,
        infer_ms: u64,
        /// Set when GPU memory forced a lower AI resolution.
        memory_note: Option<String>,
    },
    PreviewFailed {
        generation: u64,
        error: String,
    },
    ExportItem {
        index: usize,
        result: Result<Vec<PathBuf>, String>,
        secs: f32,
    },
    ExportDone {
        ok: usize,
        failed: usize,
    },
}

pub fn spawn(ctx: eframe::egui::Context) -> (Sender<Request>, Receiver<Response>, Handle) {
    let (req_tx, req_rx) = mpsc::channel::<Request>();
    let (res_tx, res_rx) = mpsc::channel::<Response>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    std::thread::Builder::new()
        .name("depth-worker".into())
        .spawn(move || {
            let mut w = Worker {
                tx: res_tx,
                ctx,
                cancel: worker_cancel,
                loaded: None,
                upscaler: None,
                prepared: None,
            };
            w.run(req_rx);
            // Free the GPU sessions here, while the process is still fully alive.
            w.upscaler = None;
            w.loaded = None;
            log::debug!("AI worker stopped; models freed");
            let _ = done_tx.send(());
        })
        .expect("spawn worker thread");
    (req_tx, res_rx, Handle { cancel, done: done_rx })
}

struct Loaded {
    id: String,
    use_gpu: bool,
    est: DepthEstimator,
}

struct LoadedUpscaler {
    id: String,
    use_gpu: bool,
    up: Upscaler,
}

/// Identifies one AI-upscaled photo, so it is only upscaled once.
#[derive(Clone, PartialEq)]
struct PrepKey {
    path: PathBuf,
    modified: Option<SystemTime>,
    upscaler: String,
    upscale: UpscaleSettings,
    max_width: u32,
}

/// A photo at the size the 3D conversion works at.
#[derive(Clone)]
struct Prepared {
    image: Arc<RgbImage>,
    upscaled_from: Option<(u32, u32)>,
}

struct Worker {
    tx: Sender<Response>,
    ctx: eframe::egui::Context,
    cancel: Arc<AtomicBool>,
    loaded: Option<Loaded>,
    upscaler: Option<LoadedUpscaler>,
    /// The last upscaled photo (upscaling takes seconds; depth tweaks shouldn't redo it).
    prepared: Option<(PrepKey, Prepared)>,
}

impl Worker {
    fn send(&self, r: Response) {
        let _ = self.tx.send(r);
        self.ctx.request_repaint();
    }

    fn run(&mut self, rx: Receiver<Request>) {
        while let Ok(first) = rx.recv() {
            // If the user flicked through several images, only preview the newest one.
            let mut batch = vec![first];
            batch.extend(rx.try_iter());
            let last_preview = batch
                .iter()
                .rposition(|r| matches!(r, Request::Preview { .. }));
            for (i, req) in batch.into_iter().enumerate() {
                match req {
                    Request::Preview { .. } if Some(i) != last_preview => {}
                    Request::Preview {
                        generation,
                        path,
                        settings,
                    } => {
                        let msg = match self.preview(generation, &path, &settings) {
                            Ok(r) => r,
                            Err(e) => Response::PreviewFailed {
                                generation,
                                error: format!("{e:#}"),
                            },
                        };
                        self.send(msg);
                    }
                    Request::Export { jobs, settings } => self.export(jobs, &settings),
                    Request::Shutdown => return,
                }
            }
        }
    }

    fn estimator(&mut self, id: &str, use_gpu: bool) -> Result<&mut DepthEstimator> {
        let reg = Registry::discover(); // cheap; picks up models downloaded while the app is open
        let model = reg.resolve(id)?.clone();
        let reuse = self
            .loaded
            .as_ref()
            .is_some_and(|l| l.id == model.id && l.use_gpu == use_gpu);
        if !reuse {
            self.loaded = None; // free GPU memory before loading the next model
            self.send(Response::Status(format!("Loading {}…", model.name)));
            let est = DepthEstimator::load(&model, &reg.path(&model), use_gpu)?;
            self.send(Response::ModelReady {
                name: model.name.clone(),
                device: est.device,
                gpu_note: est.gpu_note.clone(),
            });
            self.loaded = Some(Loaded {
                id: model.id.clone(),
                use_gpu,
                est,
            });
        }
        Ok(&mut self.loaded.as_mut().expect("just loaded").est)
    }

    /// The upscaler to use with these settings, if upscaling is on and one is available.
    /// In Auto mode a missing upscaler just means "don't upscale".
    fn upscaler_info(s: &Settings) -> Result<Option<(Registry, UpscalerInfo)>> {
        if s.upscale.mode == UpscaleMode::Off {
            return Ok(None);
        }
        let reg = Registry::discover();
        match reg.resolve_upscaler(&s.upscale.model) {
            Ok(info) => {
                let info = info.clone();
                Ok(Some((reg, info)))
            }
            Err(_) if s.upscale.mode == UpscaleMode::Auto && s.upscale.model.is_empty() => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn upscaler(&mut self, reg: &Registry, info: &UpscalerInfo, use_gpu: bool) -> Result<&mut Upscaler> {
        let reuse = self
            .upscaler
            .as_ref()
            .is_some_and(|l| l.id == info.id && l.use_gpu == use_gpu);
        if !reuse {
            self.upscaler = None;
            self.send(Response::Status(format!("Loading {}…", info.name)));
            let up = Upscaler::load(info, &reg.upscaler_path(info), use_gpu)?;
            self.upscaler = Some(LoadedUpscaler {
                id: info.id.clone(),
                use_gpu,
                up,
            });
        }
        Ok(&mut self.upscaler.as_mut().expect("just loaded").up)
    }

    /// Load a photo and, if the settings call for it, AI-upscale it (cached).
    fn prepare(&mut self, path: &Path, s: &Settings) -> Result<Prepared> {
        let Some((reg, info)) = Self::upscaler_info(s)? else {
            let img = pipeline::fit_width(pipeline::load_image(path)?, s.output.max_width);
            return Ok(Prepared {
                image: Arc::new(img),
                upscaled_from: None,
            });
        };
        let key = PrepKey {
            path: path.to_path_buf(),
            modified: std::fs::metadata(path).and_then(|m| m.modified()).ok(),
            upscaler: info.id.clone(),
            upscale: pipeline::effective_upscale(s),
            max_width: s.output.max_width,
        };
        if let Some((k, p)) = &self.prepared
            && *k == key
        {
            return Ok(p.clone());
        }
        let img = pipeline::load_image(path)?;
        let from = img.dimensions();
        if !pipeline::wants_upscale(from.0, s) {
            return Ok(Prepared {
                image: Arc::new(pipeline::fit_width(img, s.output.max_width)),
                upscaled_from: None,
            });
        }
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let scale = info.scale;
        let target = key.upscale.clone();
        let cancel = self.cancel.clone();
        let up = self.upscaler(&reg, &info, s.use_gpu)?;
        let big = upscale::apply(up, &img, &target, |done, total| {
            let _ = tx.send(Response::Status(format!(
                "AI upscaling {name} ×{scale}… {}%",
                done * 100 / total.max(1)
            )));
            ctx.request_repaint();
            !cancel.load(Ordering::Relaxed)
        })?;
        let p = Prepared {
            image: Arc::new(pipeline::fit_width(big, s.output.max_width)),
            upscaled_from: Some(from),
        };
        self.prepared = Some((key, p.clone()));
        Ok(p)
    }

    fn preview(&mut self, generation: u64, path: &Path, s: &Settings) -> Result<Response> {
        let p = self.prepare(path, s)?;
        let full_size = p.image.dimensions();
        let est = self.estimator(&s.model, s.use_gpu)?;
        let t = Instant::now();
        let raw = est.estimate(&p.image, s.depth.detail)?;
        let infer_ms = t.elapsed().as_millis() as u64;
        let memory_note = est.memory_note.clone();
        Ok(Response::Preview {
            generation,
            image: Arc::new(pipeline::fit_box(&p.image, PREVIEW_MAX)),
            raw: Arc::new(raw),
            full_size,
            upscaled_from: p.upscaled_from,
            infer_ms,
            memory_note,
        })
    }

    fn export_one(&mut self, path: &Path, s: &Settings) -> Result<Vec<PathBuf>> {
        let p = self.prepare(path, s)?;
        let raw = self.estimator(&s.model, s.use_gpu)?.estimate(&p.image, s.depth.detail)?;
        pipeline::render_outputs(
            &p.image,
            &raw,
            s,
            &pipeline::output_dir(path, &s.output),
            &pipeline::output_stem(path),
        )
    }

    fn export(&mut self, jobs: Vec<(usize, PathBuf)>, s: &Settings) {
        let (mut ok, mut failed) = (0, 0);
        for (index, path) in jobs {
            if self.cancel.load(Ordering::Relaxed) {
                return; // the window is closing
            }
            let t = Instant::now();
            let result = self.export_one(&path, s).map_err(|e| format!("{e:#}"));
            if result.is_ok() {
                ok += 1;
            } else {
                failed += 1;
            }
            self.send(Response::ExportItem {
                index,
                result,
                secs: t.elapsed().as_secs_f32(),
            });
        }
        self.send(Response::ExportDone { ok, failed });
    }
}
