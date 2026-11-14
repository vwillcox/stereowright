#!/usr/bin/env bash
# Build a self-contained Stereowright release for the system this runs on, into dist/:
#
#   Linux (x86-64)          dist/stereowright-<version>-linux-x86_64.tar.gz
#   macOS (Apple silicon)   dist/stereowright-<version>-macos-arm64.dmg   (Stereowright.app)
#   Windows (x86-64)        dist/stereowright-<version>-windows-x86_64.zip  (run from Git Bash)
#
# The unpacked release is left beside it (dist/stereowright-<version>-<os>-<arch>/) to try.
#
# Bundled: the app with its GPU runtime (CUDA provider libraries on Linux, DirectML on
# Windows, Core ML built in on macOS), the Small depth model and the three upscalers from
# models/ (get them first: scripts/fetch-models.sh small && scripts/make-upscalers.sh),
# and on Windows and macOS a static ffmpeg for video.
#
# Downloads happen here, at build time only: Rust crates, ONNX Runtime (through the ort
# crate), and that ffmpeg, which is checked against the SHA-256 pinned below.
set -euo pipefail

src="$(cd "$(dirname "$0")/.." && pwd)"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$src/Cargo.toml" | head -n1)"
case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  MINGW* | MSYS* | CYGWIN*) os=windows ;;
  *) echo "unsupported system: $(uname -s)" >&2; exit 1 ;;
esac
arch="$(uname -m)"
[ "$arch" = aarch64 ] && arch=arm64
if [ "$os" = macos ] && [ "$arch" != arm64 ]; then
  echo "macOS releases are for Apple silicon only (ONNX Runtime has no prebuilt Intel Mac library)." >&2
  exit 1
fi
name="stereowright-$version-$os-$arch"
dist="$src/dist"
stage="$dist/$name"
if [ -e "$stage" ]; then
  echo "$stage already exists: move or delete it first." >&2
  exit 1
fi

# ffmpeg 9.0.2, static GPL builds (see THIRD-PARTY-NOTICES.md).
win_ffmpeg_url="https://github.com/GyanD/codexffmpeg/releases/download/9.0.2/ffmpeg-9.0.2-essentials_build.zip"
win_ffmpeg_sha="60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba"
mac_ffmpeg_base="https://ffmpeg.martin-riedl.de/download/macos/arm64/1789931890_9.0.2"
mac_ffmpeg_sha="c8ed4c4e6978a03c485edbfe4e0a5dc2380f8a30bba5150531b31b094492d924"
mac_ffprobe_sha="fcbe839537485eaee7a7a8bc5cbc0f90d53617e80943e8a5b2e31cb851197ea6"

models=(models.toml depth-anything-v2-small.onnx realesrgan-x4plus.onnx realesrgan-x2plus.onnx realesr-general-x4v3.onnx)
for m in "${models[@]}"; do
  if [ ! -f "$src/models/$m" ]; then
    echo "models/$m is missing: run scripts/fetch-models.sh small and scripts/make-upscalers.sh first." >&2
    exit 1
  fi
done

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
fetch() { # url sha256 output
  echo "Downloading $1"
  curl -fsSL --retry 3 -o "$3" "$1"
  local got
  got="$(sha256 "$3")"
  if [ "$got" != "$2" ]; then
    echo "checksum mismatch for $1: got $got, expected $2" >&2
    exit 1
  fi
}

unpack() { # zip destination
  if command -v unzip >/dev/null; then
    unzip -q -o "$1" -d "$2"
  elif command -v 7z >/dev/null; then
    7z x -y -o"$2" "$1" >/dev/null
  else
    powershell -NoProfile -Command "Expand-Archive -Force -Path '$(cygpath -w "$1")' -DestinationPath '$(cygpath -w "$2")'"
  fi
}

mkdir -p "$dist"
downloads="$(mktemp -d "$dist/.downloads.XXXXXX")"

echo "Building Stereowright $version for $os ($arch)…"
case "$os" in
  linux) cargo build --release --manifest-path "$src/Cargo.toml" ;;
  # ONNX Runtime's prebuilt macOS library needs macOS 13.4 or newer.
  macos) MACOSX_DEPLOYMENT_TARGET=13.4 cargo build --release --manifest-path "$src/Cargo.toml" --no-default-features --features coreml ;;
  windows) cargo build --release --manifest-path "$src/Cargo.toml" --no-default-features --features directml ;;
esac
out="$src/target/release"

readme() { # destination
  sed "s/@VERSION@/$version/g" "$src/packaging/README-$os.txt" >"$1"
}
common() { # destination folder
  cp "$src/LICENSE" "$src/THIRD-PARTY-NOTICES.md" "$1/"
}
add_models() { # destination folder
  mkdir -p "$1"
  for m in "${models[@]}"; do cp "$src/models/$m" "$1/"; done
}

