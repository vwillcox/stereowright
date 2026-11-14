//! The Stereowright desktop app (egui/eframe).

mod worker;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use eframe::egui::{
    self, Color32, ColorImage, Pos2, Rect, RichText, Sense, TextureHandle, TextureOptions, Vec2,
};
use image::RgbImage;
use serde::{Deserialize, Serialize};

use crate::depth::RawDepth;
use crate::models::Registry;
use crate::pipeline;
use crate::postprocess;
use crate::quest;
use crate::sandbox::{self, Lockdown};
use crate::settings::{
    DETAIL_LEVELS, DepthSettings, EdgeMode, ImageFormat, Preset, Settings, StereoSettings,
    UpscaleMode, UpscaleSettings,
};
use crate::upscale;
use crate::{export, stereo};

use worker::{Request, Response};

pub fn run(images: Vec<PathBuf>, lockdown: Lockdown) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Stereowright")
            .with_app_id("stereowright")
            .with_inner_size([1480.0, 920.0])
            .with_min_inner_size([960.0, 600.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "Stereowright",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, images, lockdown)))),
    )
    .map_err(|e| anyhow!("could not open the window: {e}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum View {
    Original,
    Depth,
    SideBySide,
    CrossEye,
    Anaglyph,
    Wiggle,
}

impl View {
    const ALL: [View; 6] = [
        View::Original,
        View::Depth,
        View::SideBySide,
        View::CrossEye,
        View::Anaglyph,
        View::Wiggle,
    ];
    fn label(self) -> &'static str {
        match self {
            View::Original => "Original",
            View::Depth => "Depth map",
            View::SideBySide => "Side-by-side",
            View::CrossEye => "Cross-eye",
            View::Anaglyph => "Anaglyph",
            View::Wiggle => "Wiggle",
        }
    }
    fn tip(self) -> &'static str {
        match self {
            View::Original => "The photo as loaded.",
            View::Depth => "What the AI thinks: white = near, black = far.",
            View::SideBySide => "Left eye | right eye. This is what the VR file contains.",
            View::CrossEye => "Right | left. Cross your eyes until the two images merge to see 3D on a normal screen.",
            View::Anaglyph => "Red/cyan, for cheap 3D glasses.",
            View::Wiggle => "Flicks between the two eyes. The quickest way to judge depth without a headset.",
        }
    }
}

enum ItemState {
    New,
    Queued,
    Done(Vec<PathBuf>),
    Failed(String),
}

struct Item {
    path: PathBuf,
    name: String,
    state: ItemState,
}

struct Preview {
    image: Arc<RgbImage>,
    raw: Arc<RawDepth>,
    full_size: (u32, u32),
    upscaled_from: Option<(u32, u32)>,
    infer_ms: u64,
}

/// Everything that, when changed, means the AI has to run again for the preview.
#[derive(Clone, PartialEq)]
struct PreviewKey {
    path: PathBuf,
    model: String,
    use_gpu: bool,
    detail: u32,
    upscale: UpscaleSettings,
    max_width: u32,
}

struct RenderOut {
    generation: u64,
    key: (DepthSettings, StereoSettings),
    depth: image::GrayImage,
    left: RgbImage,
    right: RgbImage,
    anaglyph: RgbImage,
    ms: u64,
}

enum Dialog {
    /// Quest sharing was started/stopped (Ok) or failed (Err); the text goes in the status bar.
    Quest(Result<String, String>),
    AddFiles(Vec<PathBuf>),
    OutDir(PathBuf),
    SaveSettings(PathBuf),
    LoadSettings(PathBuf),
}

#[derive(Default)]
struct Textures {
    original: Option<TextureHandle>,
    depth: Option<TextureHandle>,
    left: Option<TextureHandle>,
    right: Option<TextureHandle>,
    anaglyph: Option<TextureHandle>,
}

struct App {
    settings: Settings,
    view: View,
    lockdown: Lockdown,
    lockdown_test: Result<(), String>,

    worker_tx: Sender<Request>,
    worker: worker::Handle,
    worker_rx: Receiver<Response>,
    dialog_tx: Sender<Dialog>,
    dialog_rx: Receiver<Dialog>,
    render_tx: Sender<RenderOut>,
    render_rx: Receiver<RenderOut>,

    items: Vec<Item>,
    selected: Option<usize>,

    generation: u64,
    requested: Option<PreviewKey>,
    preview_pending: bool,
    preview: Option<Preview>,
    preview_error: Option<String>,
    render_in_flight: bool,
    rendered: Option<(DepthSettings, StereoSettings)>,
    render_ms: u64,
    tex: Textures,
    wiggle_epoch: Instant,

    model_label: String,
    device_label: String,
    gpu_note: Option<String>,
    status: String,
    exporting: Option<(usize, usize)>,
    last_out_dir: Option<PathBuf>,

    /// Quest share helper: its status (None = off), when we last looked, Wi-Fi choice.
    quest: Option<quest::ShareStatus>,
    quest_checked: Option<Instant>,
    quest_wifi: bool,
    quest_busy: bool,
    /// The "scan with your Quest" QR code window.
    qr_open: bool,
    /// Open the QR window as soon as the helper reports its Wi-Fi link.
    qr_when_ready: bool,
    qr_tex: Option<(String, TextureHandle)>,
}

const SETTINGS_KEY: &str = "settings";
const VIEW_KEY: &str = "view";

