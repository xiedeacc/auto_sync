#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

CONFIG="${CONFIG:-conf/auto_sync.linux.toml}"
INSTALL_DIR="${INSTALL_DIR:-/opt/usr/local/auto_sync}"
FLUTTER_ROOT="${FLUTTER_ROOT:-/root/src/software/flutter}"
SOURCE_DIR="${SOURCE_DIR:-/root/src/rust/auto_sync}"
NAS_HOST="${NAS_HOST:-192.168.2.247}"
NAS_USER="${NAS_USER:-root}"
NAS_PORT="${NAS_PORT:-10022}"
NAS_KEY="${NAS_KEY:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --config)
      CONFIG="$2"
      shift 2
      ;;
    --install-dir)
      INSTALL_DIR="$2"
      shift 2
      ;;
    --host)
      NAS_HOST="$2"
      shift 2
      ;;
    --port)
      NAS_PORT="$2"
      shift 2
      ;;
    --user)
      NAS_USER="$2"
      shift 2
      ;;
    --identity-file)
      NAS_KEY="$2"
      shift 2
      ;;
    -h|--help)
      echo "Usage: scripts/deploy_nas.sh [--config PATH] [--install-dir DIR] [--host HOST] [--port PORT] [--user USER] [--identity-file PATH]"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "deploy_nas.sh must run on the dev Linux host." >&2
  exit 1
fi

if [[ "$INSTALL_DIR" != /opt/usr/local/auto_sync ]]; then
  echo "NAS install dir must stay /opt/usr/local/auto_sync; got: $INSTALL_DIR" >&2
  exit 1
fi

if [[ "$(realpath -m "$ROOT_DIR")" != "$(realpath -m "$SOURCE_DIR")" ]]; then
  echo "NAS deployment must be built from dev source dir $SOURCE_DIR; got: $ROOT_DIR" >&2
  echo "Run on dev: cd $SOURCE_DIR && scripts/deploy_nas.sh" >&2
  exit 1
fi

if [[ "$(realpath -m "$FLUTTER_ROOT")" != "$(realpath -m /root/src/software/flutter)" ]]; then
  echo "NAS deployment must use dev Flutter SDK at /root/src/software/flutter; got: $FLUTTER_ROOT" >&2
  exit 1
fi

if [[ ! -f "$CONFIG" ]]; then
  echo "Config file not found for initial NAS install: $CONFIG" >&2
  exit 1
fi

ssh_opts=(-p "$NAS_PORT")
scp_opts=(-P "$NAS_PORT")
if [[ -n "$NAS_KEY" ]]; then
  ssh_opts+=(-i "$NAS_KEY")
  scp_opts+=(-i "$NAS_KEY")
fi
NAS_DEST="${NAS_USER}@${NAS_HOST}"

export CONFIG
export INSTALL_DIR
export FLUTTER_ROOT

"$ROOT_DIR/scripts/deploy_local.sh" --config "$CONFIG" --install-dir /usr/local/auto_sync

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
payload="$tmp_dir/auto_sync_nas_payload.tgz"

mkdir -p "$tmp_dir/payload/bin" "$tmp_dir/payload/web" "$tmp_dir/payload/conf"
install -m 0755 "$ROOT_DIR/bin/auto_sync" "$tmp_dir/payload/bin/auto_sync"
cp -a "$ROOT_DIR/flutter/auto_sync_gui/build/web/." "$tmp_dir/payload/web/"
install -m 0644 "$CONFIG" "$tmp_dir/payload/conf/auto_sync.toml.initial"

tar -C "$tmp_dir/payload" -czf "$payload" .

remote_stage="/tmp/auto_sync_nas_deploy_$(date +%s)_$$"
ssh "${ssh_opts[@]}" "$NAS_DEST" "mkdir -p '$remote_stage'"
scp "${scp_opts[@]}" "$payload" "$NAS_DEST:$remote_stage/payload.tgz"

ssh "${ssh_opts[@]}" "$NAS_DEST" "INSTALL_DIR='$INSTALL_DIR' STAGE='$remote_stage' bash -s" <<'EOF_REMOTE'
set -euo pipefail

cleanup() {
  rm -rf "$STAGE"
}
trap cleanup EXIT

mkdir -p "$STAGE/payload"
tar -C "$STAGE/payload" -xzf "$STAGE/payload.tgz"

install -d -m 0755 \
  "$INSTALL_DIR/bin" \
  "$INSTALL_DIR/conf" \
  "$INSTALL_DIR/data" \
  "$INSTALL_DIR/logs" \
  "$INSTALL_DIR/web"

systemctl stop auto_sync.service >/dev/null 2>&1 || true

install -m 0755 "$STAGE/payload/bin/auto_sync" "$INSTALL_DIR/bin/auto_sync"
rm -rf "$INSTALL_DIR/web"
install -d -m 0755 "$INSTALL_DIR/web"
cp -a "$STAGE/payload/web/." "$INSTALL_DIR/web/"

if [ -f "$INSTALL_DIR/conf/auto_sync.toml" ]; then
  echo "Preserved existing NAS config $INSTALL_DIR/conf/auto_sync.toml"
else
  install -m 0644 "$STAGE/payload/conf/auto_sync.toml.initial" "$INSTALL_DIR/conf/auto_sync.toml"
  echo "Initialized NAS config $INSTALL_DIR/conf/auto_sync.toml"
fi

if [ ! -f /etc/systemd/system/auto_sync.service ]; then
  echo "Missing /etc/systemd/system/auto_sync.service; create it once and collect it before deploying." >&2
  exit 1
fi
chmod 0644 /etc/systemd/system/auto_sync.service
chown -R root:root "$INSTALL_DIR/bin" "$INSTALL_DIR/web" "$INSTALL_DIR/logs" "$INSTALL_DIR/data" 2>/dev/null || true
chmod 755 "$INSTALL_DIR" "$INSTALL_DIR/bin" "$INSTALL_DIR/web" "$INSTALL_DIR/logs" "$INSTALL_DIR/data" 2>/dev/null || true

systemctl daemon-reload
systemctl enable auto_sync.service
systemctl restart auto_sync.service
systemctl status --no-pager auto_sync.service
EOF_REMOTE
