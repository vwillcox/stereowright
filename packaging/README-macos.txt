Stereowright @VERSION@ for macOS (Apple silicon, macOS 13.4 or newer)
======================================================================

Turns your photos and videos into 3D for VR headsets, 3D TVs and red/cyan glasses, with
an AI that runs on this Mac. Your files are never uploaded.

INSTALL
  Drag Stereowright.app into Applications.

FIRST START
  The app isn't notarised by Apple (that needs a paid developer account), so macOS blocks
  it the first time. Open it once, close the warning, then go to System Settings →
  Privacy & Security, scroll down and click "Open Anyway". After that it opens normally.

NETWORK BLOCK: ON
  The first thing the app does is put itself in a macOS kernel sandbox that refuses every
  network connection, even to this Mac, for good. It then tests that the block works,
  and the window says "🔒 Offline: network blocked". The included ffmpeg runs inside the
  same sandbox.

SPEED
  The AI runs on the Apple GPU and Neural Engine through Core ML. On an M4 Mac mini
  (16 GB) the depth step takes about 1.5-2 s per photo at the Mac default detail
  (Standard, 518 px), and videos convert at about 1.2 s per frame. High detail (770 px)
  works but takes 10-20 s per photo and needs a lot of memory, so keep videos on
  Standard.

COMMAND LINE
  /Applications/Stereowright.app/Contents/MacOS/stereowright --help
  /Applications/Stereowright.app/Contents/MacOS/stereowright doctor
      (checks the network block, the models and the GPU)
  Videos: ... stereowright convert clip.mp4 (ffmpeg 9.0.2 is included).

INCLUDED
  AI models: Depth Anything V2 Small (Apache-2.0, fine for any use) and three
  Real-ESRGAN upscalers (BSD). The larger depth models are non-commercial and not included.
  Licences: LICENSE (Stereowright, MIT), THIRD-PARTY-NOTICES.md, FFMPEG-LICENSE.txt (GPLv3).

NOT YET ON macOS
  "View on Quest 3" sharing is Linux-only for now: copy the _LR files to the headset
  over USB instead. Intel Macs aren't supported.

User guide: https://github.com/vwillcox/stereowright/blob/main/docs/USER_GUIDE.md