impl App {
    fn new(cc: &eframe::CreationContext<'_>, images: Vec<PathBuf>, lockdown: Lockdown) -> Self {
        style(&cc.egui_ctx);
        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();
        let view = cc
            .storage
            .and_then(|s| eframe::get_value(s, VIEW_KEY))
            .unwrap_or(View::SideBySide);
        let (worker_tx, worker_rx, worker) = worker::spawn(cc.egui_ctx.clone());
        let (dialog_tx, dialog_rx) = mpsc::channel();
        let (render_tx, render_rx) = mpsc::channel();
        let lockdown_test = if lockdown.is_active() {
            sandbox::self_test()
        } else {
            Err("not active".into())
        };
        let mut app = Self {
            settings,
            view,
            lockdown,
            lockdown_test,
            worker_tx,
            worker,
            worker_rx,
            dialog_tx,
            dialog_rx,
            render_tx,
            render_rx,
            items: Vec::new(),
            selected: None,
            generation: 0,
            requested: None,
            preview_pending: false,
            preview: None,
            preview_error: None,
            render_in_flight: false,
            rendered: None,
            render_ms: 0,
            tex: Textures::default(),
            wiggle_epoch: Instant::now(),
            model_label: String::new(),
            device_label: String::new(),
            gpu_note: None,
            status: "Ready. Open or drop some photos to begin.".into(),
            exporting: None,
            last_out_dir: None,
            quest: None,
            quest_checked: None,
            quest_wifi: true,
            quest_busy: false,
            qr_open: false,
            // Developer/test switch: show the QR window as soon as a Wi-Fi link exists.
            qr_when_ready: std::env::var_os("STEREOWRIGHT_OPEN_QR").is_some(),
            qr_tex: None,
        };
        app.add_paths(&images);
        app
    }

    // ---------------------------------------------------------------- state plumbing

    fn add_paths(&mut self, paths: &[PathBuf]) {
        // Videos convert from the command line for now; the app lists photos only.
        let (videos, found): (Vec<PathBuf>, Vec<PathBuf>) = pipeline::collect_inputs(paths)
            .into_iter()
            .partition(|p| crate::video::is_video(p));
        let before = self.items.len();
        for p in found {
            if self.items.iter().any(|i| i.path == p) {
                continue;
            }
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.items.push(Item {
                path: p,
                name,
                state: ItemState::New,
            });
        }
        let added = self.items.len() - before;
        if added > 0 {
            self.status = format!("Added {added} image{}", if added == 1 { "" } else { "s" });
            if self.selected.is_none() {
                self.select(before);
            }
        } else if !videos.is_empty() {
            self.status = format!(
                "Videos convert from the terminal for now: stereowright convert \"{}\"",
                videos[0].display()
            );
        } else if !paths.is_empty() {
            self.status = "No new supported images found (jpg, png, webp, tiff, bmp).".into();
        }
    }

