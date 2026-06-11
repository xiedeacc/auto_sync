#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

CONFIG="${CONFIG:-conf/auto_sync.toml}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/auto_sync}"

exec scripts/deploy_local.sh \
  --config "$CONFIG" \
  --install-dir "$INSTALL_DIR" \
  "$@"
