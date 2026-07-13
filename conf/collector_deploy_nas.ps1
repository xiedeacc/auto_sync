$ErrorActionPreference = 'Stop'
# Runs on Windows. Pushes the collected NAS tree back to the Ubuntu host, then
# applies packages, services, runtimes, quotas, database restore, and cron.
$ssh  = $env:AS_SSH
$scp  = $env:AS_SCP
$dest = $env:AS_DEST
$root = $env:AS_ROOT

$opts    = @('-o','BatchMode=yes','-o','StrictHostKeyChecking=accept-new','-o','LogLevel=ERROR','-o','ConnectTimeout=20','-o','ServerAliveInterval=30','-o','ServerAliveCountMax=20')
$sshArgs = @() + $opts
$scpArgs = @('-r','-p') + $opts
if (-not [string]::IsNullOrEmpty($env:AS_PORT)) { $sshArgs += @('-p', $env:AS_PORT); $scpArgs += @('-P', $env:AS_PORT) }
if (-not [string]::IsNullOrEmpty($env:AS_KEY))  { $sshArgs += @('-i', $env:AS_KEY);  $scpArgs += @('-i', $env:AS_KEY) }

$errCount = 0
$collectPaths = @($env:AS_COLLECT_PATHS -split "`n" | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
$platformDefaultCollectPaths = @('/opt/www/gitlab_cleaner', '/opt/www/unlock-music')
foreach ($defaultPath in $platformDefaultCollectPaths) {
    if ($collectPaths -notcontains $defaultPath) { $collectPaths += $defaultPath }
}
$excludePaths = @($env:AS_EXCLUDE_PATHS -split "`n" | ForEach-Object { $_.Trim().TrimEnd([char[]]"/") } | Where-Object { $_ -ne '' })

function New-FinalSshdConfig([string]$Port) {
    if ([string]::IsNullOrWhiteSpace($Port)) { $Port = '10022' }
    return @"
Include /etc/ssh/sshd_config.d/*.conf
Port $Port
ClientAliveInterval 360
ClientAliveCountMax 0
AllowUsers root tiger git

#AuthenticationMethods publickey password
AuthenticationMethods publickey
PubkeyAuthentication yes
PasswordAuthentication no
PermitEmptyPasswords no
PermitRootLogin yes
KbdInteractiveAuthentication no
StrictModes yes
MaxAuthTries 3

HostbasedAuthentication no
UsePAM no
X11Forwarding no
PrintMotd no
IgnoreRhosts yes
UseDNS no

AcceptEnv LANG LC_*
Subsystem sftp  /usr/lib/openssh/sftp-server
"@
}

function Copy-AwsSslIntoCollectedRoot {
    if ([string]::IsNullOrWhiteSpace($root)) { return }
    $shareRoot = Split-Path -Parent $root
    if ([string]::IsNullOrWhiteSpace($shareRoot)) { return }
    $awsSsl = Join-Path $shareRoot 'aws\etc\nginx\ssl'
    if (-not (Test-Path -LiteralPath $awsSsl)) {
        Write-Host "! missing AWS SSL source $awsSsl"
        $script:errCount++
        return
    }
    $target = Join-Path $root 'etc\nginx\ssl'
    New-Item -ItemType Directory -Force -Path $target | Out-Null
    Get-ChildItem -LiteralPath $awsSsl -File -Force | Copy-Item -Destination $target -Force
    Write-Host "stage AWS SSL certificates from $awsSsl"
}

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

function Resolve-ReparseTarget([IO.FileSystemInfo]$Item) {
    if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) { return $Item.FullName }
    $target = @($Item.Target | Select-Object -First 1)
    if ([string]::IsNullOrWhiteSpace($target)) { return $Item.FullName }
    if (-not [IO.Path]::IsPathRooted($target)) {
        $target = Join-Path (Split-Path -Parent $Item.FullName) $target
    }
    return [IO.Path]::GetFullPath($target)
}

function Copy-ResolvedTree([string]$Source, [string]$Target, [string]$RemotePath, [hashtable]$SeenDirs) {
    if (Test-RemoteExcluded $RemotePath) {
        Write-Host "skip excluded $RemotePath"
        return
    }
    $item = Get-Item -LiteralPath $Source -Force
    $resolved = Resolve-ReparseTarget $item
    if ($resolved -ne $item.FullName) {
        if (-not (Test-Path -LiteralPath $resolved)) {
            Write-Host "! broken symlink $Source -> $resolved"
            return
        }
        $item = Get-Item -LiteralPath $resolved -Force
    }
    if (-not $item.PSIsContainer) {
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Target) | Out-Null
        Copy-Item -LiteralPath $item.FullName -Destination $Target -Force
        return
    }

    $resolvedDir = [IO.Path]::GetFullPath($item.FullName).TrimEnd([char[]]"\/")
    if ($SeenDirs.ContainsKey($resolvedDir)) {
        Write-Host "skip symlink cycle $RemotePath -> $resolvedDir"
        return
    }
    $SeenDirs[$resolvedDir] = $true
    New-Item -ItemType Directory -Force -Path $Target | Out-Null
    Get-ChildItem -LiteralPath $item.FullName -Force | ForEach-Object {
        $remoteChild = Join-RemotePath $RemotePath $_.Name
        $childTarget = Join-Path $Target $_.Name
        Copy-ResolvedTree $_.FullName $childTarget $remoteChild $SeenDirs
    }
    $SeenDirs.Remove($resolvedDir)
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
    Copy-ResolvedTree $local $target $remote @{}
    Write-Host "stage $remote"
}

function Invoke-Remote([string]$Script) {
    $Script = $Script -replace "`r`n", "`n"
    $localTmp = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_remote_" + [guid]::NewGuid().ToString('N') + ".sh")
    $remoteTmp = "/tmp/" + [IO.Path]::GetFileName($localTmp)
    try {
        [IO.File]::WriteAllText($localTmp, $Script, [Text.UTF8Encoding]::new($false))
        & $scp @scpArgs $localTmp "$($dest):$remoteTmp"
        if ($LASTEXITCODE -ne 0) {
            Write-Host "! remote script upload failed"
            $script:errCount++
            return
        }
        & $ssh @sshArgs $dest "bash '$remoteTmp' </dev/null; rc=`$?; rm -f '$remoteTmp'; exit `$rc"
        if ($LASTEXITCODE -ne 0) {
            $exitCode = $LASTEXITCODE
            Write-Host "! remote step exit $exitCode"
            $script:errCount++
            throw "remote step failed with exit code $exitCode"
        }
    } finally {
        Remove-Item -LiteralPath $localTmp -Force -ErrorAction SilentlyContinue
    }
}

function Transfer-CollectedPathsToStage([string[]]$RequiredPaths, [string[]]$OptionalPaths, [string]$RemoteStage) {
    $tarPaths = New-Object System.Collections.Generic.List[string]
    foreach ($p in @($RequiredPaths + $OptionalPaths)) {
        $remote = Normalize-RemotePath $p
        if (Test-RemoteExcluded $remote) {
            Write-Host "skip excluded $remote"
            continue
        }

        $local = Get-LocalCollectedPath $remote
        if (-not (Test-Path -LiteralPath $local)) {
            if ($RequiredPaths -contains $p) {
                Write-Host "! missing local $local"
                $script:errCount++
            }
            continue
        }

        [void]$tarPaths.Add($remote.TrimStart([char[]]"/"))
        Write-Host "stage $remote"
    }

    if ($tarPaths.Count -eq 0) { return }

    $excludeArgs = @()
    foreach ($exclude in $excludePaths) {
        $rel = (Normalize-RemotePath $exclude).TrimStart([char[]]"/")
        if ($rel -ne '') { $excludeArgs += "--exclude=$rel" }
    }

    $localTar = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_stage_" + [guid]::NewGuid().ToString('N') + ".tar")
    $remoteTar = "/tmp/" + [IO.Path]::GetFileName($localTar)
    try {
        $tarArgs = @('-C', $root, '-cf', $localTar) + $excludeArgs + @($tarPaths)
        & tar @tarArgs
        if ($LASTEXITCODE -ne 0) {
            Write-Host "! local tar stage creation failed"
            $script:errCount++
            return
        }
        & $scp @scpArgs $localTar "$($dest):$remoteTar"
        if ($LASTEXITCODE -ne 0) {
            Write-Host "! tar stage upload failed"
            $script:errCount++
            return
        }
        Invoke-Remote "mkdir -p $RemoteStage; tar -C $RemoteStage -xf '$remoteTar'; rc=`$?; rm -f '$remoteTar'; exit `$rc"
    } finally {
        Remove-Item -LiteralPath $localTar -Force -ErrorAction SilentlyContinue
    }
}

function Quote-ShellArg([string]$Value) {
    return "'" + $Value.Replace("'", "'""'""'") + "'"
}

function Prepare-StagedSymlinks([string]$PermsFile) {
    if ([string]::IsNullOrWhiteSpace($PermsFile) -or -not (Test-Path -LiteralPath $PermsFile)) { return }
    $commands = New-Object System.Collections.Generic.List[string]
    $commands.Add('stage="$HOME/.auto_sync_stage"')
    foreach ($line in [IO.File]::ReadAllLines($PermsFile)) {
        $t = $line.Trim()
        if (-not $t.StartsWith('symlink ')) { continue }
        $rest = $t.Substring(8)
        $sp = $rest.IndexOf(' ')
        if ($sp -lt 1) { continue }
        try { $target = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($rest.Substring(0, $sp))) } catch { continue }
        $path = $rest.Substring($sp + 1)
        if (-not $path.StartsWith('/')) { continue }
        $qPath = Quote-ShellArg $path
        $qTarget = Quote-ShellArg $target
        $commands.Add(@"
path=$qPath
target=$qTarget
case "`$target" in /*) resolved="`$target" ;; *) resolved="`$(dirname -- "`$path")/`$target" ;; esac
resolved="`$(realpath -m -- "`$resolved")"
src="`$stage`$path"
dst="`$stage`$resolved"
if [ -e "`$src" ] || [ -L "`$src" ]; then
    mkdir -p -- "`$(dirname -- "`$dst")" "`$(dirname -- "`$src")"
    if [ -d "`$src" ] && [ ! -L "`$src" ]; then
        mkdir -p -- "`$dst"
        rsync -a "`$src/" "`$dst/"
        rm -rf -- "`$src"
    else
        rm -rf -- "`$dst"
        mv -- "`$src" "`$dst"
    fi
    ln -s -- "`$target" "`$src"
fi
"@)
    }
    if ($commands.Count -gt 1) {
        Write-Host "preparing $($commands.Count - 1) staged symlink(s)"
        Invoke-Remote ($commands -join "`n")
    }
}

Copy-AwsSslIntoCollectedRoot
Invoke-Remote 'rm -rf ~/.auto_sync_stage; mkdir -p ~/.auto_sync_stage'
$generatedOptionalPaths = @('/opt/immich/conf', '/root/auto_sync_db_dumps', '/tmp/auto_sync_db_dumps')
$requiredCollectPaths = @($collectPaths | Where-Object { (Normalize-RemotePath $_) -ne '/opt/immich/conf' })
Transfer-CollectedPathsToStage $requiredCollectPaths $generatedOptionalPaths '~/.auto_sync_stage'
Prepare-StagedSymlinks $env:AS_PERMS_FILE

$sshPort = $env:AS_PORT
if ([string]::IsNullOrWhiteSpace($sshPort)) { $sshPort = '10022' }
$finalSshd = Join-Path ([IO.Path]::GetTempPath()) ("auto_sync_sshd_" + [guid]::NewGuid().ToString('N'))
try {
    [IO.File]::WriteAllText($finalSshd, (New-FinalSshdConfig $sshPort), [Text.ASCIIEncoding]::new())
    & $scp @scpArgs $finalSshd "$($dest):/root/auto_sync_sshd_config"
    if ($LASTEXITCODE -ne 0) { Write-Host "! sshd_config upload failed"; $errCount++ }
} finally {
    Remove-Item -LiteralPath $finalSshd -Force -ErrorAction SilentlyContinue
}
Invoke-Remote @"
set -e
mkdir -p /etc/ssh/sshd_config.d /root/.ssh
cp /root/auto_sync_sshd_config /etc/ssh/sshd_config
rm -f /root/auto_sync_sshd_config
rm -f /etc/ssh/sshd_config.d/50-cloud-init.conf /etc/ssh/sshd_config.d/98-auto-sync-gitlab.conf /etc/ssh/sshd_config.d/99-auto-sync-test.conf
if [ -f /tmp/auto_sync_root_key.pub ]; then
    cat /tmp/auto_sync_root_key.pub > /root/.ssh/authorized_keys
fi
chmod 700 /root /root/.ssh
chown -R root:root /root/.ssh 2>/dev/null || true
find /root/.ssh -type f -name "id_*" ! -name "*.pub" -exec chmod 600 {} \; 2>/dev/null || true
find /root/.ssh -type f -name "*.pub" -exec chmod 644 {} \; 2>/dev/null || true
chmod 600 /root/.ssh/authorized_keys 2>/dev/null || true
sshd -t
systemctl disable --now ssh.socket 2>/dev/null || true
systemctl restart ssh.service 2>/dev/null || systemctl restart sshd.service
"@

if (-not [string]::IsNullOrWhiteSpace($env:AS_PERMS_FILE) -and (Test-Path -LiteralPath $env:AS_PERMS_FILE)) {
    $links = New-Object System.Collections.Generic.List[string]
    $chmods = New-Object System.Collections.Generic.List[string]
    foreach ($line in [IO.File]::ReadAllLines($env:AS_PERMS_FILE)) {
        $t = $line.Trim(); if ($t -eq '') { continue }
        if ($t.StartsWith('symlink ')) {
            $rest = $t.Substring(8)
            $sp = $rest.IndexOf(' '); if ($sp -lt 1) { continue }
            $targetB64 = $rest.Substring(0, $sp); $path = $rest.Substring($sp + 1)
            if (-not ($path.StartsWith('/etc/') -or $path.StartsWith('/usr/local/') -or $path.StartsWith('/root/') -or $path.StartsWith('/opt/') -or $path.StartsWith('/home/'))) { continue }
            try { $target = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($targetB64)) } catch { continue }
            $qPath = Quote-ShellArg $path
            $qTarget = Quote-ShellArg $target
            $links.Add("mkdir -p -- `$(dirname -- $qPath) 2>/dev/null || true; rm -rf -- $qPath 2>/dev/null || true; ln -s -- $qTarget $qPath 2>/dev/null || true")
            continue
        }
        $sp = $t.IndexOf(' '); if ($sp -lt 1) { continue }
        $mode = $t.Substring(0, $sp); $path = $t.Substring($sp + 1)
        if ($path.StartsWith('/etc/') -or $path.StartsWith('/usr/local/') -or $path.StartsWith('/root/') -or $path.StartsWith('/opt/') -or $path.StartsWith('/home/')) {
            $chmods.Add("chmod $mode $(Quote-ShellArg $path) 2>/dev/null || true")
        }
    }
    if ($links.Count -gt 0) { Write-Host "restoring $($links.Count) recorded symlink(s)"; Invoke-Remote ($links -join "`n") }
    if ($chmods.Count -gt 0) { Write-Host "restoring $($chmods.Count) recorded permissions"; Invoke-Remote ($chmods -join "`n") }
}

$remote = @'
set -u
export DEBIAN_FRONTEND=noninteractive
GITLAB_CE_VERSION="17.11.7-ce.0"

log() { printf '[nas-deploy] %s\n' "$*"; }
try() { "$@" && return 0; log "WARN: command failed: $*"; return 0; }
unit_name() {
    unit="$1"
    if systemctl list-unit-files "$unit" --no-legend 2>/dev/null | grep -q . || [ -e "/etc/systemd/system/$unit" ] || [ -e "/lib/systemd/system/$unit" ] || [ -e "/usr/lib/systemd/system/$unit" ]; then
        printf '%s\n' "$unit"
        return 0
    fi
    case "$unit" in
        *.service|*.timer|*.socket) return 1 ;;
        *) unit="${unit}.service" ;;
    esac
    if systemctl list-unit-files "$unit" --no-legend 2>/dev/null | grep -q . || [ -e "/etc/systemd/system/$unit" ] || [ -e "/lib/systemd/system/$unit" ] || [ -e "/usr/lib/systemd/system/$unit" ]; then
        printf '%s\n' "$unit"
        return 0
    fi
    return 1
}
unit_exists() { unit_name "$1" >/dev/null; }
remove_bad_enable_links() {
    unit="$1"
    for wants_dir in /etc/systemd/system/*.target.wants; do
        [ -d "$wants_dir" ] || continue
        link="$wants_dir/$unit"
        if [ -e "$link" ] && [ ! -L "$link" ]; then
            rm -f "$link"
        fi
    done
}
enable_unit() {
    unit="$1"
    remove_bad_enable_links "$unit"
    if ! systemctl enable "$unit" >/dev/null 2>&1; then
        log "ERROR: enable $unit failed"
        systemctl enable "$unit"
        return 1
    fi
}
restart_if_exists() {
    unit="$(unit_name "$1" 2>/dev/null || true)"
    [ -n "$unit" ] || return 0
    if unit_exists "$unit"; then
        enable_unit "$unit"
        systemctl restart "$unit" && log "restarted $unit" || log "WARN: restart $unit failed"
    fi
}
disable_if_exists() {
    unit="$(unit_name "$1" 2>/dev/null || true)"
    [ -n "$unit" ] || return 0
    if unit_exists "$unit"; then
        systemctl disable "$unit" >/dev/null 2>&1 || true
        systemctl stop "$unit" >/dev/null 2>&1 || true
        systemctl reset-failed "$unit" >/dev/null 2>&1 || true
        log "disabled+stopped $unit"
    fi
}

install_staged_collected_paths() {
    stage="$HOME/.auto_sync_stage"
    [ -d "$stage" ] || return 0
    log "install collected paths after package installation"
    rsync -rltD --no-perms "$stage/" /
    rc=$?
    if [ -f /tmp/auto_sync_root_key.pub ]; then
        mkdir -p /root/.ssh
        touch /root/.ssh/authorized_keys
        grep -qxF -f /tmp/auto_sync_root_key.pub /root/.ssh/authorized_keys 2>/dev/null || cat /tmp/auto_sync_root_key.pub >> /root/.ssh/authorized_keys
        rm -f /tmp/auto_sync_root_key.pub
    fi
    chmod 755 / /etc /usr /usr/bin 2>/dev/null || true
    chown -R root:root /root/.ssh 2>/dev/null || true
    chmod 700 /root/.ssh 2>/dev/null || true
    find /root/.ssh -type f -name "id_*" ! -name "*.pub" -exec chmod 600 {} \; 2>/dev/null || true
    find /root/.ssh -type f -name "*.pub" -exec chmod 644 {} \; 2>/dev/null || true
    chmod 600 /root/.ssh/authorized_keys 2>/dev/null || true
    rm -rf "$stage"
    [ "$rc" -eq 0 ] && log "installed collected paths"
    return "$rc"
}

if [ -f /etc/apt/sources.list.d/gitlab_gitlab-ce.list ] && grep -q '/ubuntu/resolute\| resolute ' /etc/apt/sources.list.d/gitlab_gitlab-ce.list; then
    log "GitLab CE has no resolute repo yet; use noble package repo"
    sed -i 's#/ubuntu/resolute#/ubuntu/noble#g; s/ resolute / noble /g' /etc/apt/sources.list.d/gitlab_gitlab-ce.list
fi

if [ -f /etc/apt/sources.list.d/ubuntu.sources ]; then
    sed -i 's#http://archive.ubuntu.com#https://mirrors.cloud.tencent.com#g; s#http://security.ubuntu.com#https://mirrors.cloud.tencent.com#g' /etc/apt/sources.list.d/ubuntu.sources
fi
apt-get update
policy_created=0
if [ ! -e /usr/sbin/policy-rc.d ]; then
    printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d
    chmod 0755 /usr/sbin/policy-rc.d
    policy_created=1
fi
apt-get -o Dpkg::Options::=--force-confold purge -y vim || true
apt-get autoremove -y || true
curl_dev_version="$(apt-cache policy libcurl4-openssl-dev 2>/dev/null | awk '/Candidate:/ {print $2; exit}')"
if [ -n "$curl_dev_version" ] && [ "$curl_dev_version" != "(none)" ]; then
    apt-get install -y --allow-downgrades "curl=$curl_dev_version" "libcurl4t64=$curl_dev_version" "libcurl4-openssl-dev=$curl_dev_version" 2>/dev/null ||
        apt-get install -y --allow-downgrades "curl=$curl_dev_version" "libcurl4=$curl_dev_version" "libcurl4-openssl-dev=$curl_dev_version" 2>/dev/null ||
        log "WARN: libcurl dev version alignment failed"
fi
apt_result=0
apt-get -o Dpkg::Options::=--force-confold install -y \
    dialog zfsutils-linux openssh-server zsh net-tools curl aria2 iputils-ping iftop cron ca-certificates xfonts-utils dnsutils hdparm unzip \
    autoconf libtool libtool-bin cmake make gcc g++ texinfo bison flex gdb build-essential automake pkg-config help2man gettext \
    python3-pip ruby luajit netcat-openbsd gperf cscope exuberant-ctags vim-nox git git-lfs lcov graphviz \
    libgeoip-dev libxml2-dev libxslt1.1 libxslt1-dev libatomic-ops-dev libgd-dev libperl-dev libluajit-5.1-dev tcl-dev ruby-dev libncurses-dev \
    mysql-server postgresql postgresql-client libpq-dev postgresql-contrib sysstat nginx-full libnginx-mod-stream \
    ffmpeg libimage-exiftool-perl libgl1 libvips-dev libvips-tools openjdk-21-jdk-headless postgresql-server-dev-all redis quota || apt_result=1
DEBIAN_FRONTEND=noninteractive dpkg --force-confold --configure -a || apt_result=1
[ "$policy_created" -eq 0 ] || rm -f /usr/sbin/policy-rc.d
[ "$apt_result" -eq 0 ] || { log "ERROR: package installation/configuration failed"; exit 1; }
apt-get remove -y apport || true
apt-get autoremove -y || true
rm -f /etc/nginx/sites-enabled/default /etc/nginx/conf.d/default.conf 2>/dev/null || true
install_staged_collected_paths || { log "ERROR: collected path installation failed"; exit 1; }

id tiger >/dev/null 2>&1 || useradd -m -s /bin/bash tiger
ensure_software_src_layout() {
    new=/opt/src/software
    old=/opt/software/src
    mkdir -p /opt/src /opt/software
    if [ -e "$old" ] && [ ! -L "$old" ]; then
        if [ ! -e "$new" ]; then
            mv "$old" "$new"
        else
            rsync -aHAX "$old/" "$new/" || rsync -a "$old/" "$new/"
            mv "$old" "${old}.migrated-$(date +%Y%m%d%H%M%S)"
        fi
    fi
    mkdir -p "$new"
    if [ -L "$old" ] && [ "$(readlink "$old")" != "$new" ]; then
        rm -f "$old"
    fi
}
ensure_software_src_layout
rewrite_software_src_paths() {
    replacement="$1"
    for root in /etc/systemd/system /etc/profile.d /usr/local/bin /usr/local/sbin; do
        [ -e "$root" ] || continue
        find "$root" -type f -exec grep -Il '/opt/software/src' {} + 2>/dev/null |
            xargs -r sed -i "s#/opt/software/src#$replacement#g"
    done
    [ ! -f /etc/profile ] || sed -i "s#/opt/software/src#$replacement#g" /etc/profile
}
rewrite_software_src_paths /opt/src/software
ensure_opt_link() {
    path="$1"
    target="$2"
    kind="$3"
    owner="$4"
    if [ -L "$path" ] && [ "$(readlink "$path")" = "$target" ]; then
        if [ "$kind" = dir ]; then mkdir -p "$target"; else touch "$target"; fi
        return 0
    fi
    mkdir -p "$(dirname "$path")" "$(dirname "$target")"
    if [ "$kind" = dir ]; then
        mkdir -p "$target"
        if [ -d "$path" ] && [ ! -L "$path" ]; then
            rsync -a "$path/" "$target/"
        elif [ -L "$path" ] && [ -d "$path" ]; then
            rsync -aL "$path/" "$target/"
        fi
    else
        if [ -f "$path" ] || { [ -L "$path" ] && [ -f "$path" ]; }; then
            cp -aL "$path" "$target"
        elif [ ! -e "$target" ]; then
            touch "$target"
        fi
    fi
    rm -rf "$path"
    ln -s "$target" "$path"
    chown -h "$owner" "$path" 2>/dev/null || true
    case "$target" in /opt/user/*) chown -h "$owner" "$target" 2>/dev/null || true ;; esac
}
while IFS='|' read -r path target kind owner; do
    [ -n "$path" ] && ensure_opt_link "$path" "$target" "$kind" "$owner"
done <<'EOF_OPT_LINKS'
/root/.bashrc|/opt/user/root/.bashrc|file|root:root
/root/.cache|/opt/user/root/.cache|dir|root:root
/root/.cargo|/opt/user/root/.cargo|dir|root:root
/root/.codex|/opt/user/root/.codex|dir|root:root
/root/.config|/opt/user/root/.config|dir|root:root
/root/.cscope.vim|/opt/user/root/.cscope.vim|file|root:root
/root/.dotnet|/opt/user/root/.dotnet|dir|root:root
/root/.launchpadlib|/opt/user/root/.launchpadlib|dir|root:root
/root/.local|/opt/user/root/.local|dir|root:root
/root/.npm|/opt/user/root/.npm|dir|root:root
/root/.npmrc|/opt/user/root/.npmrc|file|root:root
/root/.nvm|/opt/src/software/tools/nvm|dir|root:root
/root/.oh-my-zsh|/opt/user/root/.oh-my-zsh|dir|root:root
/root/.profile|/opt/user/root/.profile|file|root:root
/root/.rustup|/opt/user/root/.rustup|dir|root:root
/root/.vim|/opt/user/root/.vim|dir|root:root
/root/.vimbackup|/opt/user/root/.vimbackup|dir|root:root
/root/.vimswap|/opt/user/root/.vimswap|dir|root:root
/root/.vimundo|/opt/user/root/.vimundo|dir|root:root
/root/.vimviews|/opt/user/root/.vimviews|dir|root:root
/root/.vscode-server|/opt/user/root/.vscode-server|dir|root:root
/root/.zprofile|/opt/user/root/.zprofile|file|root:root
/root/.zshenv|/opt/user/root/.zshenv|file|root:root
/root/.zshrc|/opt/user/root/.zshrc|file|root:root
/root/src|/opt/user/root/src|dir|root:root
/home/tiger/.bashrc|/opt/user/tiger/.bashrc|file|tiger:tiger
/home/tiger/.halo2|/opt/user/tiger/.halo2|dir|tiger:tiger
/home/tiger/.npm|/opt/user/tiger/.npm|dir|tiger:tiger
/home/tiger/.npmrc|/opt/user/tiger/.npmrc|file|tiger:tiger
/home/tiger/.nvm|/opt/src/software/tools/nvm|dir|tiger:tiger
/home/tiger/.oh-my-zsh|/opt/user/tiger/.oh-my-zsh|dir|tiger:tiger
/home/tiger/.profile|/opt/user/tiger/.profile|file|tiger:tiger
/home/tiger/.zprofile|/opt/user/tiger/.zprofile|file|tiger:tiger
/home/tiger/.zshenv|/opt/user/tiger/.zshenv|file|tiger:tiger
/home/tiger/.zshrc|/opt/user/tiger/.zshrc|file|tiger:tiger
EOF_OPT_LINKS

if [ -f /usr/share/nginx/modules-available/mod-stream.conf ]; then
    mkdir -p /etc/nginx/modules-enabled
    ln -sfn /usr/share/nginx/modules-available/mod-stream.conf /etc/nginx/modules-enabled/50-mod-stream.conf
fi
if [ ! -f /usr/lib/nginx/modules/ngx_stream_module.so ] || [ ! -e /etc/nginx/modules-enabled/50-mod-stream.conf ]; then
    log "ERROR: nginx stream module is not installed or enabled"
    exit 1
fi

fix_postgresql_config_permissions() {
    pg_fix_major="${1:-}"
    if [ -z "$pg_fix_major" ]; then
        pg_fix_major="$(ls -1 /usr/lib/postgresql 2>/dev/null | sort -V | tail -1)"
    fi
    [ -n "$pg_fix_major" ] || return 0
    [ -d "/etc/postgresql/$pg_fix_major" ] && chown -R postgres:postgres "/etc/postgresql/$pg_fix_major" 2>/dev/null || true
    [ -d "/var/lib/postgresql/$pg_fix_major" ] && chown -R postgres:postgres "/var/lib/postgresql/$pg_fix_major" 2>/dev/null || true
    find "/etc/postgresql/$pg_fix_major" -type d -exec chmod 755 {} + 2>/dev/null || true
    find "/etc/postgresql/$pg_fix_major" -type f \( -name 'pg_hba.conf' -o -name 'pg_ident.conf' \) -exec chmod 640 {} + 2>/dev/null || true
    find "/etc/postgresql/$pg_fix_major" -type f ! \( -name 'pg_hba.conf' -o -name 'pg_ident.conf' \) -exec chmod 644 {} + 2>/dev/null || true
}

ensure_postgresql_cluster() {
    pg_major="$(pg_config --version | sed -n 's/^PostgreSQL \([0-9][0-9]*\).*/\1/p')"
    [ -n "$pg_major" ] || { log "ERROR: cannot detect PostgreSQL major version"; return 1; }
    if [ -s "/var/lib/postgresql/$pg_major/main/PG_VERSION" ]; then
        pg_ctlcluster "$pg_major" main start 2>/dev/null || true
        pg_isready -q && return 0
    fi

    source_major="$(find /etc/postgresql -mindepth 1 -maxdepth 1 -type d -printf '%f\n' 2>/dev/null | grep -E '^[0-9]+$' | sort -V | tail -1)"
    source_copy="$(mktemp -d)"
    if [ -n "$source_major" ] && [ -d "/etc/postgresql/$source_major/main" ]; then
        cp -a "/etc/postgresql/$source_major/main/." "$source_copy/"
    fi
    for old_dir in /etc/postgresql/[0-9]*; do
        [ -d "$old_dir" ] || continue
        old_major="${old_dir##*/}"
        [ "$old_major" = "$pg_major" ] && continue
        mv "$old_dir" "/etc/postgresql/.auto_sync_saved_${old_major}_$(date +%Y%m%d%H%M%S)" 2>/dev/null || true
    done
    pg_dropcluster --stop "$pg_major" main 2>/dev/null || true
    rm -rf "/etc/postgresql/$pg_major/main" "/var/lib/postgresql/$pg_major/main"
    pg_createcluster --port 5432 "$pg_major" main --start
    if [ -f "$source_copy/postgresql.conf" ]; then
        for config in postgresql.conf pg_hba.conf pg_ident.conf pg_ctl.conf start.conf environment; do
            [ -f "$source_copy/$config" ] && cp -a "$source_copy/$config" "/etc/postgresql/$pg_major/main/$config"
        done
        if [ -n "$source_major" ] && [ "$source_major" != "$pg_major" ]; then
            sed -i "s#/postgresql/$source_major/#/postgresql/$pg_major/#g; s#postgresql-$source_major#postgresql-$pg_major#g" /etc/postgresql/$pg_major/main/*.conf 2>/dev/null || true
        fi
        sed -i -E \
            -e "s#^[[:space:]]*external_pid_file[[:space:]]*=.*#external_pid_file = '/var/run/postgresql/$pg_major-main.pid'#" \
            -e "s#^[[:space:]]*cluster_name[[:space:]]*=.*#cluster_name = '$pg_major/main'#" \
            "/etc/postgresql/$pg_major/main/postgresql.conf"
    fi
    rm -rf "$source_copy"
    fix_postgresql_config_permissions "$pg_major"
    systemctl restart "postgresql@$pg_major-main.service"
    pg_isready -q || { log "ERROR: PostgreSQL cluster $pg_major/main is not ready"; return 1; }
}
ensure_postgresql_cluster || exit 1

cp /usr/share/zoneinfo/Asia/Shanghai /etc/localtime

ensure_host_entry() {
    expected_ip="$1"
    host="$2"
    tmp="$(mktemp)"
    touch /etc/hosts
    awk -v expected_ip="$expected_ip" -v host="$host" '
        /^[[:space:]]*#/ || NF == 0 { print; next }
        {
            has_host = 0
            for (i = 2; i <= NF; i++) {
                if ($i == host) {
                    has_host = 1
                    break
                }
            }
            if (!has_host) {
                print
                next
            }
            if ($1 == expected_ip && found == 0) {
                print
                found = 1
                next
            }
            out = $1
            kept = 0
            for (i = 2; i <= NF; i++) {
                if ($i != host) {
                    out = out " " $i
                    kept = 1
                }
            }
            if (kept) {
                print out
            }
        }
        END {
            if (found == 0) {
                print expected_ip " " host
            }
        }
    ' /etc/hosts > "$tmp"
    cat "$tmp" > /etc/hosts
    rm -f "$tmp"
}
ensure_host_entry 127.0.0.1 code.xiedeacc.com
ensure_host_entry 127.0.0.1 unlock-music.xiedeacc.com
ensure_host_entry 127.0.0.1 immich.xiedeacc.com
ensure_host_entry 127.0.0.1 halo.xiedeacc.com
ensure_host_entry 127.0.0.1 blog.xiedeacc.com
ensure_host_entry 127.0.0.1 rblog.xiedeacc.com
ensure_host_entry 192.168.2.126 dev.xiedeacc.com
ensure_host_entry 192.168.2.126 coverage.xiedeacc.com

swapoff -a || true
sed -i '/^\/swap\.img[[:space:]]/s/^/#/' /etc/fstab 2>/dev/null || true
rm -f /swap.img

cat > /etc/profile.d/auto-sync-domestic-mirrors.sh <<'EOF_DOMESTIC_MIRRORS'
export GOPROXY=https://goproxy.cn,direct
export NVM_NODEJS_ORG_MIRROR=https://npmmirror.com/mirrors/node
export npm_config_registry=https://registry.npmmirror.com
export COREPACK_NPM_REGISTRY=https://registry.npmmirror.com
export PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple
export UV_DEFAULT_INDEX=https://pypi.tuna.tsinghua.edu.cn/simple
export UV_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
export JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64
export PATH="$JAVA_HOME/bin:$PATH"
EOF_DOMESTIC_MIRRORS
. /etc/profile.d/auto-sync-domestic-mirrors.sh
cat > /etc/pip.conf <<'EOF_PIP_MIRROR'
[global]
index-url = https://pypi.tuna.tsinghua.edu.cn/simple
timeout = 120
EOF_PIP_MIRROR
mkdir -p /root/.cargo
cat > /root/.cargo/config.toml <<'EOF_CARGO_MIRROR'
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
git-fetch-with-cli = true
EOF_CARGO_MIRROR

if [ ! -d /root/.oh-my-zsh ]; then
    RUNZSH=no CHSH=no KEEP_ZSHRC=yes sh -c "$(curl -fsSL https://raw.githubusercontent.com/ohmyzsh/ohmyzsh/master/tools/install.sh)" || log "WARN: oh-my-zsh install failed"
fi

if [ ! -x /usr/local/go/go1.25.1/bin/go ]; then
    mkdir -p /usr/local/go
    cd /usr/local/go
    rm -rf go go1.25.1 go1.25.1.linux-amd64.tar.gz
    if curl --retry 5 --retry-all-errors -L -O https://mirrors.aliyun.com/golang/go1.25.1.linux-amd64.tar.gz || curl --retry 5 --retry-all-errors -L -O https://go.dev/dl/go1.25.1.linux-amd64.tar.gz; then
        tar zxf go1.25.1.linux-amd64.tar.gz
        mv go go1.25.1
        rm -f go1.25.1.linux-amd64.tar.gz
    else
        log "WARN: go download failed"
    fi
fi
export PATH="/usr/local/go/go1.25.1/bin:/root/go/bin:$PATH"
if command -v go >/dev/null 2>&1; then
    go install github.com/google/pprof@latest || log "WARN: go install pprof failed"
fi

if [ ! -x /root/.cargo/bin/rustup ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://rsproxy.cn/rustup-init.sh | timeout --kill-after=10s 600s sh -s -- -y || log "WARN: rustup install failed"
fi

mkdir -p /opt/src/software/tools
if [ -L /opt/src/software/tools/nvm ]; then
    rm -f /opt/src/software/tools/nvm
fi
export NVM_DIR=/opt/src/software/tools/nvm
mkdir -p "$NVM_DIR"
if [ ! -s "$NVM_DIR/nvm.sh" ]; then
    curl -fsSL https://gitee.com/mirrors/nvm/raw/v0.40.3/install.sh | NVM_SOURCE=https://gitee.com/mirrors/nvm.git bash || log "WARN: nvm install failed"
fi
if [ -s "$NVM_DIR/nvm.sh" ]; then
    . "$NVM_DIR/nvm.sh"
    nvm install 24.18.0 || nvm install 24 || log "WARN: nvm install 24 failed"
    nvm use 24.18.0 || nvm use 24 || true
    hash -r 2>/dev/null || true
    if ! command -v npm >/dev/null 2>&1; then
        log "npm missing after nvm use; reinstall Node v24.18.0"
        rm -rf "$NVM_DIR/versions/node/v24.18.0"
        nvm install 24.18.0 || nvm install 24 || log "WARN: nvm reinstall 24 failed"
        nvm use 24.18.0 || nvm use 24 || true
        hash -r 2>/dev/null || true
    fi
    node_bin_dir="$(dirname "$(command -v node 2>/dev/null || true)")"
    if [ -n "$node_bin_dir" ] && [ -x "$node_bin_dir/node" ]; then
        export PATH="$node_bin_dir:$PATH"
        npm config set registry "$npm_config_registry" || log "WARN: npm mirror setup failed"
        if command -v corepack >/dev/null 2>&1; then
            corepack enable || log "WARN: corepack enable failed"
            corepack prepare pnpm@latest --activate || log "WARN: corepack prepare pnpm failed"
        fi
        if ! command -v pnpm >/dev/null 2>&1; then
            npm install -g pnpm || log "WARN: npm install pnpm failed"
        fi
        command -v pnpm >/dev/null 2>&1 && pnpm config set registry "$npm_config_registry" || true
    fi
fi
if [ -d "$NVM_DIR" ]; then
    chmod -R a+rX "$NVM_DIR" || true
fi

if [ ! -x /usr/local/bin/buildifier ]; then
    curl -L -o /usr/local/bin/buildifier https://github.com/bazelbuild/buildtools/releases/download/v7.1.2/buildifier-linux-amd64
    chmod +x /usr/local/bin/buildifier
fi

[ -x "$JAVA_HOME/bin/java" ] || { log "ERROR: OpenJDK 21 is not installed at $JAVA_HOME"; exit 1; }
[ ! -L /usr/local/java/jdk/jdk-21.0.3 ] || rm -f /usr/local/java/jdk/jdk-21.0.3
[ ! -L /opt/src/software/tools/mise/installs/java/21.0.2 ] || rm -f /opt/src/software/tools/mise/installs/java/21.0.2
if find /etc/systemd/system -type f -exec grep -q '/usr/local/java/.*/bin/java' {} \; -print -quit | grep -q .; then
    find /etc/systemd/system -type f -exec grep -l '/usr/local/java/.*/bin/java' {} + |
        xargs -r sed -i -E "s#/usr/local/java/[^[:space:]]*/bin/java#$JAVA_HOME/bin/java#g"
    systemctl daemon-reload || true
fi

mkdir -p /usr/local/sbin
cat > /usr/local/sbin/auto_sync_install_vim_tools.sh <<'EOF_VIM_TOOLS'
#!/bin/bash
set -u
export JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64
log() { printf '[vim-tools] %s\n' "$*"; }
if [ -f /root/src/share/ubuntu/conf/.vimrc ]; then
    cp /root/src/share/ubuntu/conf/.vimrc /root/.vimrc
fi
mkdir -p /root/.vim/bundle
if [ ! -d /root/.vim/bundle/Vundle.vim ]; then
    git clone https://github.com/VundleVim/Vundle.vim.git /root/.vim/bundle/Vundle.vim || true
fi
if command -v vim >/dev/null 2>&1 && [ -f /root/.vimrc ]; then
    vimrc_hash="$(sha256sum /root/.vimrc | awk '{print $1}')"
    if [ "$(cat /root/.vim/.auto_sync_vundle_hash 2>/dev/null || true)" != "$vimrc_hash" ]; then
        vundle_rc="$(mktemp)"
        sed -n '1,2p' /root/.vimrc > "$vundle_rc"
        sed -n '/^filetype off$/,/^call vundle#end()/p' /root/.vimrc >> "$vundle_rc"
        vundle_exit=0
        timeout --kill-after=10s 600s vim -Nu "$vundle_rc" -n -es -i NONE '+set nomore' '+PluginInstall' '+qall' </dev/null || vundle_exit=$?
        declared_plugins="$(grep -Ec "^[[:space:]]*Plugin[[:space:]]+'" /root/.vimrc || true)"
        installed_plugins="$(find /root/.vim/bundle -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)"
        if [ "$installed_plugins" -ge "$declared_plugins" ] &&
           [ -d /root/.vim/bundle/Vundle.vim ] &&
           [ -d /root/.vim/bundle/YouCompleteMe ] &&
           [ -d /root/.vim/bundle/vim-glaive ]; then
            printf '%s\n' "$vimrc_hash" > /root/.vim/.auto_sync_vundle_hash
            [ "$vundle_exit" -eq 0 ] || log "PluginInstall returned $vundle_exit; all $declared_plugins declared plugins are present"
        else
            log "WARN: vim plugins incomplete (declared=$declared_plugins installed=$installed_plugins exit=$vundle_exit)"
        fi
        rm -f "$vundle_rc"
    fi
fi
if [ ! -d /root/.vim/bundle/vim-airline-fonts ]; then
    git clone https://github.com/powerline/fonts /root/.vim/bundle/vim-airline-fonts || true
fi
if [ -x /root/.vim/bundle/vim-airline-fonts/install.sh ]; then
    (cd /root/.vim/bundle/vim-airline-fonts && ./install.sh) || true
fi
if [ -d /root/.vim/bundle/YouCompleteMe ]; then
    ycm_commit="$(git -C /root/.vim/bundle/YouCompleteMe rev-parse HEAD 2>/dev/null || true)"
    if [ -n "$ycm_commit" ] && [ "$(cat /root/.vim/bundle/YouCompleteMe/.auto_sync_installed 2>/dev/null || true)" != "$ycm_commit" ]; then
        ycm_path="$JAVA_HOME/bin:/usr/local/go/go1.25.1/bin:/root/go/bin:/root/.cargo/bin:/opt/src/software/tools/nvm/versions/node/v24.18.0/bin:$PATH"
        ycmd_build=/root/.vim/bundle/YouCompleteMe/third_party/ycmd/build.py
        jdt_milestone="$(sed -n "s/^JDTLS_MILESTONE = '\([^']*\)'.*/\1/p" "$ycmd_build" | head -1)"
        jdt_stamp="$(sed -n "s/^JDTLS_BUILD_STAMP = '\([^']*\)'.*/\1/p" "$ycmd_build" | head -1)"
        if command -v aria2c >/dev/null 2>&1 && [ -n "$jdt_milestone" ] && [ -n "$jdt_stamp" ]; then
            jdt_package="jdt-language-server-$jdt_milestone-$jdt_stamp.tar.gz"
            jdt_cache=/root/.vim/bundle/YouCompleteMe/third_party/ycmd/third_party/eclipse.jdt.ls/target/cache
            mkdir -p "$jdt_cache"
            timeout --kill-after=10s 300s aria2c --allow-overwrite=true --auto-file-renaming=false --continue=true -x 16 -s 16 -k 1M \
                -d "$jdt_cache" -o "$jdt_package" \
                "https://download.eclipse.org/jdtls/milestones/$jdt_milestone/$jdt_package" || log "WARN: JDT.LS cache prefetch failed"
        fi
        if (cd /root/.vim/bundle/YouCompleteMe && git submodule update --init --recursive && PATH="$ycm_path" timeout --kill-after=10s 900s python3 install.py --all --force-sudo); then
            printf '%s\n' "$ycm_commit" > /root/.vim/bundle/YouCompleteMe/.auto_sync_installed
        else
            log "WARN: YouCompleteMe install failed"
        fi
    fi