    fn select(&mut self, i: usize) {
        if i < self.items.len() && self.selected != Some(i) {
            self.selected = Some(i);
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.worker_rx.try_recv() {
            match msg {
                Response::Status(s) => self.status = s,
                Response::ModelReady {
                    name,
                    device,
                    gpu_note,
                } => {
                    self.model_label = name;
                    self.device_label = device.to_string();
                    self.gpu_note = gpu_note;
                    self.status = format!("Model ready on {device}");
                }
                Response::Preview {
                    generation,
                    image,
                    raw,
                    full_size,
                    upscaled_from,
                    infer_ms,
                    memory_note,
                } if generation == self.generation => {
                    let (w, h) = image.dimensions();
                    let ci = ColorImage::from_rgb([w as usize, h as usize], image.as_raw());
                    set_tex(ctx, &mut self.tex.original, "original", ci);
                    self.preview = Some(Preview {
                        image,
                        raw,
                        full_size,
                        upscaled_from,
                        infer_ms,
                    });
                    self.preview_pending = false;
                    self.preview_error = None;
                    self.rendered = None; // force a stereo render for the new image
                    self.status = if let Some(note) = memory_note {
                        note
                    } else if upscaled_from.is_some() {
                        format!("AI-upscaled; depth estimated in {infer_ms} ms")
                    } else {
                        format!("Depth estimated in {infer_ms} ms")
                    };
                }
                Response::PreviewFailed { generation, error } if generation == self.generation => {
                    self.preview_pending = false;
                    self.preview_error = Some(error.clone());
                    self.status = format!("Could not process image: {error}");
                }
                Response::Preview { .. } | Response::PreviewFailed { .. } => {} // stale
                Response::ExportItem {
                    index,
                    result,
                    secs,
                } => {
                    if let Some((total, done)) = &mut self.exporting {
                        *done += 1;
                        self.status = format!("Exported {done}/{total}…");
                    }
                    if let Some(item) = self.items.get_mut(index) {
                        item.state = match result {
                            Ok(files) => {
                                quest::record_outputs(&files);
                                if let Some(dir) = files.first().and_then(|f| f.parent()) {
                                    self.last_out_dir = Some(dir.to_path_buf());
                                }
                                self.status = format!("{} done in {secs:.1}s", item.name);
                                ItemState::Done(files)
                            }
                            Err(e) => ItemState::Failed(e),
                        };
                    }
                }
                Response::ExportDone { ok, failed } => {
                    self.exporting = None;
                    self.status = if failed == 0 {
                        format!("✔ Exported {ok} image{}", if ok == 1 { "" } else { "s" })
                    } else {
                        format!("Exported {ok}, {failed} failed (hover the ⚠ for details)")
                    };
                }
            }
        }

        while let Ok(d) = self.dialog_rx.try_recv() {
            match d {
                Dialog::Quest(result) => {
                    if result.is_err() {
                        self.qr_when_ready = false;
                    }
                    self.status = result.unwrap_or_else(|e| e);
                    self.quest_busy = false;
                    self.quest_checked = None; // look again now
                }
                Dialog::AddFiles(p) => self.add_paths(&p),
                Dialog::OutDir(p) => self.settings.output.out_dir = Some(p),
                Dialog::SaveSettings(p) => {
                    self.status = match toml::to_string_pretty(&self.settings)
                        .map_err(anyhow::Error::from)
                        .and_then(|s| Ok(std::fs::write(&p, s)?))
                    {
                        Ok(()) => format!("Settings saved to {}", p.display()),
                        Err(e) => format!("Could not save settings: {e}"),
                    };
                }
                Dialog::LoadSettings(p) => {
                    match std::fs::read_to_string(&p)
                        .map_err(anyhow::Error::from)
                        .and_then(|s| Ok(toml::from_str::<Settings>(&s)?))
                    {
                        Ok(s) => {
                            self.settings = s;
                            self.status = format!("Settings loaded from {}", p.display());
                        }
                        Err(e) => self.status = format!("Could not load settings: {e}"),
                    }
                }
            }
        }

        // The Quest helper runs separately; read its status file every couple of seconds.
        if self.quest_checked.is_none_or(|t| t.elapsed() > Duration::from_millis(1500)) {
            self.quest = quest::status();
            self.quest_checked = Some(Instant::now());
            let wifi_url = self.quest.as_ref().and_then(|q| q.wifi.as_ref()).is_some_and(|w| !w.url.is_empty());
            if self.qr_when_ready && wifi_url {
                self.qr_when_ready = false;
                self.qr_open = true;
            }
            if self.quest.is_none() && !self.quest_busy {
                self.qr_open = false;
            }
        }
        if self.quest.is_some() || self.quest_busy {
            ctx.request_repaint_after(Duration::from_secs(2));
        }

        while let Ok(r) = self.render_rx.try_recv() {
            self.render_in_flight = false;
            if r.generation != self.generation {
                continue;
            }
            let size = |w: u32, h: u32| [w as usize, h as usize];
            let (w, h) = r.left.dimensions();
            set_tex(ctx, &mut self.tex.depth, "depth", ColorImage::from_gray(size(w, h), r.depth.as_raw()));
            set_tex(ctx, &mut self.tex.left, "left", ColorImage::from_rgb(size(w, h), r.left.as_raw()));
            set_tex(ctx, &mut self.tex.right, "right", ColorImage::from_rgb(size(w, h), r.right.as_raw()));
            set_tex(
                ctx,
                &mut self.tex.anaglyph,
                "anaglyph",
                ColorImage::from_rgb(size(w, h), r.anaglyph.as_raw()),
            );
            self.rendered = Some(r.key);
            self.render_ms = r.ms;
        }
    }

    /// Ask the worker for AI depth whenever the image or AI settings change.
    fn request_preview(&mut self) {
        let Some(item) = self.selected.and_then(|i| self.items.get(i)) else {
            return;
        };
        let s = &self.settings;
        let key = PreviewKey {
            path: item.path.clone(),
            model: s.model.clone(),
            use_gpu: s.use_gpu,
            detail: s.depth.detail,
            // Only the upscale settings that change the result count.
            upscale: if s.upscale.mode == UpscaleMode::Off {
                UpscaleSettings {
                    mode: UpscaleMode::Off,
                    model: String::new(),
                    target_width: 0,
                }
            } else {
                pipeline::effective_upscale(s)
            },
            max_width: s.output.max_width,
        };
        if self.requested.as_ref() == Some(&key) {
            return;
        }
        self.requested = Some(key);
        self.generation += 1;
        self.preview_error = None;
        self.preview_pending = true;
        let _ = self.worker_tx.send(Request::Preview {
            generation: self.generation,
            path: item.path.clone(),
            settings: Box::new(self.settings.clone()),
        });
        self.status = format!("Estimating depth for {}…", item.name);
    }

    /// Re-synthesise the stereo preview (in the background) whenever 3D settings change.
    fn request_render(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.preview else { return };
        let key = (self.settings.depth.clone(), self.settings.stereo.clone());
        if self.render_in_flight || self.rendered.as_ref() == Some(&key) {
            return;
        }
        self.render_in_flight = true;
        let (img, raw, tx, ctx, generation) = (
            p.image.clone(),
            p.raw.clone(),
            self.render_tx.clone(),
            ctx.clone(),
            self.generation,
        );
        std::thread::spawn(move || {
            let t = Instant::now();
            let (w, h) = img.dimensions();
            let depth = postprocess::prepare(&raw, w, h, &key.0);
            let pair = stereo::synthesize(&img, &depth, &key.1);
            let anaglyph = export::anaglyph(&pair);
            let _ = tx.send(RenderOut {
                generation,
                depth: depth.to_gray8(),
                left: pair.left,
                right: pair.right,
                anaglyph,
                ms: t.elapsed().as_millis() as u64,
                key,
            });
            ctx.request_repaint();
        });
    }

    fn export(&mut self, indices: Vec<usize>) {
        if indices.is_empty() || self.exporting.is_some() {
            return;
        }
        let o = &self.settings.output;
        if !(o.sbs_full || o.sbs_half || o.top_bottom || o.anaglyph || o.depth_png || o.glb_mesh) {
            self.status = "Pick at least one output format first.".into();
            return;
        }
        let jobs: Vec<(usize, PathBuf)> = indices
            .iter()
            .map(|&i| (i, self.items[i].path.clone()))
            .collect();
        for &i in &indices {
            self.items[i].state = ItemState::Queued;
        }
        self.exporting = Some((jobs.len(), 0));
        self.status = format!("Exporting {} image(s)…", jobs.len());
        let _ = self.worker_tx.send(Request::Export {
            jobs,
            settings: Box::new(self.settings.clone()),
        });
    }

    fn spawn_dialog(&self, ctx: &egui::Context, f: impl FnOnce() -> Option<Dialog> + Send + 'static) {
        let tx = self.dialog_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Some(d) = f() {
                let _ = tx.send(d);
                ctx.request_repaint();
            }
        });
    }

    fn open_files(&self, ctx: &egui::Context) {
        self.spawn_dialog(ctx, || {
            rfd::FileDialog::new()
                .set_title("Open photos")
                .add_filter("Images", &pipeline::IMAGE_EXTENSIONS)
                .pick_files()
                .map(Dialog::AddFiles)
        });
    }

    fn open_folder(&self, ctx: &egui::Context) {
        self.spawn_dialog(ctx, || {
            rfd::FileDialog::new()
                .set_title("Open a folder of photos")
                .pick_folder()
                .map(|p| Dialog::AddFiles(vec![p]))
        });
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() {
            self.add_paths(&dropped);
        }
        let typing = ctx.memory(|m| m.focused().is_some());
        if typing || self.items.is_empty() {
            return;
        }
        let (next, prev, open, export) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowUp),
                i.modifiers.command && i.key_pressed(egui::Key::O),
                i.modifiers.command && i.key_pressed(egui::Key::E),
            )
        });
        let cur = self.selected.unwrap_or(0);
        if next {
            self.select((cur + 1).min(self.items.len() - 1));
        }
        if prev {
            self.select(cur.saturating_sub(1));
        }
        if open {
            self.open_files(ctx);
        }
        if export
            && let Some(i) = self.selected {
                self.export(vec![i]);
            }
    }

    // ---------------------------------------------------------------- panels

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Stereowright").heading().strong());
            ui.add_space(12.0);
            if ui.button("📂 Open photos…").on_hover_text("Ctrl+O").clicked() {
                self.open_files(ui.ctx());
            }
            if ui.button("🗁 Open folder…").clicked() {
                self.open_folder(ui.ctx());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.privacy_badge(ui);
                if !self.device_label.is_empty() {
                    let (txt, col) = if self.device_label.contains("GPU") {
                        (format!("⚡ {}", self.device_label), Color32::from_rgb(120, 200, 255))
                    } else {
                        (format!("🖥 {}", self.device_label), Color32::from_rgb(230, 190, 90))
                    };
                    let r = ui.label(RichText::new(txt).color(col));
                    if let Some(n) = &self.gpu_note {
                        r.on_hover_text(format!("GPU not used:\n{n}"));
                    }
                }
            });
        });
    }

    fn privacy_badge(&self, ui: &mut egui::Ui) {
        let ifs = sandbox::visible_interfaces();
        match (&self.lockdown, &self.lockdown_test) {
            (Lockdown::Active, Ok(())) => {
                let txt = if ifs.is_empty() {
                    "🔒 Offline: no network at all"
                } else {
                    "🔒 Offline: network blocked"
                };
                ui.label(RichText::new(txt).color(Color32::from_rgb(110, 210, 130)))
                    .on_hover_text(
                        "Everything happens on this computer.\n\n\
                         The Linux kernel refuses to let this app open any internet \
                         connection (seccomp filter, installed at start-up, cannot be undone). \
                         A self-test confirmed it just now.\n\n\
                         Your photos are read from and written to your disk only.",
                    );
            }
            (_, Err(e)) => {
                ui.label(RichText::new("⚠ Network block not verified").color(Color32::from_rgb(240, 120, 90)))
                    .on_hover_text(format!(
                        "The app still has no networking code, but the kernel-level block \
                         could not be confirmed: {e}"
                    ));
            }
            _ => {}
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.exporting.is_some() || self.render_in_flight || self.is_preview_loading() {
                ui.spinner();
            }
            ui.label(&self.status);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(p) = &self.preview {
                    ui.label(
                        RichText::new(format!(
                            "{}{}×{} · AI {} ms · 3D {} ms{}",
                            p.upscaled_from
                                .map_or(String::new(), |(w, h)| format!("{w}×{h} upscaled to ")),
                            p.full_size.0,
                            p.full_size.1,
                            p.infer_ms,
                            self.render_ms,
                            if self.model_label.is_empty() {
                                String::new()
                            } else {
                                format!(" · {}", self.model_label)
                            }
                        ))
                        .weak(),
                    );
                }
            });
        });
    }

    fn is_preview_loading(&self) -> bool {
        self.preview_pending
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(4.0);
            self.model_section(ui);
            ui.add_space(4.0);
            self.upscale_section(ui);
            ui.add_space(4.0);

            section(ui, "Presets", true, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for p in Preset::ALL {
                        if ui.button(p.label()).on_hover_text(p.description()).clicked() {
                            p.apply(&mut self.settings);
                        }
                    }
                });
            });

            let st = &mut self.settings.stereo;
            section(ui, "3D effect", true, |ui| {
                ui.add(
                    egui::Slider::new(&mut st.strength, 0.0..=10.0)
                        .step_by(0.1)
                        .suffix(" %")
                        .text("Strength"),
                )
                .on_hover_text(
                    "How deep the 3D is: the separation between nearest and farthest \
                     point, as a percentage of the image width.\n\
                     2–4 % is comfortable in a headset; above 6 % can strain your eyes.",
                );
                ui.add(egui::Slider::new(&mut st.convergence, 0.0..=1.0).text("Convergence"))
                    .on_hover_text(
                        "Where the 'screen' sits in the scene.\n\
                         0 = behind everything: the whole scene comes out towards you.\n\
                         1 = in front of everything: the scene sits behind, like a window.\n\
                         0.5 = the middle of the scene is at screen depth.",
                    );
                ui.label(RichText::new("0 = pops out towards you   ·   1 = sits behind the screen").small().weak());
                ui.checkbox(&mut st.swap_eyes, "Swap left/right eyes")
                    .on_hover_text("Use if the 3D looks inside-out in your player.");
            });

            let d = &mut self.settings.depth;
            section(ui, "Depth tuning", false, |ui| {
                ui.add(
                    egui::Slider::new(&mut d.curve, 0.3..=3.0)
                        .logarithmic(true)
                        .text("Depth curve"),
                )
                .on_hover_text(
                    "Above 1: more depth between things close to you (faces, subjects).\n\
                     Below 1: more depth spread into the background (landscapes).",
                );
                ui.add(egui::Slider::new(&mut d.far_clip, 0.0..=0.9).text("Background flatten"))
                    .on_hover_text(
                        "Flattens everything farther than this onto one plane. \
                         Useful to stop distant skies and walls stealing depth from the subject.",
                    );
                ui.add(egui::Slider::new(&mut d.near_clip, 0.1..=1.0).text("Foreground flatten"))
                    .on_hover_text("Flattens everything nearer than this.");
                ui.add(
                    egui::Slider::new(&mut d.smoothing, 0.0..=2.0)
                        .step_by(0.05)
                        .suffix(" %")
                        .text("Smoothing"),
                )
                .on_hover_text("Blurs the depth map: hides noise and wobbly outlines, softens detail.");
                ui.add(egui::Slider::new(&mut d.edge_dilation, 0.0..=10.0).step_by(0.5).text("Edge grow"))
                    .on_hover_text(
                        "Grows near objects slightly in the depth map so that edge artefacts \
                         fall on the background instead of on people and objects.",
                    );
                ui.checkbox(&mut d.invert, "Invert depth")
                    .on_hover_text("Swap near and far. Rarely needed.");
            });

            let st = &mut self.settings.stereo;
            section(ui, "Edges", false, |ui| {
                ui.radio_value(&mut st.edge_mode, EdgeMode::Fill, "Sharp: fill gaps from background")
                    .on_hover_text("Crisp outlines. Gaps revealed behind objects are filled with nearby background.");
                ui.radio_value(&mut st.edge_mode, EdgeMode::Stretch, "Smooth: stretch across edges")
                    .on_hover_text("No gaps, but outlines get smeared into the background.");
                if st.edge_mode == EdgeMode::Fill {
                    ui.add(
                        egui::Slider::new(&mut st.tear_threshold, 0.2..=8.0)
                            .logarithmic(true)
                            .suffix(" px")
                            .text("Edge sensitivity"),
                    )
                    .on_hover_text("Lower = more places are treated as hard edges.");
                }
            });

            self.output_section(ui);

            section(ui, "Settings", false, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Reset to defaults").clicked() {
                        let keep_model = self.settings.model.clone();
                        let keep_out = self.settings.output.out_dir.clone();
                        self.settings = Settings::default();
                        self.settings.model = keep_model;
                        self.settings.output.out_dir = keep_out;
                    }
                    if ui
                        .button("Save as…")
                        .on_hover_text("Save these settings to a .toml file (usable with `stereowright convert --settings`).")
                        .clicked()
                    {
                        self.spawn_dialog(ui.ctx(), || {
                            rfd::FileDialog::new()
                                .set_file_name("stereowright-settings.toml")
                                .add_filter("Settings", &["toml"])
                                .save_file()
                                .map(Dialog::SaveSettings)
                        });
                    }
                    if ui.button("Load…").clicked() {
                        self.spawn_dialog(ui.ctx(), || {
                            rfd::FileDialog::new()
                                .add_filter("Settings", &["toml"])
                                .pick_file()
                                .map(Dialog::LoadSettings)
                        });
                    }
                });
            });
            ui.add_space(8.0);
        });
    }

    fn model_section(&mut self, ui: &mut egui::Ui) {
        let reg = Registry::discover();
        section(ui, "AI depth model", true, |ui| {
            let current = if self.settings.model.is_empty() {
                format!(
                    "Auto ({})",
                    reg.best_installed().map_or("none installed", |m| m.name.as_str())
                )
            } else {
                reg.get(&self.settings.model)
                    .map_or(self.settings.model.clone(), |m| m.name.clone())
            };
            egui::ComboBox::from_id_salt("model")
                .width(ui.available_width() - 8.0)
                .selected_text(current)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.settings.model, String::new(), "Auto (best installed)");
                    for m in &reg.models {
                        let ok = reg.is_installed(m);
                        let label = if ok {
                            m.name.clone()
                        } else {
                            format!("{} (not downloaded)", m.name)
                        };
                        let tip = if ok {
                            m.notes.clone()
                        } else {
                            format!(
                                "{}\n\nDownload once with:\nscripts/fetch-models.sh {}",
                                m.notes,
                                m.fetch.as_deref().unwrap_or("")
                            )
                        };
                        ui.add_enabled_ui(ok, |ui| {
                            ui.selectable_value(&mut self.settings.model, m.id.clone(), label)
                                .on_hover_text(&tip)
                                .on_disabled_hover_text(&tip);
                        });
                    }
                });
            ui.horizontal(|ui| {
                ui.label("Detail");
                let cur = DETAIL_LEVELS
                    .iter()
                    .find(|(v, _)| *v == self.settings.depth.detail)
                    .map_or(format!("{} px", self.settings.depth.detail), |(v, l)| format!("{l} · {v} px"));
                egui::ComboBox::from_id_salt("detail")
                    .selected_text(cur)
                    .show_ui(ui, |ui| {
                        for (v, l) in DETAIL_LEVELS {
                            ui.selectable_value(&mut self.settings.depth.detail, v, format!("{l} · {v} px"));
                        }
                    })
                    .response
                    .on_hover_text(
                        "Resolution the AI works at. Higher = finer depth detail (hair, leaves) \
                         but slower, and very high values can lose the overall shape.\n\
                         Very high and Ultra are limited to what fits in GPU memory.",
                    );
            });
            ui.checkbox(&mut self.settings.use_gpu, "Use NVIDIA GPU (CUDA)");
            if reg.best_installed().is_none() {
                ui.colored_label(
                    Color32::from_rgb(240, 120, 90),
                    format!(
                        "No model found in {}.\nRun once:  scripts/fetch-models.sh",
                        reg.dir.display()
                    ),
                );
            }
        });
    }

    fn upscale_section(&mut self, ui: &mut egui::Ui) {
        let reg = Registry::discover();
        let any_installed = reg.upscalers.iter().any(|u| reg.upscaler_installed(u));
        let u = &mut self.settings.upscale;
        section(ui, "AI upscale (Real-ESRGAN)", u.mode != UpscaleMode::Off, |ui| {
            ui.horizontal(|ui| {
                ui.radio_value(&mut u.mode, UpscaleMode::Off, "Off")
                    .on_hover_text("Use photos at their own resolution.");
                ui.radio_value(&mut u.mode, UpscaleMode::Auto, "Auto").on_hover_text(format!(
                    "Upscale only photos narrower than {} px (2/3 of the target). \
                     Big photos are left alone.",
                    upscale::auto_threshold(u)
                ));
                ui.radio_value(&mut u.mode, UpscaleMode::Always, "Always")
                    .on_hover_text("Upscale every photo narrower than the target width.");
            });
            if u.mode == UpscaleMode::Off {
                return;
            }
            let current = if u.model.is_empty() {
                format!(
                    "Auto ({})",
                    reg.upscalers
                        .iter()
                        .find(|x| reg.upscaler_installed(x))
                        .map_or("none built", |x| x.name.as_str())
                )
            } else {
                reg.upscalers
                    .iter()
                    .find(|x| x.id == u.model)
                    .map_or(u.model.clone(), |x| x.name.clone())
            };
            egui::ComboBox::from_id_salt("upscaler")
                .width(ui.available_width() - 8.0)
                .selected_text(current)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut u.model, String::new(), "Auto (first built)");
                    for x in &reg.upscalers {
                        let ok = reg.upscaler_installed(x);
                        let tip = if ok {
                            x.notes.clone()
                        } else {
                            format!(
                                "{}\n\nBuild once with:\nscripts/make-upscalers.sh {}",
                                x.notes,
                                x.fetch.as_deref().unwrap_or("")
                            )
                        };
                        let label = if ok {
                            x.name.clone()
                        } else {
                            format!("{} (not built)", x.name)
                        };
                        ui.add_enabled_ui(ok, |ui| {
                            ui.selectable_value(&mut u.model, x.id.clone(), label)
                                .on_hover_text(&tip)
                                .on_disabled_hover_text(&tip);
                        });
                    }
                });
            ui.horizontal(|ui| {
                ui.label("Target width");
                egui::ComboBox::from_id_salt("upw")
                    .selected_text(format!("{} px", u.target_width))
                    .show_ui(ui, |ui| {
                        for w in [1920, 2560, 3840, 5120] {
                            ui.selectable_value(&mut u.target_width, w, format!("{w} px"));
                        }
                    })
                    .response
                    .on_hover_text(
                        "Width of each eye after upscaling. 3840 suits the Quest 3; \
                         full side-by-side files are twice as wide.",
                    );
            });
            ui.label(
                RichText::new("Makes small photos sharper in the headset. Runs on this computer; adds a few seconds per photo.")
                    .small()
                    .weak(),
            );
            if !any_installed {
                ui.colored_label(
                    Color32::from_rgb(240, 120, 90),
                    "No upscaler built yet.\nRun once:  scripts/make-upscalers.sh",
                );
            }
        });
    }

    fn output_section(&mut self, ui: &mut egui::Ui) {
        let o = &mut self.settings.output;
        section(ui, "Output", true, |ui| {
            ui.checkbox(&mut o.sbs_full, "Side-by-side, full  (_LR)")
                .on_hover_text("Recommended for VR. Each eye at full resolution. Works in Skybox, DeoVR, Pigasus, 4XVR, Virtual Desktop and most VR photo viewers.");
            ui.checkbox(&mut o.sbs_half, "Side-by-side, half  (_3dh)")
                .on_hover_text("Both eyes squeezed into the original frame size. For 3D TVs and players that expect half-SBS.");
            ui.checkbox(&mut o.top_bottom, "Top / bottom  (_TB)")
                .on_hover_text("Left eye above right eye.");
            ui.checkbox(&mut o.anaglyph, "Red/cyan anaglyph")
                .on_hover_text("View with red/cyan 3D glasses on any screen.");
            ui.checkbox(&mut o.depth_png, "Depth map  (16-bit PNG)")
                .on_hover_text("The depth map itself: useful for other 3D tools, Blender, ComfyUI, etc.");
            ui.checkbox(&mut o.glb_mesh, "3D mesh  (.glb)")
                .on_hover_text("A textured 3D surface you can walk around a little in a 3D model viewer.");
            if o.glb_mesh {
                ui.indent("mesh", |ui| {
                    ui.add(egui::Slider::new(&mut o.mesh_detail, 64..=2048).logarithmic(true).text("Mesh detail"));
                    ui.add(egui::Slider::new(&mut o.mesh_fov, 30.0..=120.0).suffix("°").text("Camera FOV"));
                    ui.add(egui::Slider::new(&mut o.mesh_depth, 1.2..=20.0).logarithmic(true).text("Depth range"));
                });
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.radio_value(&mut o.format, ImageFormat::Jpeg, "JPEG");
                ui.radio_value(&mut o.format, ImageFormat::Png, "PNG");
            });
            if o.format == ImageFormat::Jpeg {
                ui.add(egui::Slider::new(&mut o.jpeg_quality, 60..=100).text("JPEG quality"));
            }
            ui.horizontal(|ui| {
                ui.label("Max width");
                let label = |w: u32| if w == 0 { "Original".to_string() } else { format!("{w} px") };
                egui::ComboBox::from_id_salt("maxw")
                    .selected_text(label(o.max_width))
                    .show_ui(ui, |ui| {
                        for w in [0, 7680, 5760, 3840, 2560, 1920] {
                            ui.selectable_value(&mut o.max_width, w, label(w));
                        }
                    })
                    .response
                    .on_hover_text("Downscale big photos first. Full-SBS doubles the width, so 3840 becomes a 7680-wide file.");
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Save to:");
                match &o.out_dir {
                    Some(d) => {
                        ui.label(RichText::new(d.display().to_string()).monospace());
                    }
                    None => {
                        ui.label(RichText::new("stereowright/ next to each photo").italics());
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            if ui.button("Choose folder…").clicked() {
                self.spawn_dialog(ui.ctx(), || {
                    rfd::FileDialog::new()
                        .set_title("Where to save 3D images")
                        .pick_folder()
                        .map(Dialog::OutDir)
                });
            }
            if self.settings.output.out_dir.is_some() && ui.button("Next to photos").clicked() {
                self.settings.output.out_dir = None;
            }
        });
    }

    /// Start/stop the Quest share helper and show how to reach it.
    fn quest_block(&mut self, ui: &mut egui::Ui) {
        let green = Color32::from_rgb(110, 210, 130);
        let warn = Color32::from_rgb(240, 160, 90);
        match self.quest.clone() {
            None => {
                ui.horizontal(|ui| {
                    let b = ui.add_enabled(!self.quest_busy, egui::Button::new(RichText::new("▶ View on Quest 3").strong()));
                    if b.on_hover_text(
                        "Starts a separate helper that shows your 3D photos in the Quest browser, in 3D. \
                         New exports appear by themselves.\n\nPhotos go over the USB cable (needs developer \
                         mode once) and/or your home Wi-Fi, never the internet. Stereowright itself stays offline.",
                    )
                    .clicked()
                    {
                        self.quest_busy = true;
                        self.qr_when_ready = self.quest_wifi;
                        let (tx, wifi) = (self.dialog_tx.clone(), self.quest_wifi);
                        std::thread::spawn(move || {
                            let r = quest::start(wifi)
                                .map(|()| "Quest sharing started".to_string())
                                .map_err(|e| format!("{e:#}"));
                            let _ = tx.send(Dialog::Quest(r));
                        });
                    }
                    ui.checkbox(&mut self.quest_wifi, "Wi-Fi too")
                        .on_hover_text("Also reachable from the headset over your home network (not just the USB cable).");
                });
            }
            Some(st) => {
                ui.horizontal(|ui| {
                    ui.colored_label(green, format!("📡 Quest sharing on · {} photos", st.photos));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!self.quest_busy, egui::Button::new("Stop")).clicked() {
                            self.quest_busy = true;
                            let tx = self.dialog_tx.clone();
                            std::thread::spawn(move || {
                                let r = quest::stop()
                                    .map(|()| "Quest sharing stopped".to_string())
                                    .map_err(|e| format!("{e:#}"));
                                let _ = tx.send(Dialog::Quest(r));
                            });
                        }
                    });
                });
                if let Some(u) = &st.usb {
                    let col = if u.state == "connected" { green } else { ui.visuals().weak_text_color() };
                    ui.label(RichText::new(&u.message).small().color(col));
                }
                if let Some(w) = st.wifi.as_ref().filter(|w| !w.url.is_empty()) {
                    if ui
                        .add(egui::Button::new("📱 Show QR code").min_size(Vec2::new(ui.available_width(), 26.0)))
                        .on_hover_text("Look at it with the Quest's QR code scanner to open the gallery in the headset.")
                        .clicked()
                    {
                        self.qr_open = true;
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Wi-Fi link:").small());
                        ui.label(RichText::new(&w.url).small().monospace());
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(w.url.clone());
                        }
                    })
                    .response
                    .on_hover_text("Type this into the Quest browser once and bookmark it. Or open it from the USB gallery page.");
                    if !w.firewall.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("Firewall blocks Wi-Fi. Allow your home network once (terminal):").small().color(warn));
                            if ui.small_button("Copy command").clicked() {
                                ui.ctx().copy_text(w.firewall.clone());
                            }
                        })
                        .response
                        .on_hover_text(&w.firewall);
                    }
                }
            }
        }
    }

    /// A big QR code with the Wi-Fi link: look at it with the headset's QR code scanner.
    fn qr_window(&mut self, ctx: &egui::Context) {
        let Some(wifi) = self.quest.as_ref().and_then(|q| q.wifi.clone()).filter(|w| !w.url.is_empty()) else {
            return;
        };
        if self.qr_tex.as_ref().is_none_or(|(u, _)| *u != wifi.url) {
            self.qr_tex = crate::qr::Qr::new(&wifi.url).ok().map(|code| {
                let (side, px) = code.to_rgba(8);
                let img = ColorImage::from_rgba_unmultiplied([side, side], &px);
                (wifi.url.clone(), ctx.load_texture("quest-qr", img, TextureOptions::NEAREST))
            });
        }
        let screen = ctx.content_rect();
        let side = (screen.height() - 330.0).min(screen.width() - 120.0).clamp(220.0, 620.0);
        let modal = egui::Modal::new(egui::Id::unique("quest-qr")).show(ctx, |ui| {
            ui.set_width(side.max(420.0));
            ui.vertical_centered(|ui| {
                ui.heading("Look at this with your Quest 3");
                ui.add_space(6.0);
                if let Some((_, tex)) = &self.qr_tex {
                    ui.add(egui::Image::new(tex).fit_to_exact_size(Vec2::splat(side)));
                }
                ui.add_space(6.0);
            });
            ui.label(
                "1. In the headset, open the QR code scanner: Quick settings > QR code scanner \
                 (Horizon OS v2.9 or newer), or say \"Hey Meta, open QR code scanner\".",
            );
            ui.label("2. Look at this code, then open the link in the browser.");
            ui.label("3. First time only: accept the security warning (the certificate was made on \
                      this computer), then bookmark the page.");
            if !wifi.firewall.is_empty() {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(Color32::from_rgb(240, 160, 90), "Your firewall blocks the headset. Run once in a terminal:");
                    if ui.small_button("Copy command").clicked() {
                        ui.ctx().copy_text(wifi.firewall.clone());
                    }
                });
            }
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&wifi.url).monospace().weak());
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(wifi.url.clone());
                }
            });
            ui.add_space(6.0);
            ui.vertical_centered(|ui| ui.button("Close").clicked()).inner
        });
        if modal.inner || modal.should_close() {
            self.qr_open = false;
        }
    }

    fn images_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Photos ({})", self.items.len())).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.items.is_empty() && self.exporting.is_none() && ui.small_button("Clear").clicked() {
                    self.items.clear();
                    self.selected = None;
                    self.preview = None;
                    self.requested = None;
                    self.preview_pending = false;
                    self.tex = Textures::default();
                }
            });
        });
        ui.separator();

        // Export buttons pinned to the bottom.
        egui::Panel::bottom("export_buttons").show(ui, |ui| {
            ui.add_space(6.0);
            self.quest_block(ui);
            ui.separator();
            if let Some((total, done)) = self.exporting {
                ui.add(
                    egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                        .text(format!("Exporting {done}/{total}")),
                );
            }
            let busy = self.exporting.is_some();
            let wide = Vec2::new(ui.available_width(), 30.0);
            let can_one = self.selected.is_some() && !busy;
            if ui
                .add_enabled(can_one, egui::Button::new(RichText::new("💾 Export this photo").strong()).min_size(wide))
                .on_hover_text("Ctrl+E")
                .clicked()
                && let Some(i) = self.selected {
                    self.export(vec![i]);
                }
            let n = self.items.len();
            if ui
                .add_enabled(n > 1 && !busy, egui::Button::new(format!("Export all {n} photos")).min_size(wide))
                .clicked()
            {
                self.export((0..n).collect());
            }
            if let Some(dir) = self.last_out_dir.clone()
                && ui
                    .add(egui::Button::new("📂 Open output folder").min_size(wide))
                    .on_hover_text(dir.display().to_string())
                    .clicked()
                {
                    let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
                }
            ui.add_space(4.0);
        });

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let mut remove = None;
            let mut select = None;
            for (i, item) in self.items.iter().enumerate() {
                ui.horizontal(|ui| {
                    let (icon, col) = match &item.state {
                        ItemState::New => ("○", Color32::GRAY),
                        ItemState::Queued => ("⏳", Color32::from_rgb(230, 190, 90)),
                        ItemState::Done(_) => ("✔", Color32::from_rgb(110, 210, 130)),
                        ItemState::Failed(_) => ("⚠", Color32::from_rgb(240, 120, 90)),
                    };
                    let r = ui.label(RichText::new(icon).color(col));
                    match &item.state {
                        ItemState::Done(files) => {
                            r.on_hover_text(
                                files
                                    .iter()
                                    .map(|f| f.display().to_string())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            );
                        }
                        ItemState::Failed(e) => {
                            r.on_hover_text(e);
                        }
                        _ => {}
                    }
                    if self.exporting.is_none() && ui.small_button("✖").on_hover_text("Remove from list").clicked() {
                        remove = Some(i);
                    }
                    let sel = self.selected == Some(i);
                    let resp = ui.selectable_label(sel, &item.name);
                    if sel {
                        resp.scroll_to_me(None);
                    }
                    if resp.on_hover_text(item.path.display().to_string()).clicked() {
                        select = Some(i);
                    }
                });
            }
            if let Some(i) = select {
                self.select(i);
            }
            if let Some(i) = remove {
                self.items.remove(i);
                self.selected = match self.selected {
                    _ if self.items.is_empty() => None,
                    Some(s) if s > i => Some(s - 1),
                    Some(s) if s == i => Some(i.min(self.items.len() - 1)),
                    s => s,
                };
                if self.items.is_empty() {
                    self.preview = None;
                    self.requested = None;
                    self.preview_pending = false;
                    self.tex = Textures::default();
                }
            }
        });
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        if self.items.is_empty() {
            self.welcome(ui);
            return;
        }
        ui.horizontal(|ui| {
            for v in View::ALL {
                ui.selectable_value(&mut self.view, v, v.label())
                    .on_hover_text(v.tip());
            }
        });
        ui.add_space(4.0);

        if let Some(e) = &self.preview_error {
            ui.colored_label(Color32::from_rgb(240, 120, 90), e);
            return;
        }
        let Some(p) = &self.preview else {
            ui.centered_and_justified(|ui| {
                ui.spinner();
            });
            return;
        };
        let (w, h) = p.image.dimensions();
        let aspect = w as f32 / h as f32;
        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, Sense::hover());
        let painter = ui.painter_at(area);
        let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        let draw = |tex: &Option<TextureHandle>, rect: Rect| {
            if let Some(t) = tex {
                painter.image(t.id(), rect, uv, Color32::WHITE);
            }
        };
        let stereo_ready = self.tex.left.is_some();
        match self.view {
            View::Original => draw(&self.tex.original, fit(area, aspect)),
            View::Depth => draw(&self.tex.depth, fit(area, aspect)),
            View::Anaglyph => draw(&self.tex.anaglyph, fit(area, aspect)),
            View::SideBySide | View::CrossEye => {
                let r = fit(area, aspect * 2.0);
                let (a, b) = split(r);
                let (l, rt) = if self.view == View::SideBySide {
                    (&self.tex.left, &self.tex.right)
                } else {
                    (&self.tex.right, &self.tex.left)
                };
                draw(l, a);
                draw(rt, b);
            }
            View::Wiggle => {
                let period = Duration::from_millis(140);
                let phase = (self.wiggle_epoch.elapsed().as_millis() / period.as_millis()) % 2;
                draw(
                    if phase == 0 { &self.tex.left } else { &self.tex.right },
                    fit(area, aspect),
                );
                ui.ctx().request_repaint_after(period);
            }
        }
        if !stereo_ready && self.view != View::Original {
            painter.text(
                area.center(),
                egui::Align2::CENTER_CENTER,
                "Rendering 3D…",
                egui::FontId::proportional(18.0),
                Color32::WHITE,
            );
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.25);
            ui.label(RichText::new("Turn any photo into 3D for your VR headset").heading());
            ui.add_space(10.0);
            ui.label("Drop photos or a folder here, or:");
            ui.add_space(10.0);
            if ui
                .add(egui::Button::new(RichText::new("📂  Open photos…").size(18.0)).min_size(Vec2::new(220.0, 40.0)))
                .clicked()
            {
                self.open_files(ui.ctx());
            }
            ui.add_space(24.0);
            ui.label(
                RichText::new(
                    "A local AI model works out how far away everything is, then Stereowright \
                     renders a separate view for each eye.\nEverything happens on this computer; \
                     the app is blocked from using the network.",
                )
                .weak(),
            );
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.handle_input(&ctx);
        self.request_preview();
        self.request_render(&ctx);

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("settings")
            .resizable(true)
            .default_size(340.0)
            .min_size(280.0)
            .show(ui, |ui| self.settings_panel(ui));
        egui::Panel::right("images")
            .resizable(true)
            .default_size(250.0)
            .min_size(180.0)
            .show(ui, |ui| self.images_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.preview_panel(ui));
        if self.qr_open {
            self.qr_window(&ctx);
        }

        // Show a hint while files are dragged over the window.
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop")));
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(170));
            painter.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                "Drop photos to add them",
                egui::FontId::proportional(28.0),
                Color32::WHITE,
            );
        }
    }

    /// Called after `save`: free the AI models before the process starts exiting.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.worker.shutdown(&self.worker_tx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
        eframe::set_value(storage, VIEW_KEY, &self.view);
    }
}

