#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

OUT_DIR="${OUT_DIR:-bin/windows}"
OPENSSH_URL="${OPENSSH_URL:-https://github.com/PowerShell/Win32-OpenSSH/releases/download/10.0.0.0p2-Preview/OpenSSH-Win64.zip}"
OPENSSH_SHA256="${OPENSSH_SHA256:-23f50f3458c4c5d0b12217c6a5ddfde0137210a30fa870e98b29827f7b43aba5}"

require_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "$1 is required" >&2
    exit 1
  fi
}

verify_sha256() {
  local file="$1"
  local expected="$2"
  local actual
  actual="$(sha256sum "$file" | awk '{print $1}')"
  if [[ "$actual" != "${expected,,}" ]]; then
    echo "sha256 mismatch for $file" >&2
    echo "expected: ${expected,,}" >&2
    echo "actual:   $actual" >&2
    exit 1
  fi
}

require_tool curl
require_tool unzip
require_tool sha256sum

mkdir -p "$OUT_DIR"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

curl -fL "$OPENSSH_URL" -o "$OUT_DIR/OpenSSH-Win64.zip"
verify_sha256 "$OUT_DIR/OpenSSH-Win64.zip" "$OPENSSH_SHA256"

rm -rf "$OUT_DIR/openssh"
mkdir -p "$OUT_DIR/openssh"
unzip -q "$OUT_DIR/OpenSSH-Win64.zip" -d "$OUT_DIR/openssh"

(
  cd "$OUT_DIR"
  sha256sum OpenSSH-Win64.zip > SHA256SUMS
)

cat > "$OUT_DIR/README.txt" <<EOF
Windows runtime bundled by scripts/download_windows_runtime.sh

OpenSSH:
  OpenSSH-Win64.zip
  Source: $OPENSSH_URL

Extracted folders:
  openssh/

Windows deployment prefers the system OpenSSH installation and uses this
bundled OpenSSH only as a fallback.
EOF

echo "Windows runtime downloaded to $OUT_DIR"
