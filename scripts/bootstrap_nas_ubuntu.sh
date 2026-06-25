#!/usr/bin/env bash
set -euo pipefail

REPO_URL="${REPO_URL:-https://github.com/xiedeacc/auto_sync.git}"
REPO_DIR="${REPO_DIR:-/root/src/rust/auto_sync}"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "This bootstrap script must run on Linux." >&2
  exit 1
fi

if ! command -v apt-get >/dev/null 2>&1; then
  echo "This bootstrap script expects Ubuntu/Debian with apt-get." >&2
  exit 1
fi

SUDO=()
if [[ "${EUID}" -ne 0 ]]; then
  SUDO=(sudo)
fi

"${SUDO[@]}" apt-get update
env DEBIAN_FRONTEND=noninteractive "${SUDO[@]}" apt-get install -y \
  build-essential \
  ca-certificates \
  curl \
  git \
  libssl-dev \
  pkg-config

if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain stable
fi

if [[ -f "$HOME/.cargo/env" ]]; then
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is still unavailable after rustup install." >&2
  exit 1
fi

mkdir -p "$(dirname "$REPO_DIR")"
if [[ -d "$REPO_DIR/.git" ]]; then
  git -C "$REPO_DIR" remote set-url origin "$REPO_URL" || true
  git -C "$REPO_DIR" fetch --all --prune
else
  if [[ -e "$REPO_DIR" ]]; then
    echo "$REPO_DIR exists but is not a git repository." >&2
    exit 1
  fi
  git clone "$REPO_URL" "$REPO_DIR"
fi

echo "NAS Ubuntu build environment is ready."
echo "Repository: $REPO_DIR"
echo "Cargo: $(command -v cargo)"
