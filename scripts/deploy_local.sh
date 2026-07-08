#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

CONFIG="${CONFIG:-}"
INSTALL_DIR="${INSTALL_DIR:-/opt/auto_sync}"

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
    unzip
    xz-utils
  )

  command -v cc >/dev/null 2>&1 || need_apt=1
  command -v curl >/dev/null 2>&1 || need_apt=1
  command -v git >/dev/null 2>&1 || need_apt=1
  command -v pkg-config >/dev/null 2>&1 || need_apt=1
  command -v unzip >/dev/null 2>&1 || need_apt=1
  command -v xz >/dev/null 2>&1 || need_apt=1
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

ensure_flutter_web_environment() {
  if command -v flutter >/dev/null 2>&1; then
    return
  fi

  local flutter_root="${FLUTTER_ROOT:-$HOME/flutter}"
  if [[ ! -x "$flutter_root/bin/flutter" ]]; then
    echo "Installing Flutter stable to $flutter_root ..."
    mkdir -p "$(dirname "$flutter_root")"
    git clone --depth 1 -b stable https://github.com/flutter/flutter.git "$flutter_root"
  fi
  export PATH="$flutter_root/bin:$PATH"
  if ! command -v flutter >/dev/null 2>&1; then
    echo "flutter is unavailable after setup." >&2
    exit 1
  fi
}

build_flutter_web() {
  ensure_flutter_web_environment
  local flutter_project="$ROOT_DIR/flutter/auto_sync_gui"
  if [[ ! -f "$flutter_project/pubspec.yaml" ]]; then
    echo "Missing Flutter project: $flutter_project" >&2
    exit 1
  fi
  export PUB_HOSTED_URL="${PUB_HOSTED_URL:-https://pub.flutter-io.cn}"
  export FLUTTER_STORAGE_BASE_URL="${FLUTTER_STORAGE_BASE_URL:-https://storage.flutter-io.cn}"
  (
    cd "$flutter_project"
    flutter pub get
    flutter build web --release --base-href /
  )
}

install_if_different() {
  local mode="$1"
  local src="$2"
  local dst="$3"
  if [[ "$(readlink -f "$src")" == "$(readlink -f "$dst" 2>/dev/null || printf '%s' "$dst")" ]]; then
    "${SUDO[@]}" chmod "$mode" "$dst"
    return
  fi
  "${SUDO[@]}" install -m "$mode" "$src" "$dst"
}

ensure_linux_build_environment
build_flutter_web

# One unified backend binary. The desktop GUI is the separate Flutter
# auto_sync_gui app on Windows; Linux/NAS deploys only the backend and web UI.
cargo build --release --bin auto_sync --bin auto_syncctl
echo "Built auto_sync backend and control utility"
mkdir -p bin
install -m 0755 target/release/auto_sync bin/auto_sync
install -m 0755 target/release/auto_syncctl bin/auto_syncctl

"${SUDO[@]}" install -d -m 0755 \
  "$INSTALL_DIR/bin" \
  "$INSTALL_DIR/conf" \
  "$INSTALL_DIR/conf/state" \
  "$INSTALL_DIR/logs" \
  "$INSTALL_DIR/web"

# Retire the old split layout (separate daemon + web service/binaries).
"${SUDO[@]}" systemctl disable --now auto_sync_web.service 2>/dev/null || true
"${SUDO[@]}" rm -f /etc/systemd/system/auto_sync_web.service
"${SUDO[@]}" rm -f \
  "$INSTALL_DIR/bin/auto_syncd" \
  "$INSTALL_DIR/bin/auto_sync_web" \
  "$INSTALL_DIR/bin/auto_sync_gui"

install_if_different 0755 bin/auto_sync "$INSTALL_DIR/bin/auto_sync"
install_if_different 0755 bin/auto_syncctl "$INSTALL_DIR/bin/auto_syncctl"
"${SUDO[@]}" rm -rf "$INSTALL_DIR/web"
"${SUDO[@]}" install -d -m 0755 "$INSTALL_DIR/web"
"${SUDO[@]}" cp -a "$ROOT_DIR/flutter/auto_sync_gui/build/web/." "$INSTALL_DIR/web/"

if "${SUDO[@]}" test -f "$INSTALL_DIR/conf/auto_sync.toml"; then
  echo "Preserved existing local config $INSTALL_DIR/conf/auto_sync.toml"
else
  if [[ ! -f "$CONFIG" ]]; then
    echo "Config file not found for initial install: $CONFIG" >&2
    exit 1
  fi
  "${SUDO[@]}" install -m 0644 "$CONFIG" "$INSTALL_DIR/conf/auto_sync.toml"
  echo "Initialized local config $INSTALL_DIR/conf/auto_sync.toml from $CONFIG"
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

bin/auto_syncctl print-systemd --install-dir "$INSTALL_DIR" > "$tmp_dir/auto_sync.service"

"${SUDO[@]}" install -m 0644 "$tmp_dir/auto_sync.service" /etc/systemd/system/auto_sync.service
"${SUDO[@]}" systemctl daemon-reload
"${SUDO[@]}" systemctl enable auto_sync.service
"${SUDO[@]}" systemctl restart auto_sync.service
"${SUDO[@]}" systemctl status --no-pager auto_sync.service
