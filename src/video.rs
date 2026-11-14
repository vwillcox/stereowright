//! Video in and out, through the system's `ffmpeg`/`ffprobe` run as child processes.
//!
//! Privacy: the children inherit Stereowright's kernel network block (seccomp filters are
//! inherited across fork/exec), every input is opened with an explicit `file:` URL and a
//! `file,pipe`-only protocol whitelist, and frames travel through pipes. Nothing is
//! uploaded, and no temporary copies of the video are made.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::OnceLock;
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use image::RgbImage;
use serde_json::Value;

pub const VIDEO_EXTENSIONS: [&str; 8] = ["mp4", "m4v", "mov", "mkv", "webm", "avi", "mts", "m2ts"];

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoInfo {
    /// Display size: phone rotation already applied.
    pub width: u32,
    pub height: u32,
    /// Frame rate as a fraction (e.g. 30000/1001).
    pub fps_num: u32,
    pub fps_den: u32,
    /// Seconds.
    pub duration: f64,
    /// Number of frames Stereowright will produce (constant frame rate).
    pub frames: u64,
    /// Codec of the first audio track, if any.
    pub audio: Option<String>,
    /// HDR (HLG/PQ) video, e.g. from recent phones: tone-mapped to normal range on decode.
    pub hdr: bool,
}

impl VideoInfo {
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_den.max(1) as f64
    }
}

/// `m:ss` (or `h:mm:ss`).
pub fn format_time(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// ffmpeg treats `name:...` as a protocol; `file:` pins it to the local file.
fn file_url(path: &Path) -> String {
    format!("file:{}", path.display())
}

/// ffmpeg/ffprobe: the copy bundled next to the executable (Windows and macOS releases),
/// otherwise whichever is on the PATH.
fn tool(name: &str) -> std::path::PathBuf {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join(&file)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| file.into())
}

fn ffmpeg() -> Command {
    let mut c = Command::new(tool("ffmpeg"));
    c.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
    c
}

/// Is ffmpeg installed? (Videos need it; photos don't.)
pub fn ffmpeg_available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let ok = |p: &str| {
            Command::new(tool(p))
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        ok("ffmpeg") && ok("ffprobe")
    })
}

fn need_ffmpeg() -> Result<()> {
    if ffmpeg_available() {
        Ok(())
    } else if cfg!(target_os = "linux") {
        bail!("videos need ffmpeg, which isn't installed (e.g. sudo pacman -S ffmpeg or sudo apt install ffmpeg)")
    } else {
        bail!("videos need ffmpeg, which should be next to stereowright; reinstall the release, or install ffmpeg")
    }
}

