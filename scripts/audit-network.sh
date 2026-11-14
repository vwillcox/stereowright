#!/usr/bin/env bash
# Audit: list every crate compiled INTO the stereowright binary (normal dependencies only;
# build-time tools like the ONNX Runtime downloader are excluded because they never ship)
# and fail if any HTTP/TLS/networking library is among them.
set -euo pipefail
cd "$(dirname "$0")/.."

banned='^(reqwest|hyper|hyper-util|ureq|curl|curl-sys|isahc|attohttpc|surf|h2|h3|quinn|rustls|native-tls|openssl|openssl-sys|tungstenite|tokio-tungstenite|websocket|reqwless|minreq|socket2|trust-dns-resolver|hickory-resolver)$'

crates=$(cargo tree -e normal --prefix none --format '{p}' 2>/dev/null | awk '{print $1}' | sort -u)
echo "Crates compiled into stereowright: $(echo "$crates" | wc -l)"

hits=$(echo "$crates" | grep -E "$banned" || true)
if [ -n "$hits" ]; then
  echo "✗ networking libraries found in the runtime dependency tree:"
  echo "$hits"
  echo "Inspect with: cargo tree -e normal -i <crate>"
  exit 1
fi
echo "✓ no HTTP, TLS or networking libraries are compiled into the app."
echo "  (zbus, if listed by 'cargo tree', is local D-Bus over a Unix socket, used by the file dialog.)"