fi
EOF_VIM_TOOLS
chmod 0755 /usr/local/sbin/auto_sync_install_vim_tools.sh
if ! pgrep -f '/usr/local/sbin/auto_sync_install_vim_tools.sh' >/dev/null 2>&1; then
    log "start Vim plugin/YouCompleteMe installation in background"
    nohup /usr/local/sbin/auto_sync_install_vim_tools.sh >> /var/log/auto_sync_vim_tools.log 2>&1 &
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

deploy_immich_from_git() {
    mkdir -p /opt/src/software
    mkdir -p /opt/immich/server /opt/immich/web /opt/immich/upload /opt/immich/machine-learning /opt/immich/conf
    if [ ! -s /opt/immich/conf/immich-ml.env ]; then
        cat > /opt/immich/conf/immich-ml.env <<'EOF_IMMICH_ML_ENV'
IMMICH_HOST=0.0.0.0
IMMICH_PORT=3003
IMMICH_LOG_LEVEL=log
MACHINE_LEARNING_CACHE_FOLDER=/opt/immich/machine-learning/.cache
TRANSFORMERS_CACHE=/opt/immich/machine-learning/.cache
EOF_IMMICH_ML_ENV
        chmod 0640 /opt/immich/conf/immich-ml.env
    fi
    mkdir -p /root/.ssh
    ssh-keyscan -T 10 github.com >> /root/.ssh/known_hosts 2>/dev/null || true
    export GIT_SSH_COMMAND='ssh -o StrictHostKeyChecking=accept-new'
    repo=/opt/src/software/immich
    if [ -e "$repo" ] && [ ! -d "$repo/.git" ]; then
        mv "$repo" "${repo}.before-git-$(date +%Y%m%d%H%M%S)" || return 1
    fi
    if [ ! -d "$repo/.git" ]; then
        git clone --branch deploy git@github.com:xiedeacc/immich.git "$repo" || return 1
    fi
    (
        cd "$repo" &&
        git remote set-url origin git@github.com:xiedeacc/immich.git &&
        (git checkout -- deploy.sh 2>/dev/null || true) &&
        (git checkout -- scripts/deploy.sh 2>/dev/null || true) &&
        (git checkout -- install.sh 2>/dev/null || true) &&
        git fetch origin deploy &&
        git checkout deploy &&
        git pull --ff-only origin deploy
    ) || return 1
    immich_commit="$(git -C "$repo" rev-parse HEAD 2>/dev/null || true)"
    immich_marker=/opt/immich/.auto_sync_deploy_commit
    immich_installed=0
    if [ -f /opt/immich/server/dist/main.js ] &&
       [ -f /opt/immich/web/build/index.html ] &&
       [ -x /opt/immich/machine-learning/.venv/bin/python ]; then
        immich_installed=1
    fi
    if [ -n "$immich_commit" ] && [ "$immich_installed" -eq 1 ]; then
        if [ "$(cat "$immich_marker" 2>/dev/null || true)" = "$immich_commit" ]; then
            log "immich deploy branch unchanged; skip rebuild"
            return 0
        fi
        if [ ! -f "$immich_marker" ]; then
            printf '%s\n' "$immich_commit" > "$immich_marker"
            log "immich already installed; record current deploy commit and skip rebuild"
            return 0
        fi
    fi
    patch_immich_deploy_script() {
        script_path="$1"
        command -v python3 >/dev/null 2>&1 || return 0
        python3 - "$script_path" <<'PY_PATCH_IMMICH'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text()
needle = '  cd "$server_dir/node_modules/sharp"\n'
insert = '''  node_gyp_pkg="$(find "$server_dir/node_modules/.pnpm" -maxdepth 1 -type d -name 'node-gyp@*' 2>/dev/null | head -1)"
  if [ -n "$node_gyp_pkg" ]; then
    node_gyp_base="$(basename "$node_gyp_pkg")"
    node_gyp_link="$STAGING_DIR/$node_gyp_base"
    if [ ! -e "$node_gyp_link" ]; then
      ln -s "server/node_modules/.pnpm/$node_gyp_base" "$node_gyp_link" 2>/dev/null || true
    fi
  fi
'''
if insert not in text and needle in text:
    text = text.replace(needle, insert + needle, 1)
text = text.replace(
    'MISE_TRUSTED_CONFIG_PATHS="$BUILD_DIR/mise.toml" MISE_DISABLE_TOOLS=flutter "$TOOL_BIN/mise" //:plugins',
    'MISE_TRUSTED_CONFIG_PATHS="$REPO_DIR/mise.toml:$BUILD_DIR/mise.toml" MISE_DISABLE_TOOLS=flutter "$TOOL_BIN/mise" //:plugins',
)
text = text.replace(
    'UV_PYTHON_INSTALL_DIR="$UV_PYTHON_INSTALL_DIR" UV_LINK_MODE=copy "$UV_BIN" sync --locked --extra cpu --no-dev --compile-bytecode',
    'UV_PYTHON_INSTALL_DIR="$UV_PYTHON_INSTALL_DIR" UV_LINK_MODE=copy "$UV_BIN" sync --extra cpu --no-dev --compile-bytecode',
)
path.write_text(text)
PY_PATCH_IMMICH
    }
    for script in deploy.sh scripts/deploy.sh install.sh; do
        if [ -f "$repo/$script" ]; then
            patch_immich_deploy_script "$repo/$script"
            chmod +x "$repo/$script" 2>/dev/null || true
            immich_node_home="${IMMICH_NODE_HOME:-/opt/src/software/tools/nvm/versions/node/v24.18.0}"
            (cd "$repo" && VERSION="${IMMICH_VERSION:-deploy}" UV_DEFAULT_INDEX="${UV_DEFAULT_INDEX:-https://pypi.tuna.tsinghua.edu.cn/simple}" UV_INDEX_URL="${UV_INDEX_URL:-https://pypi.tuna.tsinghua.edu.cn/simple}" PIP_INDEX_URL="${PIP_INDEX_URL:-https://pypi.tuna.tsinghua.edu.cn/simple}" npm_config_node_gyp="$immich_node_home/lib/node_modules/npm/node_modules/node-gyp/bin/node-gyp.js" bash "$script") || return 1
            [ -z "$immich_commit" ] || printf '%s\n' "$immich_commit" > "$immich_marker"
            return 0
        fi
    done
    log "WARN: immich deploy script not found in $repo"
    return 1
}
setquota -u root 20000000 20000000 0 0 /dev/mmcblk0p2 2>/dev/null || true
for u in tiger git gitlab immich; do
    id "$u" >/dev/null 2>&1 && setquota -u "$u" 1000000 1000000 0 0 / 2>/dev/null || true
