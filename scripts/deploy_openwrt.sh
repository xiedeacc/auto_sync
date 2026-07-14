#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

CONFIG="${CONFIG:-conf/auto_sync.linux.toml}"
PROCD_INIT="${PROCD_INIT:-conf/auto_sync.procd}"
HOST="${HOST:-192.168.2.1}"
PORT="${PORT:-10022}"
USER="${USER:-root}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/auto_sync}"
BINARY_DIR="${BINARY_DIR:-}"
TARGET="${TARGET:-aarch64-unknown-linux-musl}"
OUT_DIR="${OUT_DIR:-bin/openwrt/aarch64}"
OPENWRT_TOOLCHAIN="${OPENWRT_TOOLCHAIN:-}"

TOOLCHAIN_SEARCH_ROOTS=(
  /root/src/software/openwrt
  /root/src/software
  /opt/src/software/openwrt
  /opt
)

auto_detect_toolchain() {
  local root dir
  for root in "${TOOLCHAIN_SEARCH_ROOTS[@]}"; do
    [[ -d "$root" ]] || continue
    dir="$(find "$root" -maxdepth 3 -type d \
      -name 'toolchain-aarch64_*_musl' \
      -exec ls -dt {} + 2>/dev/null | head -1)"
    if [[ -n "$dir" && -x "$dir/bin/aarch64-openwrt-linux-musl-gcc" ]]; then
      printf '%s\n' "$dir"
      return
    fi
    dir="$root/aarch64-linux-musl-cross"
    if [[ -x "$dir/bin/aarch64-linux-musl-gcc" ]]; then
      printf '%s\n' "$dir"
      return
    fi
  done
}

apply_openwrt_toolchain() {
  local tc_root="$1"
  local tc_bin="$tc_root/bin"
  local cc ar
  export PATH="$tc_bin:$PATH"

  if [[ -x "$tc_bin/aarch64-openwrt-linux-musl-gcc" ]]; then
    cc="$tc_bin/aarch64-openwrt-linux-musl-gcc"
    ar="$tc_bin/aarch64-openwrt-linux-musl-gcc-ar"
  else
    cc="$tc_bin/aarch64-linux-musl-gcc"
    ar="$tc_bin/aarch64-linux-musl-ar"
  fi
  export CC_aarch64_unknown_linux_musl="${CC_aarch64_unknown_linux_musl:-$cc}"
  export AR_aarch64_unknown_linux_musl="${AR_aarch64_unknown_linux_musl:-$ar}"
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:-$cc}"
  echo "Using OpenWrt toolchain: $tc_root"
  echo "  CC = $CC_aarch64_unknown_linux_musl"
}

build_openwrt_binaries() {
  rustup target add "$TARGET" >/dev/null

  if [[ -n "$OPENWRT_TOOLCHAIN" ]]; then
    if [[ ! -d "$OPENWRT_TOOLCHAIN/bin" ]]; then
      echo "OpenWrt toolchain bin directory not found: $OPENWRT_TOOLCHAIN/bin" >&2
      exit 1
    fi
    apply_openwrt_toolchain "$OPENWRT_TOOLCHAIN"
  else
    detected="$(auto_detect_toolchain)"
    if [[ -n "$detected" ]]; then
      apply_openwrt_toolchain "$detected"
    else
      echo "Missing aarch64 musl C compiler." >&2
      echo "Set OPENWRT_TOOLCHAIN=/path/to/toolchain or install the OpenWrt toolchain under /root/src/software/openwrt." >&2
      exit 1
    fi
  fi

  cargo build --release --target "$TARGET" \
    --no-default-features \
    --bin auto_sync

  mkdir -p "$OUT_DIR"
  install -m 0755 "target/$TARGET/release/auto_sync" "$OUT_DIR/auto_sync"

  echo "OpenWrt aarch64 binary staged in $OUT_DIR"
}

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
    --procd-init)
      PROCD_INIT="$2"
      shift 2
      ;;
    --binary-dir)
      BINARY_DIR="$2"
      shift 2
      ;;
    --target)
      TARGET="$2"
      shift 2
      ;;
    --out-dir)
      OUT_DIR="$2"
      shift 2
      ;;
    --openwrt-toolchain)
      OPENWRT_TOOLCHAIN="$2"
      shift 2
      ;;
    -h|--help)
      echo "Usage: scripts/deploy_openwrt.sh [--config PATH] [--procd-init PATH] [--host HOST] [--port PORT] [--user USER] [--install-dir DIR] [--binary-dir DIR] [--target TARGET] [--out-dir DIR] [--openwrt-toolchain DIR]"
      echo "Defaults: --host 192.168.2.1 --port 10022 --user root --install-dir /usr/local/auto_sync"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [[ ! -f "$PROCD_INIT" ]]; then
  echo "procd init template not found: $PROCD_INIT" >&2
  exit 1
