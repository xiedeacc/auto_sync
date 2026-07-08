#!/usr/bin/env bash
set -euo pipefail

# Run on the AWS host as root. Restores files from a git worktree into the
# live AWS layout, then applies sysctl/acme/systemd state.

BLOG_DIR="${BLOG_DIR:-/usr/local/blog}"
GIT_DIR="${GIT_DIR:-/usr/local/blog/.backup-worktree}"
ROOT_SNAPSHOT_DIR="${ROOT_SNAPSHOT_DIR:-}"
RUN_USER="${RUN_USER:-ubuntu}"
ACME_USER="${ACME_USER:-ubuntu}"
ACME_HOME="${ACME_HOME:-/home/${ACME_USER}/.acme.sh}"
SSL_DIR="${SSL_DIR:-/etc/nginx/ssl}"
PULL=0
DRY_RUN=0

log() {
  printf '[deploy-aws] %s\n' "$*"
}

die() {
  printf '[deploy-aws] ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF_USAGE'
Usage: deploy_aws_from_git.sh [options]

Options:
  --git-dir DIR           Git worktree to restore from.
                          Default: /usr/local/blog/.backup-worktree
  --blog-dir DIR          Destination rblog install dir.
                          Default: /usr/local/blog
  --root-snapshot-dir DIR Optional collected root snapshot. If it contains
                          etc/ or usr/local/*, those files are copied to the
                          matching absolute paths.
  --run-user USER         Owner for /usr/local/blog. Default: ubuntu
  --acme-user USER        User that owns ~/.acme.sh. Default: ubuntu
  --pull                  Run git pull --ff-only in --git-dir first
  --dry-run               Print commands without changing the host
  -h, --help              Show this help
EOF_USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --git-dir)
      GIT_DIR="${2:?missing value for --git-dir}"
      shift 2
      ;;
    --blog-dir)
      BLOG_DIR="${2:?missing value for --blog-dir}"
      shift 2
      ;;
    --root-snapshot-dir)
      ROOT_SNAPSHOT_DIR="${2:?missing value for --root-snapshot-dir}"
      shift 2
      ;;
    --run-user)
      RUN_USER="${2:?missing value for --run-user}"
      shift 2
      ;;
    --acme-user)
      ACME_USER="${2:?missing value for --acme-user}"
      ACME_HOME="/home/${ACME_USER}/.acme.sh"
      shift 2
      ;;
    --pull)
      PULL=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      die "unknown argument: $1"
      ;;
  esac
done

run() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    printf '+'
    printf ' %q' "$@"
    printf '\n'
    return 0
  fi
  "$@"
}

run_shell() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    printf '+ bash -lc %q\n' "$1"
    return 0
  fi
  bash -lc "$1"
}

require_root() {
  [[ "$DRY_RUN" -eq 1 ]] && return
  [[ "${EUID:-$(id -u)}" -eq 0 ]] || die "run this script as root"
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

copy_file_if_present() {
  local src="$1"
  local dst="$2"
  local mode="${3:-}"
  [[ -f "$src" ]] || return 1
  log "copy $src -> $dst"
  run install -D "$src" "$dst"
  if [[ -n "$mode" ]]; then
    run chmod "$mode" "$dst"
  fi
}

copy_dir_if_present() {
  local src="$1"
  local dst="$2"
  [[ -d "$src" ]] || return 1
  log "sync $src/ -> $dst/"
  run mkdir -p "$dst"
  run rsync -a --delete "$src/" "$dst/"
}

is_root_layout() {
  [[ -d "$1/usr/local/blog" || -d "$1/etc" ]]
}

blog_source_dir() {
  if [[ -d "$GIT_DIR/usr/local/blog" ]]; then
    printf '%s/usr/local/blog\n' "$GIT_DIR"
  else
    printf '%s\n' "$GIT_DIR"
  fi
}

reconstruct_split_files() {
  local root="$1"
  local marker
  while IFS= read -r -d '' marker; do
    local dir rel file base split_bytes
    dir="$(dirname "$marker")"
    rel="$(awk -F= '$1 == "original" {print $2; exit}' "$marker")"
    split_bytes="$(awk -F= '$1 == "split_bytes" {print $2; exit}' "$marker")"
    [[ -n "$rel" ]] || die "bad split marker: $marker"
    file="$root/$rel"
    base="$(basename "$file")"
    log "reconstruct split file $rel (${split_bytes:-unknown} byte chunks)"
    run_shell "cat $(printf '%q' "$dir/$base").[0-9]* > $(printf '%q' "$file")"
    run_shell "rm -f $(printf '%q' "$dir/$base").[0-9]* $(printf '%q' "$marker")"
  done < <(find "$root" -name '*.rblog-split' -print0)
}

restore_blog_dir() {
  local src staging
  src="$(blog_source_dir)"
  [[ -d "$src" ]] || die "blog source not found: $src"

  staging="$(mktemp -d)"
  run rsync -a --exclude '/.git/' --exclude '/.git/**' "$src/" "$staging/"
  reconstruct_split_files "$staging"

  log "restore rblog files from $src to $BLOG_DIR"
  run mkdir -p "$BLOG_DIR"
  run rsync -a --delete \
    --exclude '/logs/' \
    --exclude '/logs/**' \
    --exclude '/.backup-worktree/' \
    --exclude '/.backup-worktree/**' \
    --exclude '/.git/' \
    --exclude '/.git/**' \
    "$staging/" "$BLOG_DIR/"
  if id "$RUN_USER" >/dev/null 2>&1; then
    run chown -R "$RUN_USER:$RUN_USER" "$BLOG_DIR"
  else
    log "user $RUN_USER does not exist; skipping chown"
  fi
  rm -rf "$staging"
}

resolve_root_snapshot_dir() {
  if [[ -n "$ROOT_SNAPSHOT_DIR" ]]; then
    printf '%s\n' "$ROOT_SNAPSHOT_DIR"
  elif is_root_layout "$GIT_DIR"; then
    printf '%s\n' "$GIT_DIR"
  else
    printf '\n'
  fi
}

restore_host_files_from_snapshot() {
  local root="$1"
  [[ -n "$root" && -d "$root" ]] || return 0

  copy_file_if_present "$root/etc/sysctl.d/99-shadowsocks-tune.conf" \
    "/etc/sysctl.d/99-shadowsocks-tune.conf" "0644" || true

  for unit in \
    rblog.service \
    rblog-backup.service \
    rblog-backup.timer \
    shadowsocks-rust.service \
    tbox.service \
    tbox_server.service \
    vlmcsd.service \
    waiwei.service \
    xray.service; do
    copy_file_if_present "$root/etc/systemd/system/$unit" \
      "/etc/systemd/system/$unit" "0644" || true
  done

  copy_dir_if_present "$root/etc/nginx" "/etc/nginx" || true
  copy_dir_if_present "$root/usr/local/shadowsocks" "/usr/local/shadowsocks" || true
  copy_dir_if_present "$root/usr/local/tbox" "/usr/local/tbox" || true
  copy_dir_if_present "$root/usr/local/vlmcsd" "/usr/local/vlmcsd" || true
  if copy_dir_if_present "$root/home/$ACME_USER/.acme.sh" "$ACME_HOME"; then
    if id "$ACME_USER" >/dev/null 2>&1; then
      run chown -R "$ACME_USER:$ACME_USER" "$ACME_HOME"
    fi
  fi
}

ensure_default_sysctl_file() {
  if [[ -f /etc/sysctl.d/99-shadowsocks-tune.conf ]]; then
    return
  fi

  log "write default /etc/sysctl.d/99-shadowsocks-tune.conf"
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "dry-run: would write default sysctl file"
    return
  fi
  cat >/etc/sysctl.d/99-shadowsocks-tune.conf <<'EOF_SYSCTL'
# Managed by deploy_aws_from_git.sh
net.ipv4.tcp_congestion_control = bbr
net.core.default_qdisc = fq
net.ipv4.tcp_fastopen = 3
net.ipv4.tcp_slow_start_after_idle = 0
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
net.core.rmem_default = 262144
net.core.wmem_default = 262144
net.ipv4.tcp_rmem = 4096 262144 16777216
net.ipv4.tcp_wmem = 4096 262144 16777216
net.core.netdev_max_backlog = 10000
net.core.somaxconn = 8192
net.ipv4.tcp_max_syn_backlog = 8192
net.ipv4.tcp_mtu_probing = 1
net.ipv4.tcp_notsent_lowat = 131072
net.ipv4.tcp_tw_reuse = 1
net.ipv4.ip_local_port_range = 10000 65535
net.ipv4.tcp_max_tw_buckets = 32768
net.ipv4.tcp_keepalive_time  = 60
net.ipv4.tcp_keepalive_intvl = 30
net.ipv4.tcp_keepalive_probes = 9
net.ipv4.tcp_fin_timeout = 15
net.ipv4.tcp_mem = 32768 65536 131072
EOF_SYSCTL
}

service_exists() {
  [[ -f "/etc/systemd/system/$1" ]] && return 0
  systemctl list-unit-files "$1" --no-legend 2>/dev/null | awk '{print $1}' | grep -Fxq "$1"
}

resolve_tbox_service() {
  if service_exists tbox.service; then
    printf 'tbox.service\n'
  else
    printf 'tbox_server.service\n'
  fi
}

enable_restart_service() {
  local unit="$1"
  if ! service_exists "$unit"; then
    log "skip missing service $unit"
    return
  fi
  log "enable and restart $unit"
  run systemctl enable "$unit"
  run systemctl restart "$unit"
}

stop_disable_service() {
  local unit="$1"
  if ! service_exists "$unit"; then
    log "skip missing service $unit"
    return
  fi
  log "disable and stop $unit"
  run systemctl disable --now "$unit"
}

ensure_vlmcsd_path() {
  if [[ -x /usr/local/bin/vlmcsd || ! -x /usr/local/vlmcsd/bin/vlmcsd ]]; then
    return
  fi
  log "link /usr/local/bin/vlmcsd -> /usr/local/vlmcsd/bin/vlmcsd"
  run mkdir -p /usr/local/bin
  run ln -sfn /usr/local/vlmcsd/bin/vlmcsd /usr/local/bin/vlmcsd
}

apply_services() {
  local tbox_unit
  tbox_unit="$(resolve_tbox_service)"

  ensure_vlmcsd_path
  log "systemd daemon-reload"
  run systemctl daemon-reload

  stop_disable_service waiwei.service
  stop_disable_service xray.service

  enable_restart_service rblog.service
  if service_exists rblog-backup.timer; then
    log "enable and restart rblog-backup.timer"
    run systemctl enable rblog-backup.timer
    run systemctl restart rblog-backup.timer
  fi
  if service_exists rblog-backup.service; then
    log "run rblog-backup.service once"
    run systemctl restart rblog-backup.service
  fi
  enable_restart_service shadowsocks-rust.service
  enable_restart_service "$tbox_unit"
  enable_restart_service vlmcsd.service
}

sudo_as_acme_user() {
  run sudo -Hu "$ACME_USER" "$@"
}

ensure_acme_domain() {
  local domain="$1"
  local wildcard="$2"
  local acme_sh="$ACME_HOME/acme.sh"
  local domain_conf="$ACME_HOME/${domain}_ecc/${domain}.conf"

  [[ -x "$acme_sh" ]] || die "acme.sh not found: $acme_sh"
  [[ -f "$ACME_HOME/account.conf" ]] || die "acme account.conf not found: $ACME_HOME/account.conf"
  grep -q '^SAVED_AWS_ACCESS_KEY_ID=' "$ACME_HOME/account.conf" || die "missing AWS DNS key in acme account.conf"
  grep -q '^SAVED_AWS_SECRET_ACCESS_KEY=' "$ACME_HOME/account.conf" || die "missing AWS DNS secret in acme account.conf"

  if [[ ! -f "$domain_conf" ]]; then
    log "issue missing certificate for $domain and $wildcard"
    sudo_as_acme_user "$acme_sh" --issue --dns dns_aws -d "$domain" -d "$wildcard" \
      --keylength ec-256 --server zerossl
  fi

  log "install acme cert paths for $domain"
  run mkdir -p "$SSL_DIR"
  if id "$ACME_USER" >/dev/null 2>&1; then
    run chown -R "$ACME_USER:$ACME_USER" "$SSL_DIR"
    run chmod 0750 "$SSL_DIR"
  fi
  sudo_as_acme_user "$acme_sh" --install-cert -d "$domain" --ecc \
    --key-file "$SSL_DIR/${domain}.key" \
    --cert-file "$SSL_DIR/${domain}.cer" \
    --ca-file "$SSL_DIR/${domain}.ca.cer" \
    --fullchain-file "$SSL_DIR/${domain}.fullchain.cer" \
    --reloadcmd "sudo systemctl reload nginx"

  [[ -f "$domain_conf" ]] || die "acme config still missing after install: $domain_conf"
  grep -q "Le_Webroot='dns_aws'" "$domain_conf" || die "$domain does not use dns_aws"
  grep -q "Le_RealFullChainPath='$SSL_DIR/${domain}.fullchain.cer'" "$domain_conf" || die "$domain fullchain install path was not recorded"
  grep -q "Le_RealKeyPath='$SSL_DIR/${domain}.key'" "$domain_conf" || die "$domain key install path was not recorded"
}

apply_acme() {
  local acme_sh="$ACME_HOME/acme.sh"
  if [[ ! -x "$acme_sh" ]]; then
    log "skip acme setup: $acme_sh not found"
    return
  fi

  log "enable acme.sh auto-upgrade"
  sudo_as_acme_user "$acme_sh" --upgrade --auto-upgrade
  log "install acme.sh renewal cronjob"
  sudo_as_acme_user "$acme_sh" --install-cronjob
  ensure_acme_domain xiedeacc.com "*.xiedeacc.com"
  ensure_acme_domain youkechat.net "*.youkechat.net"

  if command -v nginx >/dev/null 2>&1; then
    log "test nginx config"
    run nginx -t
    log "reload nginx"
    run systemctl reload nginx
  fi
}

main() {
  require_root
  for cmd in git rsync systemctl sysctl sudo find awk grep; do
    require_command "$cmd"
  done

  [[ -d "$GIT_DIR" ]] || die "git dir not found: $GIT_DIR"
  if [[ "$PULL" -eq 1 ]]; then
    log "git pull --ff-only in $GIT_DIR"
    run git -C "$GIT_DIR" pull --ff-only
  fi

  restore_blog_dir
  restore_host_files_from_snapshot "$(resolve_root_snapshot_dir)"
  ensure_default_sysctl_file
  log "apply sysctl settings"
  run sysctl --system
  apply_acme
  apply_services
  log "done"
}

main "$@"