case "$os" in
linux)
  mkdir -p "$stage/scripts" "$stage/quest/viewer" "$stage/assets"
  cp "$out/stereowright" "$stage/"
  # GPU runtime for NVIDIA cards (TensorRT providers are not used, so not shipped).
  cp -L "$out/libonnxruntime_providers_shared.so" "$out/libonnxruntime_providers_cuda.so" "$stage/"
  add_models "$stage/models"
  cp "$src/quest/stereowright_share.py" "$stage/quest/"
  cp "$src"/quest/viewer/{index.html,viewer.js,viewer.css,icon.svg} "$stage/quest/viewer/"
  cp "$src/assets/stereowright.svg" "$stage/assets/"
  cp "$src/scripts/install.sh" "$src/scripts/run-offline.sh" "$src/scripts/fetch-models.sh" "$stage/scripts/"
  readme "$stage/README.txt"
  common "$stage"
  tar -C "$dist" -czf "$dist/$name.tar.gz" "$name"
  echo "Done: $dist/$name.tar.gz"
  ;;

macos)
  app="$stage/Stereowright.app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  sed "s/@VERSION@/$version/g" "$src/packaging/Info.plist" >"$app/Contents/Info.plist"
  cp "$out/stereowright" "$app/Contents/MacOS/"
  add_models "$app/Contents/Resources/models"

  fetch "$mac_ffmpeg_base/ffmpeg.zip" "$mac_ffmpeg_sha" "$downloads/ffmpeg.zip"
  fetch "$mac_ffmpeg_base/ffprobe.zip" "$mac_ffprobe_sha" "$downloads/ffprobe.zip"
  unpack "$downloads/ffmpeg.zip" "$app/Contents/MacOS"
  unpack "$downloads/ffprobe.zip" "$app/Contents/MacOS"
  chmod 755 "$app/Contents/MacOS/ffmpeg" "$app/Contents/MacOS/ffprobe"

  iconset="$downloads/stereowright.iconset"
  mkdir -p "$iconset"
  for s in 16 32 128 256 512; do
    sips -z "$s" "$s" "$src/assets/stereowright-1024.png" --out "$iconset/icon_${s}x${s}.png" >/dev/null
    d=$((s * 2))
    sips -z "$d" "$d" "$src/assets/stereowright-1024.png" --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns "$iconset" -o "$app/Contents/Resources/stereowright.icns"

  # Ad-hoc signature (no Apple developer account): needed for Apple silicon to run it.
  codesign --force --deep --sign - "$app"
  codesign --verify --deep --strict "$app"

  readme "$stage/READ ME FIRST.txt"
  common "$stage"
  cp "$src/packaging/GPL-3.0.txt" "$stage/FFMPEG-LICENSE.txt"
  ln -s /Applications "$stage/Applications"
  hdiutil create -quiet -volname "Stereowright $version" -srcfolder "$stage" -fs HFS+ -format UDZO "$dist/$name.dmg"
  echo "Done: $dist/$name.dmg"
  ;;

windows)
  mkdir -p "$stage"
  cp "$out/stereowright.exe" "$stage/"
  # DirectML.dll comes with ONNX Runtime (copied next to the exe by the ort crate).
  dml="$out/DirectML.dll"
  [ -f "$dml" ] || dml="$(find "$src/target/release" -name DirectML.dll -print -quit)"
  if [ -z "$dml" ] || [ ! -f "$dml" ]; then
    echo "DirectML.dll was not found under target/release." >&2
    exit 1
  fi
  cp "$dml" "$stage/"
  add_models "$stage/models"
  fetch "$win_ffmpeg_url" "$win_ffmpeg_sha" "$downloads/ffmpeg.zip"
  unpack "$downloads/ffmpeg.zip" "$downloads/ffmpeg"
  ff="$downloads/ffmpeg/ffmpeg-9.0.2-essentials_build"
  cp "$ff/bin/ffmpeg.exe" "$ff/bin/ffprobe.exe" "$stage/"
  cp "$ff/LICENSE" "$stage/FFMPEG-LICENSE.txt"
  cp "$ff/README.txt" "$stage/FFMPEG-README.txt"
  readme "$stage/README.txt"
  common "$stage"
  (command -v 7z >/dev/null && 7z a -tzip -mx=7 "$dist/$name.zip" "$stage" >/dev/null) ||
    powershell -NoProfile -Command "Compress-Archive -Path '$(cygpath -w "$stage")' -DestinationPath '$(cygpath -w "$dist/$name.zip")'"
  echo "Done: $dist/$name.zip"
  ;;
esac
echo "Downloads were kept in $downloads (safe to delete)."
