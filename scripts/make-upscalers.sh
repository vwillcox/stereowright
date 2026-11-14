#!/usr/bin/env bash
# One-time: build Stereowright's Real-ESRGAN upscalers (ONNX) from the OFFICIAL weights.
#
# Network use (download only, run by you, once):
#   * the official .pth weights from github.com/xinntao/Real-ESRGAN releases (SHA-256 pinned)
#   * Python packages for the conversion (PyTorch CPU, onnx, onnxruntime) into a throwaway
#     uv environment (cached by uv, nothing installed system-wide)
# Stereowright itself then only loads the resulting .onnx files from ./models/.
#
#   scripts/make-upscalers.sh            # all three
#   scripts/make-upscalers.sh x4plus     # any of: x4plus x2plus general-x4v3
set -euo pipefail
cd "$(dirname "$0")/.."

base=https://github.com/xinntao/Real-ESRGAN/releases/download
# arch | release path | sha256 of the official .pth | output onnx
ROWS=(
  "x4plus|v0.1.0/RealESRGAN_x4plus.pth|4fa0d38905f75ac06eb49a7951b426670021be3018265fd191d2125df9d682f1|realesrgan-x4plus.onnx"
  "x2plus|v0.2.1/RealESRGAN_x2plus.pth|49fafd45f8fd7aa8d31ab2a22d14d91b536c34494a5cfe31eb5d89c2fa266abb|realesrgan-x2plus.onnx"
  "general-x4v3|v0.2.5.0/realesr-general-x4v3.pth|8dc7edb9ac80ccdc30c3a5dca6616509367f05fbc184ad95b731f05bece96292|realesr-general-x4v3.onnx"
)

want=("$@")
[ ${#want[@]} -eq 0 ] && want=(x4plus x2plus general-x4v3)
mkdir -p models/weights

for w in "${want[@]}"; do
  found=0
  for row in "${ROWS[@]}"; do
    IFS='|' read -r arch rel sha out <<<"$row"
    [ "$arch" = "$w" ] || continue
    found=1
    pth="models/weights/$(basename "$rel")"
    if ! { [ -f "$pth" ] && echo "$sha  $pth" | sha256sum -c --status; }; then
      echo "↓ $(basename "$rel") (official release $rel)"
      curl -fL --retry 3 --progress-bar -o "$pth.part" "$base/$rel"
      echo "$sha  $pth.part" | sha256sum -c --status || { echo "✗ checksum mismatch for $pth" >&2; exit 1; }
      mv "$pth.part" "$pth"
    fi
    echo "⚙ exporting $arch → models/$out"
    uv run --quiet --no-project --python 3.12 \
      --index-strategy unsafe-best-match \
      --extra-index-url https://download.pytorch.org/whl/cpu \
      --with 'torch==2.14.1+cpu' --with onnx --with onnxscript --with onnxruntime --with numpy \
      python -I scripts/upscalers/export_realesrgan.py --arch "$arch" --weights "$pth" --out "models/$out"
  done
  [ $found -eq 1 ] || { echo "unknown upscaler '$w' (choose: x4plus x2plus general-x4v3)" >&2; exit 2; }
done
echo "✓ done. Real-ESRGAN is BSD-3-Clause licensed."
