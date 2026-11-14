#!/usr/bin/env bash
# Belt and braces: run Stereowright inside a private network namespace that has NO network
# devices at all (not even a working loopback), on top of the app's own built-in kernel
# block. Nothing can leave the computer, whatever happens inside the app.
#
#   scripts/run-offline.sh                 # open the app
#   scripts/run-offline.sh convert ~/Pictures/trip --preset strong
#   scripts/run-offline.sh doctor          # shows "no network interfaces visible"
#
# Uses only `unshare` from util-linux (no root needed).
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
bin="${STEREOWRIGHT_BIN:-}"
if [ -z "$bin" ]; then
  if [ -x "$here/target/release/stereowright" ]; then
    bin="$here/target/release/stereowright"
  elif command -v stereowright >/dev/null; then
    bin="$(command -v stereowright)"
  else
    echo "stereowright is not built yet: run  cargo build --release" >&2
    exit 1
  fi
fi

exec unshare --user --map-current-user --net -- "$bin" "$@"
