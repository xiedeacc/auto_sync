#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

CONFIG="${CONFIG:-}"
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

if [[ -z "$CONFIG" ]]; then
  CONFIG="conf/auto_sync.linux.toml"
fi

if [[ ! -f "$CONFIG" ]]; then
  echo "Config file not found: $CONFIG" >&2
  exit 1
fi

if ! command -v systemctl >/dev/null 2>&1; then
  echo "systemctl is required for Linux local deployment" >&2
  exit 1
fi

SUDO=()
if [[ "${EUID}" -ne 0 ]]; then
  SUDO=(sudo)
fi

ensure_linux_build_environment() {
  if [[ "$(uname -s)" != "Linux" ]]; then
    echo "deploy_local.sh must run on Linux." >&2
    exit 1
  fi

  local need_apt=0
  local packages=(
    build-essential
    ca-certificates
    curl
    git
    libssl-dev
    pkg-config
  )

  command -v cc >/dev/null 2>&1 || need_apt=1
  command -v curl >/dev/null 2>&1 || need_apt=1
  command -v git >/dev/null 2>&1 || need_apt=1
  command -v pkg-config >/dev/null 2>&1 || need_apt=1
  pkg-config --exists openssl >/dev/null 2>&1 || need_apt=1

  if [[ "$need_apt" -eq 1 ]]; then
    if ! command -v apt-get >/dev/null 2>&1; then
      echo "Missing build dependencies and apt-get is not available. Install: ${packages[*]}" >&2
      exit 1
    fi
    echo "Installing missing Linux build dependencies ..."
    "${SUDO[@]}" apt-get update
    env DEBIAN_FRONTEND=noninteractive "${SUDO[@]}" apt-get install -y "${packages[@]}"
  else
    echo "Linux build dependencies already set up"
  fi

  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi

  if ! command -v cargo >/dev/null 2>&1; then
    echo "Installing Rust stable toolchain ..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
      sh -s -- -y --profile minimal --default-toolchain stable
  fi

  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi

  if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is unavailable after setup." >&2
    exit 1
  fi
}

has_gui_environment() {
  [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]] && return 0
  [[ -d /usr/share/xsessions ]] && compgen -G "/usr/share/xsessions/*.desktop" >/dev/null && return 0
  [[ -d /usr/share/wayland-sessions ]] && compgen -G "/usr/share/wayland-sessions/*.desktop" >/dev/null && return 0
  command -v Xorg >/dev/null 2>&1 && return 0
  return 1
}

ensure_linux_build_environment

# One unified binary. Build with the desktop (Tauri) feature only when a GUI is
# present; otherwise build headless so hosts like the NAS need no webkit/Tauri.
if has_gui_environment; then
  cargo build --release --bin auto_sync --bin auto_syncctl
  echo "GUI environment detected; built auto_sync with desktop support"
else
  cargo build --release --no-default-features --bin auto_sync --bin auto_syncctl
  echo "No GUI environment detected; built headless auto_sync (web only)"
fi
mkdir -p bin
install -m 0755 target/release/auto_sync bin/auto_sync
install -m 0755 target/release/auto_syncctl bin/auto_syncctl

"${SUDO[@]}" install -d -m 0755 \
  "$INSTALL_DIR/bin" \
  "$INSTALL_DIR/conf" \
  "$INSTALL_DIR/conf/state" \
  "$INSTALL_DIR/logs"

# Retire the old split layout (separate daemon + web service/binaries).
"${SUDO[@]}" systemctl disable --now auto_sync_web.service 2>/dev/null || true
"${SUDO[@]}" rm -f /etc/systemd/system/auto_sync_web.service
"${SUDO[@]}" rm -f \
  "$INSTALL_DIR/bin/auto_syncd" \
  "$INSTALL_DIR/bin/auto_sync_web" \
  "$INSTALL_DIR/bin/auto_sync_gui"

"${SUDO[@]}" install -m 0755 bin/auto_sync "$INSTALL_DIR/bin/auto_sync"
"${SUDO[@]}" install -m 0755 bin/auto_syncctl "$INSTALL_DIR/bin/auto_syncctl"

"${SUDO[@]}" install -m 0644 "$CONFIG" "$INSTALL_DIR/conf/auto_sync.toml"
echo "Installed local config $INSTALL_DIR/conf/auto_sync.toml from $CONFIG"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

bin/auto_syncctl print-systemd --install-dir "$INSTALL_DIR" > "$tmp_dir/auto_sync.service"

"${SUDO[@]}" install -m 0644 "$tmp_dir/auto_sync.service" /etc/systemd/system/auto_sync.service
"${SUDO[@]}" systemctl daemon-reload
"${SUDO[@]}" systemctl enable auto_sync.service
"${SUDO[@]}" systemctl restart auto_sync.service
"${SUDO[@]}" systemctl status --no-pager auto_sync.service