done

ensure_zfs_for_gitlab() {
    if ! command -v zpool >/dev/null 2>&1; then
        log "WARN: zpool not installed; cannot ensure GitLab /zfs storage"
        return 0
    fi
    log "ensure /zfs is imported and mounted before GitLab setup"
    for pool in zfs zfs_pool ssd; do
        if ! zpool list -H "$pool" >/dev/null 2>&1; then
            zpool import "$pool" >/dev/null 2>&1 || true
        fi
    done
    zfs mount zfs >/dev/null 2>&1 || zfs mount -a >/dev/null 2>&1 || true
    zpool status zfs >/dev/null 2>&1 || log "WARN: zfs pool is not online"
    if ! findmnt -T /zfs >/dev/null 2>&1; then
        log "WARN: /zfs is not mounted"
    fi
    ls -ald /zfs >/dev/null 2>&1 || true
}

ensure_gitlab_zfs_config() {
    [ -f /etc/gitlab/gitlab.rb ] || return 0
    if ! grep -q '/zfs/gitlab_data' /etc/gitlab/gitlab.rb 2>/dev/null; then
        log "add GitLab /zfs data storage config"
        cat >> /etc/gitlab/gitlab.rb <<'EOF_GITLAB_ZFS'

# Managed by auto_sync NAS deployment: keep GitLab repository/LFS data on /zfs.
gitlab_rails['lfs_enabled'] = true
gitlab_rails['lfs_storage_path'] = "/zfs/gitlab_data/lfs-objects"
git_data_dirs({
   "default" => {"path" => "/zfs/gitlab_data"},
})
gitlab_rails['repository_storages'] = {
  'default' => {
    'path' => '/zfs/gitlab_data',
    'gitaly_address' => 'unix:/var/opt/gitlab/gitaly/gitaly.socket'
  }
}
EOF_GITLAB_ZFS
    fi
    if ! grep -q 'auto_sync GitLab frontend ports' /etc/gitlab/gitlab.rb 2>/dev/null; then
        cat >> /etc/gitlab/gitlab.rb <<'EOF_GITLAB_FRONTEND'

# Managed by auto_sync GitLab frontend ports.
# Workhorse serves nginx on 10080. Puma must not bind TCP 8080 because waiwei-web uses it.
gitlab_rails['gitlab_shell_ssh_port'] = 10022
gitlab_workhorse['listen_network'] = "tcp"
gitlab_workhorse['listen_addr'] = "127.0.0.1:10080"
puma['listen'] = ""
puma['port'] = nil
EOF_GITLAB_FRONTEND
    fi
    if grep -q "gitlab_rails\['gitlab_shell_ssh_port'\]" /etc/gitlab/gitlab.rb 2>/dev/null; then
        sed -i -E "s/^#?[[:space:]]*gitlab_rails\['gitlab_shell_ssh_port'\].*/gitlab_rails['gitlab_shell_ssh_port'] = 10022/" /etc/gitlab/gitlab.rb
    else
        cat >> /etc/gitlab/gitlab.rb <<'EOF_GITLAB_SSH_PORT'

# Managed by auto_sync GitLab SSH clone port.
gitlab_rails['gitlab_shell_ssh_port'] = 10022
EOF_GITLAB_SSH_PORT
    fi
}

