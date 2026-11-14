Stereowright @VERSION@ for Windows 10 and 11 (64-bit)
=====================================================

Turns your photos and videos into 3D for VR headsets, 3D TVs and red/cyan glasses, with
an AI that runs on this computer. Your files are never uploaded.

START
  Double-click stereowright.exe. Windows may say it "protected your PC", because the app
  isn't signed with a paid certificate: click "More info", then "Run anyway".
  Keep the files in this folder together (the app needs DirectML.dll, ffmpeg.exe,
  ffprobe.exe and the models folder next to it).

NETWORK BLOCK: NOT AVAILABLE ON WINDOWS
  On Linux and macOS, Stereowright makes the operating system refuse it any network
  connection as soon as it starts, and checks that it worked. Windows has no way for a
  program to lock itself out of the network like that, so this version cannot.
  The app still has no networking code (every release is checked for it) and only reads
  and writes files on this computer, but on Windows nothing *enforces* that. The window
  says "No network block on this system".

  For a guarantee, block it in Windows Defender Firewall. Open PowerShell as
  administrator, change to this folder (cd "path\to\this\folder"), and run:

    New-NetFirewallRule -DisplayName "Stereowright (block)" -Direction Outbound -Action Block -Program "$PWD\stereowright.exe"
    New-NetFirewallRule -DisplayName "Stereowright ffmpeg (block)" -Direction Outbound -Action Block -Program "$PWD\ffmpeg.exe"
    New-NetFirewallRule -DisplayName "Stereowright ffprobe (block)" -Direction Outbound -Action Block -Program "$PWD\ffprobe.exe"

  The rules are tied to this folder's location: move the folder first if you want it
  somewhere else. Remove them later with:  Remove-NetFirewallRule -DisplayName "Stereowright*"

GPU
  Any DirectX 12 graphics card (NVIDIA, AMD or Intel) through DirectML; otherwise the CPU.

COMMAND LINE
  stereowright.exe --help          everything it can do
  stereowright.exe doctor          checks the models and the GPU
  stereowright.exe convert photo.jpg --out folder
  Videos: stereowright.exe convert clip.mp4 (ffmpeg 9.0.2 is included).

INCLUDED
  AI models: Depth Anything V2 Small (Apache-2.0, fine for any use) and three
  Real-ESRGAN upscalers (BSD). The larger depth models are non-commercial and not included.
  Licences: LICENSE (Stereowright, MIT), THIRD-PARTY-NOTICES.md, FFMPEG-LICENSE.txt (GPLv3).

NOT YET ON WINDOWS
  "View on Quest 3" sharing is Linux-only for now: copy the _LR files to the headset
  over USB instead.

User guide: https://github.com/vwillcox/stereowright/blob/main/docs/USER_GUIDE.md
