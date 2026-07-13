#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

CONFIG="${CONFIG:-conf/auto_sync.linux.toml}"
INSTALL_DIR="${INSTALL_DIR:-/opt/auto_sync}"
FLUTTER_ROOT="${FLUTTER_ROOT:-/opt/src/software/flutter}"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "deploy_nas.sh must run on the NAS Linux host." >&2
  exit 1
fi

if [[ "$INSTALL_DIR" != /opt/auto_sync ]]; then
  echo "NAS install dir must stay /opt/auto_sync; got: $INSTALL_DIR" >&2
  exit 1
fi

if [[ ! -d /opt ]]; then
  echo "NAS deployment expects /opt to exist." >&2
  exit 1
fi

export CONFIG
export INSTALL_DIR
export FLUTTER_ROOT

exec "$ROOT_DIR/scripts/deploy_local.sh" --config "$CONFIG" --install-dir "$INSTALL_DIR"
