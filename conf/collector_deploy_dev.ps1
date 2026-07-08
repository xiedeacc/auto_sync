$ErrorActionPreference = 'Stop'
# Runs on Windows. Pushes the collected dev tree back to the Ubuntu host, then
# applies packages, services, runtimes, quotas, database restore, and cron.
$ssh  = $env:AS_SSH
$scp  = $env:AS_SCP
$dest = $env:AS_DEST
$root = $env:AS_ROOT

$opts    = @('-o','BatchMode=yes','-o','StrictHostKeyChecking=accept-new','-o','ConnectTimeout=20')
$sshArgs = @() + $opts
$scpArgs = @('-r','-p') + $opts
if (-not [string]::IsNullOrEmpty($env:AS_PORT)) { $sshArgs += @('-p', $env:AS_PORT); $scpArgs += @('-P', $env:AS_PORT) }
if (-not [string]::IsNullOrEmpty($env:AS_KEY))  { $sshArgs += @('-i', $env:AS_KEY);  $scpArgs += @('-i', $env:AS_KEY) }

$errCount = 0
$collectPaths = @($env:AS_COLLECT_PATHS -split "`n" | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
$excludePaths = @($env:AS_EXCLUDE_PATHS -split "`n" | ForEach-Object { $_.Trim().TrimEnd([char[]]"/") } | Where-Object { $_ -ne '' })

function Normalize-RemotePath([string]$Path) {
    $p = ($Path -replace '\\','/').Trim()
    if ($p -eq '') { return '/' }
    if (-not $p.StartsWith('/')) { $p = '/' + $p }
    while ($p.Length -gt 1 -and $p.EndsWith('/')) { $p = $p.Substring(0, $p.Length - 1) }
    return $p
}

function Test-RemoteExcluded([string]$Path) {
    $p = Normalize-RemotePath $Path
    foreach ($ex in $excludePaths) {
        $e = Normalize-RemotePath $ex
        if ($p -eq $e -or $p.StartsWith($e + '/')) { return $true }
    }
    return $false
}

function Get-LocalCollectedPath([string]$RemotePath) {
    return Join-Path $root ((Normalize-RemotePath $RemotePath).TrimStart([char[]]"/") -replace '/','\')
}

function Join-RemotePath([string]$Base, [string]$Relative) {
    $b = Normalize-RemotePath $Base
    $r = ($Relative -replace '\\','/').Trim([char[]]"/")
    if ($r -eq '') { return $b }
    if ($b -eq '/') { return '/' + $r }
    return $b + '/' + $r
}

function Copy-CollectedPathToStage([string]$RemotePath, [string]$StageRoot) {
    $remote = Normalize-RemotePath $RemotePath
    if (Test-RemoteExcluded $remote) {
        Write-Host "skip excluded $remote"
        return
    }

    $local = Get-LocalCollectedPath $remote
    if (-not (Test-Path -LiteralPath $local)) {
        Write-Host "! missing local $local"
        return
    }

    $target = Join-Path $StageRoot ($remote.TrimStart([char[]]"/") -replace '/','\')
    $item = Get-Item -LiteralPath $local -Force
    if (-not $item.PSIsContainer) {
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
        Copy-Item -LiteralPath $local -Destination $target -Force
        Write-Host "stage $remote"
        return
    }

    New-Item -ItemType Directory -Force -Path $target | Out-Null
    Write-Host "stage $remote"
    $baseLen = $item.FullName.TrimEnd([char[]]"\/").Length
    Get-ChildItem -LiteralPath $local -Force -Recurse | ForEach-Object {
        $rel = $_.FullName.Substring($baseLen).TrimStart([char[]]"\/")
        $remoteChild = Join-RemotePath $remote $rel
        if (-not (Test-RemoteExcluded $remoteChild)) {
            $childTarget = Join-Path $target ($rel -replace '/','\')
            if ($_.PSIsContainer) {
                New-Item -ItemType Directory -Force -Path $childTarget | Out-Null
            } else {
                New-Item -ItemType Directory -Force -Path (Split-Path -Parent $childTarget) | Out-Null
                Copy-Item -LiteralPath $_.FullName -Destination $childTarget -Force
            }
        }
    }
}

function Invoke-Remote([string]$Script) {
    $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($Script))
    & $ssh @sshArgs $dest "echo $b64 | base64 -d | bash"
    if ($LASTEXITCODE -ne 0) { Write-Host "! remote step exit $LASTEXITCODE"; $script:errCount++ }
}

Invoke-Remote 'rm -rf ~/.auto_sync_stage; mkdir -p ~/.auto_sync_stage'
$localStage = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_dev_deploy_stage_" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $localStage | Out-Null
try {
    foreach ($p in $collectPaths) {
        Copy-CollectedPathToStage $p $localStage
    }
    foreach ($p in @('/root/auto_sync_db_dumps', '/tmp/auto_sync_db_dumps')) {
        if (Test-Path -LiteralPath (Get-LocalCollectedPath $p)) {
            Copy-CollectedPathToStage $p $localStage
        }
    }

    foreach ($entry in Get-ChildItem -LiteralPath $localStage -Force) {
        & $scp @scpArgs -- $entry.FullName ('{0}:~/.auto_sync_stage/' -f $dest)
        if ($LASTEXITCODE -ne 0) { Write-Host "! scp failed for staged $($entry.Name)"; $errCount++ }
    }
} finally {
    Remove-Item -LiteralPath $localStage -Recurse -Force -ErrorAction SilentlyContinue
}

Invoke-Remote 'rsync -rlptD ~/.auto_sync_stage/ / && rm -rf ~/.auto_sync_stage && echo "installed collected paths"'

if (-not [string]::IsNullOrWhiteSpace($env:AS_PERMS_FILE) -and (Test-Path -LiteralPath $env:AS_PERMS_FILE)) {
    $chmods = New-Object System.Collections.Generic.List[string]
    foreach ($line in [IO.File]::ReadAllLines($env:AS_PERMS_FILE)) {
        $t = $line.Trim(); if ($t -eq '') { continue }
        $sp = $t.IndexOf(' '); if ($sp -lt 1) { continue }
        $mode = $t.Substring(0, $sp); $path = $t.Substring($sp + 1)
        if ($path.StartsWith('/etc/') -or $path.StartsWith('/usr/local/') -or $path.StartsWith('/root/')) {
            $chmods.Add("chmod $mode '$path' 2>/dev/null || true")
        }
    }
    if ($chmods.Count -gt 0) { Write-Host "restoring $($chmods.Count) recorded permissions"; Invoke-Remote ($chmods -join "`n") }
}

$remote = @'
set -u
export DEBIAN_FRONTEND=noninteractive

log() { printf '[dev-deploy] %s\n' "$*"; }
try() { "$@" && return 0; log "WARN: command failed: $*"; return 0; }
unit_exists() { systemctl list-unit-files "$1" >/dev/null 2>&1 || [ -e "/etc/systemd/system/$1" ] || [ -e "/lib/systemd/system/$1" ]; }
restart_if_exists() {
    unit="$1"
    if unit_exists "$unit"; then
        systemctl enable "$unit" >/dev/null 2>&1 || true
        systemctl restart "$unit" && log "restarted $unit" || log "WARN: restart $unit failed"
    fi
}
disable_if_exists() {
    unit="$1"
    if unit_exists "$unit"; then
        systemctl disable "$unit" >/dev/null 2>&1 || true
        systemctl stop "$unit" >/dev/null 2>&1 || true
        systemctl reset-failed "$unit" >/dev/null 2>&1 || true
        log "disabled+stopped $unit"
    fi
}

if [ -f /etc/apt/sources.list.d/ubuntu.sources ]; then
    sed -i 's#http://archive.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#http://security.ubuntu.com#https://mirrors.cloud.tencent.com#g' /etc/apt/sources.list.d/ubuntu.sources
fi
apt-get update
apt-get purge -y vim || true
apt-get autoremove -y || true
apt-get install -y \
    dialog zfsutils-linux openssh-server zsh net-tools curl iputils-ping iftop cron ca-certificates xfonts-utils dnsutils hdparm \
    autoconf libtool libtool-bin cmake make gcc g++ texinfo bison flex gdb build-essential automake pkg-config help2man gettext \
    python3-pip ruby luajit netcat-openbsd gperf cscope exuberant-ctags vim-nox git git-lfs lcov graphviz \
    libgeoip-dev libxml2 libxml2-dev libxslt1.1 libxslt1-dev libatomic-ops-dev libgd-dev libperl-dev libluajit-5.1-dev tcl-dev ruby-dev libncurses-dev \
    mysql-server postgresql postgresql-client libpq-dev postgresql-contrib sysstat nginx-full \
    ffmpeg libimage-exiftool-perl postgresql-server-dev-16 redis quota privoxy
apt-get remove -y apport || true
apt-get autoremove -y || true

cp /usr/share/zoneinfo/Asia/Shanghai /etc/localtime

reset_xiedeacc_hosts() {
    tmp="$(mktemp)"
    grep -Ev '(^|[[:space:]])(dev|code|unlock-music|coverage|immich)\.xiedeacc\.com([[:space:]]|$)' /etc/hosts > "$tmp" 2>/dev/null || true
    cat "$tmp" > /etc/hosts
    rm -f "$tmp"
}
set_host_entry() {
    printf '%s %s\n' "$1" "$2" >> /etc/hosts
}
reset_xiedeacc_hosts
set_host_entry 127.0.0.1 dev.xiedeacc.com
set_host_entry 192.168.2.247 code.xiedeacc.com
set_host_entry 192.168.2.247 unlock-music.xiedeacc.com
set_host_entry 127.0.0.1 coverage.xiedeacc.com

swapoff -a || true
sed -i '/^\/swap\.img[[:space:]]/s/^/#/' /etc/fstab 2>/dev/null || true
rm -f /swap.img

if [ ! -d /root/.oh-my-zsh ]; then
    RUNZSH=no CHSH=no KEEP_ZSHRC=yes sh -c "$(curl -fsSL https://raw.githubusercontent.com/ohmyzsh/ohmyzsh/master/tools/install.sh)" || log "WARN: oh-my-zsh install failed"
fi

if [ ! -x /usr/local/go/go1.25.1/bin/go ]; then
    mkdir -p /usr/local/go
    cd /usr/local/go
    rm -rf go go1.25.1 go1.25.1.linux-amd64.tar.gz
    curl -L -O https://go.dev/dl/go1.25.1.linux-amd64.tar.gz
    tar zxf go1.25.1.linux-amd64.tar.gz
    mv go go1.25.1
    rm -f go1.25.1.linux-amd64.tar.gz
fi
export PATH="/usr/local/go/go1.25.1/bin:/root/go/bin:$PATH"
if command -v go >/dev/null 2>&1; then
    go install github.com/google/pprof@latest || log "WARN: go install pprof failed"
fi

if [ ! -x /root/.cargo/bin/rustup ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y || log "WARN: rustup install failed"
fi

if [ ! -s /root/.nvm/nvm.sh ]; then
    curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.3/install.sh | bash || log "WARN: nvm install failed"
fi
if [ -s /root/.nvm/nvm.sh ]; then
    . /root/.nvm/nvm.sh
    nvm install 24 || log "WARN: nvm install 24 failed"
    nvm use 24 || true
fi

if [ ! -x /usr/local/bin/buildifier ]; then
    curl -L -o /usr/local/bin/buildifier https://github.com/bazelbuild/buildtools/releases/download/v7.1.2/buildifier-linux-amd64
    chmod +x /usr/local/bin/buildifier
fi

if [ ! -d /usr/local/java/jdk/jdk-21.0.3 ]; then
    mkdir -p /usr/local/java/jdk
    cd /usr/local/java/jdk
    rm -f jdk-21.0.3_linux-x64_bin.tar.gz
    curl -L -O https://download.oracle.com/java/21/archive/jdk-21.0.3_linux-x64_bin.tar.gz
    tar zxf jdk-21.0.3_linux-x64_bin.tar.gz
    rm -f jdk-21.0.3_linux-x64_bin.tar.gz
fi

if [ -f /root/src/share/ubuntu/conf/.vimrc ]; then
    cp /root/src/share/ubuntu/conf/.vimrc /root/.vimrc
fi
if [ ! -d /root/.vim/bundle/Vundle.vim ]; then
    git clone https://github.com/VundleVim/Vundle.vim.git /root/.vim/bundle/Vundle.vim || true
fi
if command -v vim >/dev/null 2>&1; then
    vim +PluginInstall +qall </dev/null || log "WARN: vim PluginInstall failed"
fi
if [ ! -d /root/.vim/bundle/vim-airline-fonts ]; then
    git clone https://github.com/powerline/fonts /root/.vim/bundle/vim-airline-fonts || true
fi
if [ -x /root/.vim/bundle/vim-airline-fonts/install.sh ]; then
    (cd /root/.vim/bundle/vim-airline-fonts && ./install.sh) || true
fi
if [ -d /root/.vim/bundle/YouCompleteMe ]; then
    (cd /root/.vim/bundle/YouCompleteMe && git submodule update --init --recursive && ./install.py --all) || log "WARN: YouCompleteMe install failed"
fi

grep -q '^\*[[:space:]]\+soft[[:space:]]\+core[[:space:]]\+unlimited' /etc/security/limits.conf || cat >> /etc/security/limits.conf <<'EOF_LIMITS'
*       soft    core    unlimited
*       hard    core    unlimited
EOF_LIMITS
grep -q '^kernel.core_pattern=/var/crash/core.%e.%p.%t' /etc/sysctl.conf || echo 'kernel.core_pattern=/var/crash/core.%e.%p.%t' >> /etc/sysctl.conf
mkdir -p /var/crash /etc/systemd
sysctl -p /etc/sysctl.conf || true
cat > /etc/systemd/coredump.conf <<'EOF_COREDUMP'
[Coredump]
Storage=external
ProcessSizeMax=unlimited
ExternalSizeMax=unlimited
EOF_COREDUMP

mkdir -p /root/src/software
if [ ! -d /root/src/software/pgvector ]; then
    git clone https://github.com/pgvector/pgvector.git /root/src/software/pgvector || true
fi
if [ -d /root/src/software/pgvector ]; then
    (cd /root/src/software/pgvector && make && make install) || log "WARN: pgvector build/install failed"
fi

setquota -u root 20000000 20000000 0 0 /dev/mmcblk0p2 2>/dev/null || true
for u in tiger git; do
    id "$u" >/dev/null 2>&1 && setquota -u "$u" 1000000 1000000 0 0 / 2>/dev/null || true
done

mkdir -p /root/src/share/ubuntu
if [ ! -f /root/src/share/ubuntu/backup_pg.sh ]; then
    cat > /root/src/share/ubuntu/backup_pg.sh <<'EOF_BACKUP_PG'
#!/bin/bash
set -u
BACKUP_DIR="/zfs/backup/pg_backup"
DATE=$(date +%Y%m%d_%H%M%S)
BACKUP_FILE="$BACKUP_DIR/pg_full_backup_$DATE.sql"
LOG_FILE="/var/log/pg_backup.log"

log_message() {
    echo "$(date '+%Y-%m-%d %H:%M:%S'): $1" | tee -a "$LOG_FILE"
}

mkdir -p "$BACKUP_DIR"
chown -R postgres:postgres "$BACKUP_DIR" 2>/dev/null || true
log_message "Starting PostgreSQL backup..."

if su - postgres -c "/usr/bin/pg_dumpall > '$BACKUP_FILE'"; then
    if [[ -s "$BACKUP_FILE" ]]; then
        log_message "Backup successful: $BACKUP_FILE ($(du -h "$BACKUP_FILE" | cut -f1))"
        find "$BACKUP_DIR" -name "pg_full_backup_*.sql" -mtime +7 -delete
        log_message "Cleaned up old backups (kept last 7 days)"
        exit 0
    fi
    log_message "ERROR: Backup file is empty or was not created"
    rm -f "$BACKUP_FILE"
    exit 1
fi
log_message "ERROR: pg_dumpall command failed"
rm -f "$BACKUP_FILE"
exit 1
EOF_BACKUP_PG
fi
if [ ! -f /root/src/share/ubuntu/backup_mysql.sh ]; then
    cat > /root/src/share/ubuntu/backup_mysql.sh <<'EOF_BACKUP_MYSQL'
#!/bin/bash
set -u
BACKUP_DIR="/zfs/backup/mysql_backup"
DATE=$(date +%Y%m%d_%H%M%S)
BACKUP_FILE="$BACKUP_DIR/mysql_full_backup_$DATE.sql"
LOG_FILE="/var/log/mysql_backup.log"

log_message() {
    echo "$(date '+%Y-%m-%d %H:%M:%S'): $1" | tee -a "$LOG_FILE"
}

mkdir -p "$BACKUP_DIR"
log_message "Starting MySQL backup..."

if /usr/bin/mysqldump -uroot --default-character-set=utf8mb4 -q --lock-all-tables --flush-logs -E -R --triggers --all-databases > "$BACKUP_FILE"; then
    if [[ -s "$BACKUP_FILE" ]]; then
        log_message "Backup successful: $BACKUP_FILE ($(du -h "$BACKUP_FILE" | cut -f1))"
        find "$BACKUP_DIR" -name "mysql_full_backup_*.sql" -mtime +7 -delete
        log_message "Cleaned up old backups (kept last 7 days)"
        exit 0
    fi
    log_message "ERROR: Backup file is empty or was not created"
    rm -f "$BACKUP_FILE"
    exit 1
fi
log_message "ERROR: mysqldump command failed"
rm -f "$BACKUP_FILE"
exit 1
EOF_BACKUP_MYSQL
fi
chmod +x /root/src/share/ubuntu/backup_pg.sh /root/src/share/ubuntu/backup_mysql.sh

wake_zfs() {
    if [ ! -d /zfs ]; then
        log "skip /zfs wake: /zfs not found"
        return 1
    fi
    log "wake /zfs before database restore"
    ls -ald /zfs >/dev/null 2>&1 || true
    find /zfs/backup -maxdepth 2 -type d -print -quit >/dev/null 2>&1 || true
    return 0
}

zfs_pool_for_mount() {
    src="$(findmnt -T /zfs -no SOURCE 2>/dev/null | head -1 || true)"
    if [ -n "$src" ] && printf '%s' "$src" | grep -qv '^/dev/'; then
        printf '%s\n' "$src" | cut -d/ -f1
        return 0
    fi
    src="$(df -P /zfs 2>/dev/null | awk 'NR==2 {print $1}' || true)"
    if [ -n "$src" ] && printf '%s' "$src" | grep -qv '^/dev/'; then
        printf '%s\n' "$src" | cut -d/ -f1
    fi
}

normalize_block_device() {
    dev="$1"
    [ -b "$dev" ] || return 0
    parent="$(lsblk -no PKNAME "$dev" 2>/dev/null | head -1 || true)"
    if [ -n "$parent" ]; then
        printf '/dev/%s\n' "$parent"
        return 0
    fi
    readlink -f "$dev" 2>/dev/null || printf '%s\n' "$dev"
}

zfs_block_devices() {
    pool="$(zfs_pool_for_mount || true)"
    if [ -n "$pool" ] && command -v zpool >/dev/null 2>&1; then
        zpool status -P "$pool" 2>/dev/null | awk '/\/dev\// {print $1}' | while IFS= read -r dev; do normalize_block_device "$dev"; done | sort -u
        return 0
    fi
    findmnt -T /zfs -no SOURCE 2>/dev/null | awk '/^\/dev\// {print $1}' | while IFS= read -r dev; do normalize_block_device "$dev"; done | sort -u
}

standby_zfs() {
    if [ ! -d /zfs ]; then
        return 0
    fi
    if ! command -v hdparm >/dev/null 2>&1; then
        log "skip /zfs standby: hdparm not installed"
        return 0
    fi
    devices="$(zfs_block_devices | sort -u || true)"
    if [ -z "$devices" ]; then
        log "skip /zfs standby: no block devices found"
        return 0
    fi
    log "set /zfs devices to standby"
    printf '%s\n' "$devices" | while IFS= read -r dev; do
        [ -n "$dev" ] || continue
        if [ -b "$dev" ]; then
            hdparm -y "$dev" >/dev/null 2>&1 && log "standby $dev" || log "WARN: standby $dev failed"
        fi
    done
}

newest_dump() {
    kind="$1"
    shift
    [ "$#" -gt 0 ] || return 0
    case "$kind" in
        mysql)
            find "$@" -type f \( \
                -name 'mysql_full_backup_*.sql' -o \
                -name 'mysql_full_backup_*.sql.gz' -o \
                -name 'mysql-all.sql' -o \
                -name 'mysql-all.sql.gz' -o \
                -name '*mysql*.sql' -o \
                -name '*mysql*.sql.gz' \
            \) -print0 2>/dev/null | score_dumps | sort -nr | head -1 | cut -f2-
            ;;
        postgres)
            find "$@" -type f \( \
                -name 'pg_full_backup_*.sql' -o \
                -name 'pg_full_backup_*.sql.gz' -o \
                -name 'postgres-all.sql' -o \
                -name 'postgres-all.sql.gz' -o \
                -name '*postgres*.sql' -o \
                -name '*postgres*.sql.gz' -o \
                -name '*pg*.sql' -o \
                -name '*pg*.sql.gz' \
            \) -print0 2>/dev/null | score_dumps | sort -nr | head -1 | cut -f2-
            ;;
        *)
            return 1
            ;;
    esac
}

score_dumps() {
    while IFS= read -r -d '' path; do
        base="$(basename "$path")"
        stamp="$(printf '%s\n' "$base" | sed -n 's/.*_\([0-9]\{8\}_[0-9]\{6\}\)\.sql\(\.gz\)\?$/\1/p')"
        score=""
        if [ -n "$stamp" ]; then
            score="$(date -d "${stamp:0:4}-${stamp:4:2}-${stamp:6:2} ${stamp:9:2}:${stamp:11:2}:${stamp:13:2}" +%s 2>/dev/null || true)"
        fi
        [ -n "$score" ] || score="$(stat -c %Y "$path" 2>/dev/null || echo 0)"
        printf '%s\t%s\n' "$score" "$path"
    done
}

dump_search_roots() {
    for dir in \
        /zfs/backup \
        /root/auto_sync_db_dumps \
        /tmp/auto_sync_db_dumps \
        /root/src/share/ubuntu \
        /root/src/share/nas \
        /root/src/share
    do
        [ -d "$dir" ] && printf '%s\n' "$dir"
    done
}

restore_once() {
    kind="$1"
    latest="$2"
    marker="/var/lib/auto_sync/${kind}_restore_marker"
    [ -n "$latest" ] || { log "skip $kind restore: no dump found"; return 0; }
    mkdir -p /var/lib/auto_sync
    if [ -f "$marker" ] && [ "$(cat "$marker" 2>/dev/null)" = "$latest" ]; then
        log "skip $kind restore: already restored $latest"
        return 0
    fi
    log "restore $kind from $latest"
    case "$kind" in
        mysql)
            systemctl restart mysql 2>/dev/null || systemctl restart mariadb 2>/dev/null || true
            if printf '%s' "$latest" | grep -q '\.gz$'; then zcat "$latest" | mysql; else mysql < "$latest"; fi
            ;;
        postgres)
            systemctl restart postgresql 2>/dev/null || true
            if printf '%s' "$latest" | grep -q '\.gz$'; then zcat "$latest" | sudo -u postgres psql -v ON_ERROR_STOP=1; else sudo -u postgres psql -v ON_ERROR_STOP=1 -f "$latest"; fi
            ;;
    esac && printf '%s' "$latest" > "$marker" || log "WARN: $kind restore failed"
}
zfs_woken=0
wake_zfs && zfs_woken=1 || true
dump_roots="$(dump_search_roots)"
mysql_dump="$(newest_dump mysql $dump_roots 2>/dev/null || true)"
pg_dump="$(newest_dump postgres $dump_roots 2>/dev/null || true)"
[ -n "$mysql_dump" ] && log "selected mysql dump: $mysql_dump"
[ -n "$pg_dump" ] && log "selected postgres dump: $pg_dump"
restore_once mysql "$mysql_dump"
restore_once postgres "$pg_dump"
rm -rf /root/auto_sync_db_dumps /tmp/auto_sync_db_dumps 2>/dev/null || true
if [ "$zfs_woken" = "1" ]; then
    standby_zfs
