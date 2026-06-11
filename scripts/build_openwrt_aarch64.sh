#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TARGET="${TARGET:-aarch64-unknown-linux-musl}"
OUT_DIR="${OUT_DIR:-bin/openwrt/aarch64}"
OPENWRT_TOOLCHAIN="${OPENWRT_TOOLCHAIN:-}"

TOOLCHAIN_SEARCH_ROOTS=(
  /root/src/toolchains
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

apply_toolchain() {
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

rustup target add "$TARGET" >/dev/null

if [[ -n "$OPENWRT_TOOLCHAIN" ]]; then
  if [[ ! -d "$OPENWRT_TOOLCHAIN/bin" ]]; then
    echo "OpenWrt toolchain bin directory not found: $OPENWRT_TOOLCHAIN/bin" >&2
    exit 1
  fi
  apply_toolchain "$OPENWRT_TOOLCHAIN"
else
  detected="$(auto_detect_toolchain)"
  if [[ -n "$detected" ]]; then
    apply_toolchain "$detected"
  else
    echo "Missing aarch64 musl C compiler." >&2
    echo "Set OPENWRT_TOOLCHAIN=/path/to/toolchain or install /opt/aarch64-linux-musl-cross." >&2
    exit 1
  fi
fi

cargo build --release --target "$TARGET" \
  --no-default-features \
  --bin auto_syncd \
  --bin auto_sync_web \
  --bin auto_syncctl

mkdir -p "$OUT_DIR"
install -m 0755 "target/$TARGET/release/auto_syncd" "$OUT_DIR/auto_syncd"
install -m 0755 "target/$TARGET/release/auto_sync_web" "$OUT_DIR/auto_sync_web"
install -m 0755 "target/$TARGET/release/auto_syncctl" "$OUT_DIR/auto_syncctl"

echo "OpenWrt aarch64 binaries staged in $OUT_DIR"