ensure_gitlab_repo_compatible() {
    list="/etc/apt/sources.list.d/gitlab_gitlab-ce.list"
    key=/etc/apt/keyrings/gitlab_gitlab-ce-archive-keyring.asc
    mkdir -p /etc/apt/keyrings
    for attempt in 1 2 3 4 5; do
        if curl -fsSL --connect-timeout 30 --retry 5 --retry-delay 5 --retry-all-errors \
            https://packages.gitlab.com/gitlab/gitlab-ce/gpgkey -o "$key"; then
            break
        fi
        log "WARN: GitLab repo key download failed, attempt $attempt"
        sleep $((attempt * 5))
    done
    [ -s "$key" ] || { log "ERROR: GitLab repo key download failed"; return 1; }
    printf 'deb [signed-by=%s] https://packages.gitlab.com/gitlab/gitlab-ce/ubuntu/ noble main\n' "$key" > "$list"
}

ensure_zfs_for_gitlab
installed_gitlab_version="$(dpkg-query -W -f='${Version}' gitlab-ce 2>/dev/null || true)"
if [ "$installed_gitlab_version" != "$GITLAB_CE_VERSION" ]; then
    ensure_gitlab_repo_compatible || exit 1
    apt-get update || exit 1
    apt-get install -y --allow-downgrades "gitlab-ce=$GITLAB_CE_VERSION" || exit 1
