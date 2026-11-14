Stereowright turns your photos and videos into 3D for VR headsets, 3D TVs and red/cyan
glasses, using an AI depth model that runs on your own computer. Nothing is uploaded.

Each download is self-contained: the app, the GPU runtime, the **Depth Anything V2 Small**
model and three **Real-ESRGAN** upscalers. The Windows and macOS downloads also include
ffmpeg 9.0.2 for video.

## Downloads

| System | File | GPU | Network block |
|---|---|---|---|
| **Linux** x86-64 (glibc 2.39+: Ubuntu 24.04, Debian 13, Fedora 40 or newer, Arch) | `stereowright-@VERSION@-linux-x86_64.tar.gz` | NVIDIA (CUDA 13 + cuDNN 9 installed), else CPU | ✅ **On**: Linux kernel seccomp filter |
| **macOS** 13.4+ on Apple silicon (M1 or newer) | `stereowright-@VERSION@-macos-arm64.dmg` | Apple GPU / Neural Engine (Core ML) | ✅ **On**: macOS kernel sandbox |
| **Windows** 10 / 11, 64-bit | `stereowright-@VERSION@-windows-x86_64.zip` | Any DirectX 12 GPU (DirectML), else CPU | ⚠️ **Not available**: see below |

### The network block, per system

Stereowright has no networking code, and every release build checks that no networking
library is compiled in. On top of that, it asks the operating system to *enforce* it:

- **Linux:** the first thing the app does is install a seccomp filter that makes the
  kernel refuse it any network socket, for good, and then test that the block works.
  The launcher entry from `scripts/install.sh` also starts it in an empty network
  namespace. The window shows "🔒 Offline: network blocked".
- **macOS:** the first thing the app does is enter a kernel sandbox that denies all network
  connections, even to the Mac itself, then tests it. The included ffmpeg runs inside the
  same sandbox. The window shows "🔒 Offline: network blocked".
- **Windows: no network block.** Windows has no way for a program to lock itself out of
  the network, so on Windows nothing *enforces* the no-network promise. The window shows
  "⚠ No network block on this system". To guarantee it, block the app in Windows
  Defender Firewall: the `README.txt` in the zip has the three PowerShell commands.

`stereowright doctor` (or `stereowright.exe doctor`) shows the state of the block on your
system.

## Install

- **Linux:** unpack, then run `./stereowright`, or `scripts/install.sh` to add it to your
  app launcher (no root). Videos need ffmpeg from your distribution.
- **macOS:** open the DMG and drag Stereowright to Applications. The app is not notarised
  by Apple, so the first time macOS blocks it: close the warning, open **System Settings →
  Privacy & Security**, scroll down and click **Open Anyway**.
- **Windows:** unzip anywhere and double-click `stereowright.exe`. SmartScreen may say it
  "protected your PC" (the app isn't signed with a paid certificate): click **More info →
  Run anyway**. Keep the files in the folder together.

## Good to know

- **"View on Quest 3"** (Wi-Fi/USB sharing to the headset) is **Linux-only** for now. On
  Windows and macOS, copy the `_LR` files to the headset over USB.
- **Speed:** on an M4 Mac mini the depth step takes about 1.5–2 s per photo at the Mac
  default detail (Standard, 518 px) and videos run at about 1.2 s per frame. On Windows,
  DirectML speed depends on the graphics card. Without a GPU everything works, more slowly.
- **Intel Macs** aren't supported (ONNX Runtime has no prebuilt Intel Mac library).
- The larger **Base** and **Large** depth models are not included: their licence is
  non-commercial. Linux users can fetch them with `scripts/fetch-models.sh large`.
- Licences: Stereowright is MIT. The downloads contain third-party software under its own
  licences, including ffmpeg (GPLv3) on Windows and macOS: see `THIRD-PARTY-NOTICES.md`.
