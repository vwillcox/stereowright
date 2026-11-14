# Stereowright: notes for Claude

Rust desktop app (eframe/egui 0.37 + ONNX Runtime via `ort` 2.0.0-rc.15) that turns a photo
into VR stereo images using a local depth model. Linux (Omarchy/Hyprland, Wayland), RTX 5060 Ti.

## Hard rules
- **Nothing may leave the computer at runtime.** Never add HTTP/TLS/network crates, never
  enable `ort`'s `fetch-models` feature, never add telemetry/update checks. Keep
  `sandbox::lock_down()` the very first call in `main` (before any thread spawns).
  Run `scripts/audit-network.sh` after dependency changes.
- Model downloads happen only in `scripts/fetch-models.sh` / `scripts/make-upscalers.sh`
  (SHA-256 pinned), run by the user.
- The one deliberate exception: **Quest sharing** (`quest/stereowright_share.py`), a separate,
  user-started helper (stdlib Python, run as a systemd user service via `quest.rs`) that
  serves only `_LR/_3dh/_TB` outputs to the user's headset over USB (`adb reverse`,
  127.0.0.1 only) or the LAN (HTTPS + token, private client IPs only). It must never make
  outgoing connections or load anything from a CDN; the viewer (`quest/viewer/`) stays
  self-contained. Never move this into the Stereowright process.
- **Shell safety:** never `rm -rf` with globs or after a `cd`; put experiment output in fresh
  `mktemp -d` folders in the scratchpad instead of deleting. Use absolute paths.
- Don't screenshot the user's whole screen: capture only the Stereowright window
  (`hyprctl clients -j` → geometry → `grim -g`), and close test windows afterwards.
- Don't use the user's personal photos for testing; use `/usr/share/omarchy/themes/*/backgrounds/`
  and `/usr/share/sddm/themes/maldives/background.jpg`. Those are NOT cleared for publishing:
  images in the repo (`docs/images/`) must be public domain / CC0, credited in `docs/images/CREDITS.md`.
- Run test conversions and test GUIs with `XDG_DATA_HOME=<fresh scratch dir>`: otherwise
  exports are recorded in the user's real `~/.local/share/stereowright/share/folders.txt`
  (shown in their Quest gallery) and the GUI overwrites their saved settings (`app.ron`).

## Build / test
- `cargo build --release` (dev builds are slow for image work; use release).
- `cargo test --release`, `cargo clippy --release --all-targets` (keep both clean).
- `./target/release/stereowright doctor` checks GPU, models, network block.
- Visual QA: `stereowright convert <img> --out <fresh dir>`, then crop edge regions with
  `magick … -crop` and compare side by side. Palm fronds against sky (maldives) are the
  hardest edge case.
- Docs: `README.md`, `docs/INSTALL.md`, `docs/USER_GUIDE.md`. Keep them in step with CLI
  flags, GUI labels and measured speeds when those change.
- README screenshots: seed `app.ron` in a scratch `XDG_DATA_HOME` with the wanted `"view"`,
  launch with the photos as arguments, then float/size the window and make it opaque.
  Hyprland 0.56 dispatchers are Lua, e.g.
  `hyprctl dispatch 'hl.dsp.window.float({ action = "on", window = "address:0x…" })'`, likewise
  `resize({ x, y, window })`, `move({ x, y, window })`,
  `set_prop({ prop = "opaque", value = "1", window })`, `close({ window })`.
  (`action` must be "on"/"off"/"toggle"; anything else toggles.) Capture with
  `grim -g "<at> <size>"` and mask the 11 px rounded corners to transparent.

## Architecture
`depth.rs` → `postprocess.rs` → `stereo.rs` → `export.rs`, orchestrated by `pipeline.rs`.
GUI: `gui/mod.rs` (UI + preview render thread), `gui/worker.rs` (owns the model; AI runs there).
Upscaling: `upscale.rs` (Real-ESRGAN ONNX, 256 px tiles + 16 px overlap), applied in
`pipeline::prepare_image` / the GUI worker's cached `prepare` before depth estimation.
Quest: `quest.rs` (folders.txt, status.json, systemd-run) + `quest/` (helper + WebXR viewer).
**Never run `stereowright quest start/stop` (or systemctl on `stereowright-quest-share`) in tests
without `STEREOWRIGHT_QUEST_UNIT=<test name>`: the user may have real sharing running.**
Test the helper offline: `unshare --user --map-root-user --net` + `ip link set lo up`, fake
`adb` via `--adb`, scratch `--state-dir`; the viewer can be checked in headless chromium
with a mock `navigator.xr`. `STEREOWRIGHT_OPEN_QR=1` opens the GUI's QR window once a Wi-Fi
link exists (for screenshot + `zbarimg` checks).
Stereo synthesis = per-row forward warp as a 1-D triangle strip with z-test; tears at depth
edges (Fill mode) are mirrored from the background side, then smoothed with a depth-aware blur.

## API gotchas (versions in use)
- eframe 0.37: `App::ui(&mut self, ui, frame)`; panels are `egui::Panel::left/right/top/bottom(..).show(ui, ..)`.
- egui emoji glyphs need the `monochrome_emoji_fonts` feature (enabled via the direct `egui` dep).
- `ort`: `Session::builder(&env)`; env from `ort::init().with_telemetry(false).build()`;
  builder errors are `Error<SessionBuilder>`: map with `anyhow!("{e}")` or `.recover()`.
- Depth Anything V2 ONNX (onnx-community): input `pixel_values` f32 NCHW, ImageNet norm,
  sides multiple of 14; output `[1,H,W]` relative disparity (bigger = nearer).
- GPU memory: DA-V2 attention memory grows with patches² (Ultra 1274 px on 16:9 = 6.7 GB per
  buffer). `depth::MAX_TOKENS` caps patches at 10 000; on OOM `estimate` retries at 60% and
  keeps that budget. Run options set `memory.enable_memory_arena_shrinkage` so the arena drops
  back to the weights (~0.9 GB) after each run instead of keeping its peak.
- Glyphs: egui's proportional font lacks `●` and `→` (they render as boxes); `▶ ✔ ⚠ 📡 💾` are fine.
