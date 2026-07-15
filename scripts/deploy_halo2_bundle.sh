#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: deploy_halo2_bundle.sh [options]

Deploy a Halo 2 bundle created under halo_data.

Options:
  --install-dir DIR   Halo binary install directory (default: /opt/usr/local/halo)
  --data-dir DIR      Halo working/data directory (default: /root/.halo2)
  --service NAME      systemd service name (default: halo2)
  --restore-db        Restore the latest bundled halodb dump even if halodb exists
  --skip-db-restore   Never restore the bundled database dump
  -h, --help          Show this help
EOF
}

install_dir=/opt/usr/local/halo
data_dir=/root/.halo2
service_name=halo2
restore_db=auto

while [ "$#" -gt 0 ]; do
    case "$1" in
        --install-dir)
            install_dir="${2:?missing --install-dir value}"
            shift 2
            ;;
        --data-dir)
            data_dir="${2:?missing --data-dir value}"
            shift 2
            ;;
        --service)
            service_name="${2:?missing --service value}"
            shift 2
            ;;
        --restore-db)
            restore_db=force
            shift
            ;;
        --skip-db-restore)
            restore_db=skip
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: run as root" >&2
    exit 1
fi

bundle_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
jar_path="$(find "$bundle_dir/bin" -maxdepth 1 -type f -name 'halo-*.jar' | sort -V | tail -1)"
service_template="$bundle_dir/systemd/halo2.service"
runtime_src="$bundle_dir/runtime/.halo2"
backup_dir="$bundle_dir/backups/mysql"

if [ -z "$jar_path" ] || [ ! -f "$jar_path" ]; then
    echo "ERROR: no halo jar found under $bundle_dir/bin" >&2
    exit 1
fi
if [ ! -f "$service_template" ]; then
    echo "ERROR: missing $service_template" >&2
    exit 1
fi

jar_name="$(basename "$jar_path")"
unit_path="/etc/systemd/system/${service_name}.service"

log() {
    printf '[halo2-deploy] %s\n' "$*"
}

unit_exists() {
    systemctl cat "$service_name" >/dev/null 2>&1
}

if unit_exists; then
    log "stop $service_name before installing"
    systemctl stop "$service_name" || true
fi

log "install jar to $install_dir"
mkdir -p "$install_dir"
cp -a "$jar_path" "$install_dir/$jar_name"
chown root:root "$install_dir" "$install_dir/$jar_name"
chmod 755 "$install_dir"
chmod 644 "$install_dir/$jar_name"

if [ -d "$runtime_src" ]; then
    log "install runtime data to $data_dir"
    mkdir -p "$data_dir"
    cp -a "$runtime_src"/. "$data_dir"/
    chown -R root:root "$data_dir"
    find "$data_dir" -type d -exec chmod 755 {} +
    find "$data_dir" -type f -exec chmod 644 {} +
fi

log "install systemd unit $unit_path"
tmp_unit="$(mktemp)"
python3 - "$service_template" "$tmp_unit" "$install_dir/$jar_name" "$data_dir" <<'PY'
import re
import sys

src, dst, jar, data_dir = sys.argv[1:5]
text = open(src, "r", encoding="utf-8").read()
text = re.sub(r"WorkingDirectory=.*", f"WorkingDirectory={data_dir}", text)
text = re.sub(r"-jar\s+\S*halo-[^\s\\]+\.jar", f"-jar {jar}", text)
open(dst, "w", encoding="utf-8", newline="\n").write(text)
PY
install -m 0644 -o root -g root "$tmp_unit" "$unit_path"
rm -f "$tmp_unit"
systemctl daemon-reload
systemctl enable "$service_name"

latest_dump=""
if [ -d "$backup_dir" ]; then
    latest_dump="$(find "$backup_dir" -maxdepth 1 -type f \( -name 'halodb*.sql' -o -name 'halodb*.sql.gz' \) | sort -V | tail -1)"
fi

if [ "$restore_db" != "skip" ] && [ -n "$latest_dump" ]; then
    db_exists=0
    if command -v mysql >/dev/null 2>&1; then
        mysql -uroot -NBe "SHOW DATABASES LIKE 'halodb'" 2>/dev/null | grep -qx halodb && db_exists=1 || true
    fi
    if [ "$restore_db" = "force" ] || [ "$db_exists" -eq 0 ]; then
        log "restore halodb from $latest_dump"
        systemctl start mysql 2>/dev/null || true
        if [[ "$latest_dump" == *.gz ]]; then
            gzip -dc "$latest_dump" | mysql -uroot
        else
            mysql -uroot < "$latest_dump"
        fi
    else
        log "skip database restore: halodb already exists"
    fi
fi

log "start $service_name"
systemctl restart "$service_name"
active="$(systemctl is-active "$service_name" 2>/dev/null || true)"
pid="$(systemctl show "$service_name" -p MainPID --value 2>/dev/null || true)"
[ -n "$pid" ] && [ "$pid" != "0" ] || pid=none
log "$service_name: $(systemctl is-enabled "$service_name" 2>/dev/null || true) / ${active:-unknown} / pid=$pid"
[ "$active" = "active" ]
