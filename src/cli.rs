//! Command-line interface: batch conversion, model listing and a self-check.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::depth::DepthEstimator;
use crate::models::Registry;
use crate::pipeline;
use crate::quest;
use crate::sandbox::{self, Lockdown};
use crate::settings::{EdgeMode, Preset, Settings, UpscaleMode};
use crate::upscale::Upscaler;
use crate::video;

#[derive(Parser)]
#[command(
    name = "stereowright",
    version,
    about = "Turn photos and videos into VR-ready 3D with a local AI depth model. Fully offline.",
    long_about = "Turn photos and videos into VR-ready 3D with a local AI depth model.\n\n\
        Run with no command (optionally with image or video paths) to open the app.\n\
        Network access is blocked by the kernel for this process: nothing leaves your computer."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Photos, videos or folders to open in the app.
    pub images: Vec<PathBuf>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once at startup; size doesn't matter
pub enum Command {
    /// Convert photos, videos or folders without opening the app.
    Convert(ConvertArgs),
    /// List depth models and AI upscalers, and whether they are installed.
    Models,
    /// Check GPU acceleration, models and the network lockdown.
    Doctor,
    /// Print the default settings as TOML (edit and pass with `convert --settings`).
    Settings,
    /// View your 3D photos on a Meta Quest (starts/stops the separate share helper).
    Quest {
        #[command(subcommand)]
        action: QuestAction,
    },
}

#[derive(Subcommand)]
pub enum QuestAction {
    /// Start sharing: over the USB cable, and over your home Wi-Fi unless --usb-only.
    Start {
        #[arg(long)]
        usb_only: bool,
    },
    /// Stop sharing.
    Stop,
    /// Show whether sharing is on, and the links.
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    /// Full side-by-side `_LR` (best for VR players)
    Sbs,
    /// Half side-by-side `_3dh`
    Hsbs,
    /// Top/bottom `_TB`
    Tb,
    /// Red/cyan anaglyph
    Anaglyph,
    /// 16-bit depth map PNG
    Depth,
    /// 3D mesh (.glb)
    Glb,
}

#[derive(Args)]
pub struct ConvertArgs {
    /// Photos, videos and/or folders.
    #[arg(required = true)]
    pub inputs: Vec<PathBuf>,
    /// Output folder (default: a `stereowright/` folder next to each image).
    #[arg(short, long)]
    pub out: Option<PathBuf>,
    /// Load settings from a TOML file (see `stereowright settings`). Flags override it.
    #[arg(long)]
    pub settings: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub preset: Option<Preset>,
    /// Model id (see `stereowright models`). Default: best installed.
    #[arg(long)]
    pub model: Option<String>,
    /// Don't use the GPU.
    #[arg(long)]
    pub cpu: bool,
    /// 3D strength, % of image width (e.g. 3).
    #[arg(long)]
    pub strength: Option<f32>,
    /// 0 = everything pops out … 1 = everything behind the screen.
    #[arg(long)]
    pub convergence: Option<f32>,
    /// AI resolution (short side): 392, 518, 770, 1022, 1274.
    #[arg(long)]
    pub detail: Option<u32>,
    /// Depth curve (gamma).
    #[arg(long)]
    pub curve: Option<f32>,
    /// Depth smoothing, % of width.
    #[arg(long)]
    pub smoothing: Option<f32>,
    /// Foreground edge dilation, px per 1000 px.
    #[arg(long)]
    pub dilation: Option<f32>,
    #[arg(long)]
    pub invert: bool,
    #[arg(long)]
    pub swap_eyes: bool,
    #[arg(long, value_enum)]
    pub edge_mode: Option<EdgeMode>,
    /// Comma-separated outputs, e.g. `sbs,anaglyph,depth`.
    #[arg(long, value_delimiter = ',', value_enum)]
    pub formats: Option<Vec<FormatArg>>,
    /// JPEG quality 1-100.
    #[arg(long)]
    pub quality: Option<u8>,
    /// Downscale inputs wider than this.
    #[arg(long)]
    pub max_width: Option<u32>,
    /// Write PNG instead of JPEG.
    #[arg(long)]
    pub png: bool,
    /// AI upscaling (Real-ESRGAN) before the 3D conversion. `auto` upscales photos
    /// narrower than 2/3 of --upscale-width.
    #[arg(long, value_enum)]
    pub upscale: Option<UpscaleMode>,
    /// Upscaler id (see `stereowright models`). Default: first installed.
    #[arg(long)]
    pub upscale_model: Option<String>,
    /// Width to upscale towards (default 3840).
    #[arg(long)]
    pub upscale_width: Option<u32>,
    /// Videos: width of each eye's picture (default 1920; 0 = original size).
    #[arg(long)]
    pub video_width: Option<u32>,
    /// Videos: steadier depth between frames, 0 (off) to 1 (default 0.5).
    #[arg(long)]
    pub steady: Option<f32>,
    /// Videos: encoder quality, lower = better and bigger (default 20).
    #[arg(long)]
    pub video_quality: Option<u8>,
}