fi
ensure_zfs_for_gitlab
mkdir -p /zfs/gitlab_data /zfs/gitlab_data/lfs-objects /zfs/gitlab_data/repositories 2>/dev/null || true
passwd -S git 2>/dev/null || true
if id git >/dev/null 2>&1; then
    git_pw_hash="$(openssl passwd -6 "auto-sync-git-$(date +%s)-$RANDOM" 2>/dev/null || true)"
    [ -z "$git_pw_hash" ] || usermod -p "$git_pw_hash" git 2>/dev/null || true
    rm -f /etc/ssh/sshd_config.d/98-auto-sync-gitlab.conf 2>/dev/null || true
fi
[ -d /zfs/gitlab_data ] && chown git:git /zfs/gitlab_data 2>/dev/null || true
[ -d /zfs/gitlab_data/lfs-objects ] && chown git:git /zfs/gitlab_data/lfs-objects 2>/dev/null || true
[ -d /zfs/gitlab_data/repositories ] && chown git:git /zfs/gitlab_data/repositories 2>/dev/null || true
[ -d /zfs/gitlab_data ] && chmod 2770 /zfs/gitlab_data /zfs/gitlab_data/lfs-objects /zfs/gitlab_data/repositories 2>/dev/null || true
[ -d /etc/gitlab ] && chown -R git:git /etc/gitlab 2>/dev/null || true
[ -d /etc/gitlab ] && chmod 700 /etc/gitlab 2>/dev/null || true
ensure_gitlab_zfs_config
if command -v gitlab-ctl >/dev/null 2>&1; then
    gitlab-ctl reconfigure || log "WARN: gitlab reconfigure failed"
    gitlab-ctl restart || log "WARN: gitlab restart failed"
    gitlab-ctl status || true
