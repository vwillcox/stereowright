Stereowright @VERSION@ for Linux (x86-64)
=========================================

Turns your photos and videos into 3D for VR headsets, 3D TVs and red/cyan glasses, with
an AI that runs on this computer. Your files are never uploaded.

START
  ./stereowright                   opens the app
  scripts/install.sh               adds it to your app launcher (no root needed);
                                   the launcher starts it with no network at all
  ./stereowright doctor            checks the network block, the models and the GPU

NETWORK BLOCK: ON
  The first thing the app does is ask the Linux kernel (a seccomp filter) to refuse it
  any network connection, for good, and then test that the block works. The window says
  "🔒 Offline: network blocked".

GPU
  NVIDIA cards are used when CUDA 13 and cuDNN 9 are installed (e.g. sudo pacman -S cuda
  cudnn on Arch); otherwise the CPU, more slowly.

VIDEOS
  Need ffmpeg from your distribution: sudo apt install ffmpeg / sudo pacman -S ffmpeg.
  Then: ./stereowright convert clip.mp4

VIEW ON QUEST 3
  Needs python3, adb and openssl (see the user guide). Run scripts/install.sh first.

INCLUDED
  AI models: Depth Anything V2 Small (Apache-2.0, fine for any use) and three
  Real-ESRGAN upscalers (BSD). For more detail, scripts/fetch-models.sh large downloads
  the Large model (non-commercial licence) into the models folder.
  Licences: LICENSE (Stereowright, MIT), THIRD-PARTY-NOTICES.md.

Built on Ubuntu 24.04: needs glibc 2.39 or newer (Ubuntu 24.04, Debian 13, Fedora 40 and
newer, Arch). On older distributions, build from source (see the user guide).
User guide: https://github.com/vwillcox/stereowright/blob/main/docs/USER_GUIDE.md