fi

target="${USER}@${HOST}"
arch="$(ssh -p "$PORT" "$target" "uname -m" | tr -d '\r')"

if [[ -z "$BINARY_DIR" ]]; then
  if [[ "$arch" == "aarch64" || "$arch" == "arm64" ]]; then
    build_openwrt_binaries
    BINARY_DIR="$OUT_DIR"
  elif [[ -d "bin/openwrt/$arch" ]]; then
    BINARY_DIR="bin/openwrt/$arch"
  elif [[ -d "bin/openwrt" ]]; then
    BINARY_DIR="bin/openwrt"
  else
    echo "No OpenWrt binaries found for arch '$arch'." >&2
    echo "Put auto_sync under bin/openwrt/$arch or pass --binary-dir." >&2
    exit 1
  fi
fi

if [[ ! -x "$BINARY_DIR/auto_sync" ]]; then
  echo "$BINARY_DIR/auto_sync is missing or not executable" >&2
  exit 1
fi

ssh -p "$PORT" "$target" "mkdir -p '$INSTALL_DIR/bin' '$INSTALL_DIR/conf' '$INSTALL_DIR/conf/state' '$INSTALL_DIR/logs'"

# Stop and retire the old split services/binaries.
ssh -p "$PORT" "$target" "/etc/init.d/auto_sync stop >/dev/null 2>&1 || true; /etc/init.d/auto_sync_web stop >/dev/null 2>&1 || true; /etc/init.d/auto_sync_web disable >/dev/null 2>&1 || true; rm -f /etc/init.d/auto_sync_web ${INSTALL_DIR}/bin/auto_syncd ${INSTALL_DIR}/bin/auto_sync_web ${INSTALL_DIR}/bin/auto_sync_gui ${INSTALL_DIR}/bin/auto_syncctl"

scp -O -P "$PORT" "$BINARY_DIR/auto_sync" "${target}:${INSTALL_DIR}/bin/auto_sync"

if ssh -p "$PORT" "$target" "test -f '${INSTALL_DIR}/conf/auto_sync.toml'"; then
  echo "Preserved existing OpenWrt config $INSTALL_DIR/conf/auto_sync.toml"
else
  if [[ ! -f "$CONFIG" ]]; then
    echo "Config file not found for initial install: $CONFIG" >&2
    exit 1
  fi
  scp -O -P "$PORT" "$CONFIG" "${target}:${INSTALL_DIR}/conf/auto_sync.toml"
  echo "Initialized OpenWrt config $INSTALL_DIR/conf/auto_sync.toml from $CONFIG"
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

sed "s|@INSTALL_DIR@|$INSTALL_DIR|g" "$PROCD_INIT" > "$tmp_dir/auto_sync"

scp -O -P "$PORT" "$tmp_dir/auto_sync" "${target}:/etc/init.d/auto_sync"

ssh -p "$PORT" "$target" "chmod +x /etc/init.d/auto_sync"
ssh -p "$PORT" "$target" "if command -v opkg >/dev/null 2>&1; then opkg update && opkg install openssh-client || true; fi"
ssh -p "$PORT" "$target" "/etc/init.d/auto_sync enable && /etc/init.d/auto_sync start"
ssh -p "$PORT" "$target" "/etc/init.d/auto_sync status || true"