fi

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
    tmp="$(mktemp)"
    for root in "$@"; do
        [ -d "$root" ] || continue
        maxdepth_args=()
        case "$root" in
            /zfs/backup|/zfs/backup/*) maxdepth_args=(-maxdepth 4) ;;
        esac
        case "$kind" in
            mysql)
                find "$root" "${maxdepth_args[@]}" -type f \( \
                    -name 'mysql_full_backup_*.sql' -o \
                    -name 'mysql_full_backup_*.sql.gz' -o \
                    -name 'mysql-all.sql' -o \
                    -name 'mysql-all.sql.gz' -o \
                    -name '*mysql*.sql' -o \
                    -name '*mysql*.sql.gz' \
                \) -print0 2>/dev/null >> "$tmp"
                ;;
            postgres)
                find "$root" "${maxdepth_args[@]}" -type f \( \
                    -name 'pg_full_backup_*.sql' -o \
                    -name 'pg_full_backup_*.sql.gz' -o \
                    -name 'postgres-all.sql' -o \
                    -name 'postgres-all.sql.gz' -o \
                    -name '*postgres*.sql' -o \
                    -name '*postgres*.sql.gz' -o \
                    -name '*pg*.sql' -o \
                    -name '*pg*.sql.gz' \
                \) -print0 2>/dev/null >> "$tmp"
                ;;
            *)
                rm -f "$tmp"
                return 1
                ;;
        esac
    done
    score_dumps < "$tmp" | sort -nr | head -1 | cut -f2-
    rm -f "$tmp"
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

ensure_postgresql_ready() {
    if command -v pg_lsclusters >/dev/null 2>&1; then
        pg_lsclusters -h 2>/dev/null | awk '$4 ~ /binaries_missing/ { print $1 }' | while read -r old_version; do
            [ -n "$old_version" ] || continue
            [ -d "/etc/postgresql/$old_version" ] && mv "/etc/postgresql/$old_version" "/etc/postgresql/${old_version}.disabled-by-auto-sync-$(date +%Y%m%d%H%M%S)" 2>/dev/null || true
        done
    fi
    version="$(ls -1 /usr/lib/postgresql 2>/dev/null | sort -V | tail -1)"
    [ -z "$version" ] || fix_postgresql_config_permissions "$version"
    systemctl restart postgresql 2>/dev/null || true
    if command -v pg_lsclusters >/dev/null 2>&1; then
        if ! pg_lsclusters 2>/dev/null | awk '$4 == "online" { found = 1 } END { exit found ? 0 : 1 }'; then
            if [ -n "$version" ]; then
                [ -d "/etc/postgresql/$version/main" ] || pg_createcluster "$version" main --start || true
                fix_postgresql_config_permissions "$version"
                pg_ctlcluster "$version" main start 2>/dev/null || true
            fi
        fi
        if ! pg_lsclusters 2>/dev/null | awk '$3 == "5432" && $4 == "online" { found = 1 } END { exit found ? 0 : 1 }'; then
            if [ -n "$version" ] && [ -f "/etc/postgresql/$version/main/postgresql.conf" ]; then
                pg_ctlcluster "$version" main stop 2>/dev/null || true
                sed -i -E "s/^#?[[:space:]]*port[[:space:]]*=.*/port = 5432/" "/etc/postgresql/$version/main/postgresql.conf"
                fix_postgresql_config_permissions "$version"
                pg_ctlcluster "$version" main start 2>/dev/null || true
            fi
        fi
    fi
    [ -z "$version" ] || fix_postgresql_config_permissions "$version"
    systemctl restart postgresql 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do
        pg_isready -q 2>/dev/null && return 0
        sleep 1
    done
    return 1
}

