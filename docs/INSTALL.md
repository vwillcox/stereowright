# Installing Stereowright

The quickest way in is a [ready-made download](#ready-made-downloads) for Linux, Windows or
macOS. The rest of this guide builds Stereowright from source on Linux. It assumes you're
happy typing a few commands in a terminal, and is written for Arch Linux (where Stereowright
is developed); notes for other distributions are given where they differ.

- [Ready-made downloads](#ready-made-downloads)
- [What you need](#what-you-need)
- [1. Install the build tools](#1-install-the-build-tools)
- [2. Set up the NVIDIA GPU (optional, recommended)](#2-set-up-the-nvidia-gpu-optional-recommended)
- [3. Get the code](#3-get-the-code)
- [4. Download the AI models](#4-download-the-ai-models)
- [5. Build](#5-build)
- [6. Check everything works](#6-check-everything-works)
- [7. Add it to your app launcher (optional)](#7-add-it-to-your-app-launcher-optional)
- [8. Optional extras](#8-optional-extras)
- [Updating](#updating)
- [Uninstalling](#uninstalling)
- [Troubleshooting](#troubleshooting)

## Ready-made downloads

Get the file for your system from the
[Releases page](https://github.com/vwillcox/stereowright/releases). Each one includes the
app, the GPU runtime, the Depth Anything V2 Small model (Apache-2.0) and the three
Real-ESRGAN upscalers. The larger Base and Large models are non-commercial and not included.

### Linux (x86-64)

`stereowright-<version>-linux-x86_64.tar.gz`, for distributions from 2024 onwards
(glibc 2.39+: Ubuntu 24.04, Debian 13, Fedora 40 and newer, Arch). On older ones,
build from source as below.

```bash
tar xf stereowright-*-linux-x86_64.tar.gz
stereowright-*-linux-x86_64/stereowright doctor      # network block, models, GPU
stereowright-*-linux-x86_64/scripts/install.sh       # optional: app launcher entry
```

The network block is the same as when built from source. NVIDIA GPUs are used when CUDA 13
and cuDNN 9 are installed ([step 2](#2-set-up-the-nvidia-gpu-optional-recommended));
otherwise the CPU. Videos need ffmpeg from your distribution.

### macOS (Apple silicon, macOS 13.4 or newer)

1. Open `stereowright-<version>-macos-arm64.dmg` and drag **Stereowright** into
   **Applications**.
2. The app isn't notarised by Apple (that needs a paid developer account), so the first
   time you open it macOS refuses. Close the message, open **System Settings → Privacy &
   Security**, scroll down and click **Open Anyway**. After that it opens normally.

The app puts itself in a macOS kernel sandbox that refuses all network connections (and
shows **Offline: network blocked**). The AI runs on the Apple GPU and Neural Engine through
Core ML, and ffmpeg 9.0.2 is included for video. Detail defaults to Standard (518 px) on
Macs: High works but is 5 to 10 times slower. Intel Macs aren't supported. The command
line is at `/Applications/Stereowright.app/Contents/MacOS/stereowright`.

### Windows 10 and 11 (64-bit)

1. Unzip `stereowright-<version>-windows-x86_64.zip` anywhere and keep its files together.
2. Double-click `stereowright.exe`. If Windows says it "protected your PC" (the app isn't
   signed with a paid certificate), click **More info → Run anyway**.

The GPU is any DirectX 12 graphics card (NVIDIA, AMD or Intel) through DirectML, and
ffmpeg 9.0.2 is included for video.

**There is no network block on Windows.** Windows has no way for a program to lock itself
out of the network, so nothing *enforces* that Stereowright stays offline: you're relying on
it having no networking code (every release is checked for that). The app says **No network
block on this system**. To enforce it, open PowerShell as administrator in the unzipped
folder and run:

```powershell
New-NetFirewallRule -DisplayName "Stereowright (block)" -Direction Outbound -Action Block -Program "$PWD\stereowright.exe"
New-NetFirewallRule -DisplayName "Stereowright ffmpeg (block)" -Direction Outbound -Action Block -Program "$PWD\ffmpeg.exe"
New-NetFirewallRule -DisplayName "Stereowright ffprobe (block)" -Direction Outbound -Action Block -Program "$PWD\ffprobe.exe"
```

The rules are tied to the folder's location (move it first). Remove them with
`Remove-NetFirewallRule -DisplayName "Stereowright*"`.

**View on Quest 3** sharing is Linux-only for now: on Windows and macOS, copy the `_LR`
files to the headset over USB.

## What you need

| | Required | Notes |
|---|---|---|
| **Operating system** | Linux, x86-64 | Wayland or X11. Developed on Arch Linux with Hyprland. Windows and macOS are not supported. |
| **Rust** | 1.92 or newer | From your distribution or [rustup](https://rustup.rs). |
| **C toolchain, `pkg-config`, OpenSSL headers** | yes, to build | Used only while building, to download ONNX Runtime. Not part of the app. |
| **Disk space** | about 3 GB | ~1.7 GB build folder, ~0.8 GB depth models, ~0.1 GB upscalers. |
| **NVIDIA GPU** | no, but recommended | CUDA 13 and cuDNN 9. Without one the app runs on the CPU: the AI step is about 60× slower with the Large model, so use the Small model there. 8 GB of video memory or more is comfortable. |
| **`ffmpeg`** | for videos | Provides `ffmpeg` and `ffprobe`. |
| **`python3`, `adb`, `openssl`** | for Quest sharing | `imagemagick` too, optionally, for gallery thumbnails. |
| **`uv`** | for AI upscaling | Only used once, to build the upscaler models. |

## 1. Install the build tools

**Arch Linux / Manjaro / EndeavourOS / Omarchy:**

```bash
sudo pacman -S --needed base-devel git rust ffmpeg
```

If you already use `rustup`, skip `rust` and run `rustup update stable` instead.

**Debian / Ubuntu / Linux Mint / Pop!_OS** (not tested by the author):

```bash
sudo apt install build-essential git pkg-config libssl-dev curl ffmpeg
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # installs a current Rust
```

**Fedora** (not tested by the author):

```bash
sudo dnf install gcc gcc-c++ git pkgconf-pkg-config openssl-devel
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

For videos, install `ffmpeg` from RPM Fusion: Fedora's own `ffmpeg-free` has no H.264 encoder.

Check the Rust version afterwards: `rustc --version` must say 1.92 or newer.

## 2. Set up the NVIDIA GPU (optional, recommended)

Stereowright uses NVIDIA's CUDA through ONNX Runtime. You need a recent NVIDIA driver,
**CUDA 13** and **cuDNN 9**.

```bash
sudo pacman -S --needed cuda cudnn      # Arch
```

On other distributions, install CUDA 13 and cuDNN 9 from your distribution or from NVIDIA.
`nvidia-smi` should list your GPU.

**No NVIDIA GPU (AMD, Intel, or none)?** Build without CUDA in step 5:
`cargo build --release --no-default-features`. Everything works, only slower. Use the
Small model, which is much quicker on a CPU.

## 3. Get the code

```bash
git clone https://github.com/vwillcox/stereowright.git
cd stereowright
```

The rest of this guide runs commands from inside this folder.

## 4. Download the AI models

The models are not included in the repository (they are large and have their own
licences). This script downloads them from Hugging Face and checks every file against a
SHA-256 hash pinned in the script:

```bash
scripts/fetch-models.sh small large
```

| Choice | Size | Licence | Good for |
|---|---|---|---|
| `small` | 99 MB | Apache-2.0 | Speed, CPUs, big batches, **commercial use** |
| `base` | 195 MB | CC-BY-NC-4.0 | A middle ground |
| `large` | 669 MB | CC-BY-NC-4.0 | Best detail (recommended with an NVIDIA GPU) |

`scripts/fetch-models.sh` with no arguments downloads only `small`; `all` downloads all three.
The app uses the best installed model unless you choose another. Base and Large are for
**non-commercial use only**.

The files go into `models/`. This is the only step that needs the internet, and you run it
yourself; the app never downloads anything.

## 5. Build

```bash
cargo build --release                         # with NVIDIA GPU support
cargo build --release --no-default-features   # CPU only
```

The first build takes a few minutes. It downloads Rust libraries from crates.io and the
ONNX Runtime engine (through the `ort` crate). That download happens at build time only.

The app is now at `./target/release/stereowright`.

## 6. Check everything works

```bash
./target/release/stereowright doctor
```

You should see something like:

```text
── Privacy
🔒 Network access blocked for this process (kernel seccomp filter)
  ✓ self-test: opening IPv4/IPv6 sockets is refused by the kernel

── Models
  ✓ Depth Anything V2 · Small
  ✓ Depth Anything V2 · Large

── Acceleration
  Depth Anything V2 · Large loaded on NVIDIA GPU (CUDA) in 2.0s
  inference at 518px: 124 ms per image
```

If it says **loaded on CPU** and you expected the GPU, the line below it explains why
(see [Troubleshooting](#troubleshooting)).

You can also check that no network libraries were compiled in:

```bash
scripts/audit-network.sh
```

Then open the app:

```bash
./target/release/stereowright
```

## 7. Add it to your app launcher (optional)

```bash
scripts/install.sh          # with NVIDIA GPU support
scripts/install.sh --cpu    # without (no NVIDIA GPU)
```

This builds a release copy and installs it for your user only (no `sudo`):

| Where | What |
|---|---|
| `~/.local/share/stereowright/bin/` | the app and its GPU runtime libraries |
| `~/.local/share/stereowright/models/` | the AI models (copied from `./models`) |
| `~/.local/share/stereowright/quest/` | the Quest sharing helper and 3D viewer |
| `~/.local/bin/stereowright` | the command you type in a terminal |
| `~/.local/share/applications/stereowright.desktop` | the app-launcher entry |

The launcher entry starts the app through `scripts/run-offline.sh`, which runs it in a
private network namespace with no network devices at all, on top of the app's own block.

If `~/.local/bin` is not on your `PATH`, the script tells you. Add it in your shell's
start-up file, for example `export PATH="$HOME/.local/bin:$PATH"` in `~/.bashrc`.

## 8. Optional extras

### AI upscaling (Real-ESRGAN)

Small photos look soft in a headset. The upscaler models are built on your computer from
the official Real-ESRGAN weights (BSD-3-Clause). You need [`uv`](https://docs.astral.sh/uv/)
(`sudo pacman -S uv` on Arch):

```bash
scripts/make-upscalers.sh       # all three, or name one: x4plus | x2plus | general-x4v3
```

This downloads the official weights (SHA-256 checked) and, into a throwaway `uv`
environment, the Python packages needed for the conversion (PyTorch for CPU, ONNX). It writes
`.onnx` files to `models/`. Nothing is installed system-wide. If you used
`scripts/install.sh` before, run it again to copy the new models across.

### Videos

Install `ffmpeg` (it provides `ffprobe` too). Check with `ffmpeg -version`. With an NVIDIA
GPU, Stereowright uses the GPU video encoder (NVENC) when `ffmpeg` supports it, and falls
back to `libx264`/`libx265` otherwise.

### Viewing on a Meta Quest

The Quest sharing helper is a separate small program. It needs:

```bash
sudo pacman -S --needed python android-tools openssl imagemagick   # Arch
sudo apt install python3 adb openssl imagemagick                   # Debian/Ubuntu
```

- **USB cable:** turn on developer mode for your headset once. Follow Meta's guide
  *Set up your device for development* (it needs a free Meta developer account). Plug the
  headset in, put it on, and accept **Allow USB debugging**, ticking *Always allow from
  this computer*. Check with `adb devices`.
- **Home Wi-Fi:** if you use the `ufw` firewall, the app shows the one command needed to
  let your home network reach port 8781, for example
  `sudo ufw allow from 192.168.1.0/24 to any port 8781 proto tcp`.

How to use it is in the [user guide](USER_GUIDE.md#viewing-on-a-meta-quest).

## Updating

```bash
cd stereowright
git pull
cargo build --release
scripts/install.sh            # only if you installed it to the launcher
```

Your settings, models and Quest links are kept.

## Uninstalling

```bash
scripts/install.sh --uninstall
```

This removes the app, the launcher entry and the command, and stops Quest sharing if it is
running. It leaves your models and settings in `~/.local/share/stereowright/` so a
reinstall is quick. Delete that folder by hand if you want the space back. Your converted
photos (the `stereowright/` folders next to your photos) are never touched.

## Troubleshooting

**`doctor` says "GPU not used: CUDA not available … Is cuDNN 9 installed?"**
Install cuDNN 9 (`sudo pacman -S cudnn`) and check that `nvidia-smi` works. Make sure the
build was made with GPU support (plain `cargo build --release`, not
`--no-default-features`). If you installed with `scripts/install.sh`, run it again after
rebuilding: it copies the GPU runtime libraries next to the app.

**"no depth model is installed"**
Run `scripts/fetch-models.sh small` (or `small large`). The app looks for models in
`$STEREOWRIGHT_MODELS`, then a `models/` folder next to the app (or the source folder when
run from `target/`), then the current folder, then `~/.local/share/stereowright/models/`.

**Out of GPU memory at the highest Detail settings**
Stereowright retries automatically at a lower resolution and keeps that limit for the
session. Choose a lower **Detail** level to avoid the retry.

**"Open photos…" does nothing**
The file picker uses the desktop portal (`xdg-desktop-portal`). Install it together with a
backend for your desktop (for example `xdg-desktop-portal-gtk`, `-kde`, `-gnome` or
`-hyprland`). Meanwhile you can drag photos onto the window or pass them on the command
line: `stereowright photo1.jpg photo2.jpg`.

**The build fails in `openssl-sys`**
Install the OpenSSL development files and `pkg-config` (`libssl-dev pkg-config` on
Debian/Ubuntu, `openssl-devel pkgconf-pkg-config` on Fedora).

**"⚠ Could not install the network block"**
Your kernel lacks seccomp support, which is very rare on desktop Linux. The app still works
without the kernel block. Use `scripts/run-offline.sh` to run it in an offline namespace.

**Videos: "running ffprobe (install ffmpeg to convert videos)"**
Install `ffmpeg`. The video features call the system's `ffmpeg` and `ffprobe`.
