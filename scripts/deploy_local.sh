#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

CONFIG="${CONFIG:-conf/auto_sync.toml}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/auto_sync}"

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
    -h|--help)
      echo "Usage: scripts/deploy_local.sh [--config PATH] [--install-dir DIR]"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if ! command -v systemctl >/dev/null 2>&1; then
  echo "systemctl is required for Linux local deployment" >&2
  exit 1
fi

SUDO=()
if [[ "${EUID}" -ne 0 ]]; then
  SUDO=(sudo)
fi

has_gui_environment() {
  [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]] && return 0
  [[ -d /usr/share/xsessions ]] && compgen -G "/usr/share/xsessions/*.desktop" >/dev/null && return 0
  [[ -d /usr/share/wayland-sessions ]] && compgen -G "/usr/share/wayland-sessions/*.desktop" >/dev/null && return 0
  command -v Xorg >/dev/null 2>&1 && return 0
  return 1
}

if has_gui_environment; then
  cargo build --release --bins
else
  cargo build --release --no-default-features --bin auto_syncd --bin auto_syncctl --bin auto_sync_web
fi
mkdir -p bin
install -m 0755 target/release/auto_syncd bin/auto_syncd
install -m 0755 target/release/auto_syncctl bin/auto_syncctl
install -m 0755 target/release/auto_sync_web bin/auto_sync_web
if has_gui_environment && [[ -x target/release/auto_sync_gui ]]; then
  install -m 0755 target/release/auto_sync_gui bin/auto_sync_gui
fi

"${SUDO[@]}" install -d -m 0755 \
  "$INSTALL_DIR/bin" \
  "$INSTALL_DIR/conf" \
  "$INSTALL_DIR/conf/state" \
  "$INSTALL_DIR/logs"
"${SUDO[@]}" install -m 0755 bin/auto_syncd "$INSTALL_DIR/bin/auto_syncd"
"${SUDO[@]}" install -m 0755 bin/auto_syncctl "$INSTALL_DIR/bin/auto_syncctl"
"${SUDO[@]}" install -m 0755 bin/auto_sync_web "$INSTALL_DIR/bin/auto_sync_web"
if has_gui_environment && [[ -x bin/auto_sync_gui ]]; then
  "${SUDO[@]}" install -m 0755 bin/auto_sync_gui "$INSTALL_DIR/bin/auto_sync_gui"
  echo "GUI environment detected; installed auto_sync_gui"
else
  "${SUDO[@]}" rm -f "$INSTALL_DIR/bin/auto_sync_gui"
  echo "No GUI environment detected; installed headless mode only"
fi

if "${SUDO[@]}" test -f "$INSTALL_DIR/conf/auto_sync.toml"; then
  echo "local config $INSTALL_DIR/conf/auto_sync.toml already exists; leaving it untouched"
else
  "${SUDO[@]}" install -m 0644 "$CONFIG" "$INSTALL_DIR/conf/auto_sync.toml"
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

bin/auto_syncctl print-systemd --install-dir "$INSTALL_DIR" > "$tmp_dir/auto_sync.service"
cat > "$tmp_dir/auto_sync_web.service" <<EOF
[Unit]
Description=auto_sync Web UI
After=network-online.target auto_sync.service
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$INSTALL_DIR
ExecStart=$INSTALL_DIR/bin/auto_sync_web --config $INSTALL_DIR/conf/auto_sync.toml
Restart=always
RestartSec=5
User=root
Group=root

[Install]
WantedBy=multi-user.target
EOF

"${SUDO[@]}" install -m 0644 "$tmp_dir/auto_sync.service" /etc/systemd/system/auto_sync.service
"${SUDO[@]}" install -m 0644 "$tmp_dir/auto_sync_web.service" /etc/systemd/system/auto_sync_web.service
"${SUDO[@]}" systemctl daemon-reload
"${SUDO[@]}" systemctl enable auto_sync.service auto_sync_web.service
"${SUDO[@]}" systemctl restart auto_sync.service auto_sync_web.service
"${SUDO[@]}" systemctl status --no-pager auto_sync.service auto_sync_web.service
