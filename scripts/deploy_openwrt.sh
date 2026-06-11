#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

CONFIG="${CONFIG:-conf/auto_sync.toml}"
HOST="${HOST:-192.168.2.1}"
PORT="${PORT:-10022}"
USER="${USER:-root}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/auto_sync}"
BINARY_DIR="${BINARY_DIR:-}"

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
    --binary-dir)
      BINARY_DIR="$2"
      shift 2
      ;;
    -h|--help)
      echo "Usage: scripts/deploy_openwrt.sh [--config PATH] [--host HOST] [--port PORT] [--user USER] [--install-dir DIR] [--binary-dir DIR]"
      echo "Defaults: --host 192.168.2.1 --port 10022 --user root --install-dir /usr/local/auto_sync"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

target="${USER}@${HOST}"
arch="$(ssh -p "$PORT" "$target" "uname -m" | tr -d '\r')"

if [[ -z "$BINARY_DIR" ]]; then
  if [[ "$arch" == "aarch64" || "$arch" == "arm64" ]]; then
    scripts/build_openwrt_aarch64.sh
    BINARY_DIR="bin/openwrt/aarch64"
  elif [[ -d "bin/openwrt/$arch" ]]; then
    BINARY_DIR="bin/openwrt/$arch"
  elif [[ -d "bin/openwrt" ]]; then
    BINARY_DIR="bin/openwrt"
  else
    echo "No OpenWrt binaries found for arch '$arch'." >&2
    echo "Put auto_syncd, auto_sync_web, and auto_syncctl under bin/openwrt/$arch or pass --binary-dir." >&2
    exit 1
  fi
fi

for binary in auto_syncd auto_sync_web auto_syncctl; do
  if [[ ! -x "$BINARY_DIR/$binary" ]]; then
    echo "$BINARY_DIR/$binary is missing or not executable" >&2
    exit 1
  fi
done

ssh -p "$PORT" "$target" "mkdir -p '$INSTALL_DIR/bin' '$INSTALL_DIR/conf' '$INSTALL_DIR/conf/state' '$INSTALL_DIR/logs'"

ssh -p "$PORT" "$target" "/etc/init.d/auto_sync stop >/dev/null 2>&1 || true; /etc/init.d/auto_sync_web stop >/dev/null 2>&1 || true"

for binary in auto_syncd auto_sync_web auto_syncctl; do
  scp -O -P "$PORT" "$BINARY_DIR/$binary" "${target}:${INSTALL_DIR}/bin/${binary}"
done

if ssh -p "$PORT" "$target" "test -f '$INSTALL_DIR/conf/auto_sync.toml'"; then
  echo "OpenWrt config $INSTALL_DIR/conf/auto_sync.toml already exists; leaving it untouched"
else
  scp -O -P "$PORT" "$CONFIG" "${target}:${INSTALL_DIR}/conf/auto_sync.toml"
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

cat > "$tmp_dir/auto_sync" <<EOF
#!/bin/sh /etc/rc.common
START=95
USE_PROCD=1

start_service() {
  procd_open_instance
  procd_set_param command $INSTALL_DIR/bin/auto_syncd --config $INSTALL_DIR/conf/auto_sync.toml
  procd_set_param respawn 5 5 0
  procd_set_param stdout 1
  procd_set_param stderr 1
  procd_close_instance
}
EOF

cat > "$tmp_dir/auto_sync_web" <<EOF
#!/bin/sh /etc/rc.common
START=96
USE_PROCD=1

start_service() {
  procd_open_instance
  procd_set_param command $INSTALL_DIR/bin/auto_sync_web --config $INSTALL_DIR/conf/auto_sync.toml
  procd_set_param respawn 5 5 0
  procd_set_param stdout 1
  procd_set_param stderr 1
  procd_close_instance
}
EOF

scp -O -P "$PORT" "$tmp_dir/auto_sync" "${target}:/etc/init.d/auto_sync"
scp -O -P "$PORT" "$tmp_dir/auto_sync_web" "${target}:/etc/init.d/auto_sync_web"

ssh -p "$PORT" "$target" "chmod +x /etc/init.d/auto_sync /etc/init.d/auto_sync_web"
ssh -p "$PORT" "$target" "if command -v opkg >/dev/null 2>&1; then opkg update && (opkg install rsync openssh-client || opkg install rsync); fi"
ssh -p "$PORT" "$target" "/etc/init.d/auto_sync enable && /etc/init.d/auto_sync_web enable && /etc/init.d/auto_sync start && /etc/init.d/auto_sync_web start"
ssh -p "$PORT" "$target" "/etc/init.d/auto_sync status || true; /etc/init.d/auto_sync_web status || true"
