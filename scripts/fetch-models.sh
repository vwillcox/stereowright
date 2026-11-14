#!/usr/bin/env bash
# One-time model download for Stereowright.
#
# This is the ONLY part of Stereowright that touches the network, and only when
# you run it yourself. The app never downloads anything: it just loads the
# .onnx files this script puts in ./models/. Every file is SHA-256 verified.
#
# Usage:
#   scripts/fetch-models.sh                # default: small (fast, Apache-2.0)
#   scripts/fetch-models.sh small large    # pick any of: small base large
#   scripts/fetch-models.sh all
set -euo pipefail

cd "$(dirname "$0")/../models"

HF="https://huggingface.co/onnx-community"

# name | repo | remote file | local file | sha256 | size | licence
MODELS=(
  "small|depth-anything-v2-small|onnx/model.onnx|depth-anything-v2-small.onnx|afb6a5c28f3b6bf1618c6e43f02073ef9dfdc70e937502d51603e57b0a1df10c|99 MB|Apache-2.0"
  "base|depth-anything-v2-base|onnx/model_fp16.onnx|depth-anything-v2-base-fp16.onnx|822e6c12f36cd46753dcf229ce87199310533de2d533a527756b8ef30a76d158|195 MB|CC-BY-NC-4.0 (non-commercial)"
  "large|depth-anything-v2-large|onnx/model_fp16.onnx|depth-anything-v2-large-fp16.onnx|8eefc7afb877b10d413c8e1024b012073c6e4f62735d39555b1083fee3aabf0a|669 MB|CC-BY-NC-4.0 (non-commercial)"
)

want=("$@")
[ ${#want[@]} -eq 0 ] && want=(small)
[ "${want[0]}" = "all" ] && want=(small base large)

for w in "${want[@]}"; do
  found=0
  for row in "${MODELS[@]}"; do
    IFS='|' read -r name repo remote local sha size licence <<<"$row"
    [ "$name" = "$w" ] || continue
    found=1
    if [ -f "$local" ] && echo "$sha  $local" | sha256sum -c --status; then
      echo "✓ $local already present and verified"
      continue
    fi
    echo "↓ $name: $repo ($size, licence: $licence)"
    curl -fL --retry 3 --progress-bar -o "$local.part" "$HF/$repo/resolve/main/$remote"
    if echo "$sha  $local.part" | sha256sum -c --status; then
      mv "$local.part" "$local"
      echo "✓ $local verified"
    else
      echo "✗ checksum mismatch for $local, deleting" >&2
      rm -f "$local.part"
      exit 1
    fi
  done
  [ $found -eq 1 ] || { echo "unknown model '$w' (choose: small base large all)" >&2; exit 2; }
done
