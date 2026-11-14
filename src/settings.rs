//! All user-tunable settings. Shared by the GUI (persisted automatically) and the
//! CLI (`--settings file.toml`, or individual flags).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum EdgeMode {
    /// Tear the image at depth edges and fill the revealed gap with background texture.
    /// Crisper outlines around people/objects.
    Fill,
    /// Stretch pixels across depth edges like a rubber sheet. Smoother, no gaps, softer outlines.
    Stretch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Jpeg,
    Png,
}

impl ImageFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DepthSettings {
    /// AI input resolution (short side, px). Higher = finer detail, slower. Changing it re-runs the AI.
    pub detail: u32,
    /// Flip near/far (for models or images where the depth comes out backwards).
    pub invert: bool,
    /// Everything farther than this (0 = farthest, 1 = nearest) is flattened onto the back plane.
    pub far_clip: f32,
    /// Everything nearer than this is flattened onto the front plane.
    pub near_clip: f32,
    /// Depth curve (gamma). >1 gives the foreground more depth separation, <1 gives the background more.
    pub curve: f32,
    /// Blur the depth map (percent of image width). Hides depth noise and wobbly edges.
    pub smoothing: f32,
    /// Grow foreground objects in the depth map (px per 1000 px of width) so edge
    /// artifacts land on the background instead of on people/objects.
    pub edge_dilation: f32,
}

