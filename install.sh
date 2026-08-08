#!/usr/bin/env bash
# Bootstrap installer for stt-server (Linux). Downloads the CPU-only binary only —
# no GPU build is published for Linux yet (see app/cli.py's _gpu_asset_url). Usage:
#
#   curl -sSL https://raw.githubusercontent.com/mariuszRep/stt-server/main/install.sh | bash
#
# Set STT_SERVER_VERSION to pin a specific release (e.g. "0.2.0"); otherwise this
# always installs the latest published release.
set -euo pipefail

if [ -n "${STT_SERVER_VERSION:-}" ]; then
  URL="https://github.com/mariuszRep/stt-server/releases/download/v${STT_SERVER_VERSION}/stt-server-linux-cpu"
else
  URL="https://github.com/mariuszRep/stt-server/releases/latest/download/stt-server-linux-cpu"
fi

TMP="$(mktemp -t stt-server-bootstrap.XXXXXX)"
trap 'rm -f "$TMP"' EXIT

echo "Downloading stt-server from ${URL} ..."
curl -fsSL -o "$TMP" "$URL"
chmod +x "$TMP"

echo "Installing..."
"$TMP" install
