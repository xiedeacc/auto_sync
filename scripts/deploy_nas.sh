#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

CONFIG="${CONFIG:-conf/auto_sync.toml}"
HOST="${HOST:-192.168.2.247}"
PORT="${PORT:-10022}"
USER="${USER:-root}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/auto_sync}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --config)
      CONFIG="$2"
      shift 2
      ;;
    --host)
      HOST="$2"
      shift 2
      ;;
    --port)
      PORT="$2"
      shift 2
      ;;
    --user)
      USER="$2"
      shift 2
      ;;
    --install-dir)
      INSTALL_DIR="$2"
      shift 2
      ;;
    -h|--help)
      echo "Usage: scripts/deploy_nas.sh [--config PATH] [--host HOST] [--port PORT] [--user USER] [--install-dir DIR]"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

cargo build --release --bins
mkdir -p bin
install -m 0755 target/release/auto_syncd bin/auto_syncd
install -m 0755 target/release/auto_syncctl bin/auto_syncctl
install -m 0755 target/release/auto_sync_web bin/auto_sync_web
if [[ -x target/release/auto_sync_gui ]]; then
  install -m 0755 target/release/auto_sync_gui bin/auto_sync_gui
fi

bin/auto_syncctl --config "$CONFIG" deploy-nas \
  --host "$HOST" \
  --port "$PORT" \
  --user "$USER" \
  --install-dir "$INSTALL_DIR"