impl ConvertArgs {
    fn settings(&self) -> Result<Settings> {
        let mut s = match &self.settings {
            Some(p) => toml::from_str(
                &std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
            )
            .with_context(|| format!("parsing {}", p.display()))?,
            None => Settings::default(),
        };
        if let Some(p) = self.preset {
            p.apply(&mut s);
        }
        if let Some(m) = &self.model {
            s.model = m.clone();
        }
        if self.cpu {
            s.use_gpu = false;
        }
        macro_rules! set {
            ($field:expr, $v:expr) => {
                if let Some(v) = $v {
                    $field = v;
                }
            };
        }
        set!(s.stereo.strength, self.strength);
        set!(s.stereo.convergence, self.convergence);
        set!(s.stereo.edge_mode, self.edge_mode);
        set!(s.depth.detail, self.detail);
        set!(s.depth.curve, self.curve);
        set!(s.depth.smoothing, self.smoothing);
        set!(s.depth.edge_dilation, self.dilation);
        set!(s.output.jpeg_quality, self.quality);
        set!(s.output.max_width, self.max_width);
        set!(s.upscale.mode, self.upscale);
        set!(s.upscale.model, self.upscale_model.clone());
        set!(s.upscale.target_width, self.upscale_width);
        set!(s.video.max_width, self.video_width);
        set!(s.video.steady, self.steady);
        set!(s.video.quality, self.video_quality);
        s.depth.invert |= self.invert;
        s.stereo.swap_eyes |= self.swap_eyes;
        if self.png {
            s.output.format = crate::settings::ImageFormat::Png;
        }
        if let Some(o) = &self.out {
            s.output.out_dir = Some(o.clone());
        }
        if let Some(f) = &self.formats {
            let o = &mut s.output;
            o.sbs_full = f.contains(&FormatArg::Sbs);
            o.sbs_half = f.contains(&FormatArg::Hsbs);
            o.top_bottom = f.contains(&FormatArg::Tb);
            o.anaglyph = f.contains(&FormatArg::Anaglyph);
            o.depth_png = f.contains(&FormatArg::Depth);
            o.glb_mesh = f.contains(&FormatArg::Glb);
        }
        Ok(s)
    }
}