fn parse_rate(s: &str) -> Option<(u32, u32)> {
    let (n, d) = s.split_once('/')?;
    let (n, d): (u32, u32) = (n.trim().parse().ok()?, d.trim().parse().ok()?);
    let fps = n as f64 / d.max(1) as f64;
    (d > 0 && (1.0..=240.0).contains(&fps)).then_some((n, d))
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

pub fn probe(path: &Path) -> Result<VideoInfo> {
    need_ffmpeg()?;
    let out = Command::new(tool("ffprobe"))
        .args(["-v", "error", "-protocol_whitelist", "file", "-print_format", "json"])
        .args(["-show_streams", "-show_format", "-i"])
        .arg(file_url(path))
        .stdin(Stdio::null())
        .output()
        .context("running ffprobe (install ffmpeg to convert videos)")?;
    if !out.status.success() {
        bail!(
            "not a readable video: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_probe(&serde_json::from_slice(&out.stdout).context("reading ffprobe output")?)
}

fn parse_probe(j: &Value) -> Result<VideoInfo> {
    let streams = j["streams"].as_array().cloned().unwrap_or_default();
    let v = streams
        .iter()
        .find(|s| {
            s["codec_type"] == "video"
                // Cover art inside audio files is a "video" stream too.
                && s["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 0
        })
        .ok_or_else(|| anyhow!("no video track"))?;
    let (mut w, mut h) = (
        v["width"].as_u64().unwrap_or(0) as u32,
        v["height"].as_u64().unwrap_or(0) as u32,
    );
    anyhow::ensure!(w > 0 && h > 0, "video has no picture size");

    // Phones store portrait video as landscape plus a rotation flag; ffmpeg applies it.
    let rotation = v["side_data_list"]
        .as_array()
        .and_then(|l| l.iter().find_map(|d| num(&d["rotation"])))
        .or_else(|| num(&v["tags"]["rotate"]))
        .unwrap_or(0.0);
    if (rotation.abs().round() as i64) % 180 == 90 {
        std::mem::swap(&mut w, &mut h);
    }

    let (fps_num, fps_den) = v["avg_frame_rate"]
        .as_str()
        .and_then(parse_rate)
        .or_else(|| v["r_frame_rate"].as_str().and_then(parse_rate))
        .unwrap_or((30, 1));
    let fps = fps_num as f64 / fps_den as f64;
    let duration = num(&v["duration"])
        .or_else(|| num(&j["format"]["duration"]))
        .filter(|d| *d > 0.0);
    let nb_frames = num(&v["nb_frames"]).filter(|n| *n > 0.0);
    let (duration, frames) = match (duration, nb_frames) {
        (Some(d), _) => (d, (d * fps).round().max(1.0) as u64),
        (None, Some(n)) => (n / fps, n as u64),
        (None, None) => bail!("can't tell how long the video is"),
    };

    let audio = streams
        .iter()
        .find(|s| s["codec_type"] == "audio")
        .map(|s| s["codec_name"].as_str().unwrap_or("unknown").to_string());
    let hdr = matches!(
        v["color_transfer"].as_str(),
        Some("smpte2084" | "arib-std-b67")
    );
    Ok(VideoInfo {
        width: w,
        height: h,
        fps_num,
        fps_den,
        duration,
        frames,
        audio,
        hdr,
    })
}

/// Size each eye's picture is made at: at most `max_width` wide (0 = original), even sides.
pub fn work_size(info: &VideoInfo, max_width: u32) -> (u32, u32) {
    let (w, h) = (info.width, info.height);
    let (w, h) = if max_width > 0 && w > max_width {
        (max_width, (h as f64 * max_width as f64 / w as f64).round() as u32)
    } else {
        (w, h)
    };
    ((w & !1).max(2), (h & !1).max(2))
}

/// ffmpeg filters turning decoded video into `w`×`h` RGB frames.
fn picture_filters(info: &VideoInfo, w: u32, h: u32) -> String {
    let mut f = String::new();
    if info.hdr {
        // Tone-map HDR to normal (BT.709) range, or the 3D video looks washed out.
        f.push_str(
            "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
             tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p,",
        );
    }
    f.push_str(&format!(
        "scale={w}:{h}:flags=lanczos:in_color_matrix=auto:in_range=auto,setsar=1,format=rgb24"
    ));
    f
}

/// Collect a child's stderr in the background (so a chatty child can't block on a full pipe).
fn drain(mut r: impl Read + Send + 'static) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        s
    })
}

fn last_lines(s: &str) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join(" / ")
}

/// Decodes a video into `w`×`h` RGB frames at a constant frame rate.
pub struct Decoder {
    child: Child,
    out: ChildStdout,
    err: Option<JoinHandle<String>>,
    w: u32,
    h: u32,
}

impl Decoder {
    pub fn open(path: &Path, info: &VideoInfo, w: u32, h: u32) -> Result<Self> {
        need_ffmpeg()?;
        let vf = format!(
            "fps={}/{},{}",
            info.fps_num,
            info.fps_den,
            picture_filters(info, w, h)
        );
        let mut child = ffmpeg()
            .args(["-protocol_whitelist", "file", "-i"])
            .arg(file_url(path))
            .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-vf", &vf])
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting ffmpeg (install ffmpeg to convert videos)")?;
        let out = child.stdout.take().expect("piped");
        let err = Some(drain(child.stderr.take().expect("piped")));
        Ok(Self { child, out, err, w, h })
    }

    /// The next frame, or `None` at the end of the video.
    pub fn next_frame(&mut self) -> Result<Option<RgbImage>> {
        let mut buf = vec![0u8; self.w as usize * self.h as usize * 3];
        let mut got = 0;
        while got < buf.len() {
            match self.out.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("reading decoded frames"),
            }
        }
        if got == 0 {
            let status = self.child.wait()?;
            if !status.success() {
                let msg = self.err.take().and_then(|h| h.join().ok()).unwrap_or_default();
                bail!("decoding the video failed: {}", last_lines(&msg));
            }
            return Ok(None);
        }
        anyhow::ensure!(got == buf.len(), "the video ended in the middle of a frame");
        Ok(Some(RgbImage::from_raw(self.w, self.h, buf).expect("size matches")))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One still frame `secs` into the video, at `w`×`h` (for previews).
#[allow(dead_code)] // for the app's video preview (next step)
pub fn frame_at(path: &Path, info: &VideoInfo, secs: f64, w: u32, h: u32) -> Result<RgbImage> {
    need_ffmpeg()?;
    let secs = secs.clamp(0.0, (info.duration - 0.5 / info.fps()).max(0.0));
    let out = ffmpeg()
        .args(["-ss", &format!("{secs:.3}")])
        .args(["-protocol_whitelist", "file", "-i"])
        .arg(file_url(path))
        .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-frames:v", "1"])
        .args(["-vf", &picture_filters(info, w, h)])
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
        .stdin(Stdio::null())
        .output()
        .context("running ffmpeg")?;
    let need = w as usize * h as usize * 3;
    if !out.status.success() || out.stdout.len() < need {
        bail!(
            "could not read a frame at {}: {}",
            format_time(secs),
            last_lines(&String::from_utf8_lossy(&out.stderr))
        );
    }
    let mut px = out.stdout;
    px.truncate(need);
    Ok(RgbImage::from_raw(w, h, px).expect("size matches"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb,
    Gray,
}

impl PixelFormat {
    fn ffmpeg_name(self) -> &'static str {
        match self {
            PixelFormat::Rgb => "rgb24",
            PixelFormat::Gray => "gray",
        }
    }
    fn bytes(self) -> usize {
        match self {
            PixelFormat::Rgb => 3,
            PixelFormat::Gray => 1,
        }
    }
}

/// Does this ffmpeg encoder actually work here? (NVENC needs an NVIDIA GPU and driver.)
fn encoder_works(name: &'static str) -> bool {
    static CACHE: OnceLock<std::sync::Mutex<Vec<(&'static str, bool)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(&(_, ok)) = cache.lock().unwrap().iter().find(|(n, _)| *n == name) {
        return ok;
    }
    let ok = ffmpeg()
        .args(["-f", "lavfi", "-i", "color=c=gray:s=320x240:r=30:d=0.1"])
        .args(["-frames:v", "2", "-c:v", name, "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    log::debug!("video encoder {name}: {}", if ok { "works" } else { "unavailable" });
    cache.lock().unwrap().push((name, ok));
    ok
}

/// Encoder settings for a `w`×`h` video. H.264 plays everywhere but stops at 4096 px, so
/// wider videos (full side-by-side from 4K) use HEVC, which the Quest 3 also plays.
fn codec_args(w: u32, h: u32, quality: u8) -> Vec<String> {
    let hevc = w > 4096 || h > 4096;
    let q = quality.clamp(10, 40).to_string();
    let v: Vec<&str> = match (hevc, encoder_works(if hevc { "hevc_nvenc" } else { "h264_nvenc" })) {
        (false, true) => vec![
            "-c:v", "h264_nvenc", "-preset", "p5", "-tune", "hq", "-profile:v", "high",
            "-rc", "vbr", "-cq", &q, "-b:v", "0",
        ],
        (true, true) => vec![
            "-c:v", "hevc_nvenc", "-preset", "p5", "-tune", "hq", "-profile:v", "main",
            "-rc", "vbr", "-cq", &q, "-b:v", "0", "-tag:v", "hvc1",
        ],
        (false, false) => vec!["-c:v", "libx264", "-preset", "medium", "-crf", &q],
        (true, false) => vec!["-c:v", "libx265", "-preset", "medium", "-crf", &q, "-tag:v", "hvc1"],
    };
    v.into_iter().map(String::from).collect()
}

/// Writes frames to an MP4 through ffmpeg. The file is written as `<name>.part` and only
/// renamed when complete, so a stopped or failed conversion never leaves a broken video
/// where players (or the Quest viewer) would pick it up.
pub struct Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
    err: Option<JoinHandle<String>>,
    part: PathBuf,
    path: PathBuf,
    frame_bytes: usize,
    done: bool,
}

pub struct EncoderOptions<'a> {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub info: &'a VideoInfo,
    /// Copy the sound from this video.
    pub audio_from: Option<&'a Path>,
    pub quality: u8,
}

impl Encoder {
    pub fn create(path: &Path, o: &EncoderOptions) -> Result<Self> {
        need_ffmpeg()?;
        anyhow::ensure!(o.width.is_multiple_of(2) && o.height.is_multiple_of(2), "video sides must be even");
        let mut part = path.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);

        let mut cmd = ffmpeg();
        cmd.arg("-y")
            .args(["-f", "rawvideo", "-pix_fmt", o.format.ffmpeg_name()])
            .args(["-s", &format!("{}x{}", o.width, o.height)])
            .args(["-framerate", &format!("{}/{}", o.info.fps_num, o.info.fps_den)])
            .args(["-i", "pipe:0"]);
        let audio = o.audio_from.filter(|_| o.info.audio.is_some());
        if let Some(src) = audio {
            cmd.args(["-protocol_whitelist", "file", "-i"])
                .arg(file_url(src))
                .args(["-map", "0:v:0", "-map", "1:a:0?"]);
        }
        cmd.args([
            "-vf",
            "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p",
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-color_range",
            "tv",
        ])
        .args(codec_args(o.width, o.height, o.quality));
        if audio.is_some() {
            // AAC and MP3 fit in MP4 as they are; anything else becomes AAC.
            match o.info.audio.as_deref() {
                Some("aac" | "mp3") => cmd.args(["-c:a", "copy"]),
                _ => cmd.args(["-c:a", "aac", "-b:a", "192k"]),
            };
        }
        cmd.args(["-movflags", "+faststart", "-f", "mp4"])
            .arg(file_url(&part))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("starting ffmpeg (install ffmpeg to convert videos)")?;
        let stdin = child.stdin.take();
        let err = Some(drain(child.stderr.take().expect("piped")));
        Ok(Self {
            child,
            stdin,
            err,
            part,
            path: path.to_path_buf(),
            frame_bytes: o.width as usize * o.height as usize * o.format.bytes(),
            done: false,
        })
    }

    fn failure(&mut self) -> anyhow::Error {
        let _ = self.stdin.take();
        let _ = self.child.wait();
        let msg = self.err.take().and_then(|h| h.join().ok()).unwrap_or_default();
        anyhow!(
            "encoding {} failed: {}",
            self.path.file_name().unwrap_or_default().to_string_lossy(),
            last_lines(&msg)
        )
    }

    pub fn write(&mut self, px: &[u8]) -> Result<()> {
        assert_eq!(px.len(), self.frame_bytes, "frame size");
        let ok = self.stdin.as_mut().is_some_and(|s| s.write_all(px).is_ok());
        if ok { Ok(()) } else { Err(self.failure()) }
    }

    /// Finish the file and move it into place.
    pub fn finish(mut self) -> Result<PathBuf> {
        drop(self.stdin.take()); // end of input
        let status = self.child.wait()?;
        if !status.success() {
            return Err(self.failure());
        }
        std::fs::rename(&self.part, &self.path)
            .with_context(|| format!("saving {}", self.path.display()))?;
        self.done = true;
        Ok(self.path.clone())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.done {
            // Stopped or failed: don't leave half a video behind.
            let _ = self.stdin.take();
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ffprobe_json() {
        let j: Value = serde_json::from_str(
            r#"{"streams":[
                {"codec_type":"video","width":1920,"height":1080,"avg_frame_rate":"30000/1001",
                 "duration":"10.010","nb_frames":"300","color_transfer":"arib-std-b67",
                 "side_data_list":[{"side_data_type":"Display Matrix","rotation":-90}]},
                {"codec_type":"audio","codec_name":"aac"}],
                "format":{"duration":"10.03"}}"#,
        )
        .unwrap();
        let i = parse_probe(&j).unwrap();
        assert_eq!((i.width, i.height), (1080, 1920), "portrait phone video");
        assert_eq!((i.fps_num, i.fps_den), (30000, 1001));
        assert_eq!(i.frames, 300);
        assert_eq!(i.audio.as_deref(), Some("aac"));
        assert!(i.hdr);
        assert_eq!(work_size(&i, 720), (720, 1280));
        assert_eq!(work_size(&i, 0), (1080, 1920));
    }

    #[test]
    fn rejects_silly_rates_and_formats_time() {
        assert_eq!(parse_rate("0/0"), None);
        assert_eq!(parse_rate("90000/1"), None);
        assert_eq!(parse_rate("25/1"), Some((25, 1)));
        assert_eq!(format_time(65.4), "1:05");
        assert_eq!(format_time(3725.0), "1:02:05");
        assert!(is_video(Path::new("a/B.MOV")));
        assert!(!is_video(Path::new("a/b.jpg")));
    }
}