configure_postgresql_peer_maps() {
    for hba in /etc/postgresql/[0-9]*/main/pg_hba.conf; do
        [ -f "$hba" ] || continue
        dir="$(dirname "$hba")"
        ident="$dir/pg_ident.conf"
        sed -i -E 's/^(local[[:space:]]+all[[:space:]]+all[[:space:]]+peer)([[:space:]].*)?$/\1 map=gitlab/' "$hba"
        touch "$ident"
        grep -Eq '^[[:space:]]*gitlab[[:space:]]+git[[:space:]]+gitlab([[:space:]]|$)' "$ident" || printf '\ngitlab  git  gitlab\n' >> "$ident"
        grep -Eq '^[[:space:]]*gitlab[[:space:]]+tiger[[:space:]]+immich([[:space:]]|$)' "$ident" || printf '\ngitlab  tiger  immich\n' >> "$ident"
    done
    systemctl reload postgresql 2>/dev/null || true
}

prepare_immich_database_extensions() {
    ensure_postgresql_ready >/dev/null 2>&1 || { log "WARN: PostgreSQL is not ready for immich extension check"; return 0; }
    if ! sudo -u postgres psql -Atqc "SELECT 1 FROM pg_database WHERE datname = 'immich'" 2>/dev/null | grep -qx 1; then
        return 0
    fi
    sudo -u postgres psql -d immich -v ON_ERROR_STOP=0 \
        -c "CREATE EXTENSION IF NOT EXISTS vector;" \
        -c "ALTER EXTENSION vector UPDATE;" >/dev/null 2>&1 ||
        log "WARN: immich pgvector extension preparation failed"
}