pub fn convert(args: ConvertArgs, lockdown: &Lockdown) -> Result<()> {
    print_lockdown(lockdown);
    let s = args.settings()?;
    let inputs = pipeline::collect_inputs(&args.inputs);
    anyhow::ensure!(!inputs.is_empty(), "no photos or videos found in the given paths");

    let reg = Registry::discover();
    let model = reg.resolve(&s.model)?;
    let t = Instant::now();
    let mut est = DepthEstimator::load(model, &reg.path(model), s.use_gpu)?;
    eprintln!(
        "Model: {} on {} (loaded in {:.1}s)",
        model.name,
        est.device,
        t.elapsed().as_secs_f32()
    );
    if let Some(note) = &est.gpu_note {
        eprintln!("  GPU not used: {note}");
    }

    let photos: Vec<PathBuf> = inputs.iter().filter(|p| !video::is_video(p)).cloned().collect();
    let mut up = load_upscaler(&reg, &s, &photos)?;

    let n = inputs.len();
    let mut failed = 0;
    for (i, input) in inputs.iter().enumerate() {
        let t = Instant::now();
        let result = if video::is_video(input) {
            convert_video(&mut est, input, &s, &format!("[{}/{n}]", i + 1))
        } else {
            pipeline::convert_file(&mut est, up.as_mut(), input, &s)
        };
        match result {
            Ok(files) => {
                quest::record_outputs(&files);
                eprintln!(
                    "[{}/{n}] {} ({:.1}s)",
                    i + 1,
                    input.display(),
                    t.elapsed().as_secs_f32()
                );
                for f in files {
                    eprintln!("      → {}", f.display());
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("[{}/{n}] {} FAILED: {e:#}", i + 1, input.display());
            }
        }
    }
    anyhow::ensure!(failed == 0, "{failed} of {n} files failed");
    Ok(())
}

/// Convert one video, showing progress on the terminal.
fn convert_video(est: &mut DepthEstimator, input: &Path, s: &Settings, tag: &str) -> Result<Vec<PathBuf>> {
    let info = video::probe(input)?;
    let (w, h) = pipeline::video_size(&info, s);
    eprintln!(
        "{tag} {}: {}×{} {:.3} fps, {}{}{} → 3D at {w}×{h} per eye",
        input.display(),
        info.width,
        info.height,
        info.fps(),
        video::format_time(info.duration),
        if info.audio.is_some() { ", with sound" } else { "" },
        if info.hdr { ", HDR (tone-mapped)" } else { "" },
    );
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let t = Instant::now();
    let mut last = Instant::now();
    let mut last_logged = Instant::now();
    let files = pipeline::convert_video(est, input, s, |done, total| {
        if done > 0 && last.elapsed().as_millis() >= 500 {
            last = Instant::now();
            let line = progress_line(done, total, t.elapsed().as_secs_f64(), info.fps());
            if tty {
                eprint!("\r\x1b[2K      {line}");
            } else if last_logged.elapsed().as_secs() >= 30 {
                last_logged = Instant::now();
                eprintln!("      {line}");
            }
        }
        true
    });
    if tty {
        eprint!("\r\x1b[2K");
    }
    let secs = t.elapsed().as_secs_f64();
    if files.is_ok() {
        eprintln!(
            "      {} frames in {} ({:.1} fps)",
            info.frames,
            video::format_time(secs),
            info.frames as f64 / secs.max(1e-3)
        );
    }
    files
}

/// "frame 120/1800 (7%) · 14.2 fps · 1:58 left"
pub fn progress_line(done: u64, total: u64, secs: f64, video_fps: f64) -> String {
    let total = total.max(done).max(1);
    let rate = done as f64 / secs.max(1e-3);
    let left = (total - done) as f64 / rate.max(1e-3);
    format!(
        "frame {done}/{total} ({}%) · {rate:.1} fps ({:.2}× real time) · {} left",
        done * 100 / total,
        rate / video_fps.max(1.0),
        video::format_time(left)
    )
}

/// Load the AI upscaler, but only if upscaling is on and at least one input needs it
/// (reading just the image headers to find out).
fn load_upscaler(reg: &Registry, s: &Settings, inputs: &[PathBuf]) -> Result<Option<Upscaler>> {
    if s.upscale.mode == UpscaleMode::Off {
        return Ok(None);
    }
    // EXIF rotation may swap width and height, so judge by the narrower side.
    let needed = inputs.iter().any(|p| {
        image::image_dimensions(p)
            .map(|(w, h)| pipeline::wants_upscale(w.min(h), s))
            .unwrap_or(true)
    });
    if !needed {
        return Ok(None);
    }
    let info = match reg.resolve_upscaler(&s.upscale.model) {
        Ok(i) => i,
        // Auto mode quietly does without an upscaler when none is built.
        Err(e) if s.upscale.mode == UpscaleMode::Auto && s.upscale.model.is_empty() => {
            eprintln!("Upscaling skipped: {e:#}");
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let t = Instant::now();
    let up = Upscaler::load(info, &reg.upscaler_path(info), s.use_gpu)?;
    eprintln!(
        "Upscaler: {} on {} (loaded in {:.1}s)",
        info.name,
        up.device,
        t.elapsed().as_secs_f32()
    );
    if let Some(note) = &up.gpu_note {
        eprintln!("  GPU not used: {note}");
    }
    Ok(Some(up))
}

pub fn quest(action: QuestAction) -> Result<()> {
    match action {
        QuestAction::Start { usb_only } => {
            if quest::status().is_none() {
                quest::start(!usb_only)?;
                // Give the helper a moment to write its status.
                for _ in 0..30 {
                    if quest::status().is_some_and(|s| s.usb.is_some() || s.wifi.is_some()) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            print_quest_status()
        }
        QuestAction::Stop => {
            quest::stop()?;
            println!("Quest sharing stopped.");
            Ok(())
        }
        QuestAction::Status => print_quest_status(),
    }
}

fn print_quest_status() -> Result<()> {
    let Some(st) = quest::status() else {
        println!("Quest sharing is off. Start it with: stereowright quest start");
        return Ok(());
    };
    println!("Quest sharing is on ({} 3D photos).", st.photos);
    if let Some(u) = &st.usb {
        println!("  {}", u.message);
    }
    if let Some(w) = &st.wifi {
        println!("  {}", w.message);
        if !w.url.is_empty()
            && std::io::IsTerminal::is_terminal(&std::io::stdout())
            && let Ok(code) = crate::qr::Qr::new(&w.url)
        {
            println!("\n  Or look at this with the Quest's QR code scanner:\n");
            print!("{}", code.to_terminal());
        }
        if !w.firewall.is_empty() {
            println!("  Your firewall blocks the headset. Allow your home network once with:\n    {}", w.firewall);
        }
    }
    println!("  Log: journalctl --user -u stereowright-quest-share   Stop: stereowright quest stop");
    Ok(())
}

pub fn models() -> Result<()> {
    let reg = Registry::discover();
    println!("Models folder: {}\n", reg.dir.display());
    for m in &reg.models {
        let ok = reg.is_installed(m);
        println!(
            "{} {:<10} {}\n    {}\n    {}",
            if ok { "✓" } else { "✗" },
            m.id,
            m.name,
            m.notes,
            if ok {
                format!("{}", reg.path(m).display())
            } else {
                format!(
                    "not downloaded: scripts/fetch-models.sh {}",
                    m.fetch.as_deref().unwrap_or("")
                )
            }
        );
    }
    println!("\nAI upscalers (Real-ESRGAN):\n");
    for u in &reg.upscalers {
        let ok = reg.upscaler_installed(u);
        println!(
            "{} {:<20} {} ({}x)\n    {}\n    {}",
            if ok { "✓" } else { "✗" },
            u.id,
            u.name,
            u.scale,
            u.notes,
            if ok {
                format!("{}", reg.upscaler_path(u).display())
            } else {
                format!(
                    "not built: scripts/make-upscalers.sh {}",
                    u.fetch.as_deref().unwrap_or("")
                )
            }
        );
    }
    Ok(())
}

pub fn doctor(lockdown: &Lockdown) -> Result<()> {
    println!("Stereowright {}", env!("CARGO_PKG_VERSION"));
    println!("\n── Privacy");
    print_lockdown(lockdown);
    match sandbox::self_test() {
        Ok(()) => println!("  ✓ self-test: opening IPv4/IPv6 sockets is refused by the kernel"),
        Err(e) => println!("  ✗ self-test: {e}"),
    }
    let ifs = sandbox::visible_interfaces();
    if ifs.is_empty() {
        println!("  ✓ no network interfaces visible (running in an offline namespace)");
    } else {
        println!(
            "  · network interfaces exist on this machine ({}) but this process cannot use them",
            ifs.join(", ")
        );
    }

    println!("\n── Models");
    let reg = Registry::discover();
    println!("  folder: {}", reg.dir.display());
    for m in &reg.models {
        println!(
            "  {} {}",
            if reg.is_installed(m) { "✓" } else { "✗" },
            m.name
        );
    }
    for u in &reg.upscalers {
        println!(
            "  {} {} (upscaler)",
            if reg.upscaler_installed(u) { "✓" } else { "✗" },
            u.name
        );
    }

    println!("\n── Acceleration");
    let Some(model) = reg.best_installed() else {
        println!("  (no model installed; run scripts/fetch-models.sh)");
        return Ok(());
    };
    let t = Instant::now();
    let mut est = DepthEstimator::load(model, &reg.path(model), true)?;
    println!(
        "  {} loaded on {} in {:.1}s",
        model.name,
        est.device,
        t.elapsed().as_secs_f32()
    );
    if let Some(n) = &est.gpu_note {
        println!("  GPU not used: {n}");
    }
    let img = image::RgbImage::from_fn(1600, 1200, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    est.estimate(&img, 518)?; // warm-up
    let t = Instant::now();
    let runs = 3;
    for _ in 0..runs {
        est.estimate(&img, 518)?;
    }
    println!(
        "  inference at 518px: {:.0} ms per image",
        t.elapsed().as_secs_f64() * 1000.0 / runs as f64
    );
    Ok(())
}

pub fn print_settings() -> Result<()> {
    print!("{}", toml::to_string_pretty(&Settings::default())?);
    Ok(())
}

fn print_lockdown(l: &Lockdown) {
    match l {
        Lockdown::Active => eprintln!("🔒 Network access blocked for this process (kernel seccomp filter)"),
        Lockdown::Failed(e) => eprintln!("⚠ Could not install the network block: {e}"),
        Lockdown::Unsupported => eprintln!("⚠ Network block not supported on this OS"),
    }
}
