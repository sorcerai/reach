#!/usr/bin/env bash
# Injects the Reach takeover overlay banner into noVNC's vnc.html
set -euo pipefail

TARGET="${1:-/opt/noVNC/vnc.html}"

if [ ! -f "$TARGET" ]; then
  echo "Target file $TARGET does not exist, skipping injection."
  exit 0
fi

if grep -q "reach-banner" "$TARGET" 2>/dev/null; then
  echo "Banner already injected into $TARGET"
  exit 0
fi

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ASSET_JS="$DIR/../assets/reach-banner.js"

if [ -f "$ASSET_JS" ]; then
  DEST_DIR="$(dirname "$TARGET")"
  cp -f "$ASSET_JS" "$DEST_DIR/reach-banner.js"
  # Inject script tag before </body> or EOF
  if grep -q "</body>" "$TARGET"; then
    sed -i.bak 's|</body>|<script src="reach-banner.js"></script></body>|' "$TARGET"
    rm -f "${TARGET}.bak"
  else
    echo '<script src="reach-banner.js"></script>' >> "$TARGET"
  fi
  echo "Successfully injected reach-banner.js into $TARGET"
else
  echo "Asset $ASSET_JS not found."
fi