fi

systemctl daemon-reload
for s in auto_sync auto_sync_web halo2 tbox_client tbox-logrotate.timer nginx cron mysql postgresql redis-server privoxy; do
    restart_if_exists "$s"
done
for s in waiwei waiwei-web waiwei-puller xray; do
    disable_if_exists "$s"
done

(crontab -l 2>/dev/null | grep -v '/root/src/share/ubuntu/backup_pg.sh'; echo '0 10 * * 0 /bin/bash /root/src/share/ubuntu/backup_pg.sh > /dev/null 2>&1') | crontab -
(crontab -l 2>/dev/null | grep -v '/root/src/share/ubuntu/backup_mysql.sh'; echo '5 10 * * 0 /bin/bash /root/src/share/ubuntu/backup_mysql.sh > /dev/null 2>&1') | crontab -

echo '--- final states ---'
for s in auto_sync auto_sync_web halo2 tbox_client tbox-logrotate.timer nginx cron mysql postgresql redis-server privoxy waiwei xray; do
    printf '  %s: ' "$s"; systemctl is-enabled "$s" 2>/dev/null | tr -d '\n'; printf ' / '; systemctl is-active "$s" 2>/dev/null | tr -d '\n'; echo
done
'@
Invoke-Remote $remote

if ($errCount -gt 0) { Write-Host "deploy completed with $errCount error(s)"; exit 1 }
Write-Host 'deploy completed cleanly'