impl Default for DepthSettings {
    fn default() -> Self {
        Self {
            detail: 770,
            invert: false,
            far_clip: 0.0,
            near_clip: 1.0,
            curve: 1.0,
            smoothing: 0.15,
            edge_dilation: 6.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StereoSettings {
    /// 3D strength: total parallax between nearest and farthest point, as % of image width.
    pub strength: f32,
    /// Which depth sits exactly at the "screen" (0 = farthest point, everything pops out;
    /// 1 = nearest point, everything sits behind the screen like looking through a window).
    pub convergence: f32,
    pub edge_mode: EdgeMode,
    /// (Fill mode) how much a pixel gap must stretch (px) before it is treated as a tear.
    pub tear_threshold: f32,
    /// Swap left/right (for cross-eye free viewing or a player that expects RL).
    pub swap_eyes: bool,
}

impl Default for StereoSettings {
    fn default() -> Self {
        Self {
            strength: 3.0,
            convergence: 0.5,
            edge_mode: EdgeMode::Fill,
            tear_threshold: 1.5,
            swap_eyes: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputSettings {
    /// Full side-by-side (each eye at full resolution, 2:1). Best for VR players.
    pub sbs_full: bool,
    /// Half side-by-side (each eye squeezed to half width). For 3D TVs / some players.
    pub sbs_half: bool,
    /// Top/bottom (left eye on top).
    pub top_bottom: bool,
    /// Red/cyan anaglyph (for checking with cheap 3D glasses on a normal screen).
    pub anaglyph: bool,
    /// The depth map itself (16-bit PNG, white = near).
    pub depth_png: bool,
    /// 3D mesh (.glb) for true 6DoF viewing in a 3D model viewer.
    pub glb_mesh: bool,
    pub format: ImageFormat,
    pub jpeg_quality: u8,
    /// Downscale the input to at most this width before converting (0 = keep original size).
    pub max_width: u32,
    /// Output folder. `None` = a `stereowright/` folder next to each input image.
    pub out_dir: Option<PathBuf>,
    /// Mesh: grid cells along the long side.
    pub mesh_detail: u32,
    /// Mesh: horizontal field of view the photo is assumed to have been taken with (degrees).
    pub mesh_fov: f32,
    /// Mesh: how far the farthest point is, relative to the nearest (nearest = 1 m).
    pub mesh_depth: f32,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            sbs_full: true,
            sbs_half: false,
            top_bottom: false,
            anaglyph: false,
            depth_png: true,
            glb_mesh: false,
            format: ImageFormat::Jpeg,
            jpeg_quality: 95,
            max_width: 0,
            out_dir: None,
            mesh_detail: 512,
            mesh_fov: 60.0,
            mesh_depth: 4.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum UpscaleMode {
    /// Never upscale.
    Off,
    /// Upscale only photos that are small for VR (narrower than 2/3 of the target width).
    Auto,
    /// Upscale every photo that is narrower than the target width.
    Always,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpscaleSettings {
    pub mode: UpscaleMode,
    /// Upscaler id from models/models.toml. Empty = best installed.
    pub model: String,
    /// Width to aim for. The AI result is scaled down to this if it overshoots, and photos
    /// already this wide are left alone. 3840 gives each eye 4K-wide detail on a Quest 3.
    pub target_width: u32,
}

impl Default for UpscaleSettings {
    fn default() -> Self {
        Self {
            mode: UpscaleMode::Auto,
            model: String::new(),
            target_width: 3840,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    /// Width of each eye's picture (px); bigger videos are scaled down first. 0 = original.
    /// 1920 plays smoothly everywhere and converts about 4x faster than 4K.
    pub max_width: u32,
    /// Steadier depth from frame to frame (0 = off … 1 = strongest). Stops the 3D from
    /// "breathing" or flickering, at the cost of depth reacting a little later to change.
    pub steady: f32,
    /// Encoder quality (constant quality; lower = better and bigger files). 18-24 is sensible.
    pub quality: u8,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            max_width: 1920,
            steady: 0.5,
            quality: 20,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Model id from models/models.toml. Empty = best installed model.
    pub model: String,
    /// Use the NVIDIA GPU when available (falls back to CPU automatically).
    pub use_gpu: bool,
    pub upscale: UpscaleSettings,
    pub depth: DepthSettings,
    pub stereo: StereoSettings,
    pub output: OutputSettings,
    pub video: VideoSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model: String::new(),
            use_gpu: true,
            upscale: UpscaleSettings::default(),
            depth: DepthSettings::default(),
            stereo: StereoSettings::default(),
            output: OutputSettings::default(),
            video: VideoSettings::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Preset {
    /// Gentle depth, very comfortable for long viewing.
    Subtle,
    /// Balanced default.
    Standard,
    /// Deep, dramatic 3D.
    Strong,
    /// Subject pops out towards you.
    Popout,
    /// Everything behind the screen, like looking through a window.
    Window,
}

impl Preset {
    pub const ALL: [Preset; 5] = [
        Preset::Subtle,
        Preset::Standard,
        Preset::Strong,
        Preset::Popout,
        Preset::Window,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Subtle => "Subtle",
            Preset::Standard => "Standard",
            Preset::Strong => "Strong",
            Preset::Popout => "Pop-out",
            Preset::Window => "Window",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Preset::Subtle => "Gentle depth, very comfortable for long viewing",
            Preset::Standard => "Balanced depth, a good starting point",
            Preset::Strong => "Deep, dramatic 3D",
            Preset::Popout => "The subject comes out towards you",
            Preset::Window => "Everything sits behind the screen, like a window",
        }
    }

    /// Apply this preset's 3D-effect values, leaving model/output choices alone.
    pub fn apply(self, s: &mut Settings) {
        let (strength, convergence, curve) = match self {
            Preset::Subtle => (1.8, 0.5, 1.0),
            Preset::Standard => (3.0, 0.5, 1.0),
            Preset::Strong => (5.0, 0.45, 1.15),
            Preset::Popout => (3.5, 0.15, 1.2),
            Preset::Window => (3.0, 0.95, 1.0),
        };
        s.stereo.strength = strength;
        s.stereo.convergence = convergence;
        s.depth.curve = curve;
    }
}

/// Selectable AI resolutions (short side; must be multiples of the model's patch size, 14).
pub const DETAIL_LEVELS: [(u32, &str); 5] = [
    (392, "Fast"),
    (518, "Standard"),
    (770, "High"),
    (1022, "Very high"),
    (1274, "Ultra"),
];