// -------------------------------------------------------------------- helpers

fn section(ui: &mut egui::Ui, title: &str, open: bool, body: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new(RichText::new(title).strong())
        .default_open(open)
        .show(ui, body);
}

fn set_tex(ctx: &egui::Context, slot: &mut Option<TextureHandle>, name: &str, img: ColorImage) {
    match slot {
        Some(t) => t.set(img, TextureOptions::LINEAR),
        None => *slot = Some(ctx.load_texture(name, img, TextureOptions::LINEAR)),
    }
}

/// Largest rect of the given aspect ratio centred in `area`.
fn fit(area: Rect, aspect: f32) -> Rect {
    let (aw, ah) = (area.width(), area.height());
    let size = if aw / ah > aspect {
        Vec2::new(ah * aspect, ah)
    } else {
        Vec2::new(aw, aw / aspect)
    };
    Rect::from_center_size(area.center(), size)
}

fn split(r: Rect) -> (Rect, Rect) {
    let mid = r.center().x;
    (
        Rect::from_min_max(r.min, Pos2::new(mid, r.max.y)),
        Rect::from_min_max(Pos2::new(mid, r.min.y), r.max),
    )
}

fn style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 6.0);
        s.spacing.slider_width = 170.0;
        s.spacing.button_padding = Vec2::new(8.0, 4.0);
    });
}