mysql_has_user_data() {
    command -v mysql >/dev/null 2>&1 || return 1
    count="$(mysql -NBe "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema NOT IN ('mysql','information_schema','performance_schema','sys')" 2>/dev/null || echo 0)"
    [ "${count:-0}" -gt 0 ] 2>/dev/null
}

postgres_has_user_data() {
    ensure_postgresql_ready >/dev/null 2>&1 || return 1
    count="$(sudo -u postgres psql -Atqc "SELECT COUNT(*) FROM pg_database WHERE datistemplate = false AND datname NOT IN ('postgres')" 2>/dev/null || echo 0)"
    [ "${count:-0}" -gt 0 ] 2>/dev/null
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
    case "$kind" in
        mysql)
            if mysql_has_user_data; then
                log "skip mysql restore: existing user data found"
                return 0
            fi
            ;;
        postgres)
            if postgres_has_user_data; then
                log "skip postgres restore: existing user databases found"
                return 0
            fi
            ;;
    esac
    log "restore $kind from $latest"
    case "$kind" in
        mysql)
            systemctl restart mysql 2>/dev/null || systemctl restart mariadb 2>/dev/null || true
            if printf '%s' "$latest" | grep -q '\.gz$'; then zcat "$latest" | mysql; else mysql < "$latest"; fi
            ;;
        postgres)
            ensure_postgresql_ready || log "WARN: postgresql is not ready before restore"
            configure_postgresql_peer_maps
            if printf '%s' "$latest" | grep -q '\.gz$'; then zcat "$latest" | sudo -u postgres psql -v ON_ERROR_STOP=0; else sudo -u postgres psql -v ON_ERROR_STOP=0 -f "$latest"; fi
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
prepare_immich_database_extensions
id tiger >/dev/null 2>&1 || useradd -m -s /bin/bash tiger
mkdir -p /opt/immich/upload
for d in backups encoded-video library profile thumbs upload; do
    mkdir -p "/opt/immich/upload/$d"
    touch "/opt/immich/upload/$d/.immich"
done
chown -R tiger:tiger /opt/immich/upload 2>/dev/null || true
deploy_immich_from_git || log "WARN: immich deploy from git failed"
rm -rf /root/auto_sync_db_dumps /tmp/auto_sync_db_dumps 2>/dev/null || true
if [ "$zfs_woken" = "1" ]; then
    standby_zfs
fi

mkdir -p /opt/auto_sync/logs /usr/local/blog/logs /usr/local/tbox/log /usr/local/waiwei/logs /usr/local/xray/logs /opt/immich/server /opt/immich/upload /opt/immich/machine-learning /opt/immich/conf /opt/user/tiger /home/tiger
for d in backups encoded-video library profile thumbs upload; do
    mkdir -p "/opt/immich/upload/$d"
    touch "/opt/immich/upload/$d/.immich"
done
if [ -d /opt/user/tiger/.halo2 ]; then
    rm -rf /home/tiger/.halo2
    ln -s /opt/user/tiger/.halo2 /home/tiger/.halo2
    chown -h tiger:tiger /home/tiger/.halo2 2>/dev/null || true
fi
python3 - <<'PY_TIGER_LINK_OWNERS'
import os
import pwd
import grp
from pathlib import Path

uid = pwd.getpwnam('tiger').pw_uid
gid = grp.getgrnam('tiger').gr_gid
for entry in Path('/home/tiger').iterdir():
    if entry.is_symlink():
        os.lchown(entry, uid, gid)
PY_TIGER_LINK_OWNERS
for d in /opt/auto_sync /usr/local/halo /usr/local/tbox /opt/immich /opt/user/tiger; do
    [ -e "$d" ] && chown -R tiger:tiger "$d" 2>/dev/null || true
done
for f in \
    /opt/auto_sync/bin/auto_sync \
    /opt/auto_sync/bin/auto_syncd \
    /opt/auto_sync/bin/auto_syncctl \
    /opt/auto_sync/bin/auto_sync_gui \
    /opt/auto_sync/bin/auto_sync_web \
    /usr/local/tbox/bin/tbox_client \
    /usr/local/xray/bin/xray \
    /usr/local/xray/bin/update-geo.sh \
    /usr/local/waiwei/bin/waiwei_web \
    /usr/local/waiwei/bin/waiwei_puller \
    /usr/local/blog/bin/rblog \
    /usr/local/blog/bin/rblog-backup \
    /usr/local/blog/bin/admin/* \
    /usr/local/halo/bin/* \
    /usr/local/bin/vlmcsd
do
    [ -e "$f" ] && chmod a+rx "$f" 2>/dev/null || true
done
find /opt/immich/server/bin /opt/immich/machine-learning/.venv/bin -type f -exec chmod a+rx {} + 2>/dev/null || true
chmod a+rx /opt/src/software/tools/nvm/versions/node/v24.18.0/bin/node 2>/dev/null || true
systemctl daemon-reload
systemctl reset-failed 2>/dev/null || true
rm -f /etc/nginx/sites-enabled/default /etc/nginx/conf.d/default.conf 2>/dev/null || true
for s in mysql postgresql redis-server gitlab-runsvdir gitlab immich-ml auto_sync halo2 immich tbox_client tbox-logrotate.timer waiwei-web waiwei-puller xray rblog rblog-backup.timer nginx cron; do
    restart_if_exists "$s"
done

(crontab -l 2>/dev/null | grep -v '/root/src/share/ubuntu/backup_pg.sh'; echo '0 10 * * 0 /bin/bash /root/src/share/ubuntu/backup_pg.sh > /dev/null 2>&1') | crontab -
(crontab -l 2>/dev/null | grep -v '/root/src/share/ubuntu/backup_mysql.sh'; echo '5 10 * * 0 /bin/bash /root/src/share/ubuntu/backup_mysql.sh > /dev/null 2>&1') | crontab -

echo '--- final states ---'
for s in auto_sync halo2 immich immich-ml tbox_client tbox-logrotate.timer nginx cron mysql postgresql redis-server waiwei-web waiwei-puller xray rblog rblog-backup.timer gitlab-runsvdir gitlab; do
    resolved="$(unit_name "$s" 2>/dev/null || true)"
    if [ -n "$resolved" ]; then
        printf '  %s: ' "$s"; systemctl is-enabled "$resolved" 2>/dev/null | tr -d '\n'; printf ' / '; systemctl is-active "$resolved" 2>/dev/null | tr -d '\n'; echo
    elif [ "$s" = "gitlab" ] && command -v gitlab-ctl >/dev/null 2>&1 && gitlab-ctl status >/dev/null 2>&1; then
        printf '  %s: enabled / active\n' "$s"
    else
        printf '  %s: not-found / inactive\n' "$s"
    fi
done

wait_for_unit_active() {
    name="$1"
    resolved="$(unit_name "$name" 2>/dev/null || true)"
    [ -n "$resolved" ] || return 1
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45; do
        systemctl is-active --quiet "$resolved" && return 0
        state="$(systemctl is-active "$resolved" 2>/dev/null || true)"
        [ "$state" = "activating" ] || [ "$state" = "reloading" ] || break
        sleep 2
    done
    systemctl is-active --quiet "$resolved"
}
wait_for_unit_enabled() {
    name="$1"
    resolved="$(unit_name "$name" 2>/dev/null || true)"
    [ -n "$resolved" ] || return 1
    systemctl is-enabled --quiet "$resolved"
}
wait_for_https_200() {
    url="$1"
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do
        code="$(curl -k -L --max-time 45 -o /dev/null -s -w '%{http_code}' "$url" || true)"
        [ "$code" = "200" ] && return 0
        sleep 10
    done
    log "ERROR: $url did not return HTTP 200; last status: $code"
    return 1
}

required_failed=0
for s in auto_sync halo2 immich immich-ml tbox_client tbox-logrotate.timer nginx cron mysql postgresql redis-server waiwei-web waiwei-puller xray rblog rblog-backup.timer; do
    if ! wait_for_unit_active "$s"; then
        log "ERROR: required service $s is not active"
        required_failed=1
    fi
    if ! wait_for_unit_enabled "$s"; then
        log "ERROR: required service $s is not enabled"
        required_failed=1
    fi
done
if ! pg_isready -q; then
    log "ERROR: PostgreSQL is not accepting connections"
    required_failed=1
fi
if ! command -v gitlab-ctl >/dev/null 2>&1 || ! gitlab-ctl status >/dev/null 2>&1; then
    log "ERROR: required service gitlab is not active"
    required_failed=1
fi
for url in https://code.xiedeacc.com https://unlock-music.xiedeacc.com https://halo.xiedeacc.com https://immich.xiedeacc.com https://blog.xiedeacc.com https://rblog.xiedeacc.com; do
    if ! wait_for_https_200 "$url"; then
        required_failed=1
    fi
done
exit "$required_failed"
'@
Invoke-Remote $remote

if ($errCount -gt 0) { Write-Host "deploy completed with $errCount error(s)"; exit 1 }
Write-Host 'deploy completed cleanly'
