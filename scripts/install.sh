#!/usr/bin/env bash
# Install Stereowright for the current user (no root):
#   ~/.local/share/stereowright/bin/      app + GPU runtime libraries
#   ~/.local/share/stereowright/models/   AI models (reflinked/copied from ./models)
#   ~/.local/share/stereowright/quest/    Quest share helper + 3D viewer (separate program,
#                                      started only from "View on Quest 3" / `stereowright quest`)
#   ~/.local/bin/stereowright             command-line wrapper
#   ~/.local/share/applications/stereowright.desktop   app-launcher entry
#
# The launcher entry starts Stereowright inside an offline network namespace
# (scripts/run-offline.sh), on top of the app's own built-in network block.
#
#   scripts/install.sh            build + install (with NVIDIA GPU support)
#   scripts/install.sh --cpu      build + install without CUDA (no NVIDIA GPU)
#   scripts/install.sh --uninstall
set -euo pipefail

src="$(cd "$(dirname "$0")/.." && pwd)"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
dest="$data/stereowright"
bindir="$HOME/.local/bin"
apps="$data/applications"
icons="$data/icons/hicolor/scalable/apps"

if [ "${1:-}" = "--uninstall" ]; then
  # Remove only files this script created. Models and saved settings are left alone.
  rm -f "$bindir/stereowright" "$apps/stereowright.desktop" "$icons/stereowright.svg"
  rm -f "$dest/bin/stereowright" "$dest/bin/run-offline.sh" "$dest"/bin/libonnxruntime*.so
  rmdir "$dest/bin" 2>/dev/null || true
  systemctl --user stop stereowright-quest-share.service 2>/dev/null || true
  rm -f "$dest/quest/stereowright_share.py" "$dest/quest/viewer/index.html" "$dest/quest/viewer/viewer.js" \
        "$dest/quest/viewer/viewer.css" "$dest/quest/viewer/icon.svg"
  rmdir "$dest/quest/viewer" "$dest/quest" 2>/dev/null || true
  echo "Uninstalled. Models kept in $dest/models (delete by hand if you want the space back)."
  exit 0
fi

features=()
case "${1:-}" in
  "") ;;
  --cpu) features=(--no-default-features) ;;
  *) echo "usage: scripts/install.sh [--cpu | --uninstall]" >&2; exit 2 ;;
esac

if [ -f "$src/Cargo.toml" ]; then
  echo "Building release…"
  cargo build --release --manifest-path "$src/Cargo.toml" "${features[@]}"
  built="$src/target/release"
else
  # Run from an unpacked release download: the app is already built.
  built="$src"
  [ "${#features[@]}" -eq 0 ] || echo "(--cpu only matters when building from source; ignored)"
fi

mkdir -p "$dest/bin" "$dest/models" "$bindir" "$apps" "$icons"
install -m755 "$built/stereowright" "$dest/bin/stereowright"
for lib in "$built"/libonnxruntime*.so; do
  [ -e "$lib" ] && cp -L --remove-destination "$lib" "$dest/bin/"
done
install -m755 "$src/scripts/run-offline.sh" "$dest/bin/run-offline.sh"
install -d "$dest/quest/viewer"
install -m644 "$src/quest/stereowright_share.py" "$dest/quest/stereowright_share.py"
for f in index.html viewer.js viewer.css icon.svg; do
  install -m644 "$src/quest/viewer/$f" "$dest/quest/viewer/$f"
done

install -m644 "$src/models/models.toml" "$dest/models/models.toml"
for m in "$src"/models/*.onnx; do
  [ -e "$m" ] || continue
  name="$(basename "$m")"
  if [ ! -e "$dest/models/$name" ]; then
    echo "Adding model $name"
    cp --reflink=auto "$m" "$dest/models/$name"
  fi
done

cat >"$bindir/stereowright" <<EOF
#!/usr/bin/env bash
exec "$dest/bin/stereowright" "\$@"
EOF
chmod 755 "$bindir/stereowright"

install -m644 "$src/assets/stereowright.svg" "$icons/stereowright.svg"
cat >"$apps/stereowright.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Stereowright
GenericName=2D to 3D Photo Converter
Comment=Turn photos into VR-ready 3D with a local AI depth model (offline)
Exec=env STEREOWRIGHT_BIN=$dest/bin/stereowright $dest/bin/run-offline.sh %F
Icon=stereowright
Terminal=false
Categories=Graphics;Photography;3DGraphics;
MimeType=image/jpeg;image/png;image/webp;image/tiff;image/bmp;
StartupWMClass=stereowright
Keywords=3D;VR;stereo;depth;SBS;anaglyph;
EOF
command -v update-desktop-database >/dev/null && update-desktop-database "$apps" >/dev/null 2>&1 || true

echo
echo "Installed. Launch 'Stereowright' from your app launcher, or run: stereowright --help"
case ":$PATH:" in *":$bindir:"*) ;; *) echo "Note: $bindir is not on your PATH." ;; esac
