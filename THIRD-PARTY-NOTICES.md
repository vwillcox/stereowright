# Third-party software and models in Stereowright releases

Stereowright itself is MIT licensed (see `LICENSE`). The ready-made downloads also contain
the following. Building from source fetches the same things (see `docs/INSTALL.md`).

## In every download

**ONNX Runtime** 1.31 (Microsoft), the engine that runs the AI models. MIT licence.
Built into the program (Windows and macOS) or next to it as `libonnxruntime_providers_*.so`
(Linux, for NVIDIA GPUs). Prebuilt by pyke for the `ort` Rust crate.
<https://github.com/microsoft/onnxruntime>

**Depth Anything V2 · Small** (`models/depth-anything-v2-small.onnx`), the depth model.
Apache-2.0 licence, free for any use including commercial.
By Lihe Yang et al. <https://github.com/DepthAnything/Depth-Anything-V2>;
ONNX conversion by onnx-community <https://huggingface.co/onnx-community/depth-anything-v2-small>.
The larger Base and Large models are *not* included: they are licensed CC-BY-NC-4.0
(non-commercial use only).

**Real-ESRGAN** x4plus, x2plus and general-x4v3 (`models/realesr*.onnx`), the AI upscalers.
BSD-3-Clause licence. By Xintao Wang et al. <https://github.com/xinntao/Real-ESRGAN>.
Converted to ONNX by `scripts/make-upscalers.sh` from the official weights.

**Rust libraries** compiled into the program (egui/eframe, image, ndarray, clap, serde,
rfd and others). All are under permissive licences (MIT, Apache-2.0, BSD, Zlib, ISC,
Unicode). The full list is in `Cargo.lock` in the source code.

## Windows download only

**DirectML** (`DirectML.dll`), Microsoft's GPU library for machine learning, redistributed
under Microsoft's DirectML licence terms. <https://github.com/microsoft/DirectML>

**FFmpeg 9.0.2** (`ffmpeg.exe`, `ffprobe.exe`), used only to read and write videos.
Licensed under the **GNU GPL version 3** (this build includes x264 and other GPL parts);
the licence text is in `FFMPEG-LICENSE.txt`. Static "essentials" build by Gyan Doshi:
<https://www.gyan.dev/ffmpeg/builds/> (build configuration in `FFMPEG-README.txt`).

## macOS download only

**FFmpeg 9.0.2** (`Stereowright.app/Contents/MacOS/ffmpeg` and `ffprobe`), used only to
read and write videos. Licensed under the **GNU GPL version 3** (this build includes x264
and other GPL parts); the licence text is in `FFMPEG-LICENSE.txt`. Static build by
Martin Riedl: <https://ffmpeg.martin-riedl.de/> (build scripts:
<https://git.martin-riedl.de/ffmpeg/build-script>).

## FFmpeg source code

FFmpeg is separate software that Stereowright runs as its own program; Stereowright's
code is not linked with it. Its source code is available from
<https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz>, and the source of the libraries in each
build from the builders' pages above. If any of these links stop working, open an issue at
<https://github.com/vwillcox/stereowright/issues> and the exact source will be provided.
