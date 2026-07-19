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

if [[ "$(realpath -m "$INSTALL_DIR")" == "$(realpath -m /opt/usr/local/auto_sync)" ]]; then
  echo "deploy_local.sh no longer deploys NAS directly." >&2
  echo "Run on dev instead: scripts/deploy_nas.sh" >&2
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

configure_domestic_build_mirrors() {
  export GOPROXY="${GOPROXY:-https://goproxy.cn,direct}"
  export RUSTUP_DIST_SERVER="${RUSTUP_DIST_SERVER:-https://rsproxy.cn}"
  export RUSTUP_UPDATE_ROOT="${RUSTUP_UPDATE_ROOT:-https://rsproxy.cn/rustup}"
  export CARGO_REGISTRIES_CRATES_IO_PROTOCOL="${CARGO_REGISTRIES_CRATES_IO_PROTOCOL:-sparse}"
  export PUB_HOSTED_URL="${PUB_HOSTED_URL:-https://pub.flutter-io.cn}"
  export FLUTTER_STORAGE_BASE_URL="${FLUTTER_STORAGE_BASE_URL:-https://storage.flutter-io.cn}"

  if command -v apt-get >/dev/null 2>&1; then
    local apt_changed=0
    if [[ -f /etc/apt/sources.list.d/ubuntu.sources ]] && \
      grep -Eq 'archive\.ubuntu\.com|security\.ubuntu\.com' /etc/apt/sources.list.d/ubuntu.sources; then
      "${SUDO[@]}" sed -i \
        's#http://archive.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#http://security.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#https://archive.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#https://security.ubuntu.com#https://mirrors.cloud.tencent.com#g' \
        /etc/apt/sources.list.d/ubuntu.sources
      apt_changed=1
    fi
    if [[ -f /etc/apt/sources.list ]] && \
      grep -Eq 'archive\.ubuntu\.com|security\.ubuntu\.com' /etc/apt/sources.list; then
      "${SUDO[@]}" sed -i \
        's#http://archive.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#http://security.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#https://archive.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#https://security.ubuntu.com#https://mirrors.cloud.tencent.com#g' \
        /etc/apt/sources.list
      apt_changed=1
    fi
    if [[ "$apt_changed" -eq 1 ]]; then
      echo "Configured Ubuntu apt mirror: https://mirrors.cloud.tencent.com"
    fi
  fi

  mkdir -p "$HOME/.cargo"
  if [[ ! -f "$HOME/.cargo/config.toml" ]]; then
    cat > "$HOME/.cargo/config.toml" <<'EOF_CARGO_MIRROR'
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
git-fetch-with-cli = true
EOF_CARGO_MIRROR
  elif ! grep -q 'rsproxy\.cn' "$HOME/.cargo/config.toml"; then
    echo "Existing $HOME/.cargo/config.toml does not use rsproxy; leaving it unchanged"
  fi
}

ensure_linux_build_environment() {
  if [[ "$(uname -s)" != "Linux" ]]; then
    echo "deploy_local.sh must run on Linux." >&2
    exit 1
  fi

  configure_domestic_build_mirrors

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
    curl --proto '=https' --tlsv1.2 -sSf "${RUSTUP_INIT_URL:-https://rsproxy.cn/rustup-init.sh}" |
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
  local flutter_root="${FLUTTER_ROOT:-/root/src/software/flutter}"
  if [[ "$(realpath -m "$flutter_root")" != "$(realpath -m /root/src/software/flutter)" ]]; then
    echo "Unsupported Flutter SDK path: $flutter_root" >&2
    echo "Use /root/src/software/flutter on dev. NAS is deployed from dev via scripts/deploy_nas.sh." >&2
    exit 1
  fi
  if [[ -x "$flutter_root/bin/flutter" ]]; then
    export PATH="$flutter_root/bin:$PATH"
    return
  fi

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
  configure_domestic_build_mirrors
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

write_systemd_unit() {
  local install_dir="$1"
  local output="$2"
  cat > "$output" <<EOF_SYSTEMD
[Unit]
Description=auto_sync daemon
After=local-fs.target network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$install_dir
ExecStart=$install_dir/bin/auto_sync
Restart=always
RestartSec=5
User=root
Group=root
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_SYS_RAWIO CAP_DAC_READ_SEARCH CAP_DAC_OVERRIDE CAP_FOWNER CAP_CHOWN
AmbientCapabilities=CAP_SYS_ADMIN CAP_SYS_RAWIO CAP_DAC_READ_SEARCH CAP_DAC_OVERRIDE CAP_FOWNER CAP_CHOWN
NoNewPrivileges=false

[Install]
WantedBy=multi-user.target
EOF_SYSTEMD
}

ensure_linux_build_environment
build_flutter_web

# One unified backend binary. The desktop GUI is the separate Flutter
# auto_sync_gui app on Windows; Linux/NAS deploys only the backend and web UI.
cargo build --release --bin auto_sync
echo "Built auto_sync backend"
mkdir -p bin
install -m 0755 target/release/auto_sync bin/auto_sync

"${SUDO[@]}" install -d -m 0755 \
  "$INSTALL_DIR/bin" \
  "$INSTALL_DIR/conf" \
  "$INSTALL_DIR/data" \
  "$INSTALL_DIR/logs" \
  "$INSTALL_DIR/web"

install_if_different 0755 bin/auto_sync "$INSTALL_DIR/bin/auto_sync"
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

write_systemd_unit "$INSTALL_DIR" "$tmp_dir/auto_sync.service"

"${SUDO[@]}" install -m 0644 "$tmp_dir/auto_sync.service" /etc/systemd/system/auto_sync.service
"${SUDO[@]}" systemctl daemon-reload
"${SUDO[@]}" systemctl enable auto_sync.service
"${SUDO[@]}" systemctl restart auto_sync.service
"${SUDO[@]}" systemctl status --no-pager auto_sync.service
